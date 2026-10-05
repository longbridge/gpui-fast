# Publishing gpui-fast

The release workflow publishes nine crates to crates.io: `gpui-fast`,
`gpui-fast-macros`, `gpui-fast-apple`, `gpui-fast-wgpu`, `gpui-fast-macos`,
`gpui-fast-linux`, `gpui-fast-windows`, `gpui-fast-web` and `gpui-fast-platform`.
The macro crate preserves upstream's generated `gpui::` paths, including when
an application also depends on GPUI Kit. Unmodified support crates use the
published `gpui-pre-*` snapshot pinned in `release.toml`.

## Versions and triggers

The first release is `v0.1.0`. Every release version comes from its Git tag;
neither upstream Cargo manifests nor `release.toml` store the release version.
Tags must point to commits already merged into `main`.

1. Configure the repository Actions secret `CARGO_REGISTRY_TOKEN` with a
   crates.io token permitted to publish all nine package names. The first
   publication also needs permission to create those packages.
2. Merge the release workflow and subsequent release changes through a PR.
3. Create and push a tag on the desired commit:

   ```sh
   git tag v0.1.0
   git push origin v0.1.0
   ```

Pushing `v*` runs validation on Linux, macOS and Windows, checks the Web
backend on nightly (matching upstream's Web example), and then publishes in
dependency order. Cargo verifies each unpacked package before uploading it
and waits until its version is available in the registry index.

The workflow can also be run manually against an existing tag. `dry_run`
defaults to true and performs compilation, package-file and dependency-order
checks without uploading. It does not run `cargo publish --dry-run`: that
cannot resolve unpublished sibling packages on the first release.

## Release-only changes

`script/prepare-release --tag v0.1.0` generates `target/release-workspace`.
Only these generated copies receive package names, exact sibling versions,
registry dependency substitutions, repository/homepage URLs, descriptions,
READMEs and license files. Original manifests and Rust source stay unchanged.
Published metadata retains upstream authors and adds Longbridge;
the bundled license retains Zed's copyright and credits Longbridge's contributions.
Generated copies of changed source files carry modification notices, and any
root or crate NOTICE files are included in the corresponding package.
Release manifests do not inherit workspace Git patches. The Apple package
bundles the GPUI source inputs used by cbindgen so its shader build does not
depend on a sibling checkout directory.

Applications should alias the released libraries to their original names:

```toml
[dependencies]
gpui = { package = "gpui-fast", version = "0.1.0" }
gpui_platform = { package = "gpui-fast-platform", version = "0.1.0", features = ["wayland", "x11"] }
```

Published crates cannot themselves change which GPUI version an existing
GPUI Kit dependency uses. GPUI Kit also needs to select the same fast core
before their GPUI types can be mixed.

## Local validation

Run from the repository root:

```sh
python3 -m venv target/release-tools
target/release-tools/bin/pip install tomlkit==0.13.3
target/release-tools/bin/python script/prepare-release --tag v0.1.0
target/release-tools/bin/python script/publish-release
cargo check --manifest-path target/release-workspace/Cargo.toml --workspace --lib --all-features
```

Preparation replaces the previous generated directory, including its build
outputs. Inspect `release.toml` when syncing upstream: new path dependencies
need either a compatible published snapshot or a new entry in the release
set; Git dependencies need compatible registry releases.

If publication stops midway, rerun the workflow with the same tag and
`dry_run` disabled. Already published packages are skipped only when their
archive checksum matches; yanked versions and differing contents fail and
require a new version tag. Registry or authentication errors stop the run.
