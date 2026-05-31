# Porting from `mime` (ChaiScript) to `mime-rs` (Emacs Lisp / tulisp)

`mime-rs` is the Rust/tulisp heir to [shsms's `mime`][mime], a ChaiScript buffer
editor driven by a batch program. This guide maps the `mime` buffer API onto the
`mime-rs` Emacs-Lisp builtins so you can port a `mime` script (the canonical
target being `add-anno.mime`) one operation at a time.

The big shift is the editing model. In `mime` you hold a buffer object `b` and
call methods on it (`b.find(...)`, `b.paste(...)`), with positions returned and
passed around explicitly, and a separate "cursor" object per concurrent
position. In `mime-rs` there is **one implicit current buffer** with a **point**
(the cursor) and a **mark** (the other end of a region) — the Emacs model.
Operations act on point; a region is the span between point and mark. Where
`mime` returns a position from `find`, `mime-rs` *moves point there* and returns
the new point. Where `mime` juggles several `cursor` objects, `mime-rs` uses
**markers** (durable positions that ride edits).

Every builtin named below is defined in [`src/builtins.rs`](../src/builtins.rs)
(buffer/editor primitives) or [`src/strings.rs`](../src/strings.rs) (pure string
functions). Positions are **1-based character** positions, as in Emacs.

[mime]: https://github.com/shsms/mime

---

## Operation map

### Navigation & position

| `mime` (ChaiScript)        | `mime-rs` (Emacs Lisp)                          | Notes |
|----------------------------|-------------------------------------------------|-------|
| `b.get_pos()`              | `(point)`                                       | current point |
| `b.goto_pos(p)`            | `(goto-char p)`                                 | accepts an int **or a marker** |
| `b.start_of_buffer()`      | `(goto-char (point-min))` / `(beginning-of-buffer)` | respects narrowing |
| `b.end_of_buffer()`        | `(goto-char (point-max))` / `(end-of-buffer)`   | respects narrowing |
| `b.forward(n)`             | `(forward-char n)`                              | |
| `b.backward(n)`            | `(forward-char (- n))`                          | |
| `b.next_line()`            | `(forward-line 1)`                              | `(forward-line n)` for n lines |
| (column / line queries)    | `(current-column)`, `(line-number-at-pos)`, `(line-beginning-position)`, `(line-end-position)`, `(goto-line n)` | |

### Search

| `mime` (ChaiScript)        | `mime-rs` (Emacs Lisp)                          | Notes |
|----------------------------|-------------------------------------------------|-------|
| `b.find("s")`              | `(search-forward "s" nil t)`                    | literal; moves point past the match, returns new point or nil |
| `b.find(regex("re"))`      | `(re-search-forward "re" nil t)`                | RE2 regex |
| `b.find_fuzzy("s")`        | `(search-fuzzy "s" nil t)`                       | case- and whitespace-run-insensitive |
| `b.rfind("s")`             | `(search-backward "s" nil t)`                    | literal, backward |
| `b.find(...) >= 0` (test)  | `(if (search-forward ... nil t) ...)`            | nil when not found (with the `t` "noerror" arg) |
| (count occurrences)        | `(count-matches "re")`                           | from point to end; doesn't move point |

> The third argument to the search builtins is Emacs's `NOERROR`: pass `t` so a
> miss returns `nil` instead of raising. The second is an optional `BOUND`
> (search no further than this position); `nil` means "to end of buffer".

### Match data (after a regex search)

| `mime` (ChaiScript)        | `mime-rs` (Emacs Lisp)                          | Notes |
|----------------------------|-------------------------------------------------|-------|
| (capture groups)           | `(match-string N)`                               | N=0 whole match, 1.. groups, from the last search |
| `b.replace` after a find   | `(replace-match "new")`                          | replaces the most recent match; `\\N` / `\\&` backrefs |
| (test a regex at point)    | `(looking-at "re")`                              | t if the text at point matches |

### Region, mark, copy/cut/paste

| `mime` (ChaiScript)        | `mime-rs` (Emacs Lisp)                          | Notes |
|----------------------------|-------------------------------------------------|-------|
| `b.set_mark()`             | `(set-mark (point))`                             | mark the current point |
| (region bounds)            | `(region-beginning)`, `(region-end)`             | min/max of point and mark |
| `b.copy()`                 | `(buffer-substring (region-beginning) (region-end))` | the region's text |
| `b.erase_region()`         | `(delete-region (region-beginning) (region-end))` | delete the region |
| `b.cut()`                  | `(kill-region (region-beginning) (region-end))`  | delete **and** push onto the kill-ring |
| `b.paste("s")`             | `(insert "s")`                                   | insert literal text at point |
| `b.paste(b.cut())` (move)  | `(kill-region ...)` then `(yank)` elsewhere      | yank re-inserts the last kill |
| `b.exchange_point_and_mark`| `(exchange-point-and-mark)`                       | |

> `mime`'s `copy`/`cut` operate on the marked region (point..mark); the `mime-rs`
> equivalents take the two endpoints explicitly, which `(region-beginning)` /
> `(region-end)` supply. `(copy-region-as-kill a b)` is a non-deleting copy onto
> the kill-ring.

### Replace (bulk)

| `mime` (ChaiScript)        | `mime-rs` (Emacs Lisp)                          | Notes |
|----------------------------|-------------------------------------------------|-------|
| `b.replace("a","b")`       | `(replace-string "a" "b")`                       | literal, all from point; returns the count |
| `b.replace(regex("re"),"b")` | `(replace-regexp "re" "b")`                    | regex, all from point; returns the count |
| `b.replace("a","b", 1)`    | one `(re-search-forward ...)` + `(replace-match ...)` | replace just the first; see below |
| (hand-rolled replace loop) | `(while (re-search-forward "re" nil t) (replace-match "b"))` | the explicit sequential form |

`mime`'s `replace` defaults to replace-**all** and takes an optional count to cap
the number of replacements. `mime-rs`'s `replace-string` / `replace-regexp`
always replace every match from point to the end of the (narrowed) buffer and
return how many they made. To replace exactly once (the `b.replace(a,b,1)`
case), do the search-and-replace-match pair by hand:

```elisp
(goto-char (point-min))
(when (re-search-forward "re" nil t)
  (replace-match "b"))            ; just the first hit
```

To cap at N, wrap that in a counting `while` (a `dotimes` over N).

### Narrowing

| `mime` (ChaiScript)        | `mime-rs` (Emacs Lisp)                          | Notes |
|----------------------------|-------------------------------------------------|-------|
| `b.narrow_to_region()`     | `(narrow-to-region (region-beginning) (region-end))` | restrict to point..mark |
| `b.widen()`                | `(widen)`                                        | remove the restriction |
| (scoped narrowing)         | `(save-restriction (narrow-to-region a b) ...)`  | auto-restores the old bounds |
| (scoped point/mark)        | `(save-excursion ...)`                            | auto-restores point and mark |

> `mime` narrows to the **marked region**; `mime-rs` takes the two endpoints, so
> set the mark first (or pass explicit positions). `save-restriction` /
> `save-excursion` have no direct `mime` analog — they're the Emacs way to make a
> bounded sub-edit and have the bounds/point snap back afterward.

### Multiple cursors → markers

| `mime` (ChaiScript)        | `mime-rs` (Emacs Lisp)                          | Notes |
|----------------------------|-------------------------------------------------|-------|
| `b.new_cursor()`           | `(make-marker)` / `(point-marker)` / `(copy-marker p)` | a durable position |
| `b.use_cursor(c)`          | `(goto-char m)`                                  | jump point to the marker |
| (cursor's position)        | `(marker-position m)`                            | the int it points to, or nil |
| (re-aim a cursor)          | `(set-marker m p)`                               | point the marker at p (nil detaches) |
| (is it a cursor?)          | `(markerp x)`                                    | t for a marker, nil for an int |

A `mime` script uses several `cursor` objects to keep multiple live positions in
one buffer (e.g. a "main" cursor in the body and a "notes" cursor at the end).
In `mime-rs` you hold several **markers** and `goto-char` whichever one you want
to act at. Markers auto-adjust across edits, so a marker stays on its character
even as you insert/delete before it — exactly the property the `mime` cursors
have. The [`window`](../src/builtins.rs) builtin renders a viewport around any
position (`(window N POS)`), so you can "look" through several markers at once.

### Contents, emptiness, saving

| `mime` (ChaiScript)        | `mime-rs` (Emacs Lisp)                          | Notes |
|----------------------------|-------------------------------------------------|-------|
| `b.get_contents()`         | `(buffer-string)`                                | whole (narrowed) buffer text |
| `b.empty()`                | `(= (point-min) (point-max))`                    | buffer is empty |
| `b.clear()`                | `(erase-buffer)`                                 | delete everything |
| `b.save_as(path)`          | *not a builtin* — `mimectl run --local … --write`, or the daemon `save` verb | edits never auto-persist |

> Saving is deliberately **outside** the program in `mime-rs`: a program only
> edits the in-memory buffer and returns a diff. You persist by running with
> `--write` (the `mimectl run --local` one-shot) or via the `mimed` `save`
> control verb. This is what makes `rehearse` (dry-run) and read-only sessions
> safe — nothing reaches disk unless you ask outside the program.

### String helpers (no buffer needed)

`mime`'s `to_string(...)`, string concatenation, `.at(0)`, etc. map onto the pure
string library in [`src/strings.rs`](../src/strings.rs): `number-to-string`,
`string-to-number`, `concat`-style work via `(insert ...)` or `string-join`,
`substring`, `split-string`, `string-trim`, `string-prefix-p` / `-suffix-p`,
`string-search`, `string-replace`, `replace-regexp-in-string`, `upcase` /
`downcase` / `capitalize`, `char-to-string` / `string-to-char`. These take and
return strings and never touch the buffer.

### No direct analog (new in `mime-rs`)

These have no `mime` counterpart and are worth reaching for:

- **Checkpoints** — `(checkpoint "label")`, `(restore-checkpoint "label")`,
  `(list-checkpoints)`, `(checkpoint-diff "a" "b")`: named buffer snapshots you
  can roll back to (workspace time-travel).
- **Transactions** — `(with-transaction BODY...)`: run BODY atomically; if it
  signals an error the buffer is restored to its pre-transaction state. Pair with
  `condition-case` to catch the error and report instead of failing the run.
- **Reports** — `(report "key" value)`: attach a machine-readable key/value to
  the run result. `(message "...")` adds a log line. Both are how a program tells
  the agent *what it did* without dumping the whole buffer.
- **Structural navigation** — `treesit-*` (Markdown): `(treesit-node-at)`,
  `(treesit-beginning-of-defun)` / `(treesit-end-of-defun)` move by document
  section rather than by raw text.

---

## Worked example

A small `mime` routine — the shape of `fix_basics` / `cleanup_note` in
`add-anno.mime`: normalise a couple of entities, then strip every
`<span class="page" …>…</span>` tag from the buffer.

### Before — `mime` (ChaiScript)

```js
def cleanup(b) {
    b.start_of_buffer();
    b.replace("&mdash;", "—");          // entity → glyph, everywhere
    b.replace("&nbsp;", " ");

    b.start_of_buffer();
    while (b.find("<span class=\"page\" ") >= 0) {
        b.rfind("<span class=\"page\" ");   // back to the tag start
        b.set_mark();                        // mark it
        b.find("</span>");                   // point past the closing tag
        b.erase_region();                    // delete start..point
    }
    b.start_of_buffer();
}
```

### After — `mime-rs` (Emacs Lisp / tulisp)

```elisp
;; cleanup.tl — normalise entities, then drop every <span class="page" …> … </span>.
(goto-char (point-min))
(report "mdash" (replace-string "&mdash;" "—"))   ; replace-all, returns the count
(report "nbsp"  (replace-string "&nbsp;" " "))

(goto-char (point-min))
(let ((n 0))
  ;; search-forward leaves point *after* the opening tag; the tag's start is
  ;; that position minus the tag's length, which we mark, then delete through
  ;; the closing </span>.
  (while (search-forward "<span class=\"page\" " nil t)
    (let ((tag-start (- (point) (length "<span class=\"page\" "))))
      (set-mark tag-start)                       ; one end of the region
      (search-forward "</span>" nil t)           ; point = other end
      (delete-region (region-beginning) (region-end))
      (setq n (1+ n))))
  (report "spans-removed" n))
(goto-char (point-min))
```

What changed in the port:

- `b.replace(a, b)` → `(replace-string a b)` (literal, all-from-point) — and we
  capture its count into a `report` instead of discarding it.
- `b.find(s) >= 0` → `(search-forward s nil t)`, which returns `nil` on a miss
  (so it's the `while` condition directly) and **moves point** to just after the
  match — no explicit position variable.
- `mime`'s `rfind` to re-find the tag start is unnecessary: `search-forward`
  already left point right after the opening tag, so the start is `(- (point)
  len)`. We `(set-mark ...)` there and let `(search-forward "</span>")` carry
  point to the far end, then `(delete-region (region-beginning) (region-end))`.
- The whole thing runs against the implicit buffer; to persist, run it with
  `mimectl run --local cleanup.tl --file FILE --write` (the program itself never
  saves).

Run it the same way as any example:

```
mimectl run --local cleanup.tl --file SOMEFILE          # show the diff
mimectl run --local cleanup.tl --file SOMEFILE --write  # …and persist
```

---

## Examples index

Runnable, well-commented programs live in [`../examples/`](../examples). Each is
a one-shot you run with `mimectl run --local examples/NAME.tl --file SOMEFILE`:

| File | What it shows |
|------|---------------|
| `uppercase.tl`                 | a `while` + `re-search-forward` + `replace-match` loop with a count report |
| `regexp-replace-count.tl`      | bulk `replace-regexp`, reporting the number of replacements |
| `delete-trailing-whitespace.tl`| `delete-trailing-whitespace` + a `(?m)` `count-matches` tally |
| `rename-symbol.tl`             | whole-word identifier rename via `replace-regexp` with `\b` boundaries |
| `transactional-edit.tl`        | an atomic multi-step edit guarded by `with-transaction` + `condition-case` |
| `markers-viewport.tl`          | markers tracking positions across an edit, plus two `window` viewports |

To **try** programs interactively against a warm buffer (state persists across
lines; nothing is written to disk), use the REPL:

```
mimectl repl                 # empty scratch buffer
mimectl repl --file SOMEFILE # load a file first
```

Each form you enter prints the diff (if the buffer changed), any
`report`/`message` output, and the value (after `=>`). A form split across lines
keeps reading until the parentheses balance.
