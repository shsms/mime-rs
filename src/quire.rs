//! Quire — the piece-table `TextStore` over an immutable, memory-mapped original
//! plus an append-only add buffer (M1). This is VS Code's piece tree in its
//! first, copy-on-write-ready form: the document is an ordered sequence of
//! *pieces* `(source, start, len)` that reference one of two immutable backing
//! stores; the original is `mmap`ed (paged by the OS, never fully resident) and
//! the add buffer holds only inserted text. An edit splits pieces and points a
//! new one at the add buffer — original bytes never move.
//!
//! Positions are 1-based char positions, Emacs-style, exactly like
//! [`crate::buffer::Buffer`] (the differential-test oracle this matches
//! byte-for-byte). `point_min`/`point_max` honor the narrowing.
//!
//! ## Data structure (Vec of pieces, not a tree — yet)
//! The spine is a `Vec<Piece>`. The plan's endgame is a *persistent measured
//! B-tree* (`Arc` nodes, path-copied per edit, monoid summaries for O(log n)
//! byte/char/line seeks and free checkpoints). A `Vec` keeps every method
//! correct and the editing path O(pieces) rather than O(bytes) — inserts/deletes
//! splice a handful of pieces, search/index walk pieces — but an individual
//! edit is O(pieces) to splice and seeks are O(pieces) (binary search on a
//! cached prefix sum + a within-piece byte scan). For an unedited file that is
//! one piece; it degrades linearly in the number of edits, which is why the tree
//! is the planned upgrade. Swapping `Vec<Piece>` → measured tree is local to this
//! file and changes no `TextStore` behavior. See `plan.org` §"The editor core".
//!
//! ## What is (and isn't) materialized
//! Editing, search, line/char navigation and snapshotting touch O(pieces) and
//! the OS page cache, never a full copy of the text. The *one* exception is
//! [`TextStore::text`], whose signature returns `&str`: a borrow has to point at
//! contiguous bytes that live somewhere, so the first `text()` after a mutation
//! lazily fills a cached `String` (see [`Quire::full_text`]). That cache is a
//! clearly-marked fallback for the trait's shape, invalidated on every edit, and
//! is the single place a GB file would be brought fully resident — flagged so it
//! can be removed once the surface is fully ranged/streamed. No internal method
//! calls it.

use crate::buffer::MatchData;
use crate::store::TextStore;
use std::cell::RefCell;
use std::path::Path;

/// Which immutable backing store a piece points into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// The file (mmap) or the owned text passed to [`Quire::from_string`].
    Original,
    /// The append-only add buffer — holds only inserted text.
    Add,
}

/// A reference into a backing store: `[start, start + len)` *bytes*. Never owns
/// text. `chars` caches the code-point count of that byte range so position math
/// is a prefix sum over pieces rather than a rescan of the bytes.
#[derive(Debug, Clone, Copy)]
struct Piece {
    source: Source,
    start: usize,
    len: usize,
    chars: usize,
}

/// The immutable original: either an mmap of a file or owned text. Both are
/// treated uniformly as `&[u8]` / `&str`; the mmap is never copied wholesale.
enum Original {
    /// `from_string` — owns its "original" text (no file). Needed for tests and
    /// scratch buffers without a path.
    Owned(String),
    /// `open` — the file, mmapped read-only. The OS pages it in and out.
    Mapped(memmap2::Mmap),
}

impl Original {
    fn bytes(&self) -> &[u8] {
        match self {
            Original::Owned(s) => s.as_bytes(),
            Original::Mapped(m) => m,
        }
    }

    /// The original as `&str`. Validated UTF-8 at construction, so this is total.
    fn as_str(&self) -> &str {
        // SAFETY: both constructors validate the original is UTF-8 up front
        // (`from_string` takes a `String`; `open` runs `str::from_utf8`), and the
        // bytes are immutable thereafter.
        unsafe { std::str::from_utf8_unchecked(self.bytes()) }
    }
}

/// Quire — a piece-table `TextStore`. See the module docs for the data model.
pub struct Quire {
    name: String,
    original: Original,
    /// Append-only; pieces with `source == Add` reference ranges in here.
    add: String,
    /// The document, as an ordered sequence of pieces.
    pieces: Vec<Piece>,
    /// Cached total char count of `pieces` (sum of `Piece::chars`).
    total_chars: usize,

    /// Point: 1-based char position in `point_min()..=point_max()`.
    point: usize,
    /// Mark: the other end of the region, if set (1-based char position).
    mark: Option<usize>,
    /// Narrowing `(lo, hi)`; accessible region is `[lo, hi)`. `None` = whole.
    narrowing: Option<(usize, usize)>,
    /// Most recent successful search, in 1-based char positions.
    last_match: Option<MatchData>,

    /// Lazy fallback cache for [`TextStore::text`] only (see module docs).
    /// `None` after any mutation; refilled on demand by [`Quire::full_text`].
    text_cache: RefCell<Option<String>>,
}

impl Quire {
    /// Open `path` and mmap it read-only as the immutable original. Rejects
    /// non-UTF-8 input (an explicit byte mode can come later, per the plan).
    pub fn open(path: &Path) -> std::io::Result<Quire> {
        let file = std::fs::File::open(path)?;
        // SAFETY: we treat the mapping as immutable for Quire's lifetime. The
        // plan notes a paged-LRU reader can replace mmap to rule out SIGBUS on
        // external truncation; for M1 the mmap is the backing.
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        if std::str::from_utf8(&mmap).is_err() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Quire::open: file is not valid UTF-8",
            ));
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        Ok(Quire::with_original(name, Original::Mapped(mmap)))
    }

    /// Build a Quire whose "original" is owned `text` (no file). Mirrors
    /// [`crate::buffer::Buffer::from_string`] so tests need no file on disk.
    pub fn from_string(name: impl Into<String>, text: impl Into<String>) -> Quire {
        Quire::with_original(name.into(), Original::Owned(text.into()))
    }

    fn with_original(name: String, original: Original) -> Quire {
        // One piece spanning the whole original. Counting its chars is the one
        // up-front scan; for a GB file this is the lazy line/char index the plan
        // wants done off the first-edit path — acceptable here, and the obvious
        // place to make incremental/background later (TODO).
        let bytes_len = original.bytes().len();
        let chars = original.as_str().chars().count();
        let pieces = if bytes_len == 0 {
            Vec::new()
        } else {
            vec![Piece {
                source: Source::Original,
                start: 0,
                len: bytes_len,
                chars,
            }]
        };
        Quire {
            name,
            original,
            add: String::new(),
            pieces,
            total_chars: chars,
            point: 1,
            mark: None,
            narrowing: None,
            last_match: None,
            text_cache: RefCell::new(None),
        }
    }

    /// Bytes of the backing store a piece points into.
    fn backing(&self, source: Source) -> &[u8] {
        match source {
            Source::Original => self.original.bytes(),
            Source::Add => self.add.as_bytes(),
        }
    }

    /// A piece's referenced bytes, as `&str` (always on char boundaries).
    fn piece_str(&self, p: &Piece) -> &str {
        let bytes = &self.backing(p.source)[p.start..p.start + p.len];
        // SAFETY: both backings are UTF-8 and pieces are only ever split on char
        // boundaries (see `byte_of` / `split_for`), so this slice is valid UTF-8.
        unsafe { std::str::from_utf8_unchecked(bytes) }
    }

    fn char_len(&self) -> usize {
        self.total_chars
    }

    fn point_min(&self) -> usize {
        self.narrowing.map_or(1, |(lo, _)| lo)
    }

    fn point_max(&self) -> usize {
        self.narrowing.map_or(self.total_chars + 1, |(_, hi)| hi)
    }

    /// Locate 1-based char position `p` (clamped to `1..=char_len+1`): returns
    /// the piece index and the *byte* offset of that char within the piece. A
    /// position at end-of-document returns `(pieces.len(), 0)`. O(pieces) over
    /// the prefix sum + a within-piece byte scan (the part a measured tree would
    /// turn into O(log n)).
    fn locate(&self, p: usize) -> (usize, usize) {
        let p = p.clamp(1, self.total_chars + 1);
        let target = p - 1; // chars before the position
        let mut acc = 0usize;
        for (i, piece) in self.pieces.iter().enumerate() {
            if target < acc + piece.chars {
                let within = target - acc; // chars into this piece
                let byte = self
                    .piece_str(piece)
                    .char_indices()
                    .nth(within)
                    .map_or(piece.len, |(b, _)| b);
                return (i, byte);
            }
            acc += piece.chars;
        }
        (self.pieces.len(), 0)
    }

    /// Split the piece list so that 1-based char position `p` is exactly a piece
    /// boundary; return the index of the first piece at/after `p` (the insertion
    /// index). Splits never land inside a UTF-8 scalar (offsets come from
    /// [`Self::locate`], which is char-indexed).
    fn split_at(&mut self, p: usize) -> usize {
        let (idx, byte) = self.locate(p);
        if idx >= self.pieces.len() || byte == 0 {
            return idx;
        }
        let piece = self.pieces[idx];
        let left_chars = self.piece_str(&piece)[..byte].chars().count();
        let left = Piece {
            source: piece.source,
            start: piece.start,
            len: byte,
            chars: left_chars,
        };
        let right = Piece {
            source: piece.source,
            start: piece.start + byte,
            len: piece.len - byte,
            chars: piece.chars - left_chars,
        };
        self.pieces[idx] = left;
        self.pieces.insert(idx + 1, right);
        idx + 1
    }

    /// Drop the lazy `text()` cache. Called from every mutation.
    fn invalidate(&mut self) {
        *self.text_cache.borrow_mut() = None;
    }

    /// Materialize the whole document into the lazy cache and hand back a `&str`
    /// borrow of it. **This is the one place text is brought fully resident** —
    /// a fallback for [`TextStore::text`]'s `&str` signature, nothing else calls
    /// it. The borrow is valid until the next mutation (which clears the cache).
    fn full_text(&self) -> &str {
        if self.text_cache.borrow().is_none() {
            let mut s = String::with_capacity(self.total_bytes());
            for piece in &self.pieces {
                s.push_str(self.piece_str(piece));
            }
            *self.text_cache.borrow_mut() = Some(s);
        }
        // SAFETY: we only hand out a borrow tied to `&self`; the cache is an
        // owned `String` in a `RefCell` that is replaced (not mutated in place)
        // and is cleared only behind `&mut self`. While `&self` is held no
        // mutation can run, so the `String`'s heap buffer stays put and the
        // returned `&str` outlives this borrow safely.
        let ptr = self.text_cache.as_ptr();
        unsafe { (*ptr).as_ref().unwrap().as_str() }
    }

    fn total_bytes(&self) -> usize {
        self.pieces.iter().map(|p| p.len).sum()
    }

    /// The char at 1-based position `p` (absolute; ignores narrowing), or `None`
    /// at/after end-of-document. O(pieces) + a small within-piece scan.
    fn char_at(&self, p: usize) -> Option<char> {
        if p < 1 || p > self.total_chars {
            return None;
        }
        let (idx, byte) = self.locate(p);
        let piece = self.pieces.get(idx)?;
        self.piece_str(piece)[byte..].chars().next()
    }

    /// Materialize the absolute char range `[lo, hi)` (1-based) into an owned
    /// `String` by walking pieces. Used for region reads and as the search
    /// window. O(range), not O(document) — only the requested span is copied.
    fn collect_range(&self, lo: usize, hi: usize) -> String {
        let lo = lo.clamp(1, self.total_chars + 1);
        let hi = hi.clamp(lo, self.total_chars + 1);
        let mut out = String::new();
        if lo == hi {
            return out;
        }
        let mut pos = 1usize; // 1-based char position at the start of `piece`
        for piece in &self.pieces {
            let piece_end = pos + piece.chars; // exclusive
            if piece_end > lo && pos < hi {
                let s = self.piece_str(piece);
                let from = lo.saturating_sub(pos); // chars to skip in this piece
                let to = (hi - pos).min(piece.chars); // chars to take (exclusive)
                let bstart = s.char_indices().nth(from).map_or(s.len(), |(b, _)| b);
                let bend = if to >= piece.chars {
                    s.len()
                } else {
                    s.char_indices().nth(to).map_or(s.len(), |(b, _)| b)
                };
                out.push_str(&s[bstart..bend]);
            }
            pos = piece_end;
            if pos >= hi {
                break;
            }
        }
        out
    }

    /// Count newlines in the absolute char range `[1, p)` — i.e. how many line
    /// breaks precede position `p`. Walks pieces; the part of the boundary piece
    /// before `p` is scanned. O(bytes before p) worst case (a measured tree
    /// would carry a line-count summary for O(log n)); TODO: lazy line index.
    fn newlines_before(&self, p: usize) -> usize {
        let p = p.clamp(1, self.total_chars + 1);
        if p <= 1 {
            return 0;
        }
        let mut count = 0usize;
        let mut pos = 1usize;
        for piece in &self.pieces {
            let piece_end = pos + piece.chars;
            let s = self.piece_str(piece);
            if piece_end <= p {
                count += s.bytes().filter(|&b| b == b'\n').count();
            } else {
                let within = p - pos; // chars of this piece that precede `p`
                let bend = s.char_indices().nth(within).map_or(s.len(), |(b, _)| b);
                count += s[..bend].bytes().filter(|&b| b == b'\n').count();
                break;
            }
            pos = piece_end;
            if pos >= p {
                break;
            }
        }
        count
    }

    fn goto_char(&mut self, p: usize) {
        self.point = p.clamp(self.point_min(), self.point_max());
    }

    // ---- low-level piece splicing (touch only pieces / total_chars / add) ----
    // These never touch point, mark, narrowing, last_match, or the text cache;
    // each public mutator layers its own oracle-matching bookkeeping on top.

    /// Insert `s` at the piece boundary for 1-based char position `p`, appending
    /// to the add buffer. Returns the char count inserted. O(pieces).
    fn splice_insert(&mut self, p: usize, s: &str) -> usize {
        let n = s.chars().count();
        let idx = self.split_at(p);
        let start = self.add.len();
        self.add.push_str(s);
        // The add buffer is append-only, so existing Add-pieces keep referencing
        // stable byte ranges; only the new piece points at `[start, ..)`.
        self.pieces.insert(
            idx,
            Piece {
                source: Source::Add,
                start,
                len: s.len(),
                chars: n,
            },
        );
        self.total_chars += n;
        n
    }

    /// Delete the absolute char range `[lo, hi)` (1-based, already ordered and
    /// clamped) by splitting both ends and dropping the pieces between. O(pieces).
    fn splice_delete(&mut self, lo: usize, hi: usize) {
        // Split the low end first: splitting at the (later) high end can only
        // insert pieces at an index > `lo_idx`, so `lo_idx` stays valid; doing it
        // the other way round would shift the high boundary the low split crosses.
        let lo_idx = self.split_at(lo);
        let hi_idx = self.split_at(hi);
        self.pieces.drain(lo_idx..hi_idx);
        self.total_chars -= hi - lo;
    }

    fn insert(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        let at = self.point;
        let n = self.splice_insert(at, s);
        self.point += n;
        if let Some((_, hi)) = self.narrowing.as_mut() {
            *hi += n; // inserted text falls inside the accessible region
        }
        self.last_match = None;
        self.invalidate();
    }

    fn delete_region(&mut self, a: usize, b: usize) {
        let (lo, hi) = (a.min(b), a.max(b));
        let lo = lo.clamp(1, self.total_chars + 1);
        let hi = hi.clamp(lo, self.total_chars + 1);
        if lo == hi {
            return;
        }
        let removed = hi - lo;
        self.splice_delete(lo, hi);
        if self.point >= hi {
            self.point -= removed;
        } else if self.point > lo {
            self.point = lo;
        }
        if let Some((nlo, nhi)) = self.narrowing.as_mut() {
            *nhi = nhi.saturating_sub(removed).max(*nlo);
        }
        self.last_match = None;
        self.invalidate();
    }

    fn substring(&self, a: usize, b: usize) -> String {
        let (lo, hi) = (a.min(b), a.max(b));
        self.collect_range(lo, hi)
    }

    /// Regex search forward from point (bounded by `bound` or point-max). On a
    /// hit: record match-data, move point past the match, return the new point.
    ///
    /// Materializes the *window* `[point, bound)` to run the regex, then maps the
    /// byte match back to char positions. The window is the search bound, not the
    /// document; for the common all-original case the plan calls for scanning the
    /// mmap directly and an incremental DFA across pieces — TODO. Capturing
    /// `replace_match`'s groups needs the matched substrings regardless.
    fn re_search_forward(&mut self, re: &regex::Regex, bound: Option<usize>) -> Option<usize> {
        let from = self.point;
        let to = bound.unwrap_or_else(|| self.point_max());
        if from > to {
            return None;
        }
        let window = self.collect_range(from, to);
        let caps = re.captures(&window)?;
        let whole = caps.get(0)?;
        let groups: Vec<Option<String>> = caps
            .iter()
            .map(|g| g.map(|m| m.as_str().to_string()))
            .collect();
        // Byte offsets in `window` → char offsets → absolute 1-based positions.
        let start = from + window[..whole.start()].chars().count();
        let end = from + window[..whole.end()].chars().count();
        self.last_match = Some(MatchData { start, end, groups });
        self.point = end;
        Some(end)
    }

    /// Replace the last match's region with `replacement` (after `\N` / `\&`
    /// backref expansion); leave point at the end of the inserted text.
    fn replace_match(&mut self, replacement: &str) -> Result<(), String> {
        let md = self
            .last_match
            .take()
            .ok_or("replace-match: no preceding match")?;
        let expanded = expand_backrefs(replacement, &md.groups);
        let (start, end) = (md.start, md.end);
        // Splice directly so this matches the oracle's `replace_match` byte for
        // byte: it leaves narrowing *untouched* (unlike `insert`/`delete_region`,
        // which each shift the narrowing bound) and only moves point.
        self.splice_delete(start, end);
        self.splice_insert(start, &expanded);
        self.point = start + expanded.chars().count();
        self.invalidate();
        Ok(())
    }

    fn looking_at(&self, re: &regex::Regex) -> bool {
        // Anchor a regex at point. Like the oracle, the scan window runs from
        // point to the *document* end (narrowing is ignored here), so a match
        // that needs to read past point-max still sees the bytes. TODO: replace
        // the materialized tail with an incremental DFA reading a piece cursor so
        // this never copies past the match.
        let window = self.collect_range(self.point, self.total_chars + 1);
        re.find(&window).is_some_and(|m| m.start() == 0)
    }

    fn mark(&self) -> Option<usize> {
        self.mark
    }
    fn set_mark(&mut self, p: usize) {
        self.mark = Some(p.clamp(self.point_min(), self.point_max()));
    }
    fn set_mark_opt(&mut self, m: Option<usize>) {
        self.mark = m;
    }

    fn narrowing(&self) -> Option<(usize, usize)> {
        self.narrowing
    }
    fn narrow_to_region(&mut self, a: usize, b: usize) {
        let full = self.total_chars + 1;
        let lo = a.min(b).clamp(1, full);
        let hi = a.max(b).clamp(lo, full);
        self.narrowing = Some((lo, hi));
        self.point = self.point.clamp(lo, hi);
        self.mark = self.mark.map(|m| m.clamp(lo, hi));
    }
    fn widen(&mut self) {
        self.narrowing = None;
    }
    fn set_restriction(&mut self, r: Option<(usize, usize)>) {
        let full = self.total_chars + 1;
        self.narrowing = r.map(|(lo, hi)| {
            let lo = lo.clamp(1, full);
            (lo, hi.clamp(lo, full))
        });
        let (lo, hi) = (self.point_min(), self.point_max());
        self.point = self.point.clamp(lo, hi);
    }

    /// Move point to the first char of its line (just after the previous newline).
    fn beginning_of_line(&mut self) {
        // Walk back from point over non-newline chars. O(line length). The
        // oracle's `byte_of` clamps point into the buffer first, so we do too —
        // point may sit at point-max even when that exceeds char_len+1.
        let mut p = self.point.min(self.total_chars + 1);
        while p > 1 {
            if self.char_at(p - 1) == Some('\n') {
                break;
            }
            p -= 1;
        }
        self.point = p;
    }
    /// Move point to the end of its line (just before the next newline, or eob).
    fn end_of_line(&mut self) {
        let max = self.total_chars + 1;
        let mut p = self.point.min(max); // clamp like the oracle's `byte_of`
        while p < max {
            if self.char_at(p) == Some('\n') {
                break;
            }
            p += 1;
        }
        self.point = p;
    }
    /// Move point `n` lines forward, to a line beginning. Returns the count of
    /// lines that could not be moved (0 on full success), like Emacs.
    fn forward_line(&mut self, n: i64) -> i64 {
        self.beginning_of_line();
        let mut left = n.abs();
        // Like the oracle, newline scanning runs over the *full* document; only
        // the terminal "ran off the end" case clamps to point_min/point_max.
        let doc_max = self.total_chars + 1;
        while left > 0 {
            if n >= 0 {
                // advance to just past the next newline at/after point
                let mut p = self.point;
                while p < doc_max && self.char_at(p) != Some('\n') {
                    p += 1;
                }
                if p < doc_max {
                    self.point = p + 1; // just past the newline
                } else {
                    self.point = self.point_max();
                    return left;
                }
            } else {
                // Mirror the oracle: it scans `text[..b-1]` (b = byte of point,
                // this line's start) for the *last* newline. `b-1` drops the
                // single char at `start_line - 1` — the previous line's
                // terminator — so we look for the last '\n' at a char position
                // strictly below `start_line - 1`; the previous line then begins
                // just after it. (When the previous line is empty its start *is*
                // `start_line - 1`.) If there is no such newline, point goes to
                // point_min and the move is not counted — Emacs's partial-move
                // quirk — so we return `left`.
                let start_line = self.point;
                let mut j = start_line.saturating_sub(2); // last excluded pos - 1
                let mut found = None;
                while j >= 1 {
                    if self.char_at(j) == Some('\n') {
                        found = Some(j + 1); // previous line begins after the '\n'
                        break;
                    }
                    j -= 1;
                }
                match found {
                    Some(start) => self.point = start,
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
    fn line_number_at_pos(&self, p: usize) -> usize {
        self.newlines_before(p) + 1
    }

    fn char_after(&self, p: usize) -> Option<char> {
        if p < self.point_max() {
            self.char_at(p)
        } else {
            None
        }
    }
    fn char_before(&self, p: usize) -> Option<char> {
        if p > self.point_min() {
            // Delegate through `char_after` (not `char_at`) so the point_max
            // guard applies exactly as in the oracle.
            self.char_after(p - 1)
        } else {
            None
        }
    }

    /// Exact forward search from point (bounded). On a hit: set match-data,
    /// move point past the match, return the new point.
    fn search_forward(&mut self, needle: &str, bound: Option<usize>) -> Option<usize> {
        if needle.is_empty() {
            return Some(self.point);
        }
        let from = self.point;
        let to = bound.unwrap_or_else(|| self.point_max());
        if from > to {
            return None;
        }
        let window = self.collect_range(from, to);
        let bpos = window.find(needle)?;
        let start = from + window[..bpos].chars().count();
        let end = start + needle.chars().count();
        self.last_match = Some(MatchData {
            start,
            end,
            groups: vec![Some(needle.to_string())],
        });
        self.point = end;
        Some(end)
    }
    /// Exact backward search from point (bounded below). On a hit: set
    /// match-data, move point to the match start, return it.
    fn search_backward(&mut self, needle: &str, bound: Option<usize>) -> Option<usize> {
        if needle.is_empty() {
            return Some(self.point);
        }
        let lo = bound.unwrap_or_else(|| self.point_min());
        let hi = self.point;
        if lo > hi {
            return None;
        }
        let window = self.collect_range(lo, hi);
        let bpos = window.rfind(needle)?;
        let start = lo + window[..bpos].chars().count();
        let end = start + needle.chars().count();
        self.last_match = Some(MatchData {
            start,
            end,
            groups: vec![Some(needle.to_string())],
        });
        self.point = start;
        Some(start)
    }
}

/// Quire is the piece-table `TextStore` (matches the `Buffer` oracle exactly).
impl TextStore for Quire {
    fn name(&self) -> &str {
        &self.name
    }
    fn text(&self) -> &str {
        self.full_text()
    }
    fn char_len(&self) -> usize {
        Quire::char_len(self)
    }
    fn point(&self) -> usize {
        self.point
    }
    fn point_min(&self) -> usize {
        Quire::point_min(self)
    }
    fn point_max(&self) -> usize {
        Quire::point_max(self)
    }
    fn goto_char(&mut self, p: usize) {
        Quire::goto_char(self, p)
    }
    fn mark(&self) -> Option<usize> {
        Quire::mark(self)
    }
    fn set_mark(&mut self, p: usize) {
        Quire::set_mark(self, p)
    }
    fn set_mark_opt(&mut self, m: Option<usize>) {
        Quire::set_mark_opt(self, m)
    }
    fn insert(&mut self, s: &str) {
        Quire::insert(self, s)
    }
    fn delete_region(&mut self, a: usize, b: usize) {
        Quire::delete_region(self, a, b)
    }
    fn substring(&self, a: usize, b: usize) -> String {
        Quire::substring(self, a, b)
    }
    fn re_search_forward(&mut self, re: &regex::Regex, bound: Option<usize>) -> Option<usize> {
        Quire::re_search_forward(self, re, bound)
    }
    fn search_forward(&mut self, needle: &str, bound: Option<usize>) -> Option<usize> {
        Quire::search_forward(self, needle, bound)
    }
    fn search_backward(&mut self, needle: &str, bound: Option<usize>) -> Option<usize> {
        Quire::search_backward(self, needle, bound)
    }
    fn replace_match(&mut self, replacement: &str) -> Result<(), String> {
        Quire::replace_match(self, replacement)
    }
    fn looking_at(&self, re: &regex::Regex) -> bool {
        Quire::looking_at(self, re)
    }
    fn beginning_of_line(&mut self) {
        Quire::beginning_of_line(self)
    }
    fn end_of_line(&mut self) {
        Quire::end_of_line(self)
    }
    fn forward_line(&mut self, n: i64) -> i64 {
        Quire::forward_line(self, n)
    }
    fn line_number_at_pos(&self, p: usize) -> usize {
        Quire::line_number_at_pos(self, p)
    }
    fn char_after(&self, p: usize) -> Option<char> {
        Quire::char_after(self, p)
    }
    fn char_before(&self, p: usize) -> Option<char> {
        Quire::char_before(self, p)
    }
    fn narrowing(&self) -> Option<(usize, usize)> {
        Quire::narrowing(self)
    }
    fn narrow_to_region(&mut self, a: usize, b: usize) {
        Quire::narrow_to_region(self, a, b)
    }
    fn widen(&mut self) {
        Quire::widen(self)
    }
    fn set_restriction(&mut self, r: Option<(usize, usize)>) {
        Quire::set_restriction(self, r)
    }
}

/// Expand Emacs-style `\N` (group) and `\&` (whole match) backrefs. Mirrors
/// `buffer::expand_backrefs` so `replace_match` matches the oracle exactly.
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
    use crate::buffer::Buffer;

    // ---- focused unit tests (mirror buffer.rs so failures localize) ----

    #[test]
    fn insert_at_end() {
        let mut q = Quire::from_string("t", "hello");
        let pmax = TextStore::point_max(&q);
        TextStore::goto_char(&mut q, pmax);
        TextStore::insert(&mut q, " world");
        assert_eq!(TextStore::text(&q), "hello world");
        assert_eq!(TextStore::point(&q), TextStore::point_max(&q));
    }

    #[test]
    fn regex_replace_loop() {
        let mut q = Quire::from_string("t", "world world");
        let re = regex::Regex::new("world").unwrap();
        let pmin = TextStore::point_min(&q);
        TextStore::goto_char(&mut q, pmin);
        let mut n = 0;
        while TextStore::re_search_forward(&mut q, &re, None).is_some() {
            TextStore::replace_match(&mut q, "WORLD").unwrap();
            n += 1;
        }
        assert_eq!(n, 2);
        assert_eq!(TextStore::text(&q), "WORLD WORLD");
    }

    #[test]
    fn backref_expansion() {
        let mut q = Quire::from_string("t", "Doe, John");
        let re = regex::Regex::new(r"(\w+), (\w+)").unwrap();
        TextStore::goto_char(&mut q, 1);
        assert!(TextStore::re_search_forward(&mut q, &re, None).is_some());
        TextStore::replace_match(&mut q, r"\2 \1").unwrap();
        assert_eq!(TextStore::text(&q), "John Doe");
    }

    #[test]
    fn delete_region_shifts_point() {
        let mut q = Quire::from_string("t", "abcdef");
        TextStore::goto_char(&mut q, 7);
        TextStore::delete_region(&mut q, 2, 4); // remove "bc"
        assert_eq!(TextStore::text(&q), "adef");
        assert_eq!(TextStore::point(&q), 5);
    }

    #[test]
    fn multibyte_pieces_stay_on_boundaries() {
        // Insert into the middle of multi-byte text; pieces must split on char
        // boundaries and char positions stay correct.
        let mut q = Quire::from_string("t", "héllo wörld");
        TextStore::goto_char(&mut q, 3); // after "hé"
        TextStore::insert(&mut q, "XX");
        assert_eq!(TextStore::text(&q), "héXXllo wörld");
        assert_eq!(TextStore::char_after(&q, 3), Some('X'));
        assert_eq!(TextStore::char_before(&q, 3), Some('é'));
    }

    #[test]
    fn open_mmaps_a_file() {
        let mut path = std::env::temp_dir();
        path.push(format!("quire_open_test_{}.txt", std::process::id()));
        std::fs::write(&path, "line one\nline two\nαβγ\n").unwrap();
        let mut q = Quire::open(&path).unwrap();
        assert_eq!(TextStore::text(&q), "line one\nline two\nαβγ\n");
        let pmax = TextStore::point_max(&q);
        assert_eq!(TextStore::line_number_at_pos(&q, pmax), 4);
        TextStore::goto_char(&mut q, 1);
        assert_eq!(
            TextStore::search_forward(&mut q, "two", None),
            Some("line one\nline ".chars().count() + 1 + 3)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn open_rejects_non_utf8() {
        let mut path = std::env::temp_dir();
        path.push(format!("quire_bad_utf8_{}.bin", std::process::id()));
        std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        match Quire::open(&path) {
            Ok(_) => panic!("expected non-UTF-8 file to be rejected"),
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::InvalidData),
        }
        let _ = std::fs::remove_file(&path);
    }

    // ---- the key deliverable: a seeded differential test vs the Buffer oracle ----

    /// Minimal seeded LCG (Numerical Recipes constants) — deterministic, no deps.
    struct Lcg(u64);
    impl Lcg {
        fn next_u32(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next_u32() as usize) % n.max(1)
        }
        /// A char position in `1..=char_len+1` (the full inclusive position range).
        fn pos(&mut self, char_len: usize) -> usize {
            1 + self.below(char_len + 1)
        }
    }

    /// Assert every observable bit of `TextStore` state agrees between the two
    /// stores. The differential test's whole point.
    fn assert_in_sync(b: &Buffer, q: &Quire, step: usize, op: &str) {
        let bt = TextStore::text(b);
        let qt = TextStore::text(q);
        assert_eq!(bt, qt, "text mismatch after step {step} ({op})");
        assert_eq!(
            TextStore::char_len(b),
            TextStore::char_len(q),
            "char_len mismatch after step {step} ({op})"
        );
        assert_eq!(
            TextStore::point(b),
            TextStore::point(q),
            "point mismatch after step {step} ({op}); text={bt:?}"
        );
        assert_eq!(
            TextStore::point_min(b),
            TextStore::point_min(q),
            "point_min mismatch after step {step} ({op})"
        );
        assert_eq!(
            TextStore::point_max(b),
            TextStore::point_max(q),
            "point_max mismatch after step {step} ({op})"
        );
        assert_eq!(
            TextStore::mark(b),
            TextStore::mark(q),
            "mark mismatch after step {step} ({op})"
        );
        assert_eq!(
            TextStore::narrowing(b),
            TextStore::narrowing(q),
            "narrowing mismatch after step {step} ({op})"
        );
    }

    /// Drive a long, seeded-random op sequence against BOTH a `Buffer` and a
    /// `Quire` built from the same initial string; assert they stay byte-identical
    /// after every step. This is what proves Quire matches the oracle.
    fn run_diff(seed: u64, steps: usize, initial: &str) {
        let mut b = Buffer::from_string("t", initial);
        let mut q = Quire::from_string("t", initial);
        let mut rng = Lcg(seed);

        // A small inserts palette incl. multi-byte + newlines, and needles/regexes
        // that actually occur so searches frequently hit.
        let inserts = ["x", "ab", "\n", "héllo", "世界", " foo ", "Z\nZ", "12"];
        let needles = ["a", "x", "foo", "\n", "Z", "é", "界", "ab"];
        let regexes = [
            regex::Regex::new(r"\w+").unwrap(),
            regex::Regex::new(r"[a-z]+").unwrap(),
            regex::Regex::new(r"\d").unwrap(),
            regex::Regex::new(r".").unwrap(),
            regex::Regex::new(r"foo|bar").unwrap(),
        ];
        let replacements = ["", "Q", "<\\&>", "ab", "\\1!"];

        assert_in_sync(&b, &q, 0, "init");

        for step in 1..=steps {
            let len = TextStore::char_len(&b);
            match rng.below(14) {
                0 => {
                    let s = inserts[rng.below(inserts.len())];
                    let p = rng.pos(len);
                    TextStore::goto_char(&mut b, p);
                    TextStore::goto_char(&mut q, p);
                    TextStore::insert(&mut b, s);
                    TextStore::insert(&mut q, s);
                    assert_in_sync(&b, &q, step, "insert");
                }
                1 => {
                    let a = rng.pos(len);
                    let c = rng.pos(len);
                    TextStore::delete_region(&mut b, a, c);
                    TextStore::delete_region(&mut q, a, c);
                    assert_in_sync(&b, &q, step, "delete_region");
                }
                2 => {
                    let p = rng.pos(len);
                    TextStore::goto_char(&mut b, p);
                    TextStore::goto_char(&mut q, p);
                    assert_in_sync(&b, &q, step, "goto_char");
                }
                3 => {
                    let p = rng.pos(len);
                    TextStore::set_mark(&mut b, p);
                    TextStore::set_mark(&mut q, p);
                    assert_in_sync(&b, &q, step, "set_mark");
                }
                4 => {
                    let re = &regexes[rng.below(regexes.len())];
                    let rb = TextStore::re_search_forward(&mut b, re, None);
                    let rq = TextStore::re_search_forward(&mut q, re, None);
                    assert_eq!(rb, rq, "re_search_forward result step {step}");
                    assert_in_sync(&b, &q, step, "re_search_forward");
                }
                5 => {
                    let n = needles[rng.below(needles.len())];
                    let rb = TextStore::search_forward(&mut b, n, None);
                    let rq = TextStore::search_forward(&mut q, n, None);
                    assert_eq!(rb, rq, "search_forward({n:?}) result step {step}");
                    assert_in_sync(&b, &q, step, "search_forward");
                }
                6 => {
                    let n = needles[rng.below(needles.len())];
                    let rb = TextStore::search_backward(&mut b, n, None);
                    let rq = TextStore::search_backward(&mut q, n, None);
                    assert_eq!(rb, rq, "search_backward({n:?}) result step {step}");
                    assert_in_sync(&b, &q, step, "search_backward");
                }
                7 => {
                    // search then replace, so replace_match exercises real match data
                    let re = &regexes[rng.below(regexes.len())];
                    let rep = replacements[rng.below(replacements.len())];
                    let hit_b = TextStore::re_search_forward(&mut b, re, None).is_some();
                    let hit_q = TextStore::re_search_forward(&mut q, re, None).is_some();
                    assert_eq!(hit_b, hit_q, "pre-replace search step {step}");
                    if hit_b {
                        let er = TextStore::replace_match(&mut b, rep);
                        let eq = TextStore::replace_match(&mut q, rep);
                        assert_eq!(er.is_ok(), eq.is_ok(), "replace_match status step {step}");
                    }
                    assert_in_sync(&b, &q, step, "replace_match");
                }
                8 => {
                    let a = rng.pos(len);
                    let c = rng.pos(len);
                    TextStore::narrow_to_region(&mut b, a, c);
                    TextStore::narrow_to_region(&mut q, a, c);
                    assert_in_sync(&b, &q, step, "narrow_to_region");
                }
                9 => {
                    TextStore::widen(&mut b);
                    TextStore::widen(&mut q);
                    assert_in_sync(&b, &q, step, "widen");
                }
                10 => {
                    let n = (rng.below(7) as i64) - 3; // -3..=3
                    let rb = TextStore::forward_line(&mut b, n);
                    let rq = TextStore::forward_line(&mut q, n);
                    assert_eq!(rb, rq, "forward_line({n}) result step {step}");
                    assert_in_sync(&b, &q, step, "forward_line");
                }
                11 => {
                    TextStore::beginning_of_line(&mut b);
                    TextStore::beginning_of_line(&mut q);
                    assert_in_sync(&b, &q, step, "beginning_of_line");
                }
                12 => {
                    TextStore::end_of_line(&mut b);
                    TextStore::end_of_line(&mut q);
                    assert_in_sync(&b, &q, step, "end_of_line");
                }
                _ => {
                    // read-only probes: substring, char_after/before, looking_at,
                    // line_number_at_pos — must agree pointwise.
                    let a = rng.pos(len);
                    let c = rng.pos(len);
                    assert_eq!(
                        TextStore::substring(&b, a, c),
                        TextStore::substring(&q, a, c),
                        "substring({a},{c}) step {step}"
                    );
                    let p = rng.pos(len);
                    assert_eq!(
                        TextStore::char_after(&b, p),
                        TextStore::char_after(&q, p),
                        "char_after({p}) step {step}"
                    );
                    assert_eq!(
                        TextStore::char_before(&b, p),
                        TextStore::char_before(&q, p),
                        "char_before({p}) step {step}"
                    );
                    assert_eq!(
                        TextStore::line_number_at_pos(&b, p),
                        TextStore::line_number_at_pos(&q, p),
                        "line_number_at_pos({p}) step {step}"
                    );
                    let re = &regexes[rng.below(regexes.len())];
                    assert_eq!(
                        TextStore::looking_at(&b, re),
                        TextStore::looking_at(&q, re),
                        "looking_at step {step}"
                    );
                    assert_in_sync(&b, &q, step, "read-probe");
                }
            }
        }
    }

    #[test]
    fn differential_vs_oracle_many_seeds() {
        // Several seeds × thousands of ops each = a long mixed op sequence; any
        // divergence in text/point/mark/narrowing trips immediately.
        for seed in [1u64, 7, 42, 1000, 0xdead_beef, 0x5eed] {
            run_diff(
                seed,
                3000,
                "The quick brown fox\njumps over\nthe lazy dog.\n",
            );
        }
    }

    #[test]
    fn differential_starts_empty() {
        run_diff(123, 3000, "");
    }

    #[test]
    fn differential_multibyte_initial() {
        run_diff(99, 3000, "αβγδ\n世界へようこそ\nfoo bar baz\n");
    }
}
