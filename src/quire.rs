//! Quire — the `TextStore` over an immutable, read-on-demand original plus an
//! append-only add buffer (M1), with a **persistent measured B-tree** spine.
//! This is VS Code's piece tree in its copy-on-write form: the document is an
//! ordered sequence of *pieces* `(source, start, len)` that reference one of two
//! immutable backing stores; the original file is read on demand a page at a
//! time into a bounded LRU cache (never fully resident — and never `mmap`ed, so
//! external truncation can't SIGBUS and an in-place rewrite can't alias an
//! in-flight read), and the add buffer holds only inserted text. An edit splits
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
//! **persistent** and a snapshot is just a clone of the root `Arc`.
//!
//! ## Shared, immutable backings → O(1) snapshots
//! The original (a paged file or owned string) is immutable for the program's
//! life and shared via `Rc`. The add buffer is `Arc`-shared and append-only; a
//! `Quire` appends to it in place while it is uniquely owned, and **copies it on
//! write** the first time it must grow while a snapshot still shares it (so
//! divergent timelines never clobber each other's bytes). A snapshot therefore
//! clones those two backing pointers and copies only the cursor state — no
//! document bytes move. See
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
use std::collections::HashMap;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

/// Which immutable backing store a piece points into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// The file (read on demand) or the owned text from [`Quire::from_string`].
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

/// The immutable original: either owned text (`from_string`) or a file read on
/// demand a page at a time (`open`). Read uniformly through [`Original::for_bytes`]
/// — never one contiguous `&[u8]` over the whole thing — so the file backing
/// need not be fully resident. Shared via [`Rc`] so every snapshot shares it.
enum Original {
    /// `from_string` — owns its "original" text (no file). Needed for tests and
    /// scratch buffers without a path.
    Owned(String),
    /// `open` — the file, read on demand into a bounded page cache.
    Paged(PagedFile),
}

impl Original {
    fn len(&self) -> usize {
        match self {
            Original::Owned(s) => s.len(),
            Original::Paged(p) => p.len,
        }
    }

    /// Invoke `f` with successive byte slices covering `[start, start+len)`, in
    /// order; `f` returns `false` to stop. An owned original yields the whole
    /// range as one slice; a paged original yields page-bounded slices. A slice
    /// may end mid-char — treat it as raw bytes, never `from_utf8` a single chunk
    /// on its own.
    fn for_bytes(&self, start: usize, len: usize, f: &mut dyn FnMut(&[u8]) -> bool) {
        match self {
            Original::Owned(s) => {
                f(&s.as_bytes()[start..start + len]);
            }
            Original::Paged(p) => p.for_bytes(start, len, f),
        }
    }

    /// Char and `\n` counts of the whole original — the one O(filesize) scan at
    /// open time. Owned text fans out over cores above [`PARALLEL_INDEX_THRESHOLD`];
    /// a paged file is scanned sequentially page by page (nothing kept resident).
    fn count_chars_lines(&self) -> (usize, usize) {
        match self {
            Original::Owned(s) => {
                let bytes = s.as_bytes();
                if bytes.len() >= PARALLEL_INDEX_THRESHOLD {
                    count_chars_lines_parallel(bytes)
                } else {
                    count_chars_lines(bytes)
                }
            }
            Original::Paged(p) => p.count_chars_lines(),
        }
    }
}

/// Bytes per page the on-demand reader fetches and caches.
const PAGE: usize = 64 * 1024;
/// Resident page budget: at most this many pages are cached at once (LRU
/// eviction past it), bounding a paged Quire's read footprint regardless of file
/// size. 256 × 64 KiB = 16 MiB.
const CACHE_PAGES: usize = 256;

/// A bounded LRU of file pages. Single-threaded (the engine is `!Send`); pages
/// are `Rc<[u8]>` so a reader clones one out and drops the cache borrow before
/// touching the bytes — eviction can't pull a page out from under an in-flight
/// read, and the callback can't re-enter the `RefCell`.
struct PageCache {
    pages: HashMap<u64, (Rc<[u8]>, u64)>, // page index → (bytes, last-use tick)
    tick: u64,
}

impl PageCache {
    fn new() -> PageCache {
        PageCache {
            pages: HashMap::new(),
            tick: 0,
        }
    }

    /// Touch and return the cached page, or `None` on a miss.
    fn get(&mut self, page: u64) -> Option<Rc<[u8]>> {
        self.tick += 1;
        let tick = self.tick;
        self.pages.get_mut(&page).map(|e| {
            e.1 = tick;
            e.0.clone()
        })
    }

    /// Cache `bytes` for `page`, evicting the least-recently-used page first if
    /// the budget is full.
    fn insert(&mut self, page: u64, bytes: Rc<[u8]>) {
        if self.pages.len() >= CACHE_PAGES && !self.pages.contains_key(&page) {
            let victim = self
                .pages
                .iter()
                .min_by_key(|(_, (_, t))| *t)
                .map(|(&p, _)| p);
            if let Some(v) = victim {
                self.pages.remove(&v);
            }
        }
        self.tick += 1;
        self.pages.insert(page, (bytes, self.tick));
    }
}

/// An open file read on demand, one [`PAGE`] at a time, into a bounded LRU
/// [`PageCache`]. Replaces the whole-file mmap: a `read_at` page is an owned
/// copy, so the bytes can't be mutated under an in-flight read (mmap aliasing)
/// and a truncated file can't SIGBUS — a short read just leaves the page's tail
/// zero-filled, keeping document byte offsets valid.
struct PagedFile {
    file: std::fs::File,
    len: usize,
    cache: RefCell<PageCache>,
}

impl PagedFile {
    fn open(file: std::fs::File, len: usize) -> PagedFile {
        PagedFile {
            file,
            len,
            cache: RefCell::new(PageCache::new()),
        }
    }

    /// Fill `buf` from file offset `off` with `read_at` (pread), retrying short
    /// reads; returns the bytes actually read (`< buf.len()` only on EOF /
    /// external truncation). Never faults.
    fn read_into(&self, buf: &mut [u8], off: usize) -> usize {
        let mut filled = 0;
        while filled < buf.len() {
            match self.file.read_at(&mut buf[filled..], (off + filled) as u64) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        filled
    }

    /// The page at index `pno`, reading + caching it on a miss. The page is the
    /// expected extent (`PAGE`, or shorter for the last page); a `read_at` that
    /// comes up short (truncation) leaves the tail zero-filled.
    fn page(&self, pno: u64) -> Rc<[u8]> {
        if let Some(p) = self.cache.borrow_mut().get(pno) {
            return p;
        }
        let start = pno as usize * PAGE;
        let want = PAGE.min(self.len - start);
        let mut buf = vec![0u8; want];
        self.read_into(&mut buf, start);
        let rc: Rc<[u8]> = Rc::from(buf.into_boxed_slice());
        self.cache.borrow_mut().insert(pno, rc.clone());
        rc
    }

    /// Page-chunked [`Original::for_bytes`].
    fn for_bytes(&self, start: usize, len: usize, f: &mut dyn FnMut(&[u8]) -> bool) {
        let end = start + len;
        let mut pos = start;
        while pos < end {
            let pno = (pos / PAGE) as u64;
            let page = self.page(pno);
            let within = pos - pno as usize * PAGE;
            let take = (end - pos).min(page.len() - within);
            if take == 0 {
                break; // defensive; pos < end <= len keeps page coverage non-empty
            }
            if !f(&page[within..within + take]) {
                break;
            }
            pos += take;
        }
    }

    /// Scan the whole file once, page by page, counting chars (`\n`s); keeps
    /// nothing resident. Sequential — the parallel open-time scan is owned-only
    /// for now (a paged parallel scan is a follow-up).
    fn count_chars_lines(&self) -> (usize, usize) {
        let (mut chars, mut lines) = (0usize, 0usize);
        let mut buf = vec![0u8; PAGE];
        let mut off = 0usize;
        while off < self.len {
            let want = PAGE.min(self.len - off);
            let filled = self.read_into(&mut buf[..want], off);
            let (c, l) = count_chars_lines(&buf[..filled]);
            chars += c;
            lines += l;
            off += want;
        }
        (chars, lines)
    }

    /// Validate the file is UTF-8 by a streaming scan, carrying an incomplete
    /// trailing char across page boundaries. Keeps nothing resident.
    fn is_valid_utf8(&self) -> bool {
        let mut carry: Vec<u8> = Vec::new();
        let mut buf = vec![0u8; PAGE];
        let mut off = 0usize;
        while off < self.len {
            let want = PAGE.min(self.len - off);
            let filled = self.read_into(&mut buf[..want], off);
            let combined: Vec<u8> = if carry.is_empty() {
                buf[..filled].to_vec()
            } else {
                let mut v = std::mem::take(&mut carry);
                v.extend_from_slice(&buf[..filled]);
                v
            };
            match std::str::from_utf8(&combined) {
                Ok(_) => {}
                Err(e) if e.error_len().is_none() => {
                    // An incomplete char at the chunk's tail: carry it forward.
                    carry = combined[e.valid_up_to()..].to_vec();
                }
                Err(_) => return false,
            }
            off += want;
        }
        // Leftover carry = a truncated char at end-of-file = invalid.
        carry.is_empty()
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
fn count_chars_lines(bytes: &[u8]) -> (usize, usize) {
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
fn count_chars_lines_parallel(bytes: &[u8]) -> (usize, usize) {
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
    original: Rc<Original>,
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
    /// Identity (dev/ino/mtime/size) of the visited file, captured at
    /// open/rebase time; `None` for an in-memory Quire. Save paths check it to
    /// detect an external writer before overwriting their work.
    stamp: Option<crate::safety::FileStamp>,

    /// Lazy fallback cache for [`TextStore::text`] only (see module docs).
    /// `None` after any mutation; refilled on demand by [`Quire::full_text`].
    text_cache: RefCell<Option<String>>,
    /// Content version (see `TextStore::version`): re-stamped on every text
    /// mutation; a snapshot keeps it — same version, same text.
    version: u64,
}

impl Quire {
    /// Open `path` as the immutable original, read on demand a page at a time
    /// (no mmap — so external truncation can't SIGBUS and an in-place rewrite
    /// can't alias an in-flight read). Rejects non-UTF-8 input via a streaming
    /// scan (an explicit byte mode can come later, per the plan).
    pub fn open(path: &Path) -> std::io::Result<Quire> {
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len() as usize;
        let paged = PagedFile::open(file, len);
        if !paged.is_valid_utf8() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Quire::open: file is not valid UTF-8",
            ));
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        let mut quire = Quire::with_original(name, Original::Paged(paged));
        // Stamp the visited file so save paths can detect external changes.
        // Stat-by-path right after the open; the open→stat window is tiny and
        // a writer landing inside it still differs from the *saved* stamp later.
        quire.stamp = Some(crate::safety::FileStamp::capture(path)?);
        Ok(quire)
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
        let bytes_len = original.len();
        let root = if bytes_len == 0 {
            Node::empty()
        } else {
            let (chars, lines) = original.count_chars_lines();
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
            original: Rc::new(original),
            add: Arc::new(Vec::new()),
            root,
            point: 1,
            mark: None,
            narrowing: None,
            markers: Vec::new(),
            last_match: None,
            stamp: None,
            text_cache: RefCell::new(None),
            version: crate::store::next_version(),
        }
    }

    /// An O(1)/O(log n) snapshot: clone the tree root and both backing pointers
    /// (no document bytes copied) and copy only the cursor/narrowing/match state.
    /// The result is an independent `Quire` whose future edits path-copy from the
    /// shared root and copy-on-write the add buffer, so neither version disturbs
    /// the other. This is the basis for ~KB workspace checkpoints over GB files.
    pub fn snapshot(&self) -> Quire {
        Quire {
            name: self.name.clone(),
            original: Rc::clone(&self.original),
            add: Arc::clone(&self.add),
            root: Arc::clone(&self.root),
            point: self.point,
            mark: self.mark,
            narrowing: self.narrowing,
            markers: self.markers.clone(),
            last_match: self.last_match.clone(),
            stamp: self.stamp.clone(),
            text_cache: RefCell::new(None),
            version: self.version,
        }
    }

    /// Re-base onto `path` after the buffer was just saved there: re-open the new
    /// file as a single paged `Original` piece and drop the pre-save backing (the
    /// old, now-unlinked inode + its page cache) plus the add buffer.
    /// Point/mark/narrowing/markers are kept. The saved file is byte-identical to
    /// the current content, so the char/line totals are reused from the live
    /// summary — no re-scan, no UTF-8 re-validation. O(1) + open.
    pub fn rebase_to(&mut self, path: &Path) -> std::io::Result<()> {
        let stamp = crate::safety::FileStamp::capture(path)?;
        let file = std::fs::File::open(path)?;
        let bytes_len = file.metadata()?.len() as usize;
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
        self.original = Rc::new(Original::Paged(PagedFile::open(file, bytes_len)));
        self.add = Arc::new(Vec::new());
        self.root = root;
        self.stamp = Some(stamp);
        self.invalidate();
        Ok(())
    }

    /// Invoke `f` with successive byte slices covering `[start, start+len)` of
    /// `source`, in order; `f` returns `false` to stop. The unit of access that
    /// works the same over a contiguous (Add / owned) backing and a paged
    /// original: callers scan or copy bytes rather than borrowing one `&str` over
    /// the whole range. A chunk may end mid-char, so it's raw bytes only.
    fn for_bytes(
        &self,
        source: Source,
        start: usize,
        len: usize,
        mut f: impl FnMut(&[u8]) -> bool,
    ) {
        if len == 0 {
            return;
        }
        match source {
            Source::Add => {
                f(&self.add[start..start + len]);
            }
            Source::Original => self.original.for_bytes(start, len, &mut f),
        }
    }

    /// Walk the chars of `piece` in order, one chunked byte pass, calling
    /// `f(byte_offset_of_char_start, char_index, newlines_before_char)`; `f`
    /// returns `false` to stop. Char starts are the non-continuation bytes, so no
    /// UTF-8 decode is needed — chunk splits inside a multi-byte char are
    /// invisible to the counts. The unifying primitive for every within-piece
    /// char→byte seek (locate, summary_before, the insert/delete leaf cuts).
    fn for_each_char_mark(&self, piece: &Piece, mut f: impl FnMut(usize, usize, usize) -> bool) {
        let mut bp = 0usize; // byte offset within the piece
        let mut ci = 0usize; // chars seen so far
        let mut nl = 0usize; // newlines seen so far
        self.for_bytes(piece.source, piece.start, piece.len, |chunk| {
            for &b in chunk {
                if (b & 0xC0) != 0x80 {
                    // A char starts here.
                    if !f(bp, ci, nl) {
                        return false;
                    }
                    ci += 1;
                }
                if b == b'\n' {
                    nl += 1;
                }
                bp += 1;
            }
            true
        });
    }

    /// Byte offset (within `piece`) of the start of char `n`, and the newline
    /// count before it. `n == piece.chars` yields `(piece.len, piece.lines)` —
    /// the end. One bounded chunked walk (stops at `n`, not the whole piece).
    fn scan_prefix(&self, piece: &Piece, n: usize) -> (usize, usize) {
        let mut mark = (piece.len, piece.lines);
        if n < piece.chars {
            self.for_each_char_mark(piece, |bp, ci, nl| {
                if ci == n {
                    mark = (bp, nl);
                    false
                } else {
                    true
                }
            });
        }
        mark
    }

    /// Char and newline counts of `[start, start+len)` in `source`, summed over
    /// chunks (chars = non-continuation bytes, lines = `\n` bytes — both additive
    /// across chunk splits).
    fn count_range(&self, source: Source, start: usize, len: usize) -> (usize, usize) {
        let (mut chars, mut lines) = (0usize, 0usize);
        self.for_bytes(source, start, len, |chunk| {
            let (c, l) = count_chars_lines(chunk);
            chars += c;
            lines += l;
            true
        });
        (chars, lines)
    }

    /// The char whose first byte is at offset `byte` within `piece` (a char
    /// boundary), or `None` past the piece's end. Reads only the lead char's
    /// bytes (≤4).
    fn decode_char(&self, piece: &Piece, byte: usize) -> Option<char> {
        if byte >= piece.len {
            return None;
        }
        let mut buf = [0u8; 4];
        let mut i = 0usize;
        let take = (piece.len - byte).min(4);
        self.for_bytes(piece.source, piece.start + byte, take, |chunk| {
            for &b in chunk {
                buf[i] = b;
                i += 1;
            }
            true
        });
        let clen = match buf[0] {
            b if b < 0x80 => 1,
            b if b < 0xE0 => 2,
            b if b < 0xF0 => 3,
            _ => 4,
        }
        .min(i);
        std::str::from_utf8(&buf[..clen])
            .ok()
            .and_then(|s| s.chars().next())
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
                            let byte = self.scan_prefix(piece, target).0;
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
        self.decode_char(&piece, byte)
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
                            let (bend, lines) = self.scan_prefix(piece, target);
                            acc = acc.add(Summary {
                                bytes: bend,
                                chars: target,
                                lines,
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
            let mut bytes = Vec::with_capacity(self.total_bytes());
            self.for_each_piece(|piece| {
                self.for_bytes(piece.source, piece.start, piece.len, |chunk| {
                    bytes.extend_from_slice(chunk);
                    true
                });
                true
            });
            // SAFETY: the document is valid UTF-8 (every piece spans a
            // char-aligned range of a UTF-8 backing), so the concatenation is.
            let s = unsafe { String::from_utf8_unchecked(bytes) };
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
        let mut out: Vec<u8> = Vec::new();
        if lo == hi {
            return String::new();
        }
        let mut pos = 1usize; // 1-based char position at the start of `piece`
        self.for_each_piece(|piece| {
            let piece_end = pos + piece.chars; // exclusive
            if piece_end > lo && pos < hi {
                let from = lo.saturating_sub(pos); // chars to skip in this piece
                let to = (hi - pos).min(piece.chars); // chars to take (exclusive)
                let bstart = self.scan_prefix(piece, from).0;
                let bend = self.scan_prefix(piece, to).0;
                self.for_bytes(piece.source, piece.start + bstart, bend - bstart, |chunk| {
                    out.extend_from_slice(chunk);
                    true
                });
            }
            pos = piece_end;
            pos < hi
        });
        // SAFETY: bstart/bend are char boundaries, so each copied span — and the
        // concatenation across pieces — is valid UTF-8.
        unsafe { String::from_utf8_unchecked(out) }
    }

    // ---- low-level tree splicing (touch only the spine / add buffer) -------
    // These never touch point, mark, narrowing, last_match, or the text cache;
    // each public mutator layers its own oracle-matching bookkeeping on top.
    // They rebuild only the root→leaf path (path-copying), so prior roots — and
    // therefore snapshots — are unaffected.

    /// Build a `Piece` over a freshly known byte range, counting chars/lines.
    fn make_piece(&self, source: Source, start: usize, len: usize) -> Piece {
        let (chars, lines) = self.count_range(source, start, len);
        Piece {
            source,
            start,
            len,
            chars,
            lines,
        }
    }

    /// Split `piece` at `byte` (a char boundary within it) into `(left, right)`.
    /// Counts the smaller side and derives the other from the piece's cached
    /// totals — splitting a multi-megabyte piece near one end must not rescan
    /// the rest of it (per-edit rescans made an edit sweep over a fresh
    /// document O(n²)).
    fn split_piece(&self, piece: &Piece, byte: usize) -> (Piece, Piece) {
        let (left_chars, left_lines) = if byte <= piece.len / 2 {
            self.count_range(piece.source, piece.start, byte)
        } else {
            let (c, l) = self.count_range(piece.source, piece.start + byte, piece.len - byte);
            (piece.chars - c, piece.lines - l)
        };
        let left = Piece {
            source: piece.source,
            start: piece.start,
            len: byte,
            chars: left_chars,
            lines: left_lines,
        };
        let right = Piece {
            source: piece.source,
            start: piece.start + byte,
            len: piece.len - byte,
            chars: piece.chars - left_chars,
            lines: piece.lines - left_lines,
        };
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
                    let byte = self.scan_prefix(&out[i], within).0;
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
                    // Overlaps: keep the surviving prefix and/or suffix. ONE
                    // bounded walk to the deletion's end inside this piece
                    // finds both byte cuts and the newline count up to each;
                    // the suffix summary is derived from the piece's cached
                    // totals, never by rescanning the (possibly huge) tail —
                    // a per-edit tail rescan made a replace sweep O(n²).
                    let keep_left = lo.saturating_sub(p_lo); // chars kept at front
                    let drop_to = hi.min(p_hi) - p_lo; // chars dropped up to (excl)
                    let (mut bend, mut nl_end) = (p.len, p.lines);
                    let (mut bstart, mut nl_start) = (p.len, p.lines);
                    self.for_each_char_mark(p, |bp, ci, nl| {
                        if ci == keep_left {
                            (bend, nl_end) = (bp, nl);
                        }
                        if ci == drop_to {
                            (bstart, nl_start) = (bp, nl);
                            return false;
                        }
                        true
                    });
                    if keep_left > 0 {
                        out.push(Piece {
                            source: p.source,
                            start: p.start,
                            len: bend,
                            chars: keep_left,
                            lines: nl_end,
                        });
                    }
                    if drop_to < p.chars {
                        out.push(Piece {
                            source: p.source,
                            start: p.start + bstart,
                            len: p.len - bstart,
                            chars: p.chars - drop_to,
                            lines: p.lines - nl_start,
                        });
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
        self.version = crate::store::next_version();
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
        self.version = crate::store::next_version();
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
    /// paged file directly and an incremental DFA across pieces — TODO. Capturing
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
        // Materialize one char of context BEFORE the search start (when one
        // exists inside the accessible region — point-min counts as a real
        // line beginning, so context never crosses it) and run the regex from
        // the offset past it (`captures_at`), so `^`/`\b` judge the real
        // boundary at a mid-line point. The window hard-truncates at the
        // bound, like the oracle (see its doc comment for the `$`-at-bound
        // divergence this accepts).
        let ctx_from = from.saturating_sub(1).max(self.point_min().min(from));
        let window = self.collect_range(ctx_from, to);
        let skip = if ctx_from < from {
            window.chars().next().map_or(0, char::len_utf8)
        } else {
            0
        };
        let caps = re.captures_at(&window, skip)?;
        let whole = caps.get(0)?;
        let groups: Vec<Option<String>> = caps
            .iter()
            .map(|g| g.map(|m| m.as_str().to_string()))
            .collect();
        // Byte offsets in `window` → char offsets → absolute 1-based positions.
        let start = ctx_from + window[..whole.start()].chars().count();
        let end = ctx_from + window[..whole.end()].chars().count();
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
        let expanded = crate::buffer::expand_backrefs(replacement, &md.groups);
        let (start, end) = (md.start, md.end);
        let new_len = expanded.chars().count();
        let old_len = end - start;
        self.splice_delete(start, end);
        self.splice_insert(start, &expanded);
        self.point = start + new_len;
        // Track the net length change on the narrowing bound, like
        // insert/delete_region do (the replaced span is inside the region) —
        // a length-changing replace under a restriction must not leave it stale.
        if let Some((nlo, nhi)) = self.narrowing.as_mut() {
            if new_len >= old_len {
                *nhi += new_len - old_len;
            } else {
                *nhi = nhi.saturating_sub(old_len - new_len).max(*nlo);
            }
        }
        crate::store::markers_after_delete(&mut self.markers, start, end);
        crate::store::markers_after_insert(&mut self.markers, start, new_len);
        self.version = crate::store::next_version();
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
        // that needs to read past point-max still sees the bytes; one char of
        // context before point rides along so `^`/`\b` judge the real boundary
        // — but never from before point-min, which counts as a real line
        // beginning (see re_search_forward). TODO: replace the materialized
        // tail with an incremental DFA reading a piece cursor so this never
        // copies past the match.
        let ctx_from = self
            .point
            .saturating_sub(1)
            .max(self.point_min().min(self.point));
        let window = self.collect_range(ctx_from, self.total_chars() + 1);
        let skip = if ctx_from < self.point {
            window.chars().next().map_or(0, char::len_utf8)
        } else {
            0
        };
        re.find_at(&window, skip).is_some_and(|m| m.start() == skip)
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

    // Line motion honors the narrowing, like Emacs (and the oracle): newline
    // scans run over the accessible region only, so point never escapes
    // [point_min, point_max) even when the restriction starts or ends mid-line.
    /// Move point to the first char of its line (just after the previous
    /// newline), clamped to `point_min`.
    fn beginning_of_line(&mut self) {
        // Walk back from point over non-newline chars. O(line length). Bounds
        // mirror the oracle's byte math exactly: document-clamped (a stale
        // narrowing can outlive deletions that shrank the text), and point is
        // raised to point-min but not lowered to point-max — out-of-narrowing
        // point states degrade identically in both stores.
        let doc_max = self.total_chars() + 1;
        let min = self.point_min().min(doc_max);
        let mut p = self.point.clamp(min, doc_max);
        while p > min {
            if self.char_at(p - 1) == Some('\n') {
                break;
            }
            p -= 1;
        }
        self.point = p;
    }
    /// Move point to the end of its line (just before the next newline),
    /// clamped to `point_max`.
    fn end_of_line(&mut self) {
        // Mirrors the oracle: lowered to point-max, never raised to point-min.
        let max = self.point_max().min(self.total_chars() + 1);
        let mut p = self.point.min(max);
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
        // Like the oracle, newline scanning covers the accessible region only
        // (clamped into the document — a stale narrowing can exceed it);
        // running off either end clamps to point_min/point_max.
        let doc_max = self.total_chars() + 1;
        let max = self.point_max().min(doc_max);
        let min = self.point_min().min(max);
        while left > 0 {
            if n >= 0 {
                // Advance to just past the next newline at/after point. The
                // scan starts at point itself, even when a stale narrowing has
                // left point outside [min, max] — the oracle scans from the
                // unclamped point and only clamps the *target*.
                let mut p = self.point;
                while p < max && self.char_at(p) != Some('\n') {
                    p += 1;
                }
                if p < max {
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
                // Like the forward arm, the scan starts at the unclamped point
                // (a stale narrowing can leave point outside [min, max]); only
                // the target is clamped.
                let start_line = self.point;
                let mut j = start_line.saturating_sub(2); // last excluded pos - 1
                let mut found = None;
                while j >= min {
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
    /// 1-based line number containing 1-based char position `p`, counted from
    /// the start of the accessible region (Emacs `line-number-at-pos`
    /// semantics — see the oracle). Two O(log n) tree queries.
    fn line_number_at_pos(&self, p: usize) -> usize {
        let before_p = self.newlines_before(p);
        let min = self.point_min().min(p);
        before_p - self.newlines_before(min) + 1
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
    fn set_name(&mut self, name: &str) {
        self.name = name.to_string();
    }
    fn version(&self) -> u64 {
        self.version
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
    fn marker_count(&self) -> usize {
        self.markers.len()
    }
    fn marker_set(&mut self, id: usize, pos: Option<usize>) {
        Quire::marker_set(self, id, pos)
    }
    fn file_stamp(&self) -> Option<&crate::safety::FileStamp> {
        self.stamp.as_ref()
    }
    fn rebase_to_file(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        Quire::rebase_to(self, path)
    }
    fn write_to(&self, w: &mut dyn std::io::Write) -> std::io::Result<usize> {
        // Stream each piece's bytes in document order — equals `full_text()` byte
        // for byte, but never materializes the whole document.
        let mut written = 0usize;
        let mut err = None;
        self.for_each_piece(|p| {
            let mut ok = true;
            self.for_bytes(p.source, p.start, p.len, |chunk| match w.write_all(chunk) {
                Ok(()) => {
                    written += chunk.len();
                    true
                }
                Err(e) => {
                    err = Some(e);
                    ok = false;
                    false
                }
            });
            ok
        });
        err.map_or(Ok(written), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Buffer;

    // ---- focused unit tests (mirror buffer.rs so failures localize) ----

    #[test]
    fn char_access_stops_at_the_narrowing_boundaries() {
        // Mirrors buffer.rs: boundary chars never leak through a narrowing.
        let mut q = Quire::from_string("t", "abcdef");
        TextStore::narrow_to_region(&mut q, 3, 5); // "cd"
        assert_eq!(TextStore::char_after(&q, 3), Some('c'));
        assert_eq!(TextStore::char_after(&q, 5), None);
        assert_eq!(TextStore::char_before(&q, 4), Some('c'));
        assert_eq!(TextStore::char_before(&q, 3), None);
    }

    #[test]
    fn line_motion_clamps_to_the_narrowing() {
        // Mirrors buffer.rs: line motion never escapes a mid-line narrowing.
        let mut q = Quire::from_string("t", "abc\ndef\nghi");
        TextStore::narrow_to_region(&mut q, 3, 7); // "c\nde"
        TextStore::goto_char(&mut q, 5);
        TextStore::end_of_line(&mut q);
        assert_eq!(TextStore::point(&q), 7, "end-of-line stops at point-max");
        TextStore::beginning_of_line(&mut q);
        assert_eq!(TextStore::point(&q), 5);
        TextStore::goto_char(&mut q, 3);
        TextStore::beginning_of_line(&mut q);
        assert_eq!(
            TextStore::point(&q),
            3,
            "stops at point-min, not line start"
        );
        TextStore::goto_char(&mut q, 3);
        assert_eq!(TextStore::forward_line(&mut q, 1), 0);
        assert_eq!(TextStore::point(&q), 5);
        assert_eq!(TextStore::forward_line(&mut q, 1), 1);
        assert_eq!(TextStore::point(&q), 7);
        assert_eq!(TextStore::forward_line(&mut q, -2), 2);
        assert_eq!(TextStore::point(&q), 3);
    }

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
    fn forward_line_from_a_point_outside_a_stale_narrowing_matches_oracle() {
        // A delete after the restriction shrinks its upper bound (the bound
        // tracks every length change) without touching point, leaving point
        // outside [min, max]. The line scans must then start from the real
        // point, like the oracle — clamping it into the stale restriction
        // under-counts the backward moves.
        let mut b = crate::Buffer::from_string("t", "l Z\nZ\nfXX");
        let mut q = Quire::from_string("t", "l Z\nZ\nfXX");
        for s in [&mut b as &mut dyn TextStore, &mut q as &mut dyn TextStore] {
            s.narrow_to_region(4, 7);
            s.goto_char(7);
            s.delete_region(8, 10); // after the narrowing: hi 7→5, point stays 7
            assert_eq!(s.narrowing(), Some((4, 5)));
            assert_eq!(s.point(), 7, "point escaped the restriction");
        }
        assert_eq!(
            TextStore::forward_line(&mut b, -3),
            TextStore::forward_line(&mut q, -3),
            "shortfall must match the oracle"
        );
        assert_eq!(TextStore::point(&b), TextStore::point(&q));
    }

    #[test]
    fn line_anchor_sees_real_boundaries_not_the_search_window() {
        // Mirror of the Buffer test: the materialized window carries one char
        // of context before point, so `^`/`\b` judge the real boundary there.
        let mut q = Quire::from_string("t", "one\ntwo\nthree");
        let re = regex::RegexBuilder::new("^t")
            .multi_line(true)
            .build()
            .unwrap();
        TextStore::goto_char(&mut q, 2); // mid "one" — not a line beginning
        assert_eq!(
            TextStore::re_search_forward(&mut q, &re, None),
            Some(6),
            "start of 'two'"
        );
        assert!(!TextStore::looking_at(&q, &re));
        TextStore::goto_char(&mut q, 5); // start of "two"
        assert!(TextStore::looking_at(&q, &re));
        let word = regex::Regex::new(r"\bne\b").unwrap();
        TextStore::goto_char(&mut q, 2); // "o|ne" — 'n' is not word-start
        assert_eq!(TextStore::re_search_forward(&mut q, &word, None), None);
    }

    #[test]
    fn bounded_search_fits_quantifiers_and_point_max_is_a_line_end() {
        // Mirror of the Buffer test: a greedy match backtracks to fit the
        // bound; `$` matches at point-max.
        let mut q = Quire::from_string("t", "aaa");
        let re = regex::Regex::new("a+").unwrap();
        TextStore::goto_char(&mut q, 1);
        assert_eq!(TextStore::re_search_forward(&mut q, &re, Some(3)), Some(3));
        let mut q = Quire::from_string("t", "foobar\n");
        TextStore::narrow_to_region(&mut q, 1, 4); // accessible region is "foo"
        TextStore::goto_char(&mut q, 1);
        let re2 = regex::RegexBuilder::new("o$")
            .multi_line(true)
            .build()
            .unwrap();
        assert_eq!(TextStore::re_search_forward(&mut q, &re2, None), Some(4));
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
    #[ignore = "timing only; run: cargo test --lib bench_parallel_index -- --ignored --nocapture"]
    fn bench_parallel_index() {
        use std::hint::black_box;
        use std::time::Instant;
        // A large multibyte input so the parallel split has real work to do.
        let unit = "the quick brown fox — naïve café ジャンプ here\n";
        let bytes = unit.repeat((64 * 1024 * 1024) / unit.len()).into_bytes();
        assert_eq!(
            count_chars_lines(&bytes),
            count_chars_lines_parallel(&bytes),
            "parallel count must equal sequential"
        );
        let runs = 5u32;
        let t0 = Instant::now();
        for _ in 0..runs {
            black_box(count_chars_lines(black_box(&bytes)));
        }
        let seq = t0.elapsed() / runs;
        let t1 = Instant::now();
        for _ in 0..runs {
            black_box(count_chars_lines_parallel(black_box(&bytes)));
        }
        let par = t1.elapsed() / runs;
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        eprintln!(
            "index {} MiB  seq {seq:?}  par {par:?}  speedup {:.1}x  ({cores} cores)",
            bytes.len() / (1024 * 1024),
            seq.as_secs_f64() / par.as_secs_f64().max(f64::MIN_POSITIVE),
        );
    }

    #[test]
    fn open_reads_a_file() {
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

    /// Tempfile path unique to a test name + this process.
    fn tmp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("quire_paged_{tag}_{}.txt", std::process::id()))
    }

    #[test]
    fn open_rejects_a_file_truncated_mid_char() {
        // A 3-byte char with its last byte missing: the streaming validator must
        // reject it (a leftover carry at EOF), like the whole-buffer check did.
        let path = tmp_path("trunc-char");
        let mut bytes = "ok\n".as_bytes().to_vec();
        bytes.extend_from_slice(&"€".as_bytes()[..2]); // drop the final byte
        std::fs::write(&path, &bytes).unwrap();
        assert!(
            Quire::open(&path).is_err(),
            "truncated trailing char must fail"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn open_reads_an_empty_file_via_the_paged_path() {
        // A 0-byte file through Quire::open: the streaming validator accepts it,
        // with_original short-circuits to an empty tree, and reads are empty —
        // no page() is ever called (len 0), so no offset math runs on it.
        let path = tmp_path("empty");
        std::fs::write(&path, b"").unwrap();
        let q = Quire::open(&path).unwrap();
        assert_eq!(TextStore::char_len(&q), 0);
        assert_eq!(TextStore::text(&q), "");
        assert_eq!(TextStore::char_after(&q, 1), None);
        assert_eq!(TextStore::substring(&q, 1, 1), "");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn paged_reads_multipage_content_with_a_char_straddling_a_page_edge() {
        // A 3-byte char deliberately straddling a PAGE boundary, then several
        // more pages of mixed ASCII/multibyte/newlines — exercises the chunked
        // char/byte seeks across page splits against the in-memory oracle.
        let path = tmp_path("multi");
        let mut content = String::new();
        content.push_str(&"a".repeat(PAGE - 1)); // chars 1..=PAGE-1
        content.push('€'); // char PAGE: bytes [PAGE-1, PAGE+2) straddle the page edge
        while content.len() < 3 * PAGE {
            content.push_str("xy€z\nq\t");
        }
        std::fs::write(&path, &content).unwrap();

        let mut q = Quire::open(&path).unwrap();
        let mut oracle = Buffer::from_string("oracle", &content);
        assert_eq!(TextStore::text(&q), TextStore::text(&oracle));
        assert_eq!(TextStore::char_len(&q), TextStore::char_len(&oracle));

        let euro = PAGE; // 1-based position of the straddling '€'
        for p in [euro - 1, euro, euro + 1, 1, TextStore::char_len(&q)] {
            assert_eq!(
                TextStore::char_after(&q, p),
                TextStore::char_after(&oracle, p),
                "char_after({p})"
            );
        }
        // Substring + line number spanning the page edge.
        assert_eq!(
            TextStore::substring(&q, euro - 3, euro + 4),
            TextStore::substring(&oracle, euro - 3, euro + 4),
        );
        assert_eq!(
            TextStore::line_number_at_pos(&q, euro + 1000),
            TextStore::line_number_at_pos(&oracle, euro + 1000),
        );
        // A search whose needle crosses the page boundary.
        TextStore::goto_char(&mut q, 1);
        TextStore::goto_char(&mut oracle, 1);
        assert_eq!(
            TextStore::search_forward(&mut q, "a€xy", None),
            TextStore::search_forward(&mut oracle, "a€xy", None),
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn paged_cache_eviction_keeps_reads_correct() {
        // Content larger than the page-cache budget, so reads must evict and
        // re-read pages. Materializing the whole document (text()) walks every
        // page while the cache holds only CACHE_PAGES of them at a time.
        let path = tmp_path("evict");
        let budget = CACHE_PAGES * PAGE;
        let mut content = String::with_capacity(budget + 4 * PAGE);
        while content.len() < budget + 3 * PAGE {
            content.push_str("the quick brown fox jumps over €\n");
        }
        std::fs::write(&path, &content).unwrap();

        let q = Quire::open(&path).unwrap();
        let oracle = Buffer::from_string("oracle", &content);
        assert!(
            content.len() > budget,
            "must exceed the cache budget to evict"
        );
        assert_eq!(TextStore::char_len(&q), TextStore::char_len(&oracle));
        // Full materialization is correct despite eviction mid-scan.
        assert_eq!(TextStore::text(&q), TextStore::text(&oracle));
        // Far-apart reads (forcing re-reads of long-evicted pages).
        let last = TextStore::char_len(&q);
        for p in [1, last / 3, 2 * last / 3, last - 4] {
            assert_eq!(
                TextStore::char_after(&q, p),
                TextStore::char_after(&oracle, p)
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn paged_open_survives_external_truncation() {
        // The SIGBUS-on-truncate guarantee: mmap would fault when an in-flight
        // read touched a page past a shrunk file. The pager zero-fills a short
        // read instead, so reads stay panic-free and internally consistent.
        let path = tmp_path("trunc");
        let content = "x".repeat(3 * PAGE);
        std::fs::write(&path, &content).unwrap();
        let q = Quire::open(&path).unwrap();
        let len_at_open = TextStore::char_len(&q);
        assert_eq!(len_at_open, 3 * PAGE);

        // External truncation after open, before any page is read.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(1)
            .unwrap();

        // Must NOT SIGBUS/panic; char count stays the open-time total (offsets
        // are fixed), and the materialized text matches that length.
        let text = TextStore::text(&q);
        assert_eq!(text.chars().count(), len_at_open);
        assert_eq!(TextStore::char_after(&q, 1), Some('x'));
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
        let orig_ptr = Rc::as_ptr(&q.original);
        let add_ptr = Arc::as_ptr(&q.add);
        let root_ptr = Arc::as_ptr(&q.root);
        let snap = q.snapshot();
        // Snapshot points at the very same backing + spine allocations.
        assert_eq!(Rc::as_ptr(&snap.original), orig_ptr, "original not shared");
        assert_eq!(Arc::as_ptr(&snap.add), add_ptr, "add buffer not shared");
        assert_eq!(Arc::as_ptr(&snap.root), root_ptr, "tree spine not shared");
        // All three backings are now multiply-owned (proof: no deep copy).
        assert!(Rc::strong_count(&q.original) >= 2);
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
            // Boundary assertions exercise the one-char context window the
            // Quire search materializes (the oracle sees its whole text).
            regex::RegexBuilder::new(r"^\w")
                .multi_line(true)
                .build()
                .unwrap(),
            regex::RegexBuilder::new(r"\w$")
                .multi_line(true)
                .build()
                .unwrap(),
            regex::Regex::new(r"\bfoo").unwrap(),
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
    fn save_to_open_path_keeps_paged_reads_correct() {
        // Regression for the original-aliasing bug. `save_buffer` once overwrote
        // the very file `Quire` read as its immutable original, mutating those
        // bytes under the live pieces — so file-backed reads (collect_range, behind
        // substring/read_region/search) returned shifted garbage while full_text
        // (served from the text cache) still looked fine. `safety::write_atomic`
        // (temp + rename) leaves the original inode intact. Open a file, edit so the
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

        // Save IN PLACE to the original's path — the aliasing case.
        crate::safety::write_atomic(&tmp, saved.as_bytes()).unwrap();

        // File-backed reads must still match full_text after the save.
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
    fn stamp_tracks_open_external_change_and_rebase() {
        let tmp = std::env::temp_dir().join(format!("mime-stamp-q-{}.txt", std::process::id()));
        std::fs::write(&tmp, "hello stamp\n").unwrap();

        // Opening from a file records a clean stamp; from_string records none.
        let mut q = Quire::open(&tmp).unwrap();
        assert_eq!(q.stamp.as_ref().unwrap().check(), None);
        assert!(Quire::from_string("s", "x").stamp.is_none());

        // A snapshot carries the same stamp.
        assert_eq!(q.snapshot().stamp, q.stamp);

        // An external in-place write drifts the stamp...
        std::fs::write(&tmp, "hello stamp, externally grown\n").unwrap();
        assert!(q.stamp.as_ref().unwrap().check().is_some());

        // ...and rebasing onto a fresh save re-stamps it clean.
        q.delete_region(1, q.char_len() + 1);
        q.insert("rebased content\n");
        std::fs::write(&tmp, q.full_text()).unwrap();
        q.rebase_to(&tmp).unwrap();
        let stamp = q.stamp.as_ref().unwrap();
        assert_eq!(stamp.check(), None);
        std::fs::remove_file(&tmp).ok();
        assert!(stamp.check().is_some(), "deletion must drift the new stamp");
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
        assert!(all_original, "the piece should be the new paged original");
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

    #[test]
    fn write_to_streams_full_document() {
        let mut q = Quire::from_string("t", "αβγ line\nhello world\ntail 世界\n");
        q.goto_char(1);
        q.re_search_forward(&regex::Regex::new("hello").unwrap(), None);
        q.insert("X"); // split into several pieces (original + add + original)
        let full = q.full_text().to_string();
        let mut buf: Vec<u8> = Vec::new();
        let n = TextStore::write_to(&q, &mut buf).unwrap();
        assert_eq!(n, full.len(), "byte count");
        assert_eq!(buf, full.as_bytes(), "streamed bytes equal full_text");
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
    fn differential_with_held_snapshots_paged() {
        // Same stress, but the Quire is opened from a real file → paged file-backed
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
