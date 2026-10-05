import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
export const DEST = resolve(ROOT, "target/release-workspace");

export interface ReleaseConfig {
  snapshot_version: string;
  repository: string;
  homepage: string;
  windows_capture_version: string;
  crates: string[];
}

// Cargo manifests contain arbitrary metadata and platform-specific tables.
export type Manifest = Record<string, any>;

export function readToml(path: string): Manifest {
  return Bun.TOML.parse(readFileSync(path, "utf8"));
}

export function readConfig(): ReleaseConfig {
  return readToml(resolve(ROOT, "release.toml")) as ReleaseConfig;
}

export function run(args: string[], capture = false): string {
  const result = Bun.spawnSync(args, {
    cwd: ROOT,
    stdin: "inherit",
    stdout: capture ? "pipe" : "inherit",
    stderr: "inherit",
  });
  if (result.error) throw result.error;
  if (result.exitCode !== 0) {
    throw new Error(`${args[0]} ${args[1]} exited with status ${result.exitCode}`);
  }
  return capture ? result.stdout.toString("utf8") : "";
}

export function fastName(name: string): string {
  return name === "gpui" ? "gpui-fast" : name.replace(/^gpui_/, "gpui-fast-").replaceAll("_", "-");
}
