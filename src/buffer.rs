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
    /// Cached `text.chars().count()`, maintained across every edit so position
    /// math (point-max, clamping) is O(1) instead of rescanning the string.
    char_len: usize,
    /// A known-good (1-based char position, byte offset) pair used to seed the
    /// next char↔byte conversion: sequential access (search/replace loops) then
    /// walks a small delta from here rather than from the buffer start.
    byte_hint: std::cell::Cell<(usize, usize)>,
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
    /// Content version (see `TextStore::version`): re-stamped on every text
    /// mutation; a clone (snapshot) keeps it — same version, same text.
    version: u64,
}

impl Buffer {
    pub fn from_string(name: impl Into<String>, text: impl Into<String>) -> Self {
        let text = text.into();
        let char_len = text.chars().count();
        Buffer {
            text,
            char_len,
            byte_hint: std::cell::Cell::new((1, 0)),
            point: 1,
            mark: None,
            narrowing: None,
            markers: Vec::new(),
            name: name.into(),
            last_match: None,
            version: crate::store::next_version(),
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn char_len(&self) -> usize {
        self.char_len
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

    /// Byte offset of 1-based char position `p` (clamped into the buffer). Seeds
    /// from `byte_hint` so sequential conversions cost O(distance), not O(p).
    /// Boundary answers (start/end) are O(1) and deliberately leave the hint
    /// untouched, so a `point_max` lookup mid-search doesn't reset it.
    fn byte_of(&self, p: usize) -> usize {
        let p = p.clamp(1, self.char_len + 1);
        if p == 1 {
            return 0;
        }
        if p == self.char_len + 1 {
            return self.text.len();
        }
        let (hc, hb) = self.byte_hint.get();
        let byte = if p >= hc {
            hb + self.text[hb..]
                .char_indices()
                .nth(p - hc)
                .map_or(self.text.len() - hb, |(rb, _)| rb)
        } else {
            self.text
                .char_indices()
                .nth(p - 1)
                .map_or(self.text.len(), |(b, _)| b)
        };
        self.byte_hint.set((p, byte));
        byte
    }

    /// 1-based char position of byte offset `byte`. Hint-seeded like `byte_of`.
    fn char_of(&self, byte: usize) -> usize {
        let byte = byte.min(self.text.len());
        if byte == 0 {
            return 1;
        }
        if byte == self.text.len() {
            return self.char_len + 1;
        }
        let (hc, hb) = self.byte_hint.get();
        let ch = if byte >= hb {
            hc + self.text[hb..byte].chars().count()
        } else {
            self.text[..byte].chars().count() + 1
        };
        self.byte_hint.set((ch, byte));
        ch
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
        self.char_len += n;
        self.byte_hint.set((self.point, at + s.len()));
        if let Some((_, hi)) = self.narrowing.as_mut() {
            *hi += n; // inserted text falls inside the accessible region
        }
        crate::store::markers_after_insert(&mut self.markers, at_char, n);
        self.last_match = None;
        self.version = crate::store::next_version();
    }

    pub fn delete_region(&mut self, a: usize, b: usize) {
        let (lo, hi) = (a.min(b), a.max(b));
        let (lb, hb) = (self.byte_of(lo), self.byte_of(hi));
        self.text.replace_range(lb..hb, "");
        self.char_len -= hi - lo;
        self.byte_hint.set((lo, lb));
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
        self.version = crate::store::next_version();
    }

    pub fn substring(&self, a: usize, b: usize) -> String {
        let (lo, hi) = (a.min(b), a.max(b));
        self.text[self.byte_of(lo)..self.byte_of(hi)].to_string()
    }

    /// Regex search forward from point (bounded by `bound` or point-max). On a
    /// hit: record match-data, move point past the match, return the new point.
    ///
    /// The haystack's LEFT edge is Emacs's: it starts at point-min (the
    /// accessible region's beginning counts as a real line/word boundary, so
    /// `^` matches there) and the pre-point text stays in as context, so a
    /// mid-line point doesn't pass for a line beginning. The RIGHT edge is
    /// span-bounded: the match is confined to end at or before the bound,
    /// while `$`/`\b` at an explicit mid-line BOUND consult the real buffer
    /// past it up to point-max — Emacs semantics (`regex-automata`'s
    /// `Input::span`; the high-level regex API could not both backtrack a
    /// quantifier to fit the bound and evaluate assertions past it). At
    /// point-max the cut stays a boundary: Emacs's `$` matches at the end of
    /// the accessible region.
    pub fn re_search_forward(&mut self, re: &regex::Regex, bound: Option<usize>) -> Option<usize> {
        let start_b = self.byte_of(self.point);
        let end_b = self.byte_of(bound.unwrap_or_else(|| self.point_max()));
        if start_b > end_b {
            return None;
        }
        let hay_off = self.byte_of(self.point_min()).min(start_b);
        let ctx_end = self.byte_of(self.point_max()).max(end_b);
        let (ms_b, me_b, groups) = {
            let (s, e, groups) = crate::store::span_captures(
                re,
                &self.text[hay_off..ctx_end],
                (start_b - hay_off)..(end_b - hay_off),
            )?;
            (hay_off + s, hay_off + e, groups)
        };
        let start = self.char_of(ms_b);
        let end = self.char_of(me_b);
        self.last_match = Some(MatchData { start, end, groups });
        self.point = end;
        Some(end)
    }

    /// Regex search backward from point (bounded below by `bound` or
    /// point-min): the latest-starting match wholly inside the window
    /// `[bound, point)`. On a hit: record match-data, move point to the match
    /// START, return it — Emacs `re-search-backward` semantics. The window is
    /// a span over the accessible region, so `^`/`$`/`\b` at either edge
    /// consult the real buffer beyond it (see `re_search_forward`), while the
    /// match itself stays confined to the window.
    pub fn re_search_backward(&mut self, re: &regex::Regex, bound: Option<usize>) -> Option<usize> {
        let lo = bound.unwrap_or_else(|| self.point_min()).min(self.point);
        let lo_b = self.byte_of(lo);
        let hi_b = self.byte_of(self.point);
        let min_b = self.byte_of(self.point_min()).min(lo_b);
        let max_b = self.byte_of(self.point_max()).max(hi_b);
        let (ms, me, groups) = crate::store::latest_match_in_span(
            re,
            &self.text[min_b..max_b],
            (lo_b - min_b)..(hi_b - min_b),
        )?;
        let start = self.char_of(min_b + ms);
        let end = self.char_of(min_b + me);
        self.last_match = Some(MatchData { start, end, groups });
        self.point = start;
        Some(start)
    }

    /// Replace the last match's region with `replacement` (after `\N` / `\&`
    /// backref expansion); leave point at the end of the inserted text.
    pub fn replace_match(&mut self, replacement: &str) -> Result<(), String> {
        let md = self
            .last_match
            .take()
            .ok_or("replace-match: no preceding match")?;
        let expanded = expand_backrefs(replacement, &md.groups);
        let new_len = expanded.chars().count();
        let old_len = md.end - md.start;
        let (lb, hb) = (self.byte_of(md.start), self.byte_of(md.end));
        self.text.replace_range(lb..hb, &expanded);
        self.point = md.start + new_len;
        self.char_len = self.char_len + new_len - old_len;
        self.byte_hint.set((self.point, lb + expanded.len()));
        // The narrowing's upper bound must track the net length change (the
        // replaced span lies inside the region), exactly as insert/delete_region
        // do — otherwise a length-changing replace leaves a stale restriction.
        if let Some((nlo, nhi)) = self.narrowing.as_mut() {
            if new_len >= old_len {
                *nhi += new_len - old_len;
            } else {
                *nhi = nhi.saturating_sub(old_len - new_len).max(*nlo);
            }
        }
        // A replace is a delete of the match span followed by an insert at its start.
        crate::store::markers_after_delete(&mut self.markers, md.start, md.end);
        crate::store::markers_after_insert(&mut self.markers, md.start, new_len);
        self.version = crate::store::next_version();
        Ok(())
    }

    pub fn looking_at(&self, re: &regex::Regex) -> bool {
        // `find_at` keeps the pre-point text back to point-min as boundary
        // context for `^`/`\b` (see re_search_forward — the restriction's
        // start is a real line beginning); a match counts only if it starts
        // AT point. The right side deliberately runs to the document end,
        // matching the Quire scan window.
        let b = self.byte_of(self.point);
        let min_b = self.byte_of(self.point_min()).min(b);
        re.find_at(&self.text[min_b..], b - min_b)
            .is_some_and(|m| m.start() == b - min_b)
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
    // Line motion honors the narrowing, like Emacs: the RESULT is clamped into
    // [point_min, point_max] so point never escapes the accessible region even
    // when the restriction starts or ends mid-line. The newline scan itself
    // runs over the raw text and the clamp happens in CHAR space — deliberately
    // not byte space, because `byte_of(point_min/point_max)` misses the hint's
    // boundary fast paths on a narrowed buffer and would drag the byte hint to
    // the region edge on every call, degrading line-walking loops (occur,
    // window, conflict scans) to O(region²) conversions.
    /// Move point to the first char of its line (just after the previous
    /// newline), clamped to `point_min`.
    pub fn beginning_of_line(&mut self) {
        let b = self.byte_of(self.point);
        let start = self.text[..b].rfind('\n').map_or(0, |i| i + 1);
        // The raise is document-clamped: a stale narrowing can outlive
        // deletions that shrank the text past its bounds.
        let min = self.point_min().min(self.char_len + 1);
        self.point = self.char_of(start).max(min);
    }
    /// Move point to the end of its line (just before the next newline),
    /// clamped to `point_max`.
    pub fn end_of_line(&mut self) {
        let b = self.byte_of(self.point);
        let end = self.text[b..].find('\n').map_or(self.text.len(), |i| b + i);
        self.point = self.char_of(end).min(self.point_max());
    }
    /// Move point `n` lines forward, to a line beginning. Returns the count of
    /// lines that could not be moved (0 on full success), like Emacs. A line
    /// beginning outside the narrowing is unreachable: point clamps to the
    /// boundary and the move counts as short.
    pub fn forward_line(&mut self, n: i64) -> i64 {
        self.beginning_of_line();
        let mut left = n.abs();
        while left > 0 {
            let b = self.byte_of(self.point);
            if n >= 0 {
                match self.text[b..].find('\n') {
                    Some(i) => {
                        let target = self.char_of(b + i + 1);
                        if target > self.point_max() {
                            self.point = self.point_max();
                            return left;
                        }
                        self.point = target;
                    }
                    None => {
                        self.point = self.point_max();
                        return left;
                    }
                }
            } else {
                // Exclude the previous line's terminator: cut just before the
                // char preceding point. `byte_of(point - 1)` keeps the cut on
                // a char boundary even when the clamped line start sits after
                // a multibyte char (a `b - 1` byte cut would split it).
                let cut = self.byte_of(self.point.saturating_sub(1).max(1));
                let target = match self.text[..cut].rfind('\n') {
                    Some(i) => self.char_of(i + 1),
                    None => 1,
                };
                if target <= self.point_min() {
                    let clamped = self.point_min();
                    let moved = self.point != clamped;
                    self.point = clamped;
                    // Reaching a GENUINE line beginning that coincides with
                    // point-min — the buffer start, or a restriction starting
                    // just after a newline — is a complete move (Emacs
                    // reports 0). Clamping to a mid-line restriction start,
                    // or not moving at all, stays short. The check peeks the
                    // RAW text (char_before is narrowing-clamped and would
                    // hide the newline just outside the restriction).
                    let genuine = target == clamped
                        && (clamped == 1 || self.text[..self.byte_of(clamped)].ends_with('\n'));
                    if genuine && moved {
                        left -= 1;
                        continue;
                    }
                    return left;
                }
                self.point = target;
            }
            left -= 1;
        }
        0
    }
    /// 1-based line number containing 1-based char position `p`, counted from
    /// the start of the accessible region — Emacs's `line-number-at-pos`
    /// default, so the numbers that `window`/`occur`/conflict overviews
    /// display round-trip through `goto-line` under narrowing.
    pub fn line_number_at_pos(&self, p: usize) -> usize {
        let b = self.byte_of(p);
        let min_b = self.byte_of(self.point_min()).min(b);
        self.text[min_b..b].matches('\n').count() + 1
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
    fn set_name(&mut self, name: &str) {
        self.name = name.to_string();
    }
    fn version(&self) -> u64 {
        self.version
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
    fn re_search_backward(&mut self, re: &regex::Regex, bound: Option<usize>) -> Option<usize> {
        Buffer::re_search_backward(self, re, bound)
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
    fn marker_count(&self) -> usize {
        self.markers.len()
    }
    fn marker_set(&mut self, id: usize, pos: Option<usize>) {
        Buffer::marker_set(self, id, pos)
    }
    fn rebase_to_file(&mut self, _path: &std::path::Path) -> std::io::Result<()> {
        Ok(()) // in-memory: nothing is mmapped, so there is nothing to reclaim
    }
    fn write_to(&self, w: &mut dyn std::io::Write) -> std::io::Result<usize> {
        let bytes = Buffer::text(self).as_bytes();
        w.write_all(bytes)?;
        Ok(bytes.len())
    }
}

/// Expand Emacs-style `\N` (group) and `\&` (whole match) backrefs.
/// `pub(crate)`: Quire's `replace_match` and the streaming `replace-regexp`
/// builtin share this one copy.
pub(crate) fn expand_backrefs(rep: &str, groups: &[Option<String>]) -> String {
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
    fn line_motion_clamps_to_the_narrowing() {
        // "abc\ndef\nghi": narrow to "c\nde" (mid-line on both ends). Line
        // motion must not escape the accessible region (Emacs semantics).
        let mut b = Buffer::from_string("t", "abc\ndef\nghi");
        b.narrow_to_region(3, 7); // chars 3..7 = "c\nde"
        b.goto_char(5); // on 'd'
        b.end_of_line();
        assert_eq!(b.point(), 7, "end-of-line stops at point-max, not at \\n");
        b.beginning_of_line();
        assert_eq!(
            b.point(),
            5,
            "beginning-of-line stops at the line start within"
        );
        b.goto_char(3);
        b.beginning_of_line();
        assert_eq!(
            b.point(),
            3,
            "beginning-of-line stops at point-min, not line start"
        );
        // forward_line: one newline is accessible; the second move clamps.
        b.goto_char(3);
        assert_eq!(b.forward_line(1), 0);
        assert_eq!(b.point(), 5);
        assert_eq!(b.forward_line(1), 1, "no newline left before point-max");
        assert_eq!(b.point(), 7);
        // ...and backward past point-min clamps too.
        assert_eq!(b.forward_line(-2), 2, "no line beginnings above point-min");
        assert_eq!(b.point(), 3);
    }

    #[test]
    fn char_access_stops_at_the_narrowing_boundaries() {
        // char-after(point_max) and char-before(point_min) are None — the
        // chars just outside a mid-line narrowing never leak through.
        let mut b = Buffer::from_string("t", "abcdef");
        b.narrow_to_region(3, 5); // "cd"
        assert_eq!(b.char_after(3), Some('c'));
        assert_eq!(b.char_after(4), Some('d'));
        assert_eq!(b.char_after(5), None, "'e' is outside the narrowing");
        assert_eq!(b.char_before(4), Some('c'));
        assert_eq!(b.char_before(3), None, "'b' is outside the narrowing");
    }

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
    fn line_anchor_sees_real_boundaries_not_the_search_window() {
        // A multi-line `^` must match true line starts after point, and must
        // NOT match at a mid-line point (the pre-point text is context).
        let mut b = Buffer::from_string("t", "one\ntwo\nthree");
        let re = regex::RegexBuilder::new("^t")
            .multi_line(true)
            .build()
            .unwrap();
        b.goto_char(2); // mid "one" — not a line beginning
        assert_eq!(b.re_search_forward(&re, None), Some(6), "start of 'two'");
        assert!(!b.looking_at(&re), "'wo\\n...' is not at a line start");
        b.goto_char(5); // start of "two" — a real line beginning
        assert!(b.looking_at(&re));
        // \b at the window edge: point mid-word must not fake a boundary.
        let word = regex::Regex::new(r"\bne\b").unwrap();
        b.goto_char(2); // "o|ne" — 'n' is not word-start
        assert_eq!(b.re_search_forward(&word, None), None);
    }

    #[test]
    fn bounded_search_fits_quantifiers_and_point_max_is_a_line_end() {
        // A bound confines the match, so a greedy match backtracks to fit
        // (Emacs parity); at point-max `$` matches (Emacs: the accessible
        // region's end is a line end).
        let mut b = Buffer::from_string("t", "aaa");
        let re = regex::Regex::new("a+").unwrap();
        b.goto_char(1);
        assert_eq!(b.re_search_forward(&re, Some(3)), Some(3), "\"aa\" fits");
        let mut b = Buffer::from_string("t", "foobar\n");
        b.narrow_to_region(1, 4); // accessible region is "foo"
        b.goto_char(1);
        let re2 = regex::RegexBuilder::new("o$")
            .multi_line(true)
            .build()
            .unwrap();
        assert_eq!(
            b.re_search_forward(&re2, None),
            Some(4),
            "point-max IS a line end"
        );
    }

    #[test]
    fn bounded_search_assertions_consult_past_the_bound() {
        // Span-bounded search: the match must END at or before the bound, but
        // `$`/`\b` AT the bound judge the real buffer past it, like Emacs —
        // not the cut.
        let foo_eol = regex::RegexBuilder::new("foo$")
            .multi_line(true)
            .build()
            .unwrap();
        let mut b = Buffer::from_string("t", "foobar\n");
        b.goto_char(1);
        assert_eq!(
            b.re_search_forward(&foo_eol, Some(4)),
            None,
            "the line continues past the bound"
        );
        let mut b = Buffer::from_string("t", "foo\nbar");
        b.goto_char(1);
        assert_eq!(
            b.re_search_forward(&foo_eol, Some(4)),
            Some(4),
            "a real line end at the bound"
        );
        let word = regex::Regex::new(r"foo\b").unwrap();
        let mut b = Buffer::from_string("t", "foobar");
        b.goto_char(1);
        assert_eq!(b.re_search_forward(&word, Some(4)), None, "mid-word bound");
        let mut b = Buffer::from_string("t", "foo bar");
        b.goto_char(1);
        assert_eq!(b.re_search_forward(&word, Some(4)), Some(4));
    }

    #[test]
    fn backward_search_edges_consult_the_real_buffer() {
        // Right edge (point): the match ends at or before point, but `$`
        // there reads the real buffer.
        let foo_eol = regex::RegexBuilder::new("foo$")
            .multi_line(true)
            .build()
            .unwrap();
        let mut b = Buffer::from_string("t", "foobar\n");
        b.goto_char(4);
        assert_eq!(
            b.re_search_backward(&foo_eol, None),
            None,
            "the line continues past point"
        );
        let mut b = Buffer::from_string("t", "foo\nbar");
        b.goto_char(4);
        assert_eq!(b.re_search_backward(&foo_eol, None), Some(1));
        // Left edge (an explicit BOUND): `\b` at the bound must not fake a
        // word boundary out of the cut.
        let word = regex::Regex::new(r"\boo").unwrap();
        let mut b = Buffer::from_string("t", "xoo bar");
        b.goto_char(4);
        assert_eq!(
            b.re_search_backward(&word, Some(2)),
            None,
            "'oo' at the bound is mid-word"
        );
    }

    #[test]
    fn multibyte_positions_survive_hint_walks_and_length_changes() {
        // Accents/em-dashes are multi-byte (char≠byte), so the cached char_len
        // and the byte_hint cursor must stay correct walking forward, jumping
        // back, and across length-changing replaces.
        let mut b = Buffer::from_string("t", "café — náïve — déjà — fin");
        assert_eq!(b.char_len(), "café — náïve — déjà — fin".chars().count());
        let re = regex::Regex::new("—").unwrap(); // em-dash: 3 bytes, 1 char
        b.goto_char(1);
        let mut n = 0;
        while b.re_search_forward(&re, None).is_some() {
            b.replace_match("-").unwrap(); // 3 bytes -> 1
            n += 1;
        }
        assert_eq!(n, 3);
        assert_eq!(b.text(), "café - náïve - déjà - fin");
        assert_eq!(b.char_len(), b.text().chars().count());
        // substring forward, near the end, then a backward jump (hint was at end)
        let cl = b.char_len();
        assert_eq!(b.substring(1, 5), "café");
        assert_eq!(b.substring(cl - 2, cl + 1), "fin");
        assert_eq!(b.substring(1, 5), "café");
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
