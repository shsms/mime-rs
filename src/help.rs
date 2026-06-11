//! On-demand reference text for the `help` MCP tool — the canonical
//! cheat-sheets an agent would otherwise need carried in its prompt. Each
//! topic is a small, self-contained brief; the always-loaded tool schemas can
//! stay terse and point here. Keep these in sync with the engine: they ARE
//! the contract an agent acts on, fetched only when needed.

/// The topics `help` serves, with one-line summaries — also the "unknown
/// topic" help text.
pub const TOPICS: &[(&str, &str)] = &[
    (
        "regex",
        "the regex dialect: RE2 patterns, Emacs anchors, replacement syntax",
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
        "sessions",
        "warm sessions, path addressing, saving, staleness, undo",
    ),
    ("recipes", "common edit shapes as ready-to-adapt programs"),
];

/// The reference text for `name`, or `None` for an unknown topic.
pub fn topic(name: &str) -> Option<&'static str> {
    match name {
        "regex" => Some(REGEX),
        "treesit" => Some(TREESIT),
        "conflicts" => Some(CONFLICTS),
        "sessions" => Some(SESSIONS),
        "recipes" => Some(RECIPES),
        _ => None,
    }
}

const REGEX: &str = r#"— regex dialect —
PATTERNS are RE2 (the Rust regex crate): linear-time, safe on huge files;
no backreferences, no lookaround. Compiled multi-line, so `^` / `$` anchor
LINE boundaries (Emacs semantics) and the accessible region's edges count
as real boundaries (`^` matches at point-min). `\A` / `\z` are the absolute
ends. Inline flags work: `(?i)` case-insensitive, `(?s)` dot-matches-newline.
With an explicit search BOUND, `$` / `\b` treat the cut as a boundary where
real Emacs would consult the text beyond it — the one documented divergence.

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
Languages: rust, python, markdown — detected from the buffer name's
extension; (treesit-set-language "rust") overrides (extension-less buffers
default to markdown). A "defun" = Rust function/impl/struct/enum/trait/mod,
Python function/class, Markdown section; innermost wins.

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
Gotchas: editing OUTDATES nodes from the old parse (re-fetch after edits);
treesit positions are whole-document even under narrowing; a Rust defun
node EXCLUDES its preceding #[attributes] — anchor/delete with the
attribute line in mind. (treesit-has-error) must be nil before saving code
(save: true warns automatically)."#;

const CONFLICTS: &str = r#"— merge-conflict workflow —
Never hand-edit conflict markers; drive the vocabulary. The conflicts MCP
tool renders the overview (hunk numbers, positions, labels, side sizes;
diff3/zdiff3 handled; heed its `!` line about unparseable markers).
Then, via run_program, hunk by hunk:
  (report "left" (conflict-resolve-trivial)) ; sweep the safe hunks first
  (conflict-context N)   ; one hunk rendered WITH surrounding code
  (conflict-diff N)      ; just what differs between the sides
  (conflict-text SIDE N) ; read one side
  (report "left" (conflict-keep "ours"|"theirs"|"both" N))
  (report "left" (conflict-replace "merged text\n" N))
"base"/"all" work on diff3 hunks only. Mutating calls return the REMAINING
count — wrap them in (report …) or it is invisible. Hunk numbers are
1-based and refresh after every edit: resolve top-down or re-list. Omit N
to address the hunk at point. Nested conflicts surface innermost-first.
Run (treesit-has-error) after resolving code, then save."#;

const SESSIONS: &str = r#"— sessions, saving, staleness, undo —
Every tool takes `path` (auto-opens the file into a warm session keyed by
its canonical path; relative paths resolve against the server's cwd) OR
`session` (an explicit id) — never both. Warm sessions persist buffers,
point, checkpoints, kill-ring, and defuns across calls; session_status
lists them with narrowed/stale/unsaved flags plus the writable roots.
close_session drops one (force: true discards unsaved edits).

Saving: edits live in the warm buffer until saved. Pass save:true on an
edit tool, or call save_buffer ({path} = save the visited file; to:"…" =
save-as). Saves are atomic and stale-guarded: if the file changed on disk
since open, the save refuses and the edit stays warm — re-check, then
save_buffer elsewhere or (revert-buffer) to discard. A clean-but-drifted
buffer auto-reverts before reads, programs, and rehearsals.

Safety ladder: rehearse = dry-run with full report, nothing persists;
(with-transaction …) = all-or-nothing inside a program; checkpoint /
restore_checkpoint = named restore points; undo_last = automatic rewind
to before the last mutating call (ring of 8, no redo). replace_text's
expect_unique:true makes a repeated anchor an error instead of a silent
wrong-site edit. A FAILED run_program does NOT roll back (its error JSON
says dirty:true when edits persist) — undo_last covers that too."#;

const RECIPES: &str = r#"— recipes —
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
