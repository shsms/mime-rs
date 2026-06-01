# mime-rs

A scriptable, transactional text-editing engine. You drive it by submitting
small **Emacs-Lisp** programs that edit an implicit current buffer; it returns a
structured diff plus machine-readable reports, and it can rewind to checkpoints.

It serves two audiences from the same core:

- **Scripting & automation** — run whole Emacs-Lisp programs against files for
  batch editing and build pipelines, the way you'd reach for `emacs --batch` or
  `sed`/`awk`, but with real buffers, multi-file orchestration, structured
  results, and rollback.
- **AI agents** — a bounded, auditable editing surface between brittle
  single-string "replace this exact text" edits and arbitrary code execution:
  warm buffers, cheap rollback, an allowlisted filesystem, and an MCP front door.

The launch mode picks both the protocol *and* the capability tier, so the host
decides how much power a caller gets. The full design and roadmap are in
[`plan.org`](plan.org).

## Capability tiers

- **Trusted** (local CLI) — the full language plus *orchestration*: multiple
  buffers, file I/O (`find-file`, `insert-file-contents`, `write-file`,
  `directory-files`), `with-current-buffer`, and program arguments (`arg`). Like
  `emacs --batch`: for your own scripts and automation.
- **Sandboxed** (daemon / MCP) — the editing core only: no orchestration, the
  filesystem confined to `$MIME_ROOTS`, every run audited. Safe to expose to an
  agent; the caller can't change its tier, the host fixes it at launch.

## Status

Real and dogfooded. The editor core, the `Quire` store, checkpoints /
transactions / rehearse, the warm-session daemon, the MCP server, the trusted
orchestration group, and a path-allowlist + audit safety layer all work. **145 tests,
`clippy` clean.**

- **`TextStore`** trait with two implementations: an in-memory `Buffer` (the
  differential-test oracle) and **`Quire`**, a persistent measured-B-tree piece
  store over an *mmapped* original + append-only add buffer (so multi-GB files
  never go fully resident; O(log n) seeks, O(1) structural-sharing snapshots).
  Saves are **atomic** (temp file + rename), so an in-place save never disturbs
  the live mmap.
- The **Emacs-Lisp editor vocabulary** — motion, mark/region, regex search/
  replace, kill-ring, narrowing, **markers** (durable positions), a `window`
  viewport, and an M7 `treesit-*` (tree-sitter) scaffold — plus a `regex`-backed
  string library, all on the [`tulisp`](https://github.com/shsms/tulisp)
  interpreter.
- `checkpoint` / `restore-checkpoint` / `with-transaction` — workspace snapshots
  and atomic, roll-back-on-error edits — and **`rehearse`**, a dry-run that
  returns a program's diff then rolls the buffer back so nothing persists.
- **One binary, three modes**: `mime run PROG.tl` (trusted one-shot),
  `mime --daemon` (warm sessions over a unix socket, JSON-lines), and
  `mime --mcp` (an MCP server, JSON-RPC over stdio, exposing the engine as tools
  like `open_file` / `run_program` / `rehearse` / `view` / `search` /
  `checkpoint` / `save_buffer` / `session_status`).
- **Safety**: in the sandboxed tier, file access is confined to `$MIME_ROOTS`
  (or the cwd), which `session_status` advertises up front; `$MIME_AUDIT` logs
  one JSON line per run. No shell, no network, no arbitrary filesystem access.

Regex is **RE2** (the `regex` crate) — linear-time and streamable; Emacs
*function names*, RE2 *syntax* (no in-pattern backreferences).

## Quick start

```sh
# pipe text in — stdin uses the in-memory Buffer, so no filesystem access:
printf 'hello world, brave world\n' | cargo run --bin mime -- run examples/uppercase.tl
```

`mime` prints a JSON result with the unified diff and any `(report …)` values —
here, `world` → `WORLD` twice. `run` is the default verb, so `mime
examples/uppercase.tl` works as shorthand. This embedded one-shot runs
in-process at the trusted tier (no daemon); to edit a file in place, point
`--file` at a path under the cwd (or `$MIME_ROOTS`) and add `--write`:

```sh
mime run examples/uppercase.tl --file ./in.txt --write
```

A file is opened through `Quire` (mmap-backed). The example is the map-shaped
bulk edit written as the Emacs sequential loop:

```elisp
(goto-char (point-min))
(let ((n 0))
  (while (re-search-forward "world" nil t)
    (replace-match "WORLD")
    (setq n (1+ n)))
  (report "replaced" n))
```

## The vocabulary

Programs are Emacs Lisp on `tulisp` (control flow, `let`, `lambda`, `dolist`,
`condition-case`, …) plus these editor builtins:

| Group | Primitives |
|---|---|
| Motion | `point` `point-min` `point-max` `goto-char` `goto-line` `forward-char` `forward-line` `forward-word` `backward-word` `beginning-of-buffer` `end-of-buffer` `beginning-of-line` `end-of-line` `line-beginning-position` `line-end-position` `line-number-at-pos` `current-column` |
| Predicates / chars | `bolp` `eolp` `bobp` `eobp` `char-after` `char-before` `looking-at` |
| Mark & region | `set-mark` `mark` `region-beginning` `region-end` `exchange-point-and-mark` |
| Markers | `make-marker` `point-marker` `copy-marker` `set-marker` `marker-position` `markerp` (durable positions; `goto-char` accepts a marker) |
| Edit | `insert` `insert-char` `newline` `delete-char` `delete-region` `erase-buffer` `buffer-string` `buffer-substring` `upcase-region` `downcase-region` |
| Kill ring | `kill-region` `copy-region-as-kill` `yank` |
| Search & replace | `re-search-forward` `search-forward` `search-backward` `replace-match` `match-string` `match-beginning` `match-end` `replace-string` `replace-regexp` `count-matches` `regexp-quote` |
| Narrowing & scope | `narrow-to-region` `widen` `save-excursion` `save-restriction` |
| Time travel | `checkpoint` `restore-checkpoint` `with-transaction` |
| Structural (M7) | `treesit-root-type` `treesit-node-at` `treesit-beginning-of-defun` `treesit-end-of-defun` (tree-sitter; Markdown) |
| Observability | `report` `message` `window` |
| Orchestration (trusted tier) | `find-file` `find-file-noselect` `insert-file-contents` `write-file` `write-region` `directory-files` `generate-new-buffer` `set-buffer` `with-current-buffer` `current-buffer` `buffer-name` `buffer-list` `get-buffer` `kill-buffer` `arg` |
| String library | `replace-regexp-in-string` `substring` `split-string` `string-trim`(`-left`/`-right`) `string-prefix-p` `string-suffix-p` `string-search` `string-replace` `string-join` `string-empty-p` `number-to-string` `string-to-number` `upcase` `downcase` `capitalize` `char-to-string` `string-to-char` |

## Build

```sh
cargo build
cargo test
```

`tulisp` comes from crates.io; the `tree-sitter-md` parser compiles C, so a host
C toolchain (`cc`) is required.

## Layout

- `src/store.rs` — the `TextStore` trait (the buffer seam).
- `src/buffer.rs` — in-memory `Buffer` (oracle).
- `src/quire.rs` — `Quire`, the mmap-backed persistent-B-tree piece store.
- `src/builtins.rs` — editor primitives registered on a `tulisp` context.
- `src/strings.rs` — the RE2-backed string library.
- `src/syntax.rs` — the tree-sitter scaffold behind the `treesit-*` builtins (M7).
- `src/engine.rs` — `run_program` / `rehearse`, sessions, checkpoints, capability tiers.
- `src/safety.rs` — path allowlisting (`$MIME_ROOTS`), atomic saves, audit journal.
- `src/cli.rs` — local one-shot + daemon-client verbs (`run`, `open`, `save`, `status`, `rehearse`, `repl`, `close`).
- `src/daemon.rs` — the warm-session daemon (unix socket, JSON-lines).
- `src/mcp.rs` — the MCP server (JSON-RPC over stdio).
- `src/bin/mime.rs` — the single binary; dispatches to the CLI, daemon, or MCP mode.

GPL-3.0 — see [`LICENSE`](LICENSE).
