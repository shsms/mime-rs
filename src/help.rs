//! On-demand reference text for the `help` MCP tool — the canonical
//! cheat-sheets an agent would otherwise need carried in its prompt. Each
//! topic is a small, self-contained brief; the always-loaded tool schemas can
//! stay terse and point here. Keep these in sync with the engine: they ARE
//! the contract an agent acts on, fetched only when needed.

/// The topics `help` serves, with one-line summaries — also the "unknown
/// topic" help text.
pub const TOPICS: &[(&str, &str)] = &[
    (
        "lisp",
        "the callable Lisp surface, grouped by task (search, structure, delete, narrow, transact)",
    ),
    (
        "regex",
        "the regex dialect: Emacs syntax on the RE2 engine, replacement syntax",
    ),
    (
        "treesit",
        "structural editing: defuns, nodes, queries, narrowing",
    ),
    (
        "conflicts",
        "merge-conflict workflow: overview, keep/replace, trivial sweep",
    ),
    (
        "git",
        "in-process rebase/cherry-pick/revert: plan, stop, resolve, continue",
    ),
    (
        "sessions",
        "warm sessions, path addressing, saving, staleness, undo",
    ),
    ("recipes", "common edit shapes as ready-to-adapt programs"),
];

/// The reference text for `name`, or `None` for an unknown topic.
pub fn topic(name: &str) -> Option<&'static str> {
    match name {
        "lisp" => Some(LISP),
        "regex" => Some(REGEX),
        "treesit" => Some(TREESIT),
        "conflicts" => Some(CONFLICTS),
        "git" => Some(GIT),
        "sessions" => Some(SESSIONS),
        "recipes" => Some(RECIPES),
        _ => None,
    }
}

// Mirrors the grouped index in docs/vocabulary.md — keep the two in sync.
const LISP: &str = r#"— the callable Lisp surface —
Programs are Emacs-Lisp (tulisp): let/lambda/dolist/while/condition-case plus
these editor builtins. For syntax + examples: help {regex} (patterns/replace),
help {treesit} (structure), help {recipes} (worked programs).

Motion/inspect: point point-min point-max goto-char goto-line forward-line
  beginning-of-line end-of-line line-number-at-pos current-column looking-at
  char-after buffer-substring buffer-string
Search/replace: re-search-forward re-search-backward search-forward
  search-backward replace-match match-string replace-regexp replace-string
  count-matches regexp-quote
Edit/delete: insert delete-region delete-char flush-lines keep-lines
  kill-region kill-whole-line yank sort-lines delete-trailing-whitespace
Narrowing: narrow-to-region widen save-restriction save-excursion
Structure (tree-sitter): treesit-goto-defun treesit-defun-at treesit-node-at
  treesit-node-start treesit-node-end treesit-node-text treesit-narrow-to-defun
  treesit-list-defuns treesit-has-error treesit-query; node edits
  treesit-replace-node treesit-kill-node treesit-wrap-node treesit-insert-sibling
Conflicts: conflict-keep conflict-keep-all conflict-replace conflict-resolve-trivial
  conflict-diff conflict-text conflict-context  (full workflow: help {conflicts})
Atomicity/observe: with-transaction checkpoint report message window
Strings: replace-regexp-in-string split-string string-trim string-replace
  string-join number-to-string string-prefix-p

Gotchas: only the FINAL form's value is returned — wrap earlier results in
(report "label" …). @N/point are absolute (goto-char); line numbers are
narrowing-relative (goto-line). A defun node EXCLUDES its leading attribute /
decorator / doc-comment — extend the region upward to delete the whole item."#;

const REGEX: &str = r#"— regex dialect —
PATTERNS are Emacs regexp syntax, translated onto the RE2 engine (Rust regex
crate) — you write the dialect you'd type in Emacs and keep RE2's linear-time
safety on huge files. Groups are `\(...\)`, shy groups `\(?:...\)`,
alternation `\|`, intervals `\{n,m\}`; a bare `(` `|` `{` is a LITERAL.
Classes `\w \W \b \B \< \>` work; `\`` / `\'` (the buffer ends) and `\A` /
`\z` are the absolute ends. Compiled multi-line, so `^` / `$` anchor LINE
boundaries (Emacs semantics) and the region's edges count as real boundaries
(`^` matches at point-min). Inline flags `(?i)` (case-insensitive) and `(?s)`
(dot-matches-newline) are the one RE2 form kept verbatim. For a class, use
`[[:space:]]` / `[[:alpha:]]` etc.

NOT supported — these ERROR rather than silently mismatch (RE2 doesn't
backtrack): backreferences in a PATTERN (`\1`), lookaround, `\=` (point),
`\_<` / `\_>` (symbol boundaries — use `\b`), and `\s` / `\S` (Emacs syntax
classes — use a `[[:...:]]` class). Inside `[...]` a backslash is a literal
member (Emacs has no class escapes). With an explicit search BOUND, `$` / `\b`
treat the cut as a boundary where real Emacs would consult text beyond it —
the one documented divergence.

REPLACEMENTS (replace-match / replace-regexp) use Emacs syntax: `\&` = the
whole match, `\1`..`\9` = capture groups — written "\\&", "\\1" inside a
lisp string. replace_text (the MCP tool) is fully literal on both sides: no
regex, no backref expansion, no escaping needed.

Search functions: (search-forward "lit" BOUND NOERROR) and
(search-backward …) are literal; (re-search-forward "re" BOUND NOERROR) and
(re-search-backward …) are regex. NOERROR = t makes a miss return nil
instead of erroring. Forward searches leave point AFTER the match; backward
ones AT the match start; both set the match data that (match-beginning N) /
(match-string N) / (replace-match …) read. (looking-at "re") tests at point;
(looking-back "re" LIMIT) tests text ending exactly at point. The canonical
bulk edit is a single streaming pass:
  (goto-char (point-min)) (report "n" (replace-regexp "PAT" "REP"))
(count-matches counts from POINT to the end — goto point-min first.)"#;

const TREESIT: &str = r#"— structural editing (tree-sitter) —
Languages: rust, python, markdown, html, javascript, css, toml, yaml,
elisp (.el and .tl — tulisp scripts) — detected from the buffer name's
extension; (treesit-set-language "rust") overrides (extension-less buffers
default to markdown). A "defun" = Rust function/impl/struct/enum/trait/mod,
Python function/class, Markdown section, HTML element (named by tag),
JS function/class/method, CSS rule/@media/@keyframes (named by selector),
TOML [table], YAML key (every mapping pair — the outline is the key tree),
elisp defun/defmacro; innermost wins.

Survey:    the outline MCP tool, or (treesit-list-defuns) — one
           "KIND START END NAME" line per defun.
Navigate:  (treesit-goto-defun "name") → point at its start, nil if absent.
Scope:     (treesit-narrow-to-defun) narrows to the enclosing defun;
           (widen) exits. The MCP edit tools take scope:{defun:"name"}
           to do goto+narrow+widen for one call.
Nodes are first-class: (treesit-node-at POS) / (treesit-defun-at POS),
navigate with -parent / -child N / -next-sibling / -prev-sibling /
-child-by-field-name "body"; read with -type / -start / -end / -text.
Edit ops: (treesit-replace-node N "text"), (treesit-wrap-node N "pre" "post"),
(treesit-raise-node N), (treesit-kill-node N),
(treesit-insert-sibling N "text" BEFORE).
Query: (treesit-query "(call_expression) @c") — tree-sitter .scm patterns;
reports "@capture KIND START END" and returns the nodes.
Defun spans INCLUDE decoration: Rust #[attributes] and Python decorators
belong to the defun for outline/goto/narrow/anchor purposes (raw node
accessors like treesit-node-start stay faithful to the bare node).
Gotchas: editing OUTDATES nodes from the old parse (re-fetch after edits);
treesit positions are whole-document even under narrowing.
(treesit-has-error) must be nil before saving code (save: true warns
automatically)."#;

const CONFLICTS: &str = r#"— merge-conflict workflow —
Never hand-edit conflict markers; drive the vocabulary. The conflicts MCP
tool renders the overview (hunk numbers, positions, labels, side sizes;
diff3/zdiff3 handled; heed its `!` line about unparseable markers).
Then, via run_program, hunk by hunk:
  (report "left" (conflict-resolve-trivial)) ; sweep the safe hunks first
  (report "left" (conflict-keep-all "ours"|"theirs"|"both")) ; one side, ALL hunks
  (conflict-context N)   ; one hunk rendered WITH surrounding code
  (conflict-diff N)      ; just what differs between the sides
  (conflict-text SIDE N) ; read one side
  (report "left" (conflict-keep "ours"|"theirs"|"both" N))
  (report "left" (conflict-replace "merged text\n" N))
"base"/"all" work on diff3 hunks only. Mutating calls return the REMAINING
count — wrap them in (report …) or it is invisible. Hunk numbers are
1-based and refresh after every edit: resolve highest-N first or re-list
(conflict-keep-all sweeps one side over all hunks for you). Omit N
to address the hunk at point. Nested conflicts surface innermost-first.
Run (treesit-has-error) after resolving code, then save."#;

const GIT: &str = r#"— git history workflow —
In-process rebase/cherry-pick/revert: no network, no hooks, no exec; the
worktree is the warm buffer set. Plan with git_log (oid + summary over a
range like main..HEAD) and git_show (a commit's diff + metadata). git_blame
{path, lines?} reports which commit last touched each line — the find-the-commit
half for a fixup/edit plan.
  git_rebase {onto, plan?}  plan = [{commit, action, message?, message_edits?,
    into?}], action = pick|reword|squash|fixup|edit|split|drop; list order is the
    new commit order. Omit plan to replay all of onto..HEAD. rehearse:true
    previews the result (and whether it is a pure reorder/fold) without applying.
    An `edit` step applies the commit then PAUSES with it checked out: edit the
    worktree, then git_continue folds the changes into that commit. message_edits
    ([{find, replace?}] to replace/delete literal text, [{append}] to add a
    trailing line) tweak a reword/squash/fixup/edit message without retyping it,
    so the sign-off and the rest survive. A `split` step partitions one commit
    into the commits in into = [{message, paths?, hunks?}] — paths takes whole
    files, hunks = [{path, lines:[a,b]}] takes hunks by post-commit line span;
    one part may omit both as the catch-all for everything else.
  git_cherry_pick {commits} / git_revert {commits}  on top of the tip.
  git_move {from, to, paths?, hunks?}  relocate a change between two ADJACENT
    commits (the moved change must be in `from`, not `to`), then replay the rest;
    the branch's final tree is unchanged. hunks = [{path, lines:[a,b]}].
Each STOPS on the first conflict. Then, per stop:
  git_status     which step of how many + the unresolved files
  resolve each file with the conflicts vocabulary (help conflicts), SAVE
  git_continue   commit the resolution + resume (errors while marker lines
                 remain; force:true overrides); at an edit pause, amends the
                 paused commit to match the worktree, then resumes
  git_skip       drop the stopped commit and resume; at an edit pause, resume
                 leaving the landed commit unchanged
  git_abort      restore HEAD/branch/worktree to the pre-op state
Resolve via the keep/replace vocabulary, never by hand-splicing markers."#;

const SESSIONS: &str = r#"— sessions, saving, staleness, undo —
Every tool takes `path` (auto-opens the file into a warm session keyed by
its canonical path; relative paths resolve against the server's cwd) OR
`session` (an explicit id) — never both. Warm sessions persist buffers,
point, checkpoints, kill-ring, and defuns across calls; session_status
lists them with narrowed/stale/unsaved flags and each session's checkpoint
labels, plus the writable roots. close_session drops one (force: true
discards unsaved edits).

Saving: edits live in the warm buffer until saved. Pass save:true on an
edit tool, or call save_buffer ({path} = save the visited file; to:"…" on a
DIFFERENT file = save-as a COPY — it does not rebind the session, so a later
plain save still targets the original; to:"…" on the visited file is just an
in-place save, and an unbound in-memory buffer adopts the file). A visited-file
save is atomic and stale-guarded: if the file changed on disk since open, the
save refuses and the edit stays warm — re-check, then save_buffer elsewhere or
(revert-buffer) to discard. A save-as copy is atomic but NOT stale-guarded (it
writes a different file). A clean-but-drifted buffer auto-reverts before reads,
programs, and rehearsals.

Coding: a file's BOM and DOS (`\r\n`) line endings are detected on open and the
buffer is a normalized VIEW (no BOM character, LF lines — so `\n` patterns and
char positions behave) over the raw paged file, so even huge CRLF files are NOT
materialized. Save keeps untouched regions byte-exact (mixed endings preserved)
and encodes inserted text to the file's EOL. A lone `\r` (classic-Mac CR) is NOT
a line ending — such a file is plain utf-8-unix, its `\r`s kept byte-for-byte.
session_status shows a `coding` (e.g. utf-8-with-signature-dos) when it isn't the
plain utf-8-unix default. `(set-buffer-file-coding-system "utf-8-unix")` strips a
BOM and forces LF on the next save (re-encoding the whole file — the "re-save as
UTF-8" idiom); "utf-8-dos" / "…-with-signature" force those.

Safety ladder: rehearse = dry-run with full report, nothing persists;
(with-transaction …) = all-or-nothing inside a program; checkpoint /
restore_checkpoint = named restore points; undo_last = automatic rewind
to before the last mutating call (ring of 8, no redo). replace_text's
expect_unique:true makes a repeated anchor an error instead of a silent
wrong-site edit. A FAILED run_program does NOT roll back (its error JSON
says dirty:true when edits persist) — undo_last covers that too."#;

const RECIPES: &str = r#"— recipes —
Cross-file rename (one call, atomic across the set, saved only if every
file succeeds — list exactly the files you grepped):
  replace_text {files: [p1, p2, …], pattern, replacement, all: true, save: true}
Replace inside one function (no program needed):
  replace_text {path, pattern, replacement, scope: {defun: "name"}}
Add a function after another:
  insert_text {path, text: "\n\nfn new() {…}", anchor: {defun: "prev"}}
Bulk regex sweep with count:
  run_program: (goto-char (point-min)) (report "n" (replace-regexp "PAT" "REP"))
Per-match logic:
  (while (re-search-forward "PAT" nil t)
    (replace-match "REP"))           ; or inspect (match-string 1) first
Delete every line matching X (from point):
  (goto-char (point-min)) (report "n" (flush-lines "X"))
Edit only lines 100–200:
  (goto-line 100) (narrow-to-region (point) (progn (goto-line 201) (point)))
  … edits … (widen)
Move a block: (kill-region A B) … (goto-char DEST) (yank)
Delete a whole defun incl. its leading #[attr]/decorator/// (the defun node
EXCLUDES those — walk up first):
  (when (treesit-goto-defun "name")
    (let ((end (treesit-node-end (treesit-defun-at))))
      (goto-char (treesit-node-start (treesit-defun-at))) (beginning-of-line)
      ;; (not (bobp)) is essential: at the top of the buffer (forward-line -1)
      ;; can't advance, so without it a comment/attr on line 1 spins forever.
      (while (and (not (bobp))
                  (save-excursion (forward-line -1) (looking-at "[ \t]*(#\\[|//|///)")))
        (forward-line -1))
      (delete-region (point) end)))   ; save:true — fmt tidies any leftover blank
Replace or wrap a whole defun (structural):
  (treesit-replace-node (treesit-defun-at) "fn name() { todo!() }")
  (treesit-wrap-node (treesit-defun-at) "mod tests {\n" "\n}")
Visit every top-level defun by name:
  (dolist (name (treesit-list-defuns)) (treesit-goto-defun name) …)
Whole-word rename (Emacs-dialect patterns everywhere — incl. looking-at /
re-search-forward; literal replace_text would also hit substrings):
  (goto-char (point-min)) (report "n" (replace-regexp "\\bfoo\\b" "bar"))
Resolve merge conflicts (overview: the conflicts tool; vocab: help {conflicts}).
Sweep the safe hunks, then take one side for ALL the rest in a single call:
  (report "left" (conflict-resolve-trivial))      ; identical/whitespace hunks
  (report "left" (conflict-keep-all "ours"))      ; or "theirs" | "both"
Mixed per-hunk: (conflict-keep "ours"|"theirs"|"both" N) or a hand-merge
(conflict-replace "merged\n" N); inspect first with (conflict-diff N). N
renumbers after every resolve, so resolve the HIGHEST N first. Then check
(treesit-has-error) and save.
Preview anything non-trivial first: rehearse {program}, inspect the diff,
then run_program the same program (with save:true when done)."#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_topic_resolves() {
        for (name, _) in TOPICS {
            assert!(topic(name).is_some(), "topic {name} must resolve");
        }
        assert!(topic("nope").is_none());
    }
}
