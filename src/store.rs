//! `TextStore` — the editing surface the engine drives, and the seam between
//! the M0 in-memory [`crate::buffer::Buffer`] (the differential-test oracle) and
//! `Quire`, the piece-tree-over-mmap store (M1). All positions are 1-based char
//! positions, Emacs-style; `point_min`/`point_max` honor the narrowing.
pub trait TextStore {
    fn name(&self) -> &str;
    /// The most recent search's match data (whole-match span + group texts).
    fn last_match(&self) -> Option<&crate::buffer::MatchData>;
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
}
