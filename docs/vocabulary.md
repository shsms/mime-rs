# The editing vocabulary

Programs are Emacs Lisp on [`tulisp`](https://github.com/shsms/tulisp)
(control flow, `let`, `lambda`, `dolist`, `condition-case`, …) plus these
editor builtins. At runtime, the MCP `help` tool serves topic briefs on the
regex dialect, the treesit layer, conflicts, and session semantics.

| Group | Primitives |
|---|---|
| Motion | `point` `point-min` `point-max` `goto-char` `goto-line` `forward-char` `forward-line` `forward-word` `backward-word` `forward-paragraph` `beginning-of-buffer` `end-of-buffer` `beginning-of-line` `end-of-line` `back-to-indentation` `line-beginning-position` `line-end-position` `line-number-at-pos` `current-column` `current-indentation` |
| Predicates / chars | `bolp` `eolp` `bobp` `eobp` `char-after` `char-before` `looking-at` `looking-back` |
| Mark & region | `set-mark` `mark` `region-beginning` `region-end` `exchange-point-and-mark` |
| Markers | `make-marker` `point-marker` `copy-marker` `set-marker` `marker-position` `markerp` (durable positions; `goto-char` accepts a marker) |
| Edit | `insert` `insert-char` `newline` `delete-char` `delete-region` `erase-buffer` `buffer-string` `buffer-substring` `upcase-region` `downcase-region` `delete-trailing-whitespace` `keep-lines` `flush-lines` `sort-lines` |
| Kill ring | `kill-region` `kill-line` `kill-whole-line` `copy-region-as-kill` `yank` |
| Search & replace | `re-search-forward` `re-search-backward` `search-forward` `search-backward` `replace-match` `match-string` `match-beginning` `match-end` `replace-string` `replace-regexp` `count-matches` `regexp-quote` |
| Narrowing & scope | `narrow-to-region` `widen` `save-excursion` `save-restriction` |
| Time travel | `checkpoint` `restore-checkpoint` `with-transaction` |
| Merge conflicts | `conflict-count` `conflict-hunks` `conflict-goto` `conflict-context` `conflict-text` `conflict-diff` `conflict-keep` `conflict-replace` `conflict-resolve-trivial` (git/diff3 markers; smerge-flavored) |
| Structural | `treesit-language` `treesit-set-language` `treesit-root-type` `treesit-has-error` `treesit-beginning-of-defun` `treesit-end-of-defun` `treesit-defun-name` `treesit-narrow-to-defun` `treesit-list-defuns` `treesit-goto-defun` (tree-sitter; Markdown, Rust, Python, HTML, JavaScript, CSS, TOML, YAML, Elisp — `.el`/`.tl`) |
| Nodes (first-class values) | `treesit-node-at` `treesit-defun-at` `treesit-query` (each returns nodes) · `treesit-node-type` `treesit-node-start` `treesit-node-end` `treesit-node-text` `treesit-node-parent` `treesit-node-child` `treesit-node-child-count` `treesit-node-next-sibling` `treesit-node-prev-sibling` `treesit-node-child-by-field-name` `treesit-node-p` · edit ops `treesit-replace-node` `treesit-wrap-node` `treesit-raise-node` `treesit-kill-node` `treesit-insert-sibling` (a buffer edit outdates old nodes) |
| Observability | `report` `message` `window` `occur` `buffer-file-name` `buffer-stale-p` |
| Orchestration (trusted tier only) | `find-file` `find-file-noselect` `insert-file-contents` `write-file` `write-region` `directory-files` `generate-new-buffer` `set-buffer` `with-current-buffer` `current-buffer` `buffer-name` `buffer-list` `get-buffer` `kill-buffer` `arg` |
| String library | `replace-regexp-in-string` `substring` `split-string` `string-trim`(`-left`/`-right`) `string-prefix-p` `string-suffix-p` `string-search` `string-replace` `string-join` `string-empty-p` `number-to-string` `string-to-number` `upcase` `downcase` `capitalize` `char-to-string` `string-to-char` |
