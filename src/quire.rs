//! Quire — the `TextStore` over an immutable, memory-mapped original plus an
//! append-only add buffer (M1), with a **persistent measured B-tree** spine.
//! This is VS Code's piece tree in its copy-on-write form: the document is an
//! ordered sequence of *pieces* `(source, start, len)` that reference one of two
//! immutable backing stores; the original is `mmap`ed (paged by the OS, never
//! fully resident) and the add buffer holds only inserted text. An edit splits
//! pieces and points a new one at the add buffer — original bytes never move.
//!
//! Positions are 1-based char positions, Emacs-style, exactly like
//! [`crate::buffer::Buffer`] (the differential-test oracle this matches
//! byte-for-byte). `point_min`/`point_max` honor the narrowing.
//!
//! ## Data structure: a persistent measured B-tree
//! The spine is an [`Arc`]-wrapped B-tree ([`Node`]). Internal nodes hold child
//! pointers plus a monoid [`Summary`] (total bytes / chars / lines) for the whole
//! subtree; leaves hold a small `Vec<Piece>` and that run's summary. Seeks
//! (char → leaf/piece/offset, char → line, accumulated newlines before a
//! position) descend the tree guided by the summaries, so they are **O(log n)**
//! in the number of pieces instead of a linear scan of a piece vector. Edits
//! (`insert`, `delete_region`, `replace_match`) rebuild only the root→leaf path
//! they touch ([path-copying]); every prior version stays intact, so the tree is
//! **persistent** and a snapshot is just a clone of the root `Arc`. See
//! `plan.org` §"The editor core" and §"Performance".
//!
//! ## Shared, immutable backings → O(1) snapshots
//! Both backing stores are `Arc`-shared. The original (mmap or owned string) is
//! immutable for the program's life. The add buffer is append-only; a `Quire`
//! appends to it in place while it is uniquely owned, and **copies it on write**
//! the first time it must grow while a snapshot still shares it (so divergent
//! timelines never clobber each other's bytes). A snapshot therefore clones two
//! `Arc`s and copies only the cursor state — no document bytes move. See
//! [`Quire::snapshot`].
//!
//! ## What is (and isn't) materialized
//! Editing, search, line/char navigation and snapshotting touch O(log n) nodes
//! and the OS page cache, never a full copy of the text. The *one* exception is
//! [`TextStore::text`], whose signature returns `&str`: a borrow has to point at
//! contiguous bytes that live somewhere, so the first `text()` after a mutation
//! lazily fills a cached `String` (see [`Quire::full_text`]). That cache is a
//! clearly-marked fallback for the trait's shape, invalidated on every edit, and
//! is the single place a GB file would be brought fully resident — flagged so it
//! can be removed once the surface is fully ranged/streamed. No internal method
//! calls it.
//!
//! [path-copying]: https://en.wikipedia.org/wiki/Persistent_data_structure

use crate::buffer::MatchData;
use crate::store::TextStore;
use std::cell::RefCell;
use std::path::Path;
use std::sync::Arc;

/// Which immutable backing store a piece points into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// The file (mmap) or the owned text passed to [`Quire::from_string`].
    Original,
    /// The append-only add buffer — holds only inserted text.
    Add,
}

/// A reference into a backing store: `[start, start + len)` *bytes*. Never owns
/// text. `chars` / `lines` cache the code-point and newline counts of that byte
/// range so the tree summaries are prefix sums over pieces, not rescans.
#[derive(Debug, Clone, Copy)]
struct Piece {
    source: Source,
    start: usize,
    len: usize,
    chars: usize,
    /// Number of `\n` bytes in `[start, start + len)`.
    lines: usize,
}

impl Piece {
    fn summary(&self) -> Summary {
        Summary {
            bytes: self.len,
            chars: self.chars,
            lines: self.lines,
        }
    }
}

/// The immutable original: either an mmap of a file or owned text. Both are
/// treated uniformly as `&[u8]` / `&str`; the mmap is never copied wholesale.
/// Wrapped in an [`Arc`] so every snapshot shares it with zero copying.
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
}

// ----------------------------------------------------------------------------
// Initial char/line index (the one O(filesize) scan at open time)
// ----------------------------------------------------------------------------

/// Below this many bytes the initial char/line scan stays single-threaded:
/// spawning scoped threads (stack setup, joins) has a roughly fixed cost
/// (~0.2 ms on commodity hardware) that swamps the work on small inputs — at
/// 256 KiB the parallel path is actually ~2x *slower*, and it only pulls clearly
/// ahead past ~1 MiB. We set the cutoff at 1 MiB, where the sequential scan is
/// still only ~0.5 ms (so nothing user-visible is lost below it) and the
/// parallel driver already wins ~1.8x and climbs toward the core count as the
/// file grows. Above this, `count_chars_lines_parallel` fans the scan out over
/// the available cores.
const PARALLEL_INDEX_THRESHOLD: usize = 1024 * 1024;

/// Count Unicode scalar values (chars) and `\n` bytes in a UTF-8 slice.
///
/// Chars are counted as the number of non-continuation bytes — in valid UTF-8
/// every scalar value has exactly one leading byte `b` with `(b & 0xC0) != 0x80`
/// — which equals `bytes.chars().count()` but works directly on `&[u8]`. This
/// is a *pure* helper so the parallel driver and the unit tests can compare it
/// against the sequential count. O(bytes).
///
/// `#[doc(hidden)] pub` only so `benches/parallel_index.rs` can time it against
/// the parallel driver; it is not part of the supported API.
#[doc(hidden)]
pub fn count_chars_lines(bytes: &[u8]) -> (usize, usize) {
    let mut chars = 0;
    let mut lines = 0;
    for &b in bytes {
        // Leading byte of a scalar value (ASCII or a multi-byte lead): not a
        // 0b10xx_xxxx UTF-8 continuation byte.
        chars += usize::from((b & 0xC0) != 0x80);
        lines += usize::from(b == b'\n');
    }
    (chars, lines)
}

/// Advance `i` forward to the next UTF-8 char boundary at or after it, so a
/// chunk split never lands inside a multi-byte scalar value. Continuation bytes
/// match `(b & 0xC0) == 0x80`; we skip past them. `bytes.len()` is always a
/// boundary, so this terminates. Equivalent to `str::is_char_boundary` but on
/// the raw slice we already hold.
fn next_char_boundary(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && (bytes[i] & 0xC0) == 0x80 {
        i += 1;
    }
    i
}

/// Parallel driver for [`count_chars_lines`]: split `bytes` into roughly
/// `available_parallelism()` contiguous chunks **aligned to char boundaries**,
/// count each chunk on its own scoped thread, and sum the per-chunk totals.
///
/// Because the splits land only on char boundaries and char/newline counts are
/// additive over a partition, the result is **bit-for-bit identical** to the
/// sequential `count_chars_lines(bytes)`. Uses [`std::thread::scope`] so the
/// threads borrow `bytes` directly — no copy, no `'static` bound, no new crate.
///
/// `#[doc(hidden)] pub` only so `benches/parallel_index.rs` can time it against
/// the sequential helper; it is not part of the supported API.
#[doc(hidden)]
pub fn count_chars_lines_parallel(bytes: &[u8]) -> (usize, usize) {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .max(1);
    if threads == 1 || bytes.len() < 2 {
        return count_chars_lines(bytes);
    }

    // Partition into char-boundary-aligned chunks: target an even byte split,
    // then nudge each boundary forward off any continuation byte. Empty trailing
    // chunks (if a nudge consumed the rest) are simply skipped.
    let target = bytes.len().div_ceil(threads);
    let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(threads);
    let mut start = 0;
    while start < bytes.len() {
        let raw_end = (start + target).min(bytes.len());
        let end = next_char_boundary(bytes, raw_end);
        ranges.push((start, end));
        start = end;
    }

    // Scoped threads borrow `bytes` immutably; the scope joins them all before
    // returning, so no `'static` lifetime is required and nothing is copied.
    let partials: Vec<(usize, usize)> = std::thread::scope(|scope| {
        let handles: Vec<_> = ranges
            .iter()
            .map(|&(lo, hi)| scope.spawn(move || count_chars_lines(&bytes[lo..hi])))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    partials
        .into_iter()
        .fold((0, 0), |(c, l), (pc, pl)| (c + pc, l + pl))
}

// ----------------------------------------------------------------------------
// Measured B-tree
// ----------------------------------------------------------------------------

/// Monoid summary of a subtree (or a single piece): the measures we seek by.
/// `lines` is the newline count (not 1-based line numbers), which composes
/// additively; the 1-based line number at a position is `lines_before + 1`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Summary {
    bytes: usize,
    chars: usize,
    lines: usize,
}

impl Summary {
    fn add(self, other: Summary) -> Summary {
        Summary {
            bytes: self.bytes + other.bytes,
            chars: self.chars + other.chars,
            lines: self.lines + other.lines,
        }
    }
}

/// Branching factor: the max number of children (internal) or pieces (leaf) a
/// node holds before it splits at its midpoint. Small documents in the
/// differential test keep the tree shallow regardless; this governs fan-out at
/// scale. A midpoint split of `MAX + 1` items yields two well-occupied halves
/// (≥ `MAX / 2` each). Persistence does not require strict B-tree underflow
/// rebalancing for correctness, so deletes may leave a node sparsely populated;
/// that only affects depth, never results — a TODO for a later compaction pass.
const MAX: usize = 16;

/// A node of the persistent measured B-tree. Shared via [`Arc`]; edits copy only
/// the root→leaf path (path-copying), so older roots stay valid snapshots.
enum Node {
    /// A run of pieces plus their combined summary.
    Leaf {
        pieces: Vec<Piece>,
        summary: Summary,
    },
    /// Child pointers plus the combined summary of the whole subtree. `height`
    /// is the number of internal levels below the root inclusive (a leaf is 0).
    Internal {
        children: Vec<Arc<Node>>,
        summary: Summary,
        height: usize,
    },
}

impl Node {
    fn summary(&self) -> Summary {
        match self {
            Node::Leaf { summary, .. } | Node::Internal { summary, .. } => *summary,
        }
    }

    fn height(&self) -> usize {
        match self {
            Node::Leaf { .. } => 0,
            Node::Internal { height, .. } => *height,
        }
    }

    /// A leaf holding no pieces — the canonical empty subtree. `delete_rec`
    /// drops exactly these from a rebuilt parent (an internal node that lost all
    /// its children is collapsed to one of these, never kept as a child).
    fn is_empty_leaf(&self) -> bool {
        matches!(self, Node::Leaf { pieces, .. } if pieces.is_empty())
    }

    fn leaf(pieces: Vec<Piece>) -> Node {
        let summary = pieces
            .iter()
            .fold(Summary::default(), |acc, p| acc.add(p.summary()));
        Node::Leaf { pieces, summary }
    }

    fn internal(children: Vec<Arc<Node>>) -> Node {
        let height = children.first().map_or(1, |c| c.height() + 1);
        let summary = children
            .iter()
            .fold(Summary::default(), |acc, c| acc.add(c.summary()));
        Node::Internal {
            children,
            summary,
            height,
        }
    }

    /// An empty document: a single empty leaf.
    fn empty() -> Arc<Node> {
        Arc::new(Node::leaf(Vec::new()))
    }
}

/// Quire — a persistent measured B-tree `TextStore`. See the module docs.
pub struct Quire {
    name: String,
    /// Immutable original backing, shared by every snapshot.
    original: Arc<Original>,
    /// Append-only add buffer, shared by every snapshot; copied-on-write before
    /// a shared `Quire` grows it (see [`Quire::add_mut`]). Pieces with
    /// `source == Add` reference byte ranges in here.
    add: Arc<Vec<u8>>,
    /// The document spine: a persistent measured B-tree of pieces.
    root: Arc<Node>,

    /// Point: 1-based char position in `point_min()..=point_max()`.
    point: usize,
    /// Mark: the other end of the region, if set (1-based char position).
    mark: Option<usize>,
    /// Narrowing `(lo, hi)`; accessible region is `[lo, hi)`. `None` = whole.
    narrowing: Option<(usize, usize)>,
    /// Live markers, indexed by id; `None` = detached. Absolute 1-based positions
    /// that auto-adjust across edits (Emacs markers). Cloned on `snapshot`.
    markers: Vec<Option<usize>>,
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
        // One piece spanning the whole original. Counting its chars/lines is the
        // one up-front scan and the real cost of opening a multi-GB file. It is
        // parallelized over the available cores above `PARALLEL_INDEX_THRESHOLD`
        // (see `count_chars_lines_parallel`); the tree itself is still the single
        // whole-file piece — only the counting is fanned out. (Making it
        // incremental/background remains a later option — TODO.)
        let bytes = original.bytes();
        let bytes_len = bytes.len();
        let root = if bytes_len == 0 {
            Node::empty()
        } else {
            let (chars, lines) = if bytes_len >= PARALLEL_INDEX_THRESHOLD {
                count_chars_lines_parallel(bytes)
            } else {
                count_chars_lines(bytes)
            };
            Arc::new(Node::leaf(vec![Piece {
                source: Source::Original,
                start: 0,
                len: bytes_len,
                chars,
                lines,
            }]))
        };
        Quire {
            name,
            original: Arc::new(original),
            add: Arc::new(Vec::new()),
            root,
            point: 1,
            mark: None,
            narrowing: None,
            markers: Vec::new(),
            last_match: None,
            text_cache: RefCell::new(None),
        }
    }

    /// An O(1)/O(log n) snapshot: clone the tree root and both backing `Arc`s
    /// (no document bytes copied) and copy only the cursor/narrowing/match state.
    /// The result is an independent `Quire` whose future edits path-copy from the
    /// shared root and copy-on-write the add buffer, so neither version disturbs
    /// the other. This is the basis for ~KB workspace checkpoints over GB files.
    pub fn snapshot(&self) -> Quire {
        Quire {
            name: self.name.clone(),
            original: Arc::clone(&self.original),
            add: Arc::clone(&self.add),
            root: Arc::clone(&self.root),
            point: self.point,
            mark: self.mark,
            narrowing: self.narrowing,
            markers: self.markers.clone(),
            last_match: self.last_match.clone(),
            text_cache: RefCell::new(None),
        }
    }

    /// Re-base onto `path` after the buffer was just saved there: re-mmap the new
    /// file as a single `Original` piece and drop the pre-save backing (the old,
    /// now-unlinked mmap inode) plus the add buffer. Point/mark/narrowing/markers
    /// are kept. The saved file is byte-identical to the current content, so the
    /// char/line totals are reused from the live summary — no re-scan. O(1) + mmap.
    pub fn rebase_to(&mut self, path: &Path) -> std::io::Result<()> {
        let file = std::fs::File::open(path)?;
        // SAFETY: same immutable-mapping contract as `Quire::open`.
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        let bytes_len = mmap.len();
        debug_assert_eq!(
            bytes_len,
            self.total_bytes(),
            "rebase: saved file size must equal the current content"
        );
        let summary = self.root.summary();
        let root = if bytes_len == 0 {
            Node::empty()
        } else {
            Arc::new(Node::leaf(vec![Piece {
                source: Source::Original,
                start: 0,
                len: bytes_len,
                chars: summary.chars,
                lines: summary.lines,
            }]))
        };
        self.original = Arc::new(Original::Mapped(mmap));
        self.add = Arc::new(Vec::new());
        self.root = root;
        self.invalidate();
        Ok(())
    }

    /// Bytes of the backing store a piece points into.
    fn backing(&self, source: Source) -> &[u8] {
        match source {
            Source::Original => self.original.bytes(),
            Source::Add => &self.add,
        }
    }

    /// A piece's referenced bytes, as `&str` (always on char boundaries).
    fn piece_str(&self, p: &Piece) -> &str {
        let bytes = &self.backing(p.source)[p.start..p.start + p.len];
        // SAFETY: both backings are UTF-8 and pieces are only ever split on char
        // boundaries (see `split_piece`), so this slice is valid UTF-8.
        unsafe { std::str::from_utf8_unchecked(bytes) }
    }

    /// A mutable handle to the add buffer, performing copy-on-write if it is
    /// still shared by a snapshot. Appends thereafter are in place and O(1)
    /// amortized; the COW clone happens at most once per "first edit after a
    /// snapshot" and copies only inserted text (typically tiny vs the document).
    fn add_mut(&mut self) -> &mut Vec<u8> {
        Arc::make_mut(&mut self.add)
    }

    fn char_len(&self) -> usize {
        self.root.summary().chars
    }

    fn total_chars(&self) -> usize {
        self.root.summary().chars
    }

    fn point_min(&self) -> usize {
        self.narrowing.map_or(1, |(lo, _)| lo)
    }

    fn point_max(&self) -> usize {
        self.narrowing.map_or(self.total_chars() + 1, |(_, hi)| hi)
    }

    fn goto_char(&mut self, p: usize) {
        self.point = p.clamp(self.point_min(), self.point_max());
    }

    /// Drop the lazy `text()` cache. Called from every mutation.
    fn invalidate(&mut self) {
        *self.text_cache.borrow_mut() = None;
    }

    // ---- O(log n) seeks via the tree summaries ----------------------------

    /// Locate 1-based char position `p` (clamped to `1..=char_len+1`): descend
    /// the tree by the `chars` summary to the leaf and piece containing it, then
    /// resolve the *byte* offset of that char within the piece. Returns
    /// `(leaf piece slice as &str, byte offset, char offset within piece)` for
    /// the boundary piece, or `None` at end-of-document. **O(log n)** node hops
    /// plus one within-piece char scan (was O(pieces) over a Vec prefix sum).
    fn locate(&self, p: usize) -> Option<(Piece, usize)> {
        let p = p.clamp(1, self.total_chars() + 1);
        let mut target = p - 1; // chars before the position
        let mut node = self.root.as_ref();
        // Descend internal levels, peeling off whole children by char count.
        loop {
            match node {
                Node::Internal { children, .. } => {
                    let mut next = None;
                    for child in children {
                        let c = child.summary().chars;
                        if target < c {
                            next = Some(child.as_ref());
                            break;
                        }
                        target -= c;
                    }
                    match next {
                        Some(n) => node = n,
                        // `target` ran past the last child → end of document.
                        None => return None,
                    }
                }
                Node::Leaf { pieces, .. } => {
                    for piece in pieces {
                        if target < piece.chars {
                            let byte = self
                                .piece_str(piece)
                                .char_indices()
                                .nth(target)
                                .map_or(piece.len, |(b, _)| b);
                            return Some((*piece, byte));
                        }
                        target -= piece.chars;
                    }
                    return None;
                }
            }
        }
    }

    /// The char at 1-based position `p` (absolute; ignores narrowing), or `None`
    /// at/after end-of-document. O(log n) + a small within-piece scan.
    fn char_at(&self, p: usize) -> Option<char> {
        if p < 1 || p > self.total_chars() {
            return None;
        }
        let (piece, byte) = self.locate(p)?;
        self.piece_str(&piece)[byte..].chars().next()
    }

    /// Accumulated summary of everything strictly before 1-based char position
    /// `p` (clamped). Descending the tree, we sum the summaries of children/
    /// pieces wholly before `p` and scan only the boundary piece's prefix.
    /// **O(log n)** + one within-piece byte scan. `summary.lines` is the count
    /// of newlines before `p`; `summary.bytes` the byte offset of `p`.
    fn summary_before(&self, p: usize) -> Summary {
        let p = p.clamp(1, self.total_chars() + 1);
        let mut target = p - 1; // chars before the position
        let mut acc = Summary::default();
        let mut node = self.root.as_ref();
        loop {
            match node {
                Node::Internal { children, .. } => {
                    let mut descended = false;
                    for child in children {
                        let c = child.summary().chars;
                        if target < c {
                            node = child.as_ref();
                            descended = true;
                            break;
                        }
                        target -= c;
                        acc = acc.add(child.summary());
                    }
                    if !descended {
                        return acc; // past the end: whole-tree summary accumulated
                    }
                }
                Node::Leaf { pieces, .. } => {
                    for piece in pieces {
                        if target < piece.chars {
                            // Partial piece: scan its first `target` chars.
                            let s = self.piece_str(piece);
                            let bend = s.char_indices().nth(target).map_or(s.len(), |(b, _)| b);
                            let pre = &s[..bend];
                            acc = acc.add(Summary {
                                bytes: bend,
                                chars: target,
                                lines: pre.bytes().filter(|&b| b == b'\n').count(),
                            });
                            return acc;
                        }
                        target -= piece.chars;
                        acc = acc.add(piece.summary());
                    }
                    return acc;
                }
            }
        }
    }

    /// Count newlines in the absolute char range `[1, p)` — how many line breaks
    /// precede position `p`. **O(log n)** via [`Self::summary_before`].
    fn newlines_before(&self, p: usize) -> usize {
        self.summary_before(p).lines
    }

    fn total_bytes(&self) -> usize {
        self.root.summary().bytes
    }

    /// Visit each piece of the document in order. The closure may stop early by
    /// returning `false`. Used by whole-document and range materialization.
    fn for_each_piece<F: FnMut(&Piece) -> bool>(&self, mut f: F) {
        fn walk<F: FnMut(&Piece) -> bool>(node: &Node, f: &mut F) -> bool {
            match node {
                Node::Leaf { pieces, .. } => {
                    for p in pieces {
                        if !f(p) {
                            return false;
                        }
                    }
                    true
                }
                Node::Internal { children, .. } => {
                    for c in children {
                        if !walk(c, f) {
                            return false;
                        }
                    }
                    true
                }
            }
        }
        walk(self.root.as_ref(), &mut f);
    }

    /// Materialize the whole document into the lazy cache and hand back a `&str`
    /// borrow of it. **This is the one place text is brought fully resident** —
    /// a fallback for [`TextStore::text`]'s `&str` signature, nothing else calls
    /// it. The borrow is valid until the next mutation (which clears the cache).
    fn full_text(&self) -> &str {
        if self.text_cache.borrow().is_none() {
            let mut s = String::with_capacity(self.total_bytes());
            self.for_each_piece(|piece| {
                s.push_str(self.piece_str(piece));
                true
            });
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

    /// Materialize the absolute char range `[lo, hi)` (1-based) into an owned
    /// `String` by walking pieces. Used for region reads and as the search
    /// window. O(range + log n) — only the requested span is copied.
    fn collect_range(&self, lo: usize, hi: usize) -> String {
        let lo = lo.clamp(1, self.total_chars() + 1);
        let hi = hi.clamp(lo, self.total_chars() + 1);
        let mut out = String::new();
        if lo == hi {
            return out;
        }
        let mut pos = 1usize; // 1-based char position at the start of `piece`
        self.for_each_piece(|piece| {
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
            pos < hi
        });
        out
    }

    // ---- low-level tree splicing (touch only the spine / add buffer) -------
    // These never touch point, mark, narrowing, last_match, or the text cache;
    // each public mutator layers its own oracle-matching bookkeeping on top.
    // They rebuild only the root→leaf path (path-copying), so prior roots — and
    // therefore snapshots — are unaffected.

    /// Build a `Piece` over a freshly known byte range, counting chars/lines.
    fn make_piece(&self, source: Source, start: usize, len: usize) -> Piece {
        let bytes = &self.backing(source)[start..start + len];
        // SAFETY: callers pass char-boundary-aligned ranges into UTF-8 backings.
        let s = unsafe { std::str::from_utf8_unchecked(bytes) };
        Piece {
            source,
            start,
            len,
            chars: s.chars().count(),
            lines: s.bytes().filter(|&b| b == b'\n').count(),
        }
    }

    /// Split `piece` at `byte` (a char boundary within it) into `(left, right)`.
    fn split_piece(&self, piece: &Piece, byte: usize) -> (Piece, Piece) {
        let left = self.make_piece(piece.source, piece.start, byte);
        let right = self.make_piece(piece.source, piece.start + byte, piece.len - byte);
        (left, right)
    }

    /// Insert `s` at 1-based char position `p` (append to the add buffer, then
    /// splice one new piece into the spine). Returns the char count inserted.
    /// Rebuilds only the touched root→leaf path. O(log n) + the within-leaf
    /// piece shuffle.
    fn splice_insert(&mut self, p: usize, s: &str) -> usize {
        let n = s.chars().count();
        let start = self.add.len();
        self.add_mut().extend_from_slice(s.as_bytes());
        let piece = self.make_piece(Source::Add, start, s.len());
        let target = p.clamp(1, self.total_chars() + 1) - 1; // chars before p
        let root = Arc::clone(&self.root);
        self.root = self.insert_piece(&root, target, piece);
        n
    }

    /// Delete the absolute char range `[lo, hi)` (1-based, already ordered and
    /// clamped). Rebuilds only the touched paths. O(log n) per boundary.
    fn splice_delete(&mut self, lo: usize, hi: usize) {
        if lo >= hi {
            return;
        }
        let from = lo - 1; // chars before lo
        let len = hi - lo; // chars to remove
        let root = Arc::clone(&self.root);
        self.root = Self::delete_chars(self, &root, from, len);
    }

    /// Persistently insert `piece` so that `target` chars precede it. Returns a
    /// new root; the input root is untouched (path-copying). Splits propagate up.
    fn insert_piece(&self, root: &Arc<Node>, target: usize, piece: Piece) -> Arc<Node> {
        match self.insert_rec(root, target, piece) {
            (node, None) => node,
            // Root split: grow a new level.
            (left, Some(right)) => Arc::new(Node::internal(vec![left, right])),
        }
    }

    /// Recursive insert. Returns the rebuilt node and, if it overflowed, the
    /// right half of a split to be linked in by the caller. A piece straddled by
    /// the insertion point is split on its char boundary first (recounting
    /// chars/lines from the real backing via [`Self::split_piece`]), so the new
    /// piece always lands at an exact boundary.
    fn insert_rec(
        &self,
        node: &Arc<Node>,
        target: usize,
        piece: Piece,
    ) -> (Arc<Node>, Option<Arc<Node>>) {
        match node.as_ref() {
            Node::Leaf { pieces, .. } => {
                let mut out = pieces.clone();
                // Find the piece index and within-piece char offset for `target`.
                let mut acc = 0usize;
                let mut idx = out.len();
                let mut split_at: Option<(usize, usize)> = None; // (piece idx, char off)
                for (i, p) in out.iter().enumerate() {
                    if target < acc + p.chars {
                        let within = target - acc;
                        if within == 0 {
                            idx = i;
                        } else {
                            split_at = Some((i, within));
                            idx = i + 1;
                        }
                        break;
                    }
                    acc += p.chars;
                    idx = i + 1;
                }
                if let Some((i, within)) = split_at {
                    // Split the straddled piece on a char boundary, then insert.
                    let s = self.piece_str(&out[i]);
                    let byte = s.char_indices().nth(within).map_or(out[i].len, |(b, _)| b);
                    let (l, r) = self.split_piece(&out[i], byte);
                    out[i] = l;
                    out.insert(i + 1, r);
                }
                out.insert(idx, piece);
                Self::leaf_from(out)
            }
            Node::Internal {
                children, height, ..
            } => {
                let mut t = target;
                let mut ci = children.len() - 1;
                for (i, c) in children.iter().enumerate() {
                    let cc = c.summary().chars;
                    if t <= cc {
                        ci = i;
                        break;
                    }
                    t -= cc;
                }
                let (new_child, split) = self.insert_rec(&children[ci], t, piece);
                let mut kids = children.clone();
                kids[ci] = new_child;
                if let Some(extra) = split {
                    kids.insert(ci + 1, extra);
                }
                Self::internal_from(kids, *height)
            }
        }
    }

    /// Persistently delete `len` chars starting after `from` chars. Returns a new
    /// root; the input root is untouched. The root is collapsed while it has a
    /// single child so height stays minimal.
    fn delete_chars(&self, root: &Arc<Node>, from: usize, len: usize) -> Arc<Node> {
        let node = self.delete_rec(root, from, len);
        Self::collapse_root(node)
    }

    /// Recursive delete of `[from, from+len)` chars within `node`. Returns the
    /// rebuilt node (possibly underfull — see [`MIN`]).
    fn delete_rec(&self, node: &Arc<Node>, from: usize, len: usize) -> Arc<Node> {
        match node.as_ref() {
            Node::Leaf { pieces, .. } => {
                let mut out: Vec<Piece> = Vec::with_capacity(pieces.len() + 1);
                let mut acc = 0usize;
                let lo = from;
                let hi = from + len;
                for p in pieces {
                    let p_lo = acc;
                    let p_hi = acc + p.chars;
                    acc = p_hi;
                    if p_hi <= lo || p_lo >= hi {
                        // Entirely outside the deletion range: keep as-is.
                        out.push(*p);
                        continue;
                    }
                    // Overlaps: keep the surviving prefix and/or suffix.
                    let keep_left = lo.saturating_sub(p_lo); // chars kept at front
                    let drop_to = hi.min(p_hi) - p_lo; // chars dropped up to (excl)
                    let s = self.piece_str(p);
                    if keep_left > 0 {
                        let bend = s.char_indices().nth(keep_left).map_or(s.len(), |(b, _)| b);
                        out.push(self.make_piece(p.source, p.start, bend));
                    }
                    if drop_to < p.chars {
                        let bstart = s.char_indices().nth(drop_to).map_or(s.len(), |(b, _)| b);
                        out.push(self.make_piece(p.source, p.start + bstart, p.len - bstart));
                    }
                }
                Arc::new(Node::leaf(out))
            }
            Node::Internal { children, .. } => {
                let mut kids: Vec<Arc<Node>> = Vec::with_capacity(children.len());
                let mut acc = 0usize;
                let lo = from;
                let hi = from + len;
                for c in children {
                    let c_lo = acc;
                    let c_chars = c.summary().chars;
                    let c_hi = acc + c_chars;
                    acc = c_hi;
                    if c_hi <= lo || c_lo >= hi {
                        kids.push(Arc::clone(c)); // untouched subtree, shared
                        continue;
                    }
                    let local_from = lo.saturating_sub(c_lo);
                    let local_len = hi.min(c_hi) - lo.max(c_lo);
                    let nc = self.delete_rec(c, local_from, local_len);
                    // Keep the rebuilt child unless the deletion emptied it.
                    if !nc.is_empty_leaf() {
                        kids.push(nc);
                    }
                }
                if kids.is_empty() {
                    return Node::empty();
                }
                Arc::new(Node::internal(kids))
            }
        }
    }

    /// Collapse single-child internal roots so the root's height is minimal, and
    /// turn an all-empty tree into the canonical empty leaf.
    fn collapse_root(node: Arc<Node>) -> Arc<Node> {
        let mut node = node;
        loop {
            match node.as_ref() {
                Node::Internal { children, .. } if children.len() == 1 => {
                    node = Arc::clone(&children[0]);
                }
                _ => break,
            }
        }
        node
    }

    /// Build a leaf from `pieces`, splitting into two sibling leaves if it now
    /// exceeds [`MAX`]. Returns `(node, split-right?)`.
    fn leaf_from(mut pieces: Vec<Piece>) -> (Arc<Node>, Option<Arc<Node>>) {
        if pieces.len() <= MAX {
            return (Arc::new(Node::leaf(pieces)), None);
        }
        let right = pieces.split_off(pieces.len() / 2);
        (
            Arc::new(Node::leaf(pieces)),
            Some(Arc::new(Node::leaf(right))),
        )
    }

    /// Build an internal node from `children`, splitting into two siblings at the
    /// same `height` if it exceeds [`MAX`].
    fn internal_from(
        mut children: Vec<Arc<Node>>,
        height: usize,
    ) -> (Arc<Node>, Option<Arc<Node>>) {
        if children.len() <= MAX {
            return (Arc::new(Node::internal_h(children, height)), None);
        }
        let right = children.split_off(children.len() / 2);
        (
            Arc::new(Node::internal_h(children, height)),
            Some(Arc::new(Node::internal_h(right, height))),
        )
    }
}

impl Node {
    /// Build an internal node at an explicit `height` (used when rebuilding a
    /// level whose children already carry their own heights).
    fn internal_h(children: Vec<Arc<Node>>, height: usize) -> Node {
        let summary = children
            .iter()
            .fold(Summary::default(), |acc, c| acc.add(c.summary()));
        Node::Internal {
            children,
            summary,
            height,
        }
    }
}

impl Quire {
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
        crate::store::markers_after_insert(&mut self.markers, at, n);
        self.last_match = None;
        self.invalidate();
    }

    fn delete_region(&mut self, a: usize, b: usize) {
        let (lo, hi) = (a.min(b), a.max(b));
        let lo = lo.clamp(1, self.total_chars() + 1);
        let hi = hi.clamp(lo, self.total_chars() + 1);
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
        crate::store::markers_after_delete(&mut self.markers, lo, hi);
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
    ///
    /// TODO (M6, deferred): parallelize search/replace the way the initial
    /// char/line index already is (see `count_chars_lines_parallel`). It is left
    /// sequential on purpose — unlike a char/newline *count*, which is additive
    /// over any char-boundary partition, a regex match can straddle a chunk seam,
    /// so naive chunking misses boundary-spanning hits and double-counts overlaps.
    /// Doing it right needs per-chunk overlap regions (≥ the max match width) and
    /// dedup of seam matches, which interacts with `last_match`/replace ordering;
    /// out of scope for this slice.
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
        crate::store::markers_after_delete(&mut self.markers, start, end);
        crate::store::markers_after_insert(&mut self.markers, start, expanded.chars().count());
        self.invalidate();
        Ok(())
    }

    // ---- markers ----
    fn marker_create(&mut self, pos: Option<usize>) -> usize {
        let pos = pos.map(|p| p.clamp(1, Quire::char_len(self) + 1));
        self.markers.push(pos);
        self.markers.len() - 1
    }
    fn marker_position(&self, id: usize) -> Option<usize> {
        self.markers.get(id).copied().flatten()
    }
    fn marker_set(&mut self, id: usize, pos: Option<usize>) {
        let pos = pos.map(|p| p.clamp(1, Quire::char_len(self) + 1));
        if let Some(slot) = self.markers.get_mut(id) {
            *slot = pos;
        }
    }

    fn looking_at(&self, re: &regex::Regex) -> bool {
        // Anchor a regex at point. Like the oracle, the scan window runs from
        // point to the *document* end (narrowing is ignored here), so a match
        // that needs to read past point-max still sees the bytes. TODO: replace
        // the materialized tail with an incremental DFA reading a piece cursor so
        // this never copies past the match.
        let window = self.collect_range(self.point, self.total_chars() + 1);
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
        let full = self.total_chars() + 1;
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
        let full = self.total_chars() + 1;
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
        let mut p = self.point.min(self.total_chars() + 1);
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
        let max = self.total_chars() + 1;
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
        let doc_max = self.total_chars() + 1;
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

/// Quire is the persistent-B-tree `TextStore` (matches the `Buffer` oracle).
impl TextStore for Quire {
    fn name(&self) -> &str {
        &self.name
    }
    fn last_match(&self) -> Option<&MatchData> {
        self.last_match.as_ref()
    }
    fn snapshot(&self) -> Box<dyn TextStore> {
        Box::new(Quire::snapshot(self))
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
    fn marker_create(&mut self, pos: Option<usize>) -> usize {
        Quire::marker_create(self, pos)
    }
    fn marker_position(&self, id: usize) -> Option<usize> {
        Quire::marker_position(self, id)
    }
    fn marker_set(&mut self, id: usize, pos: Option<usize>) {
        Quire::marker_set(self, id, pos)
    }
    fn rebase_to_file(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        Quire::rebase_to(self, path)
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

    // ---- parallel initial char/line index (M6) ----

    /// The pure per-chunk counter must equal the obvious `chars().count()` /
    /// newline filter on a known mixed-width string.
    #[test]
    fn count_chars_lines_matches_str_methods() {
        let s = "aé世\n𝄞z\n\nbar"; // 1+2+3-byte chars, blank line, ascii tail
        let (chars, lines) = count_chars_lines(s.as_bytes());
        assert_eq!(chars, s.chars().count());
        assert_eq!(lines, s.bytes().filter(|&b| b == b'\n').count());
        // Empty slice: zero of both.
        assert_eq!(count_chars_lines(b""), (0, 0));
    }

    /// `next_char_boundary` snaps forward off continuation bytes and is a no-op
    /// on bytes that already start a scalar value (matching `is_char_boundary`).
    #[test]
    fn next_char_boundary_aligns_forward() {
        let s = "a世b"; // bytes: [a][世.0][世.1][世.2][b]; 世 spans 1..=3
        let b = s.as_bytes();
        assert_eq!(next_char_boundary(b, 0), 0); // 'a' lead, already aligned
        assert_eq!(next_char_boundary(b, 1), 1); // '世' lead, already aligned
        assert_eq!(next_char_boundary(b, 2), 4); // mid-'世' → next lead is 'b'
        assert_eq!(next_char_boundary(b, 3), 4); // still mid-'世'
        assert_eq!(next_char_boundary(b, 4), 4); // 'b' lead
        assert_eq!(next_char_boundary(b, b.len()), b.len()); // end is a boundary
        for i in 0..=b.len() {
            // Every result must be a real char boundary of the str.
            assert!(s.is_char_boundary(next_char_boundary(b, i)));
        }
    }

    /// The parallel driver must return EXACTLY the sequential count, including
    /// when chunk splits would otherwise fall inside a multi-byte char. Covers
    /// empty, tiny, and large multibyte inputs of many sizes.
    #[test]
    fn parallel_count_equals_sequential() {
        // A unit whose every char is multi-byte so naive even-byte splits land
        // mid-char unless realigned; the repeats below sweep many total lengths
        // (and thus many distinct split offsets relative to char boundaries).
        let unit = "héllo 世界 𝄞\nnaïve café—test\n";
        for reps in [0usize, 1, 2, 3, 5, 17, 100, 1000, 5000, 20_000] {
            let s = unit.repeat(reps);
            let bytes = s.as_bytes();
            let seq = count_chars_lines(bytes);
            let par = count_chars_lines_parallel(bytes);
            assert_eq!(
                par,
                seq,
                "parallel != sequential at reps={reps} (len={} bytes)",
                bytes.len()
            );
            // And both agree with the std-library ground truth.
            assert_eq!(
                seq,
                (s.chars().count(), s.bytes().filter(|&b| b == b'\n').count()),
                "sequential disagrees with str methods at reps={reps}"
            );
        }
        // Pure-ASCII and empty edge cases through the parallel path too.
        assert_eq!(count_chars_lines_parallel(b""), (0, 0));
        assert_eq!(count_chars_lines_parallel(b"x"), (1, 0));
        assert_eq!(count_chars_lines_parallel(b"\n\n\n"), (3, 3));
    }

    /// End-to-end: a `from_string` original past the parallel threshold must
    /// report char_len / line counts identical to a `Buffer` oracle over the
    /// same text — i.e. the parallel index wired into `with_original` is exact.
    #[test]
    fn with_original_large_index_matches_oracle() {
        let unit = "αβγ line with 世界 and a tail café\n";
        // Comfortably above PARALLEL_INDEX_THRESHOLD so the parallel path runs.
        let reps = (PARALLEL_INDEX_THRESHOLD / unit.len()) + 1_000;
        let text = unit.repeat(reps);
        assert!(text.len() >= PARALLEL_INDEX_THRESHOLD);
        let q = Quire::from_string("t", text.clone());
        let oracle = Buffer::from_string("t", text.clone());
        assert_eq!(TextStore::char_len(&q), TextStore::char_len(&oracle));
        let pmax = TextStore::point_max(&q);
        assert_eq!(
            TextStore::line_number_at_pos(&q, pmax),
            TextStore::line_number_at_pos(&oracle, pmax),
            "total line count drifted for the parallel index"
        );
        assert_eq!(TextStore::char_len(&q), text.chars().count());
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

    // ---- snapshots: cheap + independent (the persistent-tree payoff) ----

    #[test]
    fn snapshot_is_independent_of_later_edits() {
        let mut q = Quire::from_string("t", "hello world");
        let snap = q.snapshot();
        // Mutate the original after snapshotting.
        let pmax = TextStore::point_max(&q);
        TextStore::goto_char(&mut q, pmax);
        TextStore::insert(&mut q, "!!!");
        TextStore::goto_char(&mut q, 1);
        TextStore::delete_region(&mut q, 1, 6); // drop "hello"
        // The snapshot still reads the original document.
        assert_eq!(TextStore::text(&snap), "hello world");
        assert_eq!(TextStore::text(&q), " world!!!");
    }

    #[test]
    fn editing_snapshot_does_not_disturb_original() {
        let mut q = Quire::from_string("t", "alpha beta");
        let mut snap = q.snapshot();
        // Mutate the snapshot; the original is untouched.
        TextStore::goto_char(&mut snap, 1);
        TextStore::insert(&mut snap, ">>");
        assert_eq!(TextStore::text(&snap), ">>alpha beta");
        assert_eq!(TextStore::text(&q), "alpha beta");
        // And the original can still be edited independently afterwards.
        let pmax = TextStore::point_max(&q);
        TextStore::goto_char(&mut q, pmax);
        TextStore::insert(&mut q, "!");
        assert_eq!(TextStore::text(&q), "alpha beta!");
        assert_eq!(TextStore::text(&snap), ">>alpha beta");
    }

    #[test]
    fn snapshot_shares_backings_and_spine_without_deep_copy() {
        let mut q = Quire::from_string("t", "shared original text");
        // Edit so there is a non-trivial add buffer and tree to share.
        TextStore::goto_char(&mut q, 7);
        TextStore::insert(&mut q, "INSERTED ");
        let orig_ptr = Arc::as_ptr(&q.original);
        let add_ptr = Arc::as_ptr(&q.add);
        let root_ptr = Arc::as_ptr(&q.root);
        let snap = q.snapshot();
        // Snapshot points at the very same backing + spine allocations.
        assert_eq!(Arc::as_ptr(&snap.original), orig_ptr, "original not shared");
        assert_eq!(Arc::as_ptr(&snap.add), add_ptr, "add buffer not shared");
        assert_eq!(Arc::as_ptr(&snap.root), root_ptr, "tree spine not shared");
        // All three backings are now multiply-owned (proof: no deep copy).
        assert!(Arc::strong_count(&q.original) >= 2);
        assert!(Arc::strong_count(&q.add) >= 2);
        assert!(Arc::strong_count(&q.root) >= 2);
    }

    #[test]
    fn snapshot_chain_each_independent() {
        // A sequence of snapshots, each edited, all observable independently —
        // the workspace-checkpoint pattern.
        let mut q = Quire::from_string("t", "v0");
        let s0 = q.snapshot();
        let pmax = TextStore::point_max(&q);
        TextStore::goto_char(&mut q, pmax);
        TextStore::insert(&mut q, "-v1");
        let s1 = q.snapshot();
        TextStore::insert(&mut q, "-v2");
        assert_eq!(TextStore::text(&s0), "v0");
        assert_eq!(TextStore::text(&s1), "v0-v1");
        assert_eq!(TextStore::text(&q), "v0-v1-v2");
    }

    #[test]
    fn deep_tree_grows_and_stays_correct() {
        // Force a genuinely multi-level tree so the internal-node insert/split/
        // root-growth paths (not just the single-leaf fast path) are exercised,
        // then check the summaries answer seeks correctly and a deep-tree
        // snapshot is independent. Each insert at the front splits the straddled
        // piece, so the piece count climbs well past one leaf's worth.
        let mut q = Quire::from_string("t", "");
        for i in 0..400 {
            TextStore::goto_char(&mut q, 1 + (i % 7)); // scatter the insertions
            TextStore::insert(&mut q, "ab\n");
        }
        assert!(
            q.root.height() >= 2,
            "expected a multi-level tree, got height {}",
            q.root.height()
        );
        let full = TextStore::text(&q).to_string();
        assert_eq!(full.chars().count(), TextStore::char_len(&q));
        // O(log n) seeks must agree with a linear oracle over the same text.
        let oracle = Buffer::from_string("t", full.clone());
        for p in [
            1usize,
            2,
            50,
            123,
            400,
            full.chars().count(),
            full.chars().count() + 1,
        ] {
            assert_eq!(
                TextStore::line_number_at_pos(&q, p),
                TextStore::line_number_at_pos(&oracle, p),
                "line_number_at_pos({p}) on deep tree"
            );
            assert_eq!(
                TextStore::char_after(&q, p),
                TextStore::char_after(&oracle, p),
                "char_after({p}) on deep tree"
            );
        }
        // A snapshot of the deep tree shares the root and is edit-independent.
        let snap = q.snapshot();
        assert_eq!(Arc::as_ptr(&snap.root), Arc::as_ptr(&q.root));
        TextStore::goto_char(&mut q, 1);
        let end = TextStore::char_len(&q) + 1;
        TextStore::delete_region(&mut q, 1, end); // erase
        assert_eq!(TextStore::text(&q), "");
        assert_eq!(TextStore::text(&snap), full); // snapshot untouched
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

    #[test]
    fn save_to_open_path_keeps_mmap_reads_correct() {
        // Regression for the mmap-aliasing bug. `save_buffer` once overwrote the
        // very file `Quire` had mmapped as its immutable original, mutating those
        // bytes under the live pieces — so mmap-backed reads (collect_range, behind
        // substring/read_region/search) returned shifted garbage while full_text
        // (served from the text cache) still looked fine. `safety::write_atomic`
        // (temp + rename) leaves the mmapped inode intact. Open a file, edit so the
        // content shifts, save IN PLACE to the same path, and confirm windowed reads
        // stay byte-correct.
        let initial = format!(
            "αβγ HEAD line — start\n{}UNIQUE-TAIL café naïve\n",
            "filler — line ~tilde~ ‸ here\n".repeat(300)
        );
        let tmp = std::env::temp_dir().join(format!("mime-atomic-{}.txt", std::process::id()));
        std::fs::write(&tmp, &initial).unwrap();
        let mut q = Quire::open(&tmp).unwrap();
        // Insert near the front so every Original byte after it shifts position.
        q.goto_char(1);
        q.re_search_forward(&regex::Regex::new("HEAD line").unwrap(), None);
        q.insert(" <INSERTED so everything after shifts> ");
        let saved = q.full_text().to_string();

        // Save IN PLACE to the path the mmap maps — the aliasing case.
        crate::safety::write_atomic(&tmp, saved.as_bytes()).unwrap();

        // Reads through the mmap must still match full_text after the save.
        let flen = saved.chars().count();
        let cl = q.char_len();
        assert_eq!(cl, flen, "char_len drifted after in-place save");
        let tail = q.substring(cl + 1 - 40, cl + 1);
        let tail_ref: String = saved.chars().skip(flen - 40).collect();
        let mid = q.substring(500, 560);
        let mid_ref: String = saved.chars().skip(499).take(60).collect();
        std::fs::remove_file(&tmp).ok();
        assert_eq!(tail, tail_ref, "tail read corrupted after in-place save");
        assert_eq!(mid, mid_ref, "interior read corrupted after in-place save");
    }

    #[test]
    fn rebase_to_collapses_and_keeps_reads_correct() {
        let tmp = std::env::temp_dir().join(format!("mime-rebase-{}.txt", std::process::id()));
        std::fs::write(
            &tmp,
            "αβγ HEAD line\nmiddle filler line\nUNIQUE-TAIL café\n",
        )
        .unwrap();
        let mut q = Quire::open(&tmp).unwrap();
        q.goto_char(1);
        assert!(
            q.re_search_forward(&regex::Regex::new("HEAD").unwrap(), None)
                .is_some()
        );
        q.insert(" <INS> "); // grows the add buffer and splits the original piece
        let content = q.full_text().to_string();
        let (pt, mk) = (q.point(), q.mark());

        std::fs::write(&tmp, &content).unwrap(); // the save (write_atomic in production)
        q.rebase_to(&tmp).unwrap();

        let (mut n, mut all_original) = (0usize, true);
        q.for_each_piece(|p| {
            n += 1;
            all_original &= matches!(p.source, Source::Original);
            true
        });
        assert_eq!(n, 1, "rebase should collapse to a single piece");
        assert!(all_original, "the piece should be the new mmap original");
        assert!(q.add.is_empty(), "add buffer should be reset");
        assert_eq!(q.full_text(), content, "content preserved");
        assert_eq!(
            q.substring(1, q.char_len() + 1),
            content,
            "collect_range over the rebased single-piece tree"
        );
        assert_eq!((q.point(), q.mark()), (pt, mk), "cursor/mark preserved");
        std::fs::remove_file(&tmp).ok();
    }

    /// Windowed reads (the `collect_range` path behind `substring`/`read_region`)
    /// must match the oracle for many ranges, not just the whole text.
    fn assert_reads_in_sync(b: &Buffer, q: &Quire, step: usize) {
        let len = TextStore::char_len(b);
        let probes = [
            (1, len + 1),
            (len.saturating_sub(5).max(1), len + 1), // tail window
            (len / 2 + 1, len + 1),
            (1, len / 2 + 1),
            (len / 3 + 1, 2 * len / 3 + 1),
        ];
        for (a, c) in probes {
            assert_eq!(
                TextStore::substring(b, a, c),
                TextStore::substring(q, a, c),
                "substring({a},{c}) mismatch after step {step}; len={len}"
            );
        }
    }

    /// Like `run_diff`, but holds a rolling set of `snapshot()`s so the add/root
    /// `Arc`s stay shared — every edit then copies-on-write off a live snapshot,
    /// the warm-session regime `run_diff` never exercises. Checks windowed reads
    /// each step, since `full_text` can be right while a windowed seek is wrong.
    fn run_diff_snap(seed: u64, steps: usize, initial: &str, mut q: Quire) {
        let mut b = Buffer::from_string("t", initial);
        let mut rng = Lcg(seed);
        let mut held: Vec<Quire> = Vec::new();
        let inserts = ["x", "ab", "\n", "héllo", "世界", " foo ", "Z\nZ", "12"];
        let needles = ["a", "x", "foo", "\n", "Z", "é", "界", "ab"];
        let regexes = [
            regex::Regex::new(r"\w+").unwrap(),
            regex::Regex::new(r"[a-z]+").unwrap(),
            regex::Regex::new(r".").unwrap(),
        ];
        let replacements = ["", "Q", "<\\&>", "ab"];
        assert_in_sync(&b, &q, 0, "init");
        for step in 1..=steps {
            let len = TextStore::char_len(&b);
            match rng.below(8) {
                0 | 1 => {
                    let s = inserts[rng.below(inserts.len())];
                    let p = rng.pos(len);
                    TextStore::goto_char(&mut b, p);
                    TextStore::goto_char(&mut q, p);
                    TextStore::insert(&mut b, s);
                    TextStore::insert(&mut q, s);
                }
                2 => {
                    let (a, c) = (rng.pos(len), rng.pos(len));
                    TextStore::delete_region(&mut b, a, c);
                    TextStore::delete_region(&mut q, a, c);
                }
                3 | 4 => {
                    let re = &regexes[rng.below(regexes.len())];
                    let rep = replacements[rng.below(replacements.len())];
                    let hb = TextStore::re_search_forward(&mut b, re, None).is_some();
                    let hq = TextStore::re_search_forward(&mut q, re, None).is_some();
                    assert_eq!(hb, hq, "search step {step}");
                    if hb {
                        let _ = TextStore::replace_match(&mut b, rep);
                        let _ = TextStore::replace_match(&mut q, rep);
                    }
                }
                5 => {
                    let n = needles[rng.below(needles.len())];
                    let p = rng.pos(len);
                    TextStore::goto_char(&mut b, p);
                    TextStore::goto_char(&mut q, p);
                    let rb = TextStore::search_forward(&mut b, n, None);
                    let rq = TextStore::search_forward(&mut q, n, None);
                    assert_eq!(rb, rq, "search_forward step {step}");
                }
                _ => {
                    // Hold a snapshot so the next edits copy-on-write off it.
                    held.push(q.snapshot());
                    if held.len() > 40 {
                        held.remove(0);
                    }
                }
            }
            assert_in_sync(&b, &q, step, "snap-op");
            assert_reads_in_sync(&b, &q, step);
        }
        let _ = held;
    }

    const SNAP_INITIAL: &str =
        "αβγδ\n世界へようこそ\nThe quick brown fox\njumps over\nthe lazy dog.\nfoo bar baz qux\n";
    const SNAP_SEEDS: [u64; 8] = [1, 7, 42, 1000, 0xdead_beef, 0x5eed, 0xc0ffee, 0x1234_5678];

    #[test]
    fn differential_with_held_snapshots() {
        for seed in SNAP_SEEDS {
            run_diff_snap(
                seed,
                4000,
                SNAP_INITIAL,
                Quire::from_string("t", SNAP_INITIAL),
            );
        }
    }

    #[test]
    fn differential_with_held_snapshots_mmap() {
        // Same stress, but the Quire is opened from a real file → mmap-backed
        // original (the open_file path the warm MCP server uses), combined with
        // held snapshots and windowed reads.
        let path = std::env::temp_dir().join(format!("mime-quire-snap-{}.txt", std::process::id()));
        std::fs::write(&path, SNAP_INITIAL).unwrap();
        for seed in SNAP_SEEDS {
            run_diff_snap(seed, 4000, SNAP_INITIAL, Quire::open(&path).unwrap());
        }
        std::fs::remove_file(&path).ok();
    }
}
