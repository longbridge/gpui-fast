import { expect, test } from "bun:test";
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import { dependency } from "./prepare-release";
import { validate } from "./publish-release";
import { fastName, readConfig, readToml } from "./release-common";

const config = readConfig();

test("publication validates caret sibling requirements and rejects exact pins", () => {
  const root = mkdtempSync(resolve(tmpdir(), "gpui-fast-validate-"));
  const release = { ...config, crates: ["gpui_macros", "gpui_platform"] };
  const manifests: Record<string, any> = {};
  try {
    for (const crate of release.crates) {
      const dir = resolve(root, crate);
      mkdirSync(resolve(dir, "src"), { recursive: true });
      writeFileSync(resolve(dir, "src/lib.rs"), "");
      writeFileSync(resolve(dir, "README.md"), "Release fixture");
      writeFileSync(resolve(dir, "LICENSE"), "Apache-2.0");
      manifests[crate] = {
        package: { name: fastName(crate), license: "Apache-2.0", description: "Release fixture",
          version: "0.1.6", edition: "2021", publish: ["crates-io"],
          repository: config.repository, homepage: config.homepage },
      };
    }
    manifests.gpui_platform.dependencies = {
      gpui_macros: dependency("gpui_macros", { path: "unused" }, {}, config, "0.1.6"),
    };
    for (const crate of release.crates) {
      writeFileSync(resolve(root, crate, "Cargo.toml"), Bun.TOML.stringify(manifests[crate]));
    }
    expect(validate(release, root)).toBe("0.1.6");
    manifests.gpui_platform.dependencies.gpui_macros.version = "=0.1.6";
    writeFileSync(resolve(root, "gpui_platform/Cargo.toml"), Bun.TOML.stringify(manifests.gpui_platform));
    expect(() => validate(release, root)).toThrow("gpui_platform: inconsistent sibling version");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("release dependencies retain inherited features and pin support snapshots", () => {
  const workspace = { dependencies: {
    gpui: { path: "crates/gpui", features: ["test-support"] },
    util: { path: "crates/util" },
  } };
  const input = { workspace: true, optional: true, features: ["test-support", "screen-capture"] };
  expect(dependency("gpui", input, workspace, config, "0.1.5")).toEqual({
    package: "gpui-fast", version: "^0.1.5", path: "../gpui", optional: true,
    features: ["test-support", "screen-capture"],
  });
  expect(input.workspace).toBe(true);
  const requirement = dependency("gpui", { path: "unused" }, {}, config, "0.1.5").version;
  expect(Bun.semver.satisfies("0.1.6", requirement)).toBe(true);
  expect(Bun.semver.satisfies("0.2.0", requirement)).toBe(false);
  expect(Bun.semver.satisfies("0.1.4", requirement)).toBe(false);
  expect(dependency("util", { workspace: true }, workspace, config, "0.1.5")).toEqual({
    package: "gpui-pre-util", version: "=" + config.snapshot_version,
  });
});

test("Cargo can update only gpui-fast while its platform remains locked", () => {
  const root = mkdtempSync(resolve(tmpdir(), "gpui-fast-update-"));
  const patches: Record<string, unknown> = {};
  // Model published manifests: Cargo strips sibling paths from registry packages.
  const publishedDependency = (key: string, version: string) => {
    const value = dependency(key, { path: "unused" }, {}, config, version);
    delete value.path;
    return value;
  };
  const writePackage = (name: string, version: string, dependencies = {}, source = "") => {
    const dir = resolve(root, name + "-" + version);
    mkdirSync(resolve(dir, "src"), { recursive: true });
    writeFileSync(resolve(dir, "Cargo.toml"), Bun.TOML.stringify({
      package: { name, version, edition: "2021" }, dependencies,
    }));
    writeFileSync(resolve(dir, "src/lib.rs"), source);
    patches[name + "-" + version.replaceAll(".", "-")] = { package: name, path: dir };
  };
  const writeConsumer = () => {
    writeFileSync(resolve(root, "Cargo.toml"), Bun.TOML.stringify({
      package: { name: "consumer", version: "0.0.0", edition: "2021" },
      dependencies: { gpui: { package: "gpui-fast", version: "0.1.5" },
        platform: { package: "gpui-fast-platform", version: "0.1.5" } },
      patch: { "crates-io": patches },
    }));
  };
  const cargo = (...args: string[]) => {
    const result = Bun.spawnSync(["cargo", ...args, "--offline"], {
      cwd: root, stdout: "pipe", stderr: "pipe",
    });
    expect(result.exitCode, result.stdout.toString() + result.stderr.toString()).toBe(0);
  };
  const versions = (name: string) => readToml(resolve(root, "Cargo.lock")).package
    .filter((pkg: any) => pkg.name === name).map((pkg: any) => pkg.version);
  try {
    mkdirSync(resolve(root, "src"));
    writeFileSync(resolve(root, "src/lib.rs"), "pub fn value() -> gpui::Value { platform::value() }");
    writePackage("gpui-fast-macros", "0.1.5");
    writePackage("gpui-fast", "0.1.5", { macros: publishedDependency("gpui_macros", "0.1.5") }, "pub struct Value;");
    writePackage("gpui-fast-platform", "0.1.5", { gpui: publishedDependency("gpui", "0.1.5") },
      "pub fn value() -> gpui::Value { gpui::Value }");
    writeConsumer();
    cargo("generate-lockfile");
    expect(versions("gpui-fast")).toEqual(["0.1.5"]);

    writePackage("gpui-fast-macros", "0.1.6");
    writePackage("gpui-fast", "0.1.6", { macros: publishedDependency("gpui_macros", "0.1.6") }, "pub struct Value;");
    writeConsumer();
    cargo("update", "-p", "gpui-fast");
    expect(versions("gpui-fast")).toEqual(["0.1.6"]);
    expect(versions("gpui-fast-macros")).toEqual(["0.1.6"]);
    expect(versions("gpui-fast-platform")).toEqual(["0.1.5"]);
    cargo("check", "--locked");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
