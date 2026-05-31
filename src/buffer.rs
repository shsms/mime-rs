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
    pub name: String,
    pub last_match: Option<MatchData>,
}

impl Buffer {
    pub fn from_string(name: impl Into<String>, text: impl Into<String>) -> Self {
        Buffer {
            text: text.into(),
            point: 1,
            mark: None,
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
        1
    }
    pub fn point_max(&self) -> usize {
        self.char_len() + 1
    }

    /// Byte offset of 1-based char position `p` (clamped into the buffer).
    fn byte_of(&self, p: usize) -> usize {
        let p = p.clamp(1, self.point_max());
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
        let at = self.byte_of(self.point);
        self.text.insert_str(at, s);
        self.point += s.chars().count();
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
}
