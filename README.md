# gpui-fast release workspace for testing Allsum (not for merging)

Generated with `bun script/prepare-release.ts --tag v0.1.7` from
`fast-wheel-hover-freeze` (8d081df: #56 + #57, on top of #48 merged into
main) plus the `fast/layout.rs` change of #55 (c826472).

Allsum's test branch patches the crates.io `gpui-fast*` packages with this
branch. Delete it once the PRs are merged or the test is over.
