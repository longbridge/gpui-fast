# gpui-fast

A fork of Zed's GPUI (`crates/gpui` and the Zed crates it depends on) that we
keep merging upstream changes into. To keep those merges easy:

- **Put gpui-fast's code in `crates/<crate>/src/fast/`**, one file per topic.
  New work goes in a new `crates/gpui/src/fast/<topic>.rs` (or `fast/<topic>/`);
  its tests go in `crates/gpui/src/fast/tests/<topic>.rs`, or at the bottom
  of the topic's file when they need its private items.
- **Upstream files only get small hooks**: one field holding a `fast/` struct,
  one-line calls or forwarding method bodies, `pub(crate)` visibility bumps,
  `mod` lines, or a `#[path = "fast/<file>.rs"]` redirect to a rewrite.
  No new types, algorithms, tests or explanatory comments in upstream files,
  and no reformatting of upstream code. Methods on upstream types live in
  `impl` blocks in `fast/`.
- **Outside `fast/`, name fast code by its full path where it is used**:
  `crate::fast::<topic>::Name`, never a `use crate::fast::…` line, so every
  hook shows where its code lives. The one exception is `gpui.rs` exporting a
  `test-support` item (`pub use fast::stats::LayoutStats;`). Inside `fast/`,
  `use` lines are fine; globs never are, `use super::*` included. Call a
  method of a `fast/` type by its path too,
  `crate::fast::layout_key::WindowLayout::end_frame(&mut self.fast_layout)`,
  not `self.fast_layout.end_frame()`.
- **No new public API.** The public API stays upstream's. What tests and
  `gpui_perf` need (`LayoutStats`, `Window::layout_stats`,
  `set_view_retention`) is `#[cfg(any(test, feature = "test-support"))]`.
  The few forced differences are listed in `docs/upstream-sync.md`
  ("Where our API differs from upstream's").
- **New files only inside `fast/` or in our own crates** (`crates/gpui_perf`).
- **Run `script/check-upstream` before committing.** It fails when a change to an
  upstream file is more than a hook.
- **Never commit to `main` directly; every change goes through a pull
  request.** Work on a topic branch off `origin/main`, and when the change is
  ready to merge, push the branch and open a PR.

Upstream directories and the commit they came from are in `UPSTREAM`. The full
rules, the check and the upstream sync procedure are in `docs/upstream-sync.md`.
