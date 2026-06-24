# mime-rs

Emacs-style batch editing as an engine: you send it a small **Emacs-Lisp
program** (or one declarative tool call), it edits a buffer and hands back a
unified diff plus machine-readable reports — and anything can be rewound.

```elisp
(goto-char (point-min))
(report "n" (replace-regexp "world" "WORLD"))
```

```sh
printf 'hello world, brave world\n' | mime run prog.tl
# → JSON: { diff: "-hello world…+hello WORLD…", reports: { n: "2" }, … }
```

Built for two users at once:

- **You**, scripting bulk edits the way you'd reach for `emacs --batch` or
  `sed` — but with real buffers, regex + structural search, multi-file
  orchestration, and rollback.
- **AI agents**, over MCP: a bounded editing surface where the common edit is
  one call, an ambiguous edit is an *error* instead of a silent mistake, and
  recovery from a misfire is one call (`undo_last`).

## Why it's interesting

- **Transactional everywhere.** Dry-run any program (`rehearse`), make
  multi-step edits all-or-nothing (`with-transaction`), checkpoint and
  rewind. Saves are atomic and refuse to clobber a file an external writer
  changed — the edit stays warm instead of getting lost.
- **Structural editing, nine languages.** Outline a file, jump to a function
  *by name*, scope an edit to one defun, run tree-sitter queries — Rust,
  Python, Markdown, HTML, JavaScript, CSS, TOML, YAML, Elisp. Defun spans
  include `#[attributes]` / decorators, so "delete this test" is one motion.
- **Huge files are fine.** The file-backed store is a persistent B-tree piece
  table over a paged, read-on-demand original: O(log n) seeks, O(1)
  snapshots, streaming searches, one parallel validate+index pass at open.
  Multi-GB files never go fully resident.
- **Two capability tiers, fixed by the host.** The local CLI is trusted
  (full orchestration, like `emacs --batch`); the MCP/daemon tier is
  sandboxed — filesystem confined to `$MIME_ROOTS`, every run audited, no
  shell, no network.
- **Git history editing, in-process.** A `git_*` tool group drives rebase /
  cherry-pick / revert as an agent-friendly sequencer (`git2` / vendored
  libgit2): plan steps are data, conflicts surface through the same
  merge-conflict vocabulary (resolve, then `git_continue`), and `git_rebase`
  can `rehearse` a plan first. No `git` subprocess, no network, no hooks or
  exec; repos stay confined to `$MIME_ROOTS`, and every destructive op first
  stamps a `refs/mime-backup/<branch>` recovery ref.
- **Honest results.** Token-frugal diffs (clamped when huge), per-call
  reports, `stale`/`unsaved` flags only when true, and a save-time syntax
  check for code buffers.

## Quick start

```sh
cargo install --path .            # or: make claude (see below)
printf 'a world\n' | mime run examples/uppercase.tl
mime run examples/uppercase.tl --file ./in.txt --write
mime repl --file ./in.txt         # warm interactive session
```

The vocabulary is Emacs: `goto-char`, `re-search-forward`, `replace-match`,
narrowing, markers, the kill ring, `occur`, merge-conflict resolution,
`treesit-*` — the full table is in
[docs/vocabulary.md](docs/vocabulary.md). Regex is RE2 syntax under Emacs
names (linear-time; no backreferences in patterns).

## For agents (MCP)

```sh
make claude    # cargo install + register `mime --mcp` with Claude Code
```

Twenty tools; the ones that matter:

```json
replace_text {path, pattern, replacement, expect_unique: true, save: true}
replace_text {files: [a, b, c], pattern, replacement, all: true, save: true}
insert_text  {path, text, anchor: {defun: "parse_args"}}
outline      {path}            // KIND START END NAME, per defun
rehearse     {path, program}   // dry-run any lisp program, see the diff
undo_last    {path}            // rewind the last mutating call
help         {topic}           // regex | treesit | conflicts | sessions | recipes
```

Multi-file edits are atomic across the set; `scope: {defun: "name"}` confines
an edit to one function; warm sessions are bounded and never evicted with
unsaved work. The full catalogue is generated from the live schemas into
[docs/mcp-tools.md](docs/mcp-tools.md) (`make docs`), so it can't drift.

A `git_*` group adds agent-driven history editing — `git_rebase` (with
`rehearse`), `git_cherry_pick`, `git_revert`, `git_continue`, `git_skip`,
`git_abort`, `git_status`, `git_log`, `git_show`. A conflicted step stops with
diff3 markers in the worktree; resolve with the merge-conflict tools above, then
`git_continue` (or `git_abort`). Each op first stamps a `refs/mime-backup/<branch>`
ref, so the pre-op state is always recoverable.

## Build & layout

`cargo build` (a C toolchain is needed for the tree-sitter grammars);
`make test` runs the CI gate — fmt, `clippy -D warnings`, 280+ tests
including differential suites that pin the file-backed store to an in-memory
oracle. Pending work: [`todo.org`](todo.org).

The map: `store.rs` (the buffer trait) · `buffer.rs` / `quire.rs` (the two
stores) · `builtins.rs` + `strings.rs` + `conflict.rs` + `syntax.rs` (the
vocabulary) · `engine.rs` (sessions, tiers, time travel) · `sequencer.rs`
(the git rebase/cherry-pick/revert state machine) · `safety.rs` (roots,
atomic saves, audit) · `cli.rs` / `daemon.rs` / `mcp.rs` (the three front
ends).

## Dogfooding

mime-rs is largely developed *through* mime-rs, by an agent driving it over
MCP. If you are an agent using it and something slows you down — a missing
builtin, a confusing report, an inexpressible query — say so and propose the
fix; `todo.org` is the list to extend. Much of the agent surface exists
because an agent hit the gap and said so.

GPL-3.0 — see [`LICENSE`](LICENSE).
