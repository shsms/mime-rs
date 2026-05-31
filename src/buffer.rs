//! In-memory text buffer — the M0 `TextStore` oracle (char-positioned, 1-based,
//! Emacs-style). `Quire` (piece-tree-over-mmap) replaces this behind the same
//! surface in M1; this stays as the differential-test oracle.

/// Result of the most recent successful search, in 1-based char positions.
#[derive(Debug, Clone)]
pub struct MatchData {
    pub start: usize,
    pub end: usize,
    /// Captured groups; index 0 is the whole match. `None` = group absent.
    pub groups: Vec<Option<String>>,
}

#[derive(Debug, Clone)]
pub struct Buffer {
    text: String,
    /// Point: 1-based char position in `1..=point_max()`.
    point: usize,
    /// Mark: the other end of the region, if set (1-based char position).
    mark: Option<usize>,
    /// Narrowing restriction `(lo, hi)` in 1-based char positions; the accessible
    /// region is `[lo, hi)`. `None` = whole buffer.
    narrowing: Option<(usize, usize)>,
    /// Live markers, indexed by id; `None` = detached. Absolute 1-based
    /// positions that auto-adjust across edits (Emacs markers).
    markers: Vec<Option<usize>>,
    pub name: String,
    pub last_match: Option<MatchData>,
}

impl Buffer {
    pub fn from_string(name: impl Into<String>, text: impl Into<String>) -> Self {
        Buffer {
            text: text.into(),
            point: 1,
            mark: None,
            narrowing: None,
            markers: Vec::new(),
            name: name.into(),
            last_match: None,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn char_len(&self) -> usize {
        self.text.chars().count()
    }
    pub fn point(&self) -> usize {
        self.point
    }
    pub fn point_min(&self) -> usize {
        self.narrowing.map_or(1, |(lo, _)| lo)
    }
    pub fn point_max(&self) -> usize {
        self.narrowing.map_or(self.char_len() + 1, |(_, hi)| hi)
    }

    /// Byte offset of 1-based char position `p` (clamped into the buffer).
    fn byte_of(&self, p: usize) -> usize {
        // Positions are absolute (narrowing-independent); clamp to the full text.
        let p = p.clamp(1, self.char_len() + 1);
        self.text
            .char_indices()
            .nth(p - 1)
            .map_or(self.text.len(), |(b, _)| b)
    }

    /// 1-based char position of byte offset `byte`.
    fn char_of(&self, byte: usize) -> usize {
        let byte = byte.min(self.text.len());
        self.text[..byte].chars().count() + 1
    }

    pub fn goto_char(&mut self, p: usize) {
        self.point = p.clamp(self.point_min(), self.point_max());
    }

    pub fn insert(&mut self, s: &str) {
        let n = s.chars().count();
        let at_char = self.point;
        let at = self.byte_of(self.point);
        self.text.insert_str(at, s);
        self.point += n;
        if let Some((_, hi)) = self.narrowing.as_mut() {
            *hi += n; // inserted text falls inside the accessible region
        }
        crate::store::markers_after_insert(&mut self.markers, at_char, n);
        self.last_match = None;
    }

    pub fn delete_region(&mut self, a: usize, b: usize) {
        let (lo, hi) = (a.min(b), a.max(b));
        let (lb, hb) = (self.byte_of(lo), self.byte_of(hi));
        self.text.replace_range(lb..hb, "");
        if self.point >= hi {
            self.point -= hi - lo;
        } else if self.point > lo {
            self.point = lo;
        }
        if let Some((nlo, nhi)) = self.narrowing.as_mut() {
            *nhi = nhi.saturating_sub(hi - lo).max(*nlo);
        }
        crate::store::markers_after_delete(&mut self.markers, lo, hi);
        self.last_match = None;
    }

    pub fn substring(&self, a: usize, b: usize) -> String {
        let (lo, hi) = (a.min(b), a.max(b));
        self.text[self.byte_of(lo)..self.byte_of(hi)].to_string()
    }

    /// Regex search forward from point (bounded by `bound` or point-max). On a
    /// hit: record match-data, move point past the match, return the new point.
    pub fn re_search_forward(&mut self, re: &regex::Regex, bound: Option<usize>) -> Option<usize> {
        let start_b = self.byte_of(self.point);
        let end_b = self.byte_of(bound.unwrap_or_else(|| self.point_max()));
        if start_b > end_b {
            return None;
        }
        // Pull owned data out of the match so the borrow on `self.text` ends
        // before we mutate `self` below.
        let (ms_b, me_b, groups) = {
            let caps = re.captures(&self.text[start_b..end_b])?;
            let whole = caps.get(0)?;
            let groups: Vec<Option<String>> = caps
                .iter()
                .map(|g| g.map(|m| m.as_str().to_string()))
                .collect();
            (start_b + whole.start(), start_b + whole.end(), groups)
        };
        let start = self.char_of(ms_b);
        let end = self.char_of(me_b);
        self.last_match = Some(MatchData { start, end, groups });
        self.point = end;
        Some(end)
    }

    /// Replace the last match's region with `replacement` (after `\N` / `\&`
    /// backref expansion); leave point at the end of the inserted text.
    pub fn replace_match(&mut self, replacement: &str) -> Result<(), String> {
        let md = self
            .last_match
            .take()
            .ok_or("replace-match: no preceding match")?;
        let expanded = expand_backrefs(replacement, &md.groups);
        let (lb, hb) = (self.byte_of(md.start), self.byte_of(md.end));
        self.text.replace_range(lb..hb, &expanded);
        self.point = md.start + expanded.chars().count();
        // A replace is a delete of the match span followed by an insert at its start.
        crate::store::markers_after_delete(&mut self.markers, md.start, md.end);
        crate::store::markers_after_insert(&mut self.markers, md.start, expanded.chars().count());
        Ok(())
    }

    pub fn looking_at(&self, re: &regex::Regex) -> bool {
        let b = self.byte_of(self.point);
        re.find(&self.text[b..]).is_some_and(|m| m.start() == 0)
    }

    // ---- mark & region ----
    pub fn mark(&self) -> Option<usize> {
        self.mark
    }
    pub fn set_mark(&mut self, p: usize) {
        self.mark = Some(p.clamp(self.point_min(), self.point_max()));
    }
    pub fn set_mark_opt(&mut self, m: Option<usize>) {
        self.mark = m;
    }

    // ---- narrowing ----
    pub fn narrowing(&self) -> Option<(usize, usize)> {
        self.narrowing
    }
    pub fn narrow_to_region(&mut self, a: usize, b: usize) {
        let full = self.char_len() + 1;
        let lo = a.min(b).clamp(1, full);
        let hi = a.max(b).clamp(lo, full);
        self.narrowing = Some((lo, hi));
        self.point = self.point.clamp(lo, hi);
        self.mark = self.mark.map(|m| m.clamp(lo, hi));
    }
    pub fn widen(&mut self) {
        self.narrowing = None;
    }
    /// Restore a saved restriction (used by `save-restriction`), clamped to the
    /// current text.
    pub fn set_restriction(&mut self, r: Option<(usize, usize)>) {
        let full = self.char_len() + 1;
        self.narrowing = r.map(|(lo, hi)| {
            let lo = lo.clamp(1, full);
            (lo, hi.clamp(lo, full))
        });
        let (lo, hi) = (self.point_min(), self.point_max());
        self.point = self.point.clamp(lo, hi);
    }

    // ---- line navigation ----
    /// Move point to the first char of its line (just after the previous newline).
    pub fn beginning_of_line(&mut self) {
        let b = self.byte_of(self.point);
        let start = self.text[..b].rfind('\n').map_or(0, |i| i + 1);
        self.point = self.char_of(start);
    }
    /// Move point to the end of its line (just before the next newline, or eob).
    pub fn end_of_line(&mut self) {
        let b = self.byte_of(self.point);
        let end = self.text[b..].find('\n').map_or(self.text.len(), |i| b + i);
        self.point = self.char_of(end);
    }
    /// Move point `n` lines forward, to a line beginning. Returns the count of
    /// lines that could not be moved (0 on full success), like Emacs.
    pub fn forward_line(&mut self, n: i64) -> i64 {
        self.beginning_of_line();
        let mut left = n.abs();
        while left > 0 {
            let b = self.byte_of(self.point);
            if n >= 0 {
                match self.text[b..].find('\n') {
                    Some(i) => self.point = self.char_of(b + i + 1),
                    None => {
                        self.point = self.point_max();
                        return left;
                    }
                }
            } else {
                match self.text[..b.saturating_sub(1)].rfind('\n') {
                    Some(i) => self.point = self.char_of(i + 1),
                    None => {
                        self.point = self.point_min();
                        return left;
                    }
                }
            }
            left -= 1;
        }
        0
    }
    /// 1-based line number containing 1-based char position `p`.
    pub fn line_number_at_pos(&self, p: usize) -> usize {
        self.text[..self.byte_of(p)].matches('\n').count() + 1
    }

    // ---- char access (returns Unicode code points, like Emacs characters) ----
    pub fn char_after(&self, p: usize) -> Option<char> {
        if p < self.point_max() {
            self.text[self.byte_of(p)..].chars().next()
        } else {
            None
        }
    }
    pub fn char_before(&self, p: usize) -> Option<char> {
        if p > self.point_min() {
            self.char_after(p - 1)
        } else {
            None
        }
    }

    // ---- exact search ----
    /// Exact forward search from point (bounded). On a hit: set match-data,
    /// move point past the match, return the new point.
    pub fn search_forward(&mut self, needle: &str, bound: Option<usize>) -> Option<usize> {
        if needle.is_empty() {
            return Some(self.point);
        }
        let start_b = self.byte_of(self.point);
        let end_b = self.byte_of(bound.unwrap_or_else(|| self.point_max()));
        let abs = start_b + self.text.get(start_b..end_b)?.find(needle)?;
        let end = self.char_of(abs + needle.len());
        self.last_match = Some(MatchData {
            start: self.char_of(abs),
            end,
            groups: vec![Some(needle.to_string())],
        });
        self.point = end;
        Some(end)
    }
    /// Exact backward search from point (bounded below). On a hit: set
    /// match-data, move point to the match start, return it.
    pub fn search_backward(&mut self, needle: &str, bound: Option<usize>) -> Option<usize> {
        if needle.is_empty() {
            return Some(self.point);
        }
        let lo_b = self.byte_of(bound.unwrap_or_else(|| self.point_min()));
        let hi_b = self.byte_of(self.point);
        let abs = lo_b + self.text.get(lo_b..hi_b)?.rfind(needle)?;
        let start = self.char_of(abs);
        self.last_match = Some(MatchData {
            start,
            end: self.char_of(abs + needle.len()),
            groups: vec![Some(needle.to_string())],
        });
        self.point = start;
        Some(start)
    }

    // ---- markers ----
    pub fn marker_create(&mut self, pos: Option<usize>) -> usize {
        let pos = pos.map(|p| p.clamp(1, self.char_len() + 1));
        self.markers.push(pos);
        self.markers.len() - 1
    }
    pub fn marker_position(&self, id: usize) -> Option<usize> {
        self.markers.get(id).copied().flatten()
    }
    pub fn marker_set(&mut self, id: usize, pos: Option<usize>) {
        let pos = pos.map(|p| p.clamp(1, self.char_len() + 1));
        if let Some(slot) = self.markers.get_mut(id) {
            *slot = pos;
        }
    }
}

/// Thin delegation: the in-memory `Buffer` is the `TextStore` oracle.
impl crate::store::TextStore for Buffer {
    fn name(&self) -> &str {
        &self.name
    }
    fn last_match(&self) -> Option<&MatchData> {
        self.last_match.as_ref()
    }
    fn snapshot(&self) -> Box<dyn crate::store::TextStore> {
        Box::new(self.clone())
    }
    fn text(&self) -> &str {
        Buffer::text(self)
    }
    fn char_len(&self) -> usize {
        Buffer::char_len(self)
    }
    fn point(&self) -> usize {
        Buffer::point(self)
    }
    fn point_min(&self) -> usize {
        Buffer::point_min(self)
    }
    fn point_max(&self) -> usize {
        Buffer::point_max(self)
    }
    fn goto_char(&mut self, p: usize) {
        Buffer::goto_char(self, p)
    }
    fn mark(&self) -> Option<usize> {
        Buffer::mark(self)
    }
    fn set_mark(&mut self, p: usize) {
        Buffer::set_mark(self, p)
    }
    fn set_mark_opt(&mut self, m: Option<usize>) {
        Buffer::set_mark_opt(self, m)
    }
    fn insert(&mut self, s: &str) {
        Buffer::insert(self, s)
    }
    fn delete_region(&mut self, a: usize, b: usize) {
        Buffer::delete_region(self, a, b)
    }
    fn substring(&self, a: usize, b: usize) -> String {
        Buffer::substring(self, a, b)
    }
    fn re_search_forward(&mut self, re: &regex::Regex, bound: Option<usize>) -> Option<usize> {
        Buffer::re_search_forward(self, re, bound)
    }
    fn search_forward(&mut self, needle: &str, bound: Option<usize>) -> Option<usize> {
        Buffer::search_forward(self, needle, bound)
    }
    fn search_backward(&mut self, needle: &str, bound: Option<usize>) -> Option<usize> {
        Buffer::search_backward(self, needle, bound)
    }
    fn replace_match(&mut self, replacement: &str) -> Result<(), String> {
        Buffer::replace_match(self, replacement)
    }
    fn looking_at(&self, re: &regex::Regex) -> bool {
        Buffer::looking_at(self, re)
    }
    fn beginning_of_line(&mut self) {
        Buffer::beginning_of_line(self)
    }
    fn end_of_line(&mut self) {
        Buffer::end_of_line(self)
    }
    fn forward_line(&mut self, n: i64) -> i64 {
        Buffer::forward_line(self, n)
    }
    fn line_number_at_pos(&self, p: usize) -> usize {
        Buffer::line_number_at_pos(self, p)
    }
    fn char_after(&self, p: usize) -> Option<char> {
        Buffer::char_after(self, p)
    }
    fn char_before(&self, p: usize) -> Option<char> {
        Buffer::char_before(self, p)
    }
    fn narrowing(&self) -> Option<(usize, usize)> {
        Buffer::narrowing(self)
    }
    fn narrow_to_region(&mut self, a: usize, b: usize) {
        Buffer::narrow_to_region(self, a, b)
    }
    fn widen(&mut self) {
        Buffer::widen(self)
    }
    fn set_restriction(&mut self, r: Option<(usize, usize)>) {
        Buffer::set_restriction(self, r)
    }
    fn marker_create(&mut self, pos: Option<usize>) -> usize {
        Buffer::marker_create(self, pos)
    }
    fn marker_position(&self, id: usize) -> Option<usize> {
        Buffer::marker_position(self, id)
    }
    fn marker_set(&mut self, id: usize, pos: Option<usize>) {
        Buffer::marker_set(self, id, pos)
    }
    fn rebase_to_file(&mut self, _path: &std::path::Path) -> std::io::Result<()> {
        Ok(()) // in-memory: nothing is mmapped, so there is nothing to reclaim
    }
}

/// Expand Emacs-style `\N` (group) and `\&` (whole match) backrefs.
fn expand_backrefs(rep: &str, groups: &[Option<String>]) -> String {
    let mut out = String::new();
    let mut it = rep.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.peek() {
            Some('&') => {
                it.next();
                if let Some(Some(g)) = groups.first() {
                    out.push_str(g);
                }
            }
            Some(d) if d.is_ascii_digit() => {
                let n = it.next().unwrap().to_digit(10).unwrap() as usize;
                if let Some(Some(g)) = groups.get(n) {
                    out.push_str(g);
                }
            }
            Some('\\') => {
                it.next();
                out.push('\\');
            }
            _ => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_at_end() {
        let mut b = Buffer::from_string("t", "hello");
        b.goto_char(b.point_max());
        b.insert(" world");
        assert_eq!(b.text(), "hello world");
        assert_eq!(b.point(), b.point_max());
    }

    #[test]
    fn regex_replace_loop() {
        let mut b = Buffer::from_string("t", "world world");
        let re = regex::Regex::new("world").unwrap();
        b.goto_char(b.point_min());
        let mut n = 0;
        while b.re_search_forward(&re, None).is_some() {
            b.replace_match("WORLD").unwrap();
            n += 1;
        }
        assert_eq!(n, 2);
        assert_eq!(b.text(), "WORLD WORLD");
    }

    #[test]
    fn backref_expansion() {
        let mut b = Buffer::from_string("t", "Doe, John");
        let re = regex::Regex::new(r"(\w+), (\w+)").unwrap();
        b.goto_char(1);
        assert!(b.re_search_forward(&re, None).is_some());
        b.replace_match(r"\2 \1").unwrap();
        assert_eq!(b.text(), "John Doe");
    }

    #[test]
    fn delete_region_shifts_point() {
        let mut b = Buffer::from_string("t", "abcdef");
        b.goto_char(7);
        b.delete_region(2, 4); // remove "bc"
        assert_eq!(b.text(), "adef");
        assert_eq!(b.point(), 5);
    }

    #[test]
    fn marker_adjusts_on_insert() {
        let mut b = Buffer::from_string("t", "abcdef");
        let m = b.marker_create(Some(4)); // at 'd'
        b.goto_char(2);
        b.insert("XY"); // 2 chars before the marker → it shifts right
        assert_eq!(b.marker_position(m), Some(6));
        b.goto_char(6);
        b.insert("Z"); // exactly at the marker → stays put (insertion-type nil)
        assert_eq!(b.marker_position(m), Some(6));
        b.goto_char(b.point_max());
        b.insert("!"); // after the marker → no move
        assert_eq!(b.marker_position(m), Some(6));
    }

    #[test]
    fn marker_adjusts_on_delete() {
        let mut b = Buffer::from_string("t", "abcdefgh");
        let before = b.marker_create(Some(2));
        let inside = b.marker_create(Some(4));
        let after = b.marker_create(Some(7));
        b.delete_region(3, 6); // remove "cde"
        assert_eq!(b.text(), "abfgh");
        assert_eq!(b.marker_position(before), Some(2)); // unchanged
        assert_eq!(b.marker_position(inside), Some(3)); // collapsed to region start
        assert_eq!(b.marker_position(after), Some(4)); // 7 - 3
    }

    #[test]
    fn marker_survives_replace_match() {
        let mut b = Buffer::from_string("t", "foo bar");
        let m = b.marker_create(Some(5)); // start of "bar"
        let re = regex::Regex::new("foo").unwrap();
        b.goto_char(1);
        assert!(b.re_search_forward(&re, None).is_some());
        b.replace_match("hello").unwrap(); // grows the match by 2 chars
        assert_eq!(b.text(), "hello bar");
        assert_eq!(b.marker_position(m), Some(7)); // still at 'b' of "bar"
    }

    #[test]
    fn marker_detach_and_clamp() {
        let mut b = Buffer::from_string("t", "abc");
        let m = b.marker_create(None);
        assert_eq!(b.marker_position(m), None); // detached
        b.marker_set(m, Some(99)); // clamps to char_len + 1
        assert_eq!(b.marker_position(m), Some(4));
        b.marker_set(m, None);
        assert_eq!(b.marker_position(m), None);
    }
}
