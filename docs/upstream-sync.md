# Keeping upstream files upstream

gpui-fast is a fork of GPUI, the UI framework in Zed's `crates/gpui`, together
with the Zed crates it depends on. Zed keeps changing those crates, and we want
to take their changes with a merge that mostly resolves itself. That only works
if our changes to upstream's files stay small: a file we rewrote conflicts on
every sync, a file with a one-line hook almost never does.

So gpui-fast's code lives in files upstream doesn't have, and upstream's files
only hold the hooks that call into it.

## The rules

1. **gpui-fast's logic lives in `crates/<crate>/src/fast/`.** One file per
   topic: `crates/gpui/src/fast/retained.rs`, `fast/dependencies.rs`,
   `fast/layout.rs`, and so on. A new topic gets a new `fast/<topic>.rs`, or
   `fast/<topic>/` when it needs more than one file. Other upstream crates get
   their own `src/fast/` when they need one. Tests of what we add go in
   `crates/gpui/src/fast/tests/<topic>.rs` or at the bottom of the topic's file.
2. **Upstream files only hold small hooks, and every hook names `fast`:**
   - one field holding the topic's state, typed as a struct defined in `fast/`
     (`pub(crate) fast_layout: crate::fast::layout_key::WindowLayout`) and
     initialized by its path (`crate::fast::layout_key::WindowLayout::default()`);
   - a one-line call into `crate::fast::...`, or a method whose body only
     forwards to one. The call names the path even where a method would read
     shorter: `crate::fast::dependencies::note_notify(&mut self.entities, id)`,
     not `self.entities.note_notify(id)`, and
     `crate::fast::dependencies::StateVersion::bump(&state.version)`, not
     `state.version.bump()`. Whoever merges upstream can then tell every line
     of ours from upstream's at a glance;
   - a visibility bump (`fn` to `pub(crate) fn`) so a `fast/` module can reach
     upstream's items. Methods of upstream types can be defined in an
     `impl Window { ... }` block inside a `fast/` file for `fast/` code to
     call, but an upstream file calls a free function in `fast/` instead, so
     the call site shows where the code lives;
   - where no path fits — a field, parameter or local of a plain type that a
     hook threads through upstream code — an identifier named `fast_...`
     (`fast_layout_key: u64`);
   - `mod` lines;
   - a `#[path = "fast/<file>.rs"] mod <name>;` redirect when we replaced a
     whole upstream file with our own rewrite. The upstream file then stays
     exactly as upstream has it, unused.
3. **No new types, algorithms, bookkeeping or tests in upstream files.** No
   reformatting, reordering or renaming of upstream code either: code we don't
   need to change stays byte-for-byte upstream's.
4. **Name fast code by its full path.** Outside `fast/`, write
   `crate::fast::<topic>::Name` where it is used, never a
   `use crate::fast::…` line, so every hook shows where its code lives. The
   one exception is `gpui.rs` exporting a `test-support` item one at a time
   (rule 5). Inside `fast/`, listing names in a `use` line is fine. Glob
   imports or re-exports of `fast` (`pub use fast::*`,
   `use crate::fast::layout::*`) are never allowed.
5. **No new public API.** gpui-fast changes how GPUI draws, not what it offers:
   its public API is upstream's. What tests and `gpui_perf` need to measure or
   switch retained mode is compiled only under `test-support`, and exported
   from `gpui.rs` one item at a time:
   `#[cfg(any(test, feature = "test-support"))] pub use fast::stats::LayoutStats;`.
6. **New files only inside `fast/`, or in our own crates** such as
   `crates/gpui_perf` (benchmarks, examples and the frame-measuring app).
   Documentation goes in `docs/`.

Which directories are upstream's is recorded in [`UPSTREAM`](../UPSTREAM) at
the repository root: every directory in `crates/` except `gpui_perf`, and
`tooling/perf`.

## Checking

```sh
script/check-upstream                   # compare with upstream as imported into our history
script/check-upstream --zed ~/github/zed  # compare with a zed checkout at the recorded commit
script/check-upstream --report          # print the table without failing
script/check-upstream --all             # also list the changed files that pass
```

The script compares every file in the upstream directories with upstream's
version of it: by default `git show <import_commit>:<path>` from our own history,
or with `--zed` the same path in a zed checkout at `zed_commit`. It checks the
working tree, so run it before committing. It fails when:

- a file outside `src/fast/` is added to, or removed from, an upstream
  directory (`LICENSE*` files excepted);
- a hunk of a changed upstream file adds more than 8 lines (`--max-hunk`);
- a changed upstream file adds more than 40 lines (`--max-added`) or removes
  more than 20 (`--max-removed`);
- a hunk of a changed upstream `.rs` file adds lines none of which names
  `fast`: a path such as `crate::fast::...`, `mod fast;` or
  `#[path = "fast/..."]`, or an identifier starting with `fast_`. One mention
  covers the hunk, since rustfmt may spread one call over several lines. Hunks
  that only remove lines, or only change `use` declarations or blank lines,
  are exempt;
- a line added to an upstream `.rs` file calls a method on a value named
  `fast_...` (`self.fast_layout.end_frame()`): the name marks the hunk but
  hides which module the method is in, so the hook calls it by its path,
  `crate::fast::layout_key::WindowLayout::end_frame(&mut self.fast_layout)`;
- a binary file differs;
- any file glob-imports from `fast` (`use ...fast::*`,
  `use ...fast::<topic>::*`), or a file inside `fast/` has a glob import of
  any kind, `use super::*` in a test module included;
- a file outside `fast/` has a `use` of `fast` at all (`use crate::fast::…`,
  `pub(crate) use …`): only a public `pub use fast::…` export passes.

Apart from the glob rules, files under any `src/fast/` directory are never
checked. A line that differs from upstream's only by a visibility bump
(`pub(crate)`, `pub(super)`) is a hook by definition: it is counted in the
table's `pub(crate)` column and not against any budget.

A justified exception goes in `script/upstream-allowlist`: one line per path or
glob, optionally raising the budgets (`hunk=N`, `added=N`, `removed=N`) or
allowing any change (`any`), always with a reason. The usual reasons are a hub
file with many one-line hooks, and an upstream body replaced by a `fast/`
implementation it now forwards to:

```text
crates/gpui/src/view.rs removed=210 # ViewElement's cache-by-bounds is replaced by fast::retained's retained views
```

Removed lines deserve the most care: upstream's changes to code we deleted
conflict on every sync, and have to be ported into `fast/` by hand.

### Where our API differs from upstream's

The check cannot see a change to what upstream's public types offer, so the
few places where gpui-fast's differs from upstream's are listed here. Each is
forced by what `fast/` keeps; anything not listed here is upstream's API
unchanged, and a new entry needs as good a reason.

- `GlobalElementId` carries the hash of its path next to the path
  (`fast::global_id::PathHash`), so ids compare and hash in constant time.
  It no longer implements `DerefMut`: changing the path in place would leave
  the hash stale. `Default`, `PartialEq`, `Eq` and `Hash` are implemented in
  `fast/global_id.rs` instead of derived, with upstream's meaning.
- `ViewElement`'s `Element::RequestLayoutState` and `PrepaintState` are
  `fast::retained::ViewLayoutState` and `ViewPrepaintState`, opaque types, in
  place of `Option<AnyElement>`. `ViewElement` is `#[doc(hidden)]`, and the
  states are only ever handed back to it by GPUI.
- `crates/gpui/Cargo.toml` names this repository and sets `publish = false`.
- The crates GPUI Kit depends on are named after the gpui-pre snapshot it pins
  (`gpui-pre`, `gpui-pre-platform`, `gpui-pre-web`, `gpui-pre-macros`,
  `gpui-pre-reqwest-client`, `gpui-pre-sum-tree`) at its version, with `[lib]`
  keeping upstream's crate name, so an application patches gpui-fast in with
  `[patch.crates-io]`. The version moves with the snapshot GPUI Kit pins.
- `App::register_inspector_element` takes a factory, the form newer upstream
  has and GPUI Kit is written against; `fast::inspector` adapts it onto this
  snapshot's registry.

When the check fails, move the change into a `fast/` module and leave a hook
behind that names it; use `git diff <import_commit> -- <file>` to see what
differs.

## Syncing with upstream

Upstream is [zed-industries/zed](https://github.com/zed-industries/zed). The
commit our copy was taken from is `zed_commit` in `UPSTREAM`, and
`import_commit` is our commit holding that copy unchanged. To take a newer zed:

1. In a zed checkout, pick the new commit. Start a branch in this repository at
   the last vendor commit (`import_commit` the first time), replace each
   directory listed in `UPSTREAM` with zed's version of it at the new commit,
   and commit that as the new vendor commit (`zed: import <short hash>`). This
   commit holds nothing but upstream's code.
2. Merge that branch into ours. Conflicts should only touch hooks; resolve them
   by keeping upstream's code and putting our hook back.
3. For every file we redirect with `#[path = "fast/..."]`, look at what
   upstream changed in the original (`git diff <old vendor commit> <new vendor
   commit> -- <file>`) and port it into our copy by hand. The merge won't
   conflict on those files, so this step is easy to forget.
4. Update `zed_commit` and `import_commit` in `UPSTREAM` to the new zed commit
   and the new vendor commit, and add or remove entries under `tracked` if zed
   added or removed crates we use.
5. Run `script/check-upstream`, the tests and clippy.
