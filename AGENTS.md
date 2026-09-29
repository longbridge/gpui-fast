# gpui-fast for agents

Read [CLAUDE.md](CLAUDE.md) and [docs/upstream-sync.md](docs/upstream-sync.md)
before changing anything. In short:

- **Everything gpui-fast changes relative to the baseline GPUI** (the import
  commit in `UPSTREAM`) **is implemented in `crates/<crate>/src/fast/`**:
  functions, types, algorithms, bookkeeping, tests and the comments that
  explain them. Methods of upstream types go in `impl` blocks inside `fast/`.
- **Upstream files only hold hooks**: a field typed by a `fast/` struct, a
  one-line call into `fast/`, a body that only forwards to `fast/`, a
  visibility bump, a `#[path = "fast/<file>.rs"]` redirect. No logic, and no
  explanatory comments beyond what a hook needs to be understood.
- **Outside `fast/`, name fast code by its full path at the point of use**
  (`crate::fast::retained::RetainedState::new(cx)`), never through a `use`
  line. The only `use` of `fast` outside `fast/` is `gpui.rs` exporting a
  `test-support` item (`pub use fast::stats::LayoutStats;`). Inside `fast/`,
  `use` lines are fine; never glob imports.
- **No new public API**, and `script/check-upstream` must pass.
- **Changes reach `main` through pull requests** from a topic branch; never
  commit to `main` directly.
