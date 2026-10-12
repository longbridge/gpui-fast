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

Changes to release configuration also run the validation jobs on pull requests,
using version `0.1.0` for rehearsal; the publish job is disabled for PRs.
Validation includes `cargo publish --workspace --dry-run`, which packages and
builds all nine archives against Cargo's temporary registry without uploading
anything. Packaging the entire workspace supports unpublished sibling packages.
The `release-crates` Actions artifact contains the nine rehearsed `.crate`
archives for inspection.

After the workflow is merged into `main`, it can also be run manually against
an existing tag. `dry_run` defaults to true and runs the same rehearsal. Pushing
a release tag starts a real release automatically, so use PR validation or
local rehearsal to test before creating that tag.

## Release-only changes

`bun script/prepare-release.ts --tag v0.1.0` generates `target/release-workspace`.
Only these generated copies receive package names, compatible sibling versions,
registry dependency substitutions, repository/homepage URLs, descriptions,
READMEs and license files. Original manifests and Rust source stay unchanged.
Published metadata retains upstream authors and adds Longbridge;
the bundled license retains Zed's copyright and credits Longbridge's contributions.
Generated copies of changed source files carry modification notices, and any
root or crate NOTICE files are included in the corresponding package.
Release manifests do not inherit workspace Git patches. The Apple package
bundles the GPUI source inputs used by cbindgen so its shader build does not
depend on a sibling checkout directory.
The core's Windows screen-capture feature constrains `windows-capture` to
the registry version in `release.toml`; version 1.5 changed the constructor
used by `zed-scap` and cannot compile that backend.

Sibling dependencies use Cargo caret requirements such as `^0.1.6`. This allows
`cargo update -p gpui-fast` to update the core and its required dependencies
while compatible platform packages remain locked. For `0.1.x`, the requirement
allows newer patch releases but excludes `0.2.0`; breaking changes must use a
new compatibility series. Support snapshots (`gpui-pre-*`) remain exactly pinned.
Versions already published with exact sibling requirements cannot be changed.
Consumers must first update the full `gpui-fast-*` set to a release using caret
requirements before subsequent core-only updates can work.

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
bun test script/release.test.ts
bun script/prepare-release.ts --tag v0.1.0
bun script/publish-release.ts
cargo check --manifest-path target/release-workspace/Cargo.toml --workspace --lib --all-features
bun script/publish-release.ts --dry-run
```

Preparation replaces the previous generated directory, including its build
outputs. Inspect `release.toml` when syncing upstream: new path dependencies
need either a compatible published snapshot or a new entry in the release
set; Git dependencies need compatible registry releases.

If publication stops midway, rerun the workflow with the same tag and
`dry_run` disabled. Already published packages are skipped only when their
archive checksum matches; yanked versions and differing contents fail and
require a new version tag. Registry or authentication errors stop the run.

CI pins Bun 1.4.2. The release scripts use Bun's built-in TOML parser and
serializer and require no npm packages.
