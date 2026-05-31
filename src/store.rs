//! `TextStore` — the editing surface the engine drives, and the seam between
//! the M0 in-memory [`crate::buffer::Buffer`] (the differential-test oracle) and
//! `Quire`, the piece-tree-over-mmap store (M1). All positions are 1-based char
//! positions, Emacs-style; `point_min`/`point_max` honor the narrowing.
pub trait TextStore {
    fn name(&self) -> &str;
    /// The most recent search's match data (whole-match span + group texts).
    fn last_match(&self) -> Option<&crate::buffer::MatchData>;
    /// A cheap, independent copy of this store (structural sharing for Quire,
    /// a full clone for the in-memory Buffer). Backs checkpoints/transactions.
    fn snapshot(&self) -> Box<dyn TextStore>;
    fn text(&self) -> &str;
    fn char_len(&self) -> usize;

    fn point(&self) -> usize;
    fn point_min(&self) -> usize;
    fn point_max(&self) -> usize;
    fn goto_char(&mut self, p: usize);

    fn mark(&self) -> Option<usize>;
    fn set_mark(&mut self, p: usize);
    fn set_mark_opt(&mut self, m: Option<usize>);

    fn insert(&mut self, s: &str);
    fn delete_region(&mut self, a: usize, b: usize);
    fn substring(&self, a: usize, b: usize) -> String;

    fn re_search_forward(&mut self, re: &regex::Regex, bound: Option<usize>) -> Option<usize>;
    fn search_forward(&mut self, needle: &str, bound: Option<usize>) -> Option<usize>;
    fn search_backward(&mut self, needle: &str, bound: Option<usize>) -> Option<usize>;
    fn replace_match(&mut self, replacement: &str) -> Result<(), String>;
    fn looking_at(&self, re: &regex::Regex) -> bool;

    fn beginning_of_line(&mut self);
    fn end_of_line(&mut self);
    fn forward_line(&mut self, n: i64) -> i64;
    fn line_number_at_pos(&self, p: usize) -> usize;
    fn char_after(&self, p: usize) -> Option<char>;
    fn char_before(&self, p: usize) -> Option<char>;

    fn narrowing(&self) -> Option<(usize, usize)>;
    fn narrow_to_region(&mut self, a: usize, b: usize);
    fn widen(&mut self);
    fn set_restriction(&mut self, r: Option<(usize, usize)>);

    /// Create a marker at absolute position `pos` (detached if `None`); returns
    /// its id. Markers auto-adjust as text is inserted/deleted — Emacs markers,
    /// the durable positions that back multiple cursors/viewports.
    fn marker_create(&mut self, pos: Option<usize>) -> usize;
    /// The marker's current absolute 1-based position, or `None` if detached.
    fn marker_position(&self, id: usize) -> Option<usize>;
    /// Point marker `id` at absolute `pos`, or detach it with `None`.
    fn marker_set(&mut self, id: usize, pos: Option<usize>);

    /// After the buffer has been saved to `path`, re-base the store onto that
    /// file — for `Quire`, re-mmap the new file as one fresh original and drop the
    /// pre-save backing (the pinned old mmap inode) and the add buffer; a no-op for
    /// the in-memory `Buffer`. Content and point/mark/narrowing are unchanged.
    fn rebase_to_file(&mut self, path: &std::path::Path) -> std::io::Result<()>;

    /// Stream the buffer's bytes into `w` and return the byte count written. The
    /// streaming atomic save uses this so a multi-GB `Quire` is written piece by
    /// piece, never materialized into one allocation; `Buffer` writes its string.
    fn write_to(&self, w: &mut dyn std::io::Write) -> std::io::Result<usize>;
}

/// Shift markers after inserting `len` chars at absolute position `at`. Emacs
/// insertion-type nil: a marker exactly at `at` stays before the new text.
pub(crate) fn markers_after_insert(markers: &mut [Option<usize>], at: usize, len: usize) {
    for m in markers.iter_mut().flatten() {
        if *m > at {
            *m += len;
        }
    }
}

/// Shift markers after deleting the absolute region `[start, end)`: positions
/// inside collapse to `start`, positions at or beyond `end` shift down by its width.
pub(crate) fn markers_after_delete(markers: &mut [Option<usize>], start: usize, end: usize) {
    for m in markers.iter_mut().flatten() {
        if *m >= end {
            *m -= end - start;
        } else if *m > start {
            *m = start;
        }
    }
}
