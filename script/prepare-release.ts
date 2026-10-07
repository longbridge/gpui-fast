#!/usr/bin/env bun
// Generate registry-only release manifests without touching upstream files.
import { cpSync, existsSync, mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { extname, relative, resolve, sep } from "node:path";
import { parseArgs } from "node:util";
import { DEST, ROOT, fastName, readConfig, readToml, run, type Manifest, type ReleaseConfig } from "./release-common";

export function versionFromTag(tag: string): string {
  const match = /^v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$/.exec(tag);
  if (!match || match[4]?.split(".").some(part => /^0\d+$/.test(part))) {
    throw new Error("Tag must have the form v0.1.0 or v0.1.0-rc.1");
  }
  if (Number(match[1]) === 0 && Number(match[2]) === 0) {
    throw new Error("The first release version is 0.1.0");
  }
  return tag.slice(1);
}

function snapshotName(name: string): string {
  // gpui_util is distinct from Zed's util crate.
  return "gpui-pre-" + name.replace(/^gpui_/, "").replaceAll("_", "-");
}

export function dependency(key: string, input: any, workspace: Manifest, config: ReleaseConfig, version: string): any {
  if (typeof input === "string") return input;
  let value = structuredClone(input);
  if (value.workspace) {
    delete value.workspace;
    let inherited = structuredClone(workspace.dependencies[key]);
    if (typeof inherited === "string") inherited = { version: inherited };
    const features = [...new Set([...(inherited.features ?? []), ...(value.features ?? [])])];
    delete value.features;
    value = { ...inherited, ...value };
    if (features.length) value.features = features;
  }
  if ("path" in value) {
    delete value.path;
    Object.assign(value, config.crates.includes(key)
      ? { package: fastName(key), version: "=" + version, path: "../" + key }
      : { package: snapshotName(key), version: "=" + config.snapshot_version });
  }
  if ("git" in value) {
    if (!value.version) {
      if (key !== "proptest") throw new Error(`${key}: Git dependency has no registry version`);
      value.version = "1";
    }
    for (const field of ["git", "rev", "branch", "tag"]) delete value[field];
  }
  return value;
}

export function prepare(config: ReleaseConfig, version: string, dest = DEST): void {
  versionFromTag("v" + version);
  const workspace = readToml(resolve(ROOT, "Cargo.toml")).workspace;
  rmSync(dest, { recursive: true, force: true });
  mkdirSync(dest, { recursive: true });
  writeFileSync(resolve(dest, "Cargo.toml"), Bun.TOML.stringify({
    workspace: { resolver: "2", members: config.crates },
  }));
  cpSync(resolve(ROOT, "rust-toolchain.toml"), resolve(dest, "rust-toolchain.toml"));

  for (const name of config.crates) {
    const source = resolve(ROOT, "crates", name);
    const target = resolve(dest, name);
    cpSync(source, target, { recursive: true });
    const manifest = readToml(resolve(source, "Cargo.toml"));
    const pkg = manifest.package;
    Object.assign(pkg, {
      name: fastName(name), version, publish: ["crates-io"],
      edition: workspace.package.edition, repository: config.repository,
      homepage: config.homepage, readme: "README.md", license: "Apache-2.0",
      documentation: `https://docs.rs/${fastName(name)}/${version}/${name}/`,
      description: `${name}: GPUI with gpui-fast rendering and layout optimizations`,
      autoexamples: false, autobenches: false, autotests: false,
      authors: [...new Set([...(pkg.authors ?? []), "Longbridge"])],
      include: ["/src/**", "/resources/**", "/build.rs", "/Cargo.toml", "/README.md", "/LICENSE", "/NOTICE"],
    });
    delete pkg.metadata;
    manifest.lib.name = name;
    for (const field of ["lints", "example", "bench", "test", "dev-dependencies"]) delete manifest[field];
    // Published crates have no workspace to inherit lints from; keep its rustc
    // lints so upstream's cfgs, such as `rust_analyzer`, stay allowed.
    manifest.lints = { rust: workspace.lints.rust };
    for (const group of [manifest, ...Object.values(manifest.target ?? {})] as Manifest[]) {
      delete group["dev-dependencies"];
      for (const section of ["dependencies", "build-dependencies"]) {
        if (group[section]) group[section] = Object.fromEntries(
          Object.entries(group[section]).map(([key, value]) => [key, dependency(key, value, workspace, config, version)]),
        );
      }
    }
    // zed-scap allows windows-capture 1.5, whose settings API is incompatible.
    // A direct registry constraint also reaches consumers without a root patch.
    if (name === "gpui") {
      manifest.target ??= {};
      const windows = manifest.target['cfg(target_os = "windows")'] ??= {};
      windows.dependencies ??= {};
      windows.dependencies["windows-capture"] = {
        version: config.windows_capture_version, optional: true,
      };
      manifest.features["screen-capture"].push("dep:windows-capture");
    }
    writeFileSync(resolve(target, "Cargo.toml"),
      "# Modified by Longbridge for gpui-fast publication.\n" + Bun.TOML.stringify(manifest));
    cpSync(resolve(ROOT, "LICENSE"), resolve(target, "LICENSE"));
    const notices = [resolve(ROOT, "NOTICE"), resolve(source, "NOTICE")]
      .filter(existsSync).map(path => readFileSync(path, "utf8"));
    if (notices.length) writeFileSync(resolve(target, "NOTICE"), notices.join("\n\n"));
    writeFileSync(resolve(target, "README.md"),
      `# ${fastName(name)}\n\nGPUI with gpui-fast rendering and layout optimizations.\n\n`
      + `Source and documentation: [${config.repository}](${config.repository}).\n\n`
      + "Forked from Zed's GPUI; distributed under Apache-2.0.\n\n"
      + `Use the core as \`gpui = { package = "gpui-fast", version = "${version}" }\` to preserve paths emitted by its macros.\n`);
  }

  // cbindgen must read bundled inputs rather than a sibling registry package.
  const apple = resolve(dest, "gpui_apple");
  const bundled = resolve(apple, "src/fast/release_gpui/src");
  mkdirSync(bundled, { recursive: true });
  for (const file of ["scene.rs", "geometry.rs", "color.rs", "window.rs", "platform.rs"]) {
    cpSync(resolve(ROOT, "crates/gpui/src", file), resolve(bundled, file));
  }
  for (const directory of ["window", "platform"]) {
    cpSync(resolve(ROOT, "crates/gpui/src", directory), resolve(bundled, directory), { recursive: true });
  }
  const build = resolve(apple, "build.rs");
  const original = readFileSync(build, "utf8");
  const needle = '.join("../gpui")';
  if (original.split(needle).length !== 2) {
    throw new Error("gpui_apple build script changed; review bundled cbindgen inputs");
  }
  writeFileSync(build, "// Modified for gpui-fast releases: bundle cbindgen inputs inside this package.\n"
    + original.replace(needle, '.join("src/fast/release_gpui")'));

  // Apache-2.0 section 4(b) requires change notices in distributed copies.
  const upstream = readToml(resolve(ROOT, "UPSTREAM"));
  const changed = run([
    "git", "diff", "--name-only", upstream.import_commit, "--", ...config.crates.map(name => "crates/" + name),
  ], true).trim().split(/\r?\n/).filter(Boolean);
  for (const filename of changed) {
    const parts = relative(resolve(ROOT, "crates"), resolve(ROOT, filename)).split(sep);
    const distributed = [resolve(dest, ...parts)];
    if (parts[0] === "gpui" && parts[1] === "src") distributed.push(resolve(bundled, ...parts.slice(2)));
    for (const file of distributed) {
      if (existsSync(file) && statSync(file).isFile() && [".rs", ".metal", ".wgsl", ".hlsl"].includes(extname(file))) {
        writeFileSync(file, "// Modified by Longbridge for gpui-fast.\n" + readFileSync(file, "utf8"));
      }
    }
  }
  console.log(`Prepared ${config.crates.length} crates at ${dest}, version ${version}`);
}

if (import.meta.main) {
  try {
    const { values } = parseArgs({ args: Bun.argv.slice(2), options: { tag: { type: "string" }, help: { type: "boolean" } } });
    if (values.help) console.log("Usage: bun script/prepare-release.ts --tag v0.1.0");
    else {
      if (!values.tag) throw new Error("--tag is required (first release: v0.1.0)");
      prepare(readConfig(), versionFromTag(values.tag));
    }
  } catch (error) {
    console.error(error instanceof Error ? error.message : error);
    process.exitCode = 1;
  }
}
