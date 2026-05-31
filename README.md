# mime-rs

A scriptable, transactional text-editing engine for AI agents — the Rust/tulisp
heir to shsms's `mime`. You drive it by submitting small **Emacs-Lisp** programs
that edit an implicit current buffer; it returns a structured diff plus
machine-readable reports, and it can rewind to checkpoints.

The thesis: today's agent edit tools are stateless ("replace this exact
string"). mime-rs gives the agent a third option between brittle single-string
edits and arbitrary code execution — a bounded, auditable editing language with
warm buffers and cheap rollback. See [`plan.org`](plan.org) for the full design
and roadmap.

## Status

Early but real: **M0–M4 work end to end** — the editor core, the `Quire` store,
checkpoints/transactions, the `mimed` daemon (warm sessions), and **`mime-mcp`** (the
MCP server). 62 tests, `clippy` clean.

- **`TextStore`** trait with two implementations: an in-memory `Buffer` (the
  differential-test oracle) and **`Quire`**, a persistent measured-B-tree piece store
  over an *mmapped* original + append-only add buffer (so multi-GB files never go
  fully resident; O(log n) seeks, O(1) structural-sharing snapshots).
- ~60 Emacs-Lisp editor primitives + a `regex`-backed string library, all on the
  [`tulisp`](../tulisp) interpreter.
- `checkpoint` / `restore-checkpoint` / `with-transaction` — workspace snapshots
  and atomic, roll-back-on-error edits.
- **`mimed`** — a daemon serving warm sessions over a unix socket (JSON-lines); and
  **`mime-mcp`** — an MCP server (JSON-RPC over stdio) exposing the engine as 10 tools
  (`open_file`, `run_program`, `read_region`, `search`, `checkpoint`, …) for agents.

Regex is **RE2** (the `regex` crate) — linear-time and streamable; Emacs
*function names*, RE2 *syntax* (no in-pattern backreferences).

## Quick start

```sh
printf 'hello world, brave world\n' > /tmp/in.txt
cargo run --bin mimectl -- run --local examples/uppercase.tl --file /tmp/in.txt
```

A file is opened through `Quire` (mmap-backed); stdin uses the in-memory
`Buffer`. The program (`examples/uppercase.tl`) replaces every `world` with
`WORLD` and reports the count; `mimectl` prints a JSON result with the unified
diff. Add `--write` to save the edited text back to the file.

```elisp
;; A map-shaped bulk edit, expressed as the Emacs loop:
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
| Edit | `insert` `insert-char` `newline` `delete-char` `delete-region` `erase-buffer` `buffer-string` `buffer-substring` `upcase-region` `downcase-region` |
| Kill ring | `kill-region` `copy-region-as-kill` `yank` |
| Search & replace | `re-search-forward` `search-forward` `search-backward` `replace-match` `match-string` `replace-string` `replace-regexp` `count-matches` |
| Narrowing & scope | `narrow-to-region` `widen` `save-excursion` `save-restriction` |
| Time travel | `checkpoint` `restore-checkpoint` `with-transaction` |
| Observability | `report` `message` |
| String library | `replace-regexp-in-string` `substring` `split-string` `string-trim`(`-left`/`-right`) `string-prefix-p` `string-suffix-p` `string-search` `string-replace` `string-join` `string-empty-p` `number-to-string` `string-to-number` `upcase` `downcase` `capitalize` `char-to-string` `string-to-char` |

## Build

```sh
cargo build      # needs the sibling ../tulisp checkout (path dependency)
cargo test
```

## Layout

- `src/store.rs` — the `TextStore` trait (the buffer seam).
- `src/buffer.rs` — in-memory `Buffer` (oracle).
- `src/quire.rs` — `Quire`, the mmap-backed persistent-B-tree piece store.
- `src/builtins.rs` — editor primitives registered on a `tulisp` context.
- `src/strings.rs` — the RE2-backed string library.
- `src/engine.rs` — `run_program`, the session, checkpoints.
- `src/bin/mimectl.rs` — the CLI client (`--local` one-shot + daemon verbs).
- `src/bin/mimed.rs` — the warm-session daemon (unix socket, JSON-lines).
- `src/bin/mime-mcp.rs` — the MCP server (JSON-RPC over stdio).

GPL-3.0 (via `tulisp`).
