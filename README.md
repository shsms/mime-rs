# mime-rs

A scriptable, **transactional** text-editing engine. You hand it a small
Emacs-Lisp program — or a single declarative tool call — and it edits a buffer
and hands back a unified diff plus machine-readable reports. Nothing touches
disk until you save, and anything can be rewound.

```elisp
;; prog.tl — the vocabulary is Emacs Lisp
(goto-char (point-min))
(report "n" (replace-regexp "world" "WORLD"))
```

```sh
printf 'hello world, brave world\n' | mime run prog.tl
# → { "diff": "-hello world…\n+hello WORLD…", "reports": { "n": "2" }, … }
```

It's `emacs --batch` reimagined as a server: real buffers with point, mark,
narrowing, the kill-ring, regex **and** structural search — but every run is a
transaction that reports what it did and can be undone.

## Two users at once

mime-rs is built for two kinds of caller, sharing one engine:

- **You**, scripting bulk edits where you'd otherwise reach for `emacs --batch`,
  `sed`, or a one-off script — but with real buffers, structural (tree-sitter)
  edits, multi-file orchestration, dry-runs, and rollback.
- **AI agents**, over [MCP](https://modelcontextprotocol.io): a *bounded*
  editing surface designed for models. The common edit is one call; an
  ambiguous edit is an **error**, not a silent mistake; and recovering from a
  misfire is a single `undo_last`. The hard parts of agent editing — "did that
  land where I meant?", "is this still the file I read?" — are answered by the
  tool, not guessed by the model.

The same buffers, vocabulary, and transactional guarantees back both; only the
front end and the capability tier differ.

## What makes it different

- **Transactional everywhere.** `rehearse` dry-runs any program and returns the
  diff it *would* make, changing nothing. `(with-transaction …)` makes a
  multi-step program all-or-nothing. Checkpoints and an undo ring let you rewind.
  Saves are atomic and **refuse to clobber** a file an external writer changed
  since you opened it — the edit stays warm in the session instead of vanishing.

- **Structural editing, twelve languages.** Outline a file, jump to a function
  *by name*, scope an edit to a single defun, or run a tree-sitter query —
  across Rust, Python, Markdown, HTML, JavaScript, TypeScript, TSX, Go, CSS,
  TOML, YAML, and Elisp.
  Defun spans include Rust `#[attributes]` and Python decorators, so "delete
  this test" is one motion, and a save-time parse check warns before you commit
  a syntactically broken code buffer.

- **Huge files stay cheap.** The file-backed store is a persistent B-tree piece
  table over a paged, read-on-demand original: O(log n) seeks, O(1) snapshots,
  streaming searches, and a single parallel validate-and-index pass at open. A
  multi-GB file never goes fully resident, and a checkpoint is a pointer copy.

- **In-process git history editing.** A `git_*` tool group drives rebase,
  cherry-pick, and revert as a sequencer (on `git2` / vendored libgit2): the
  plan is *data*, a conflicted step surfaces through the very same
  merge-conflict vocabulary you'd use by hand, and `git_rebase` can `rehearse`
  a plan before running it. `autosquash` folds commits without a full plan:
  a sparse `{commit, into}` list, or `true` to fold the branch's
  `fixup!`/`squash!` commits into the commits their subjects name (git's
  `--autosquash`). One-call helpers sit on top: `git_fixup` and
  `git_absorb` fold worktree changes into the commits that own them,
  `git_reword` / `git_msg_rewrite` edit messages, and `git_range_diff` checks
  a rewrite after the fact. No `git` subprocess, no network, no hooks or exec;
  every destructive op first stamps a `refs/mime-backup/<branch>` recovery ref.

- **Honest results.** Diffs are token-frugal (clamped with an elision marker
  when huge), every call returns structured reports, `stale`/`unsaved` flags
  appear only when actually true, and errors name the session so a misfire is
  recoverable rather than lost.

- **Two capability tiers, fixed by the host.** The local CLI is *trusted* —
  full orchestration, unrestricted filesystem, like `emacs --batch`. The
  MCP/daemon tier is *sandboxed*: the filesystem is confined to `$MIME_ROOTS`,
  every run is audited, and there is **no shell, no process spawn, and no
  network** — the git tools included (they work in-process, so they never
  breach that boundary). The one exception is `git_exec_over`, which runs a
  build command at each commit — it stays disabled unless the host sets
  `MIME_EXEC=1`.

## Install

```sh
cargo install --path .
```

A host **C toolchain** is required: the tree-sitter grammars compile C, and
`git2` builds a vendored libgit2 with `cc` (no system libgit2 needed).

## Using it from the shell

```sh
# Pipe through a program (stdin → edited stdout as a JSON report)
printf 'a world\n' | mime run examples/uppercase.tl

# Edit a file in place (omit --write to preview the diff without saving)
mime run examples/uppercase.tl --file ./in.txt --write

# A warm, interactive session — point/mark and buffer state persist between forms
mime repl --file ./in.txt

# Dry-run a program: see the diff it would make, change nothing
mime rehearse prog.tl --file ./in.txt

# Step a script form by form in a terminal UI (build with `--features tui`)
mime tui prog.tl --file ./in.txt
```

The vocabulary is Emacs Lisp (via [tulisp](https://crates.io/crates/tulisp)):
`goto-char`, `re-search-forward`, `replace-match`, narrowing, markers, the
kill-ring, `occur`, merge-conflict resolution, the `treesit-*` family, and the
`replace-regexp` streaming bulk pass. Regex is Emacs syntax (`\(...\)`, `\|`,
`\{n,m\}`) on the RE2 engine — linear-time, no backreferences in patterns. The
full table lives in
[docs/vocabulary.md](docs/vocabulary.md).

## Using it from an agent (MCP)

mime is a standard MCP server — `mime --mcp` over **stdio** or `mime --http` for
**Streamable HTTP** — so any MCP client drives it the same way: Claude Code,
Cursor, Cline, Continue, VS Code, the Gemini/Codex CLIs, or your own harness.
It's self-describing: `initialize` returns how-to-drive `instructions` and a tool
index, and every tool carries MCP `annotations` (read-only vs destructive), so a
client onboards its model straight from the protocol — no per-client setup file.
[docs/clients.md](docs/clients.md) has copy-paste registration for each client
(and the HTTP endpoint); for Claude Code there's a shortcut:

```sh
make claude   # cargo install + register `mime --mcp` (MIME_ROOTS) with Claude Code
```

Each tool takes a `path` and auto-opens the file into a warm session keyed by its
canonical path; mutating tools take `save: true` for an atomic, stale-guarded
write-back. The catalogue is generated from the live schemas into
[docs/mcp-tools.md](docs/mcp-tools.md) (`make docs`), so the docs can't drift from
the code. The edits that matter:

```json
replace_text {path, pattern, replacement, expect_unique: true, save: true}
replace_in_files {files: [a, b, c], pattern, replacement, all: true, save: true}
insert_text  {path, text, anchor: {defun: "parse_args", where: "after"}}
outline      {path}            // KIND START END NAME, per defun
rehearse     {path, program}   // dry-run any lisp program; inspect the diff
undo_last    {path}            // rewind the last mutating call
help         {topic}           // lisp | regex | treesit | conflicts | git | sessions | recipes
```

The design leans on conveniences that matter most for less capable models:
`expect_unique` turns an ambiguous anchor into an error (with the candidate
lines) instead of editing the wrong one; `scope: {defun: "name"}` confines an
edit to one function with no narrowing dance; multi-file `files:` batches are
all-or-nothing; and warm sessions are bounded but never evicted while they hold
unsaved work.

A `git_*` group adds history editing. The core is the sequencer: `git_rebase`
(with a `rehearse` dry-run), `git_cherry_pick`, `git_revert`, and
`git_continue` / `git_skip` / `git_abort`, plus the read-only `git_status`,
`git_log`, `git_show`, and `git_blame` (whose worktree mode maps each
uncommitted hunk to the commit that owns it). On top sit one-call helpers:
`git_fixup` and `git_absorb` fold uncommitted changes into the commits that
own them, `git_move` relocates a change between two adjacent commits,
`git_reword` and `git_msg_rewrite` edit commit messages (one commit / a whole
range), `git_discard` drops selected uncommitted hunks (recoverably), and
`git_range_diff` compares a branch before and after a rewrite. A conflicted
step stops with diff3 markers in the worktree; resolve them with the
conflict tools above, then `git_continue` (or `git_skip` / `git_abort`). Repos are confined to `$MIME_ROOTS`, and each op
stamps a `refs/mime-backup/<branch>` ref so the pre-op state is recoverable.

## How it works

```
          cli.rs     mcp.rs / http.rs / daemon.rs    ← four front ends
        (trusted)            (sandboxed)
                \              /
                 engine.rs                          ← sessions, capability tiers,
              (warm sessions,                          time-travel (checkpoints/undo)
               transactions)
                    |
       builtins.rs · strings.rs · conflict.rs       ← the Emacs-Lisp vocabulary
       syntax.rs (tree-sitter) · sequencer.rs          + structural + git ops
                    |
                store.rs                            ← the TextStore trait
                /        \
          buffer.rs     quire.rs                    ← in-memory oracle / the
        (the oracle)  (piece-tree-over-mmap)           real file-backed store
                          \
                       safety.rs                    ← roots, atomic saves, audit
```

- **`store.rs`** defines `TextStore`, the buffer interface every primitive edits
  through. **`buffer.rs`** is a simple in-memory implementation that doubles as
  a differential-testing oracle; **`quire.rs`** is the production store — the
  persistent B-tree piece table over a paged mmap.
- **`engine.rs`** owns warm sessions, the capability tier, and time travel
  (checkpoints + the undo ring). The tier is fixed at construction: the CLI gets
  trusted orchestration; the MCP server and daemon get the sandboxed vocabulary.
- The vocabulary lives in **`builtins.rs`** / **`strings.rs`**, with
  **`conflict.rs`** (merge-conflict parsing/resolution), **`syntax.rs`**
  (tree-sitter integration), and **`sequencer.rs`** (the git rebase/cherry-pick/
  revert state machine, persisted to `.git/mime-sequencer.json`).
- **`safety.rs`** is the single filesystem chokepoint: `$MIME_ROOTS` enforcement,
  atomic writes, and the audit log.
- **`cli.rs`**, **`mcp.rs`**, **`http.rs`**, and **`daemon.rs`** are the four
  front ends — one-shot/REPL, stdio MCP, MCP over Streamable HTTP, and a
  long-lived unix-socket daemon. The two MCP transports share one dispatch core.
  An optional fifth front end, `tui.rs` (behind the `tui` feature), steps a
  script form by form in a terminal UI.

## Building & testing

```sh
cargo build          # C toolchain required (tree-sitter grammars + vendored libgit2)
make test            # the CI gate: fmt, clippy -D warnings, the full test suite
make docs            # regenerate docs/mcp-tools.md from the live tool schemas
```

`make test` runs 440+ tests, including **differential suites** that pin the
file-backed `Quire` store to the in-memory `Buffer` oracle — every operation is
run against both and the results compared, so the fast store can't silently
diverge from the obvious one. Pending work is tracked in [`todo.org`](todo.org).

## Contributing & dogfooding

mime-rs is largely developed *through* mime-rs — by an agent driving it over
MCP to edit its own source. That feedback loop is the point: if you are an agent
using it and something slows you down — a missing builtin, a confusing report,
an inexpressible query, a tool that should exist — say so and propose the fix.
Much of the agent-facing surface exists because an agent hit the gap and said
so; [`todo.org`](todo.org) is the list to extend.

## License

GPL-3.0 — see [`LICENSE`](LICENSE).
