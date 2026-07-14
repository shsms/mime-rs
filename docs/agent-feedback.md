# Field notes from an AI agent using mime

Notes from one long working session. The work was on a repo with
some Rust code and some Python code. It included many small edits,
several multi-file edits, two full interactive-rebase cycles with
fixup commits, and merge-conflict resolution. All examples below
are generic.

## What worked well

### Rehearsed rebases are the killer feature

`git_rebase {rehearse: true}` caught two planning mistakes before
anything was touched:

- A fixup targeted a commit where the API it patched did not exist
  yet (the field was introduced three commits later). The rehearsal
  stopped on the conflict, so the fixup could be retargeted with
  zero cleanup.
- For a message-only reword pass, the rehearsal printed
  "resulting tree IDENTICAL". That one line replaced the whole
  manual backup-branch-and-compare-tree ritual.

### Sparse autosquash

Passing only the relocations (`{commit, into}` pairs) instead of
transcribing a 20-step plan is exactly right. Plans were short and
readable, and the untouched commits could not be mis-transcribed.

### `edits` batches and `expect_unique`

One `replace_text` call with six `edits` rewrote a file in one shot,
with each pattern checked. `expect_unique` turned a
formatter-drifted pattern (a match that shell `sed` would silently
miss or mis-hit) into a hard error. That failure mode had already
bitten this project once through plain shell replacement.

### Error messages that list the valid arguments

`unknown argument "new_text" ... valid arguments: all, edits,
expect_unique, ...` made the retry correct on the first attempt. The
same for `open_file`'s unknown-argument error. Please keep doing
this everywhere.

### Conflict tools

`conflicts` → `conflict-diff` → `conflict-replace` → `git_continue`
resolved five conflicts across two rebases without ever hand-editing
marker lines. Labeling each side with its commit subject made
"ours vs theirs" unambiguous mid-rebase.

### Backup refs

`pre-op tip backed up at refs/mime-backup/<branch>` lowers the cost
of running a rebase considerably.

## Pain points

### Stale warm sessions after external changes

This was the most frequent friction. The disk changed under warm
buffers many times: `git checkout -- <file>`, a rebase moving HEAD,
`cargo fmt`. The buffer keeps serving the old content, so the next
`replace_text` matches against a world that no longer exists. The
workaround was `close_session` before re-editing, but only after
learning the hard way. `open_file` has no reload flag (tried
`reload: true`, not an argument).

Wanted, in order of preference:

1. Auto-revert a *clean* (no unsaved edits) buffer when the file on
   disk has changed. There is nothing to lose in that case.
2. A `revert`/`from_disk` flag on `open_file` or on the edit tools.
3. At minimum: any tool touching a clean-but-outdated buffer should
   say so in its result ("disk changed under this session"), the way
   `save_buffer` already stale-guards writes. Reads deserve the
   same guard as writes.

### Rehearsal stops at the first conflict

Planning a 5-fixup autosquash took three rehearse rounds because
each rehearsal reports only the first conflicting step. A mode that
continues past conflicts and lists *every* step that would conflict
(with the files) would let the plan be repaired in one pass.

Related wish: when a fixup conflicts, say *why* in terms of history —
e.g. "the fixup's hunks touch regions that do not exist at the
target commit; last reshaped by <sha> <subject>". That is exactly
the information needed to pick the right `into`, and mime already
has it (a blame of the fixup's hunk ranges).

### No hunk-level staging for building fixups

`git_rebase` can `split` an existing commit by hunks, but there is
no equivalent for the *working tree*. The session's workflow was:
make all review fixes at once, then split them into fixup commits
for different targets. When one file carried hunks for two targets,
the only route was: snapshot the file, `git checkout` it, re-apply
group A, commit, re-apply group B, commit. Twice.

Wanted: `git_fixup` (or a sibling) that takes working-tree hunks
directly:

```
git_fixup {target: "<sha>", hunks: [{path, lines: [start, end]}]}
```

creating the fixup commit from just those hunks and leaving the rest
of the working tree in place — `git add -p` for agents, who cannot
use the interactive version. (If `git_fixup` already supports
something like this, its one-line description did not make that
discoverable; I defaulted to `git commit --fixup` + shell.)

### `conflict-diff` output is hard to read

The diff comes back as a JSON-escaped string inside the `value`
field (`"\"@@ -0,0 +1,25 @@\\n+def ...\""`). Every other tool that
returns prose renders it as plain text. Rendering the diff as the
tool-result body (like `view` does for file content) would remove a
mental unescaping pass.

### Post-resolution whitespace

Twice, a resolved conflict in a Python file ended with one blank
line between functions where the surrounding file uses two. Not
really mime's fault — the sides genuinely disagreed about the blank
run — but a note in the `conflict-replace` result when the joined
seam's blank-line count differs from both sides would have saved two
follow-up edits and one re-save each.

## Smaller wishes

- **`conflicts` overview with context**: the overview gives position
  and side sizes, but resolving still needs a `read_region` to see
  the surrounding function. Two or three context lines per hunk in
  the overview would often make the separate read unnecessary.
- **More than one backup ref**: "the next op on this branch
  overwrites it" made a three-rebase session feel like walking
  without a rope after the first one. A small ring
  (`refs/mime-backup/<branch>/{0,1,2}`) would cover it.
- **Rebase + per-step checks**: no `exec` is a fair design choice,
  but a rehearsal option that lists which files each replayed commit
  touches would help spot intermediate commits that cannot build
  (e.g. a moved guard landing before the helper it calls exists).
- **`run_program` partial-effect footgun**: documented, and
  `rehearse` covers it, but a failed program could say in its error
  "N edits were applied before the failure and persist; undo_last
  reverts them" so the recovery action is in the error itself.

## Summary

The transactional model (warm buffers, nothing on disk until save,
undo ring) plus rehearsable git surgery is a genuinely better fit
for agent editing than raw shell. The two changes that would remove
most of the remaining friction: treat a clean buffer whose file
changed on disk as revertable (or at least loudly stale) everywhere,
and give fixup-building the same hunk-level precision that
`git_rebase split` already has.
