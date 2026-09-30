---
description: Sync gpui-fast with a newer zed-industries/zed commit (vendor import, merge, port, check, PR)
argument-hint: "[zed commit, default: zed main HEAD]"
---

Take a newer upstream zed into gpui-fast, following "Syncing with upstream" in
`docs/upstream-sync.md`. Read that document and `UPSTREAM` first; they are the
rules, this is the checklist. Target zed commit: `$ARGUMENTS` (empty means the
current `main` of zed-industries/zed).

## 0. Prepare

- Working tree must be clean; `git fetch origin`. Never commit to `main`.
- A zed checkout lives at `~/github/zed`. If missing:
  `git clone --filter=blob:none https://github.com/zed-industries/zed ~/github/zed`.
  Otherwise `git -C ~/github/zed fetch origin`.
- Resolve the target to a full hash (`NEW`). Read `zed_commit` (`OLD`) and
  `import_commit` (`OLD_VENDOR`) from `UPSTREAM`. Report how many zed commits
  lie between `OLD` and `NEW` and which touch the tracked directories
  (`git -C ~/github/zed log --oneline OLD..NEW -- <tracked dirs>`).
- Check whether zed now has crates that tracked crates depend on but
  `UPSTREAM` doesn't track (new `path`/`workspace = true` deps in their
  `Cargo.toml`s), or tracked crates zed deleted.
- Check whether zed merged window composition (zed#62379). If it did, the
  sync replaces `fast::composition` with upstream's version (see
  "Where our API differs from upstream's"); stop and confirm the plan with
  the user before doing that.

## 1. Vendor commit (upstream code only)

Keep zed's commits: the vendor branch gets one commit per zed commit, not a
single squashed import.

- If zed added crates we now need, add them to `tracked` in `UPSTREAM` first
  (uncommitted is fine; the script reads the working tree's `UPSTREAM`).
  Leave crates zed deleted in `tracked` for the import, so their deletion is
  replayed; drop them from `tracked` in step 4.
- Run `script/import-upstream --zed ~/github/zed NEW`. Starting at
  `OLD_VENDOR`, it creates branch `upstream/zed-<short NEW>` in a worktree
  (`../gpui-fast-upstream-<short NEW>`) and replays every zed commit in
  `OLD..NEW` that touches a tracked directory, limited to those directories,
  with its original author, committer, dates and message plus an
  `Upstream-commit:` trailer. It fails unless every tracked directory then
  matches zed at `NEW`. Its last line prints the new vendor commit.
- Nothing of ours goes on this branch; root `Cargo.toml`/`Cargo.lock` are
  ours and change in step 2.

## 2. Merge

- Create the topic branch `sync/zed-<short NEW>` from `origin/main` and
  `git merge --no-ff` the vendor branch.
- Resolve conflicts by keeping upstream's code and putting our hook back.
  Never rewrite upstream code to fit ours; move adaptation into `fast/`.
- Files with `removed=` entries in `script/upstream-allowlist` (upstream
  bodies we replaced with a forward to `fast/`) and anything else a hook
  replaced: look at `git diff OLD_VENDOR NEW_VENDOR -- <file>` and port
  upstream's changes to the replaced code into the `fast/` implementation.
- Root workspace: add new workspace dependencies / members zed's crates
  need to the root `Cargo.toml` (copy versions from zed's root
  `Cargo.toml`), then `cargo update -w` / let cargo refresh `Cargo.lock`
  with minimal churn. Keep `rust-toolchain.toml` in step with zed's channel
  if zed's code needs it.

## 3. Redirected files

For every `#[path = "fast/<file>.rs"]` redirect (`grep -rn '#\[path = "fast'
crates`), diff upstream's original between the vendor commits and port the
change into our rewrite by hand. The merge doesn't conflict on these.

## 4. Record the sync

Update `zed_commit` and `import_commit` in `UPSTREAM` to `NEW` and the new
vendor commit (its hash on the vendor branch; the merge keeps it reachable),
and `tracked` if crates were added or removed. Update `docs/upstream-sync.md`
if an API difference went away (e.g. upstream shipped something we carried).

## 5. Verify

Run all of these and fix what fails, fixes going into `fast/` or hooks:

- `script/check-upstream` (and `--report` to see the budgets). Only raise an
  allowlist budget with a specific reason, never to get past a real rule.
- `cargo check --workspace --all-targets`
- `cargo clippy --workspace --all-targets -- -D warnings` (as far as it was
  clean before the sync: compare with `origin/main` if unsure)
- `cargo test -p gpui --features test-support` and the tests of any other
  crate that changed in a way touching our code.
- Build `gpui_perf` (`cargo build -p gpui_perf --release --examples`).

Report failures faithfully; don't paper over them.

## 6. Pull request

- Push the vendor branch too. The vendor commit must end up in `main`'s
  history, or the next sync's merge base falls back to the first import:
  the PR must be merged with a **merge commit, never squashed**. The
  repository only allows squash by default, so tell the user to enable
  "Allow merge commits" in the repository settings before merging (don't
  change settings yourself), and say so at the top of the PR description.
- Push `sync/zed-<short NEW>` and open a PR against `main` titled
  `Sync with zed <short NEW>`, listing: the zed range and commit count,
  notable upstream changes to gpui, conflicts resolved and how, code ported
  into `fast/` or redirects, `UPSTREAM`/allowlist/Cargo changes, and the
  verification results.
- Remove the worktree when done.
