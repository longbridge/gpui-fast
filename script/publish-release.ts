#!/usr/bin/env bun
// Publish a prepared release in dependency order, safely resuming partial runs.
import { createHash } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
import { resolve } from "node:path";
import { parseArgs } from "node:util";
import { DEST, readConfig, readToml, run, type Manifest, type ReleaseConfig } from "./release-common";

interface RegistryVersion { yanked: boolean; checksum: string }

export async function registryVersion(name: string, version: string): Promise<RegistryVersion | null> {
  const response = await fetch(`https://crates.io/api/v1/crates/${name}/${version}`, {
    headers: { "User-Agent": "gpui-fast-release (https://github.com/longbridge/gpui-fast)" },
    signal: AbortSignal.timeout(30_000),
  });
  if (response.status === 404) return null;
  if (!response.ok) throw new Error(`crates.io returned HTTP ${response.status} for ${name}@${version}`);
  const data = await response.json() as { version: RegistryVersion };
  return data.version;
}

function cargo(command: string, crate: string, ...options: string[]): void {
  run(["cargo", command, "--manifest-path", resolve(DEST, crate, "Cargo.toml"), "--allow-dirty", ...options]);
}

function requireCondition(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

export function validate(config: ReleaseConfig, dest = DEST): string {
  const seen = new Set<string>();
  const versions = new Set<string>();
  for (const crate of config.crates) {
    const manifest = readToml(resolve(dest, crate, "Cargo.toml"));
    const pkg = manifest.package;
    versions.add(pkg.version);
    requireCondition(JSON.stringify(pkg.publish) === '["crates-io"]', `${crate}: unexpected publish registry`);
    requireCondition(pkg.repository === config.repository && pkg.homepage === config.homepage, `${crate}: unexpected release URLs`);
    for (const group of [manifest, ...Object.values(manifest.target ?? {})] as Manifest[]) {
      for (const section of ["dependencies", "build-dependencies"]) {
        for (const dep of Object.values(group[section] ?? {}) as any[]) {
          if (typeof dep === "string") continue;
          requireCondition(!("git" in dep) && !("workspace" in dep) && dep.version, `${crate}: unresolved dependency`);
          if ("path" in dep) {
            requireCondition(seen.has(dep.package), `${crate}: release order is not topological`);
            requireCondition(dep.version === "^" + pkg.version, `${crate}: inconsistent sibling version`);
          }
        }
      }
    }
    seen.add(pkg.name);
    const files = run([
      "cargo", "package", "--manifest-path", resolve(dest, crate, "Cargo.toml"), "--list", "--allow-dirty",
    ], true).trim().split(/\r?\n/).map(file => file.replaceAll("\\", "/"));
    requireCondition(files.includes("LICENSE") && files.includes("README.md"), `${crate}: missing license or README`);
    requireCondition(!files.some(file => /^(examples|benches|tests)\//.test(file)), `${crate}: development files included in package`);
    if (crate === "gpui") {
      for (const resource of ["resources/windows/gpui.rc", "resources/windows/gpui.manifest.xml"]) {
        requireCondition(files.includes(resource), `${crate}: missing Windows build resource ${resource}`);
      }
    }
  }
  requireCondition(versions.size === 1, "Release packages must have exactly one version");
  return [...versions][0];
}

export async function publish(config: ReleaseConfig, version: string): Promise<void> {
  // Check every existing version before uploading anything. Matching archive
  // checksums permit resuming a partial release without accepting different code.
  const existing = new Set<string>();
  for (const crate of config.crates) {
    const name = readToml(resolve(DEST, crate, "Cargo.toml")).package.name;
    const published = await registryVersion(name, version);
    if (!published) continue;
    if (published.yanked) throw new Error(`${name}@${version} is yanked; choose a new tag`);
    cargo("package", crate, "--no-verify");
    const archive = resolve(DEST, "target/package", `${name}-${version}.crate`);
    if (createHash("sha256").update(readFileSync(archive)).digest("hex") !== published.checksum) {
      throw new Error(`${name}@${version} already exists with different contents; choose a new tag`);
    }
    existing.add(crate);
  }
  for (const crate of config.crates) {
    if (existing.has(crate)) {
      console.log(`Already published with identical contents: ${crate}@${version}`);
      continue;
    }
    // Cargo verifies the unpacked archive and waits for index availability.
    cargo("publish", crate, "--registry", "crates-io");
  }
}

if (import.meta.main) {
  try {
    const { values } = parseArgs({ args: Bun.argv.slice(2), options: {
      publish: { type: "boolean" }, "dry-run": { type: "boolean" }, help: { type: "boolean" },
    } });
    if (values.help) console.log("Usage: bun script/publish-release.ts [--dry-run | --publish]");
    else {
      if (values.publish && values["dry-run"]) throw new Error("--publish and --dry-run are mutually exclusive");
      const config = readConfig();
      const version = validate(config);
      // Workspace packaging needs a complete lockfile to resolve unpublished
      // transitive siblings, including when run immediately after preparation.
      if ((values.publish || values["dry-run"]) && !existsSync(resolve(DEST, "Cargo.lock"))) {
        run(["cargo", "generate-lockfile", "--manifest-path", resolve(DEST, "Cargo.toml")]);
      }
      if (values.publish) await publish(config, version);
      else if (values["dry-run"]) run([
        "cargo", "publish", "--manifest-path", resolve(DEST, "Cargo.toml"),
        "--workspace", "--dry-run", "--allow-dirty", "--registry", "crates-io",
      ]);
      else console.log(`Validated packaging and dependency order for ${config.crates.length} crates at ${version}`);
    }
  } catch (error) {
    console.error(error instanceof Error ? error.message : error);
    process.exitCode = 1;
  }
}
