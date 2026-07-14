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

    /// Whether a paged file has been observed drifted on a fresh read since open
    /// (always `false` for owned text — it has no file to drift).
    fn drifted(&self) -> bool {
        match self {
            Original::Owned(_) => false,
            Original::Paged(p) => p.drifted(),
        }
    }
}

/// Bytes per page the on-demand reader fetches and caches.
const PAGE: usize = 64 * 1024;
/// Resident page budget: at most this many pages are cached at once (LRU
/// eviction past it), bounding a paged Quire's read footprint regardless of file
/// size. 256 × 64 KiB = 16 MiB.
const CACHE_PAGES: usize = 256;

/// Initial chars materialized by an adaptively-windowed regex search/`looking_at`
/// before it grows toward the bound. A hit within this window of point never
/// copies the document tail; only a far hit (or a genuine miss) grows past it.
const SEARCH_WINDOW_START: usize = 8 * 1024;

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
    /// Identity of the file at open time. A fresh page read re-checks it (a
    /// stat-by-path) and latches `drifted` on a mismatch — once an external
    /// writer has touched the file, a not-yet-cached page can no longer be
    /// trusted to be consistent with the open-time char/line summaries.
    stamp: crate::safety::FileStamp,
    /// Sticky: set the first time a fresh read sees the file drifted, so the
    /// buffer keeps reporting stale even if the writer later restores the mtime
    /// (which a bare stat would then read as clean again).
    drifted: std::cell::Cell<bool>,
    /// True while a bulk [`PagedFile::for_bytes`] scan is in flight: the scan
    /// statted for drift ONCE on entry, so the per-miss stat in
    /// [`PagedFile::page`] is suppressed — a cold scan of a clean file used
    /// to stat ~16k times per GB. Isolated page misses (char-at-style
    /// lookups) keep the per-miss stat, so the latch test's contract — a
    /// fresh read of a changed file detects it — holds at operation
    /// granularity.
    bulk_scan: std::cell::Cell<bool>,
}

impl PagedFile {
    fn open(file: std::fs::File, len: usize, stamp: crate::safety::FileStamp) -> PagedFile {
        PagedFile {
            file,
            len,
            cache: RefCell::new(PageCache::new()),
            stamp,
            drifted: std::cell::Cell::new(false),
            bulk_scan: std::cell::Cell::new(false),
        }
    }

    /// Stat the visited file and latch the sticky drift flag on a mismatch.
    fn check_drift(&self) {
        if !self.drifted.get() && self.stamp.check().is_some() {
            self.drifted.set(true);
        }
    }

    /// Whether a fresh read has ever observed the file drifted since open.
    fn drifted(&self) -> bool {
        self.drifted.get()
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

    /// Detect the BOM and DOS/Unix line ending by scanning to the first `\n`
    /// (DOS iff it is preceded by `\r`). Uncapped: a file whose first line is
    /// huge is still classified correctly, and a newline-free file simply reads
    /// through (Unix). Reads in modest chunks, so for real text (first `\n`
    /// early) only the leading bytes are touched. CR-only (classic-Mac) files
    /// have no `\n`, so they are Unix and kept byte-exact.
    fn detect_coding(&self) -> crate::coding::FileCoding {
        use crate::coding::{Eol, FileCoding};
        const CHUNK: usize = 8 * 1024;
        let mut head = [0u8; 3];
        let n0 = self.read_into(&mut head, 0);
        let had_bom = head[..n0].starts_with(&crate::coding::BOM);
        let mut prev = 0u8;
        let mut off = 0usize;
        let mut buf = vec![0u8; CHUNK];
        while off < self.len {
            let n = self.read_into(&mut buf[..CHUNK.min(self.len - off)], off);
            if n == 0 {
                break;
            }
            for &b in &buf[..n] {
                if b == b'\n' {
                    let eol = if prev == b'\r' { Eol::Dos } else { Eol::Unix };
                    return FileCoding::new(had_bom, eol);
                }
                prev = b;
            }
            off += n;
        }
        FileCoding::new(had_bom, Eol::Unix) // no `\n` anywhere
    }

    /// LOGICAL char/line counts for the normalized view: a leading BOM and the
    /// `\r` of each `\r\n` count as neither char nor (for the `\r`) line. One
    /// streaming pass; bounded RAM (the view never materializes the file).
    fn count_view(&self, had_bom: bool, dos: bool) -> (usize, usize) {
        let (mut chars, mut lines) = (0usize, 0usize);
        let mut prev_cr = false;
        let mut off = 0usize;
        let mut buf = vec![0u8; PAGE];
        while off < self.len {
            let n = self.read_into(&mut buf[..PAGE.min(self.len - off)], off);
            if n == 0 {
                break;
            }
            for (k, &b) in buf[..n].iter().enumerate() {
                let is_bom = had_bom && off + k < 3;
                let absorbed_lf = dos && b == b'\n' && prev_cr;
                chars += usize::from((b & 0xC0) != 0x80 && !is_bom && !absorbed_lf);
                lines += usize::from(b == b'\n');
                prev_cr = dos && b == b'\r';
            }
            off += n;
        }
        (chars, lines)
    }

    /// The page at index `pno`, reading + caching it on a miss. The page is the
    /// expected extent (`PAGE`, or shorter for the last page); a `read_at` that
    /// comes up short (truncation) leaves the tail zero-filled.
    fn page(&self, pno: u64) -> Rc<[u8]> {
        if let Some(p) = self.cache.borrow_mut().get(pno) {
            return p;
        }
        // Fresh read: the file may have drifted since open. Detect it once
        // per read OPERATION (a bulk scan stats on entry and suppresses the
        // per-miss stat here) so `is_stale` reports it durably. We still
        // serve the page (no fault, no hard stop) — but note already-cached
        // pages keep their pre-drift bytes while this fresh page reads the
        // changed file, so a post-drift read can interleave old and new
        // content. That's why the sticky flag is the only correctness signal
        // here: callers must treat a drifted buffer as untrustworthy and
        // revert, not parse it.
        if !self.bulk_scan.get() {
            self.check_drift();
        }
        let start = pno as usize * PAGE;
        let want = PAGE.min(self.len - start);
        let mut buf = vec![0u8; want];
        self.read_into(&mut buf, start);
        let rc: Rc<[u8]> = Rc::from(buf.into_boxed_slice());
        self.cache.borrow_mut().insert(pno, rc.clone());
        rc
    }

    /// Page-chunked [`Original::for_bytes`]. Drift is statted ONCE here for
    /// the whole scan; the per-page check is suppressed for its duration.
    fn for_bytes(&self, start: usize, len: usize, f: &mut dyn FnMut(&[u8]) -> bool) {
        self.check_drift();
        let prev = self.bulk_scan.replace(true);
        self.for_bytes_inner(start, len, f);
        self.bulk_scan.set(prev);
    }

    fn for_bytes_inner(&self, start: usize, len: usize, f: &mut dyn FnMut(&[u8]) -> bool) {
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

    /// ONE pass over the whole file: validate UTF-8 AND count chars/lines —
    /// fusing the two open-time scans that used to read a multi-GB file
    /// twice. `None` = not valid UTF-8. Keeps nothing resident. Above
    /// [`PARALLEL_INDEX_THRESHOLD`] the pass fans out over the available
    /// cores (threads `read_at` disjoint byte ranges; chars split by the
    /// partition are stitched at the seams), mirroring the owned-text
    /// parallel count.
    fn validate_and_count(&self) -> Option<(usize, usize)> {
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .max(1);
        if threads == 1 || self.len < PARALLEL_INDEX_THRESHOLD {
            return self.validate_and_count_seq();
        }
        let target = self.len.div_ceil(threads).max(PAGE);
        let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(threads);
        let mut start = 0usize;
        while start < self.len {
            let end = (start + target).min(self.len);
            ranges.push((start, end));
            start = end;
        }
        // Only the File crosses threads (`read_at` is positional + &self);
        // the PagedFile itself is !Sync (page cache, drift cells).
        let file = &self.file;
        let parts: Vec<ScanPart> = std::thread::scope(|scope| {
            let handles: Vec<_> = ranges
                .iter()
                .map(|&(lo, hi)| scope.spawn(move || scan_range(file, lo, hi)))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        // Stitch: each seam's (previous tail carry + next head skip) must
        // form EXACTLY one valid char — which can never be '\n' (single
        // byte, never split), so only the char count grows. A leading
        // continuation byte in the first chunk, a dangling tail at EOF, or
        // any malformed seam is invalid.
        let (mut chars, mut lines) = (0usize, 0usize);
        let mut prev_tail: Vec<u8> = Vec::new();
        for part in parts {
            let (head, c, l, tail) = part?;
            if !prev_tail.is_empty() || !head.is_empty() {
                let mut seam = std::mem::take(&mut prev_tail);
                seam.extend_from_slice(&head);
                let s = std::str::from_utf8(&seam).ok()?;
                if s.chars().count() != 1 {
                    return None;
                }
                chars += 1;
            }
            chars += c;
            lines += l;
            prev_tail = tail;
        }
        prev_tail.is_empty().then_some((chars, lines))
    }

    /// Sequential body of [`validate_and_count`], page by page, carrying an
    /// incomplete trailing char across page boundaries.
    fn validate_and_count_seq(&self) -> Option<(usize, usize)> {
        let (mut chars, mut lines) = (0usize, 0usize);
        let mut carry: Vec<u8> = Vec::new();
        let mut buf = vec![0u8; PAGE];
        let mut off = 0usize;
        while off < self.len {
            let want = PAGE.min(self.len - off);
            let filled = self.read_into(&mut buf[..want], off);
            let combined: Vec<u8>;
            let chunk: &[u8] = if carry.is_empty() {
                &buf[..filled]
            } else {
                let mut v = std::mem::take(&mut carry);
                v.extend_from_slice(&buf[..filled]);
                combined = v;
                &combined
            };
            let valid_to = match std::str::from_utf8(chunk) {
                Ok(_) => chunk.len(),
                Err(e) if e.error_len().is_none() => {
                    // An incomplete char at the chunk's tail: carry it forward
                    // and count only the validated prefix this round.
                    let v = e.valid_up_to();
                    carry = chunk[v..].to_vec();
                    v
                }
                Err(_) => return None,
            };
            let (c, l) = count_chars_lines(&chunk[..valid_to]);
            chars += c;
            lines += l;
            off += want;
        }
        // Leftover carry = a truncated char at end-of-file = invalid.
        carry.is_empty().then_some((chars, lines))
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

/// A root that is one whole-file Original piece `[start, start+len)` with the
/// given logical char/line counts — or the empty node when `len == 0`. Used at
/// open and rebase; `start` is 3 for a BOM file (the BOM bytes precede the piece)
/// and 0 otherwise.
fn single_original_root(start: usize, len: usize, chars: usize, lines: usize) -> Arc<Node> {
    if len == 0 {
        Node::empty()
    } else {
        Arc::new(Node::leaf(vec![Piece {
            source: Source::Original,
            start,
            len,
            chars,
            lines,
        }]))
    }
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

/// One parallel-scan worker's result: the raw bytes it skipped at its head
/// (leading continuation bytes of a char split by the partition), the
/// chars/lines of its validated interior, and the trailing bytes of an
/// incomplete char cut by its end. `None` = hard-invalid UTF-8 inside the
/// chunk.
type ScanPart = Option<(Vec<u8>, usize, usize, Vec<u8>)>;

/// Validate + count one byte range of `file` for the parallel fused open
/// scan — `read_at` is positional and thread-safe, so workers share the fd.
/// Ragged edges are the caller's problem: head continuation bytes (max 3)
/// are skipped and returned verbatim, an incomplete trailing char is carried
/// out, and the seam stitching in `validate_and_count` re-joins them.
fn scan_range(file: &std::fs::File, mut off: usize, end: usize) -> ScanPart {
    use std::os::unix::fs::FileExt;
    let mut head: Vec<u8> = Vec::new();
    let (mut chars, mut lines) = (0usize, 0usize);
    let mut carry: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; PAGE];
    let mut first = true;
    while off < end {
        let want = PAGE.min(end - off);
        let mut filled = 0usize;
        while filled < want {
            match file.read_at(&mut buf[filled..want], (off + filled) as u64) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let mut chunk: &[u8] = &buf[..filled];
        if first {
            first = false;
            let skip = chunk
                .iter()
                .take(3)
                .take_while(|&&b| (b & 0xC0) == 0x80)
                .count();
            head = chunk[..skip].to_vec();
            chunk = &chunk[skip..];
        }
        let combined: Vec<u8>;
        let piece: &[u8] = if carry.is_empty() {
            chunk
        } else {
            let mut v = std::mem::take(&mut carry);
            v.extend_from_slice(chunk);
            combined = v;
            &combined
        };
        let valid_to = match std::str::from_utf8(piece) {
            Ok(_) => piece.len(),
            Err(e) if e.error_len().is_none() => {
                let v = e.valid_up_to();
                carry = piece[v..].to_vec();
                v
            }
            Err(_) => return None,
        };
        let (c, l) = count_chars_lines(&piece[..valid_to]);
        chars += c;
        lines += l;
        if filled == 0 {
            break; // truncation — count what we saw
        }
        off += filled;
    }
    Some((head, chars, lines, carry))
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
    /// The TARGET coding for the next save (Emacs `buffer-file-coding-system`):
    /// what `write_to` encodes to, what `set_coding` changes, what session_status
    /// shows. Defaults to `view_coding` at open.
    coding: crate::coding::FileCoding,
    /// The format the paged Original's RAW bytes are actually in (the coding
    /// detected at open) — the basis for the normalized view (`strips_crlf`) and
    /// for a byte-exact save when it still equals `coding`. Immutable after open;
    /// `set_coding` never touches it. `default()` for in-memory/plain.
    view_coding: crate::coding::FileCoding,
}

impl Quire {
    /// Open `path` as the immutable original, read on demand a page at a time
    /// (no mmap — so external truncation can't SIGBUS and an in-place rewrite
    /// can't alias an in-flight read). Rejects non-UTF-8 input via a streaming
    /// scan (an explicit byte mode can come later, per the plan).
    pub fn open(path: &Path) -> std::io::Result<Quire> {
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len() as usize;
        // Stamp the visited file so save paths (and the pager's fresh-read drift
        // check) can detect an external writer. Captured right after the open;
        // the open→stat window is tiny and a writer landing inside it still
        // differs from the *saved* stamp later.
        let stamp = crate::safety::FileStamp::capture(path)?;
        let paged = PagedFile::open(file, len, stamp.clone());
        // One fused pass: UTF-8 validation AND the char/line index (the two
        // used to be separate whole-file scans — the dominant open cost).
        let Some(raw_counts) = paged.validate_and_count() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Quire::open: file is not valid UTF-8",
            ));
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        // A non-plain file keeps its raw BOM/CRLF bytes on the paged backing (no
        // materialization, so large-file support is preserved); the char/byte
        // scan primitives present a normalized LF view (`strips_crlf`) and the BOM
        // is excluded from the piece below, so the tree's piece summaries must be
        // the LOGICAL counts, not the raw ones.
        let coding = paged.detect_coding();
        let counts = if coding.is_plain() {
            raw_counts
        } else {
            paged.count_view(coding.had_bom, coding.eol == crate::coding::Eol::Dos)
        };
        // The BOM bytes are NOT part of any piece: the Original starts 3 bytes in,
        // and `write_to` re-emits the BOM. This keeps the signature robust against
        // inserts/deletes at the buffer start (it can't be relocated or pruned).
        let start = if coding.had_bom { 3 } else { 0 };
        let mut quire =
            Quire::with_original_counted(name, Original::Paged(paged), Some(counts), start);
        quire.stamp = Some(stamp);
        quire.coding = coding;
        quire.view_coding = coding;
        Ok(quire)
    }

    /// Build a Quire whose "original" is owned `text` (no file). Mirrors
    /// [`crate::buffer::Buffer::from_string`] so tests need no file on disk.
    pub fn from_string(name: impl Into<String>, text: impl Into<String>) -> Quire {
        Quire::with_original(name.into(), Original::Owned(text.into()))
    }

    fn with_original(name: String, original: Original) -> Quire {
        Quire::with_original_counted(name, original, None, 0)
    }

    /// [`with_original`] with the char/line counts already in hand (the fused
    /// open scan supplies them); `None` falls back to counting here.
    fn with_original_counted(
        name: String,
        original: Original,
        counts: Option<(usize, usize)>,
        start: usize,
    ) -> Quire {
        // One piece spanning the original from `start` (3 past a BOM, else 0) to
        // the end. Counting its chars/lines is the one up-front scan and the real
        // cost of opening a multi-GB file. It is parallelized over the available
        // cores above `PARALLEL_INDEX_THRESHOLD` (see `count_chars_lines_parallel`);
        // the tree itself is still the single whole-file piece — only the counting
        // is fanned out. (Making it incremental/background remains a later option.)
        let (chars, lines) = counts.unwrap_or_else(|| original.count_chars_lines());
        let root = single_original_root(start, original.len() - start, chars, lines);
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
            coding: crate::coding::FileCoding::default(),
            view_coding: crate::coding::FileCoding::default(),
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
            coding: self.coding,
            view_coding: self.view_coding,
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
        // Only a fully-plain save (target AND original coding plain) writes the
        // pieces' raw bytes verbatim, so its size equals the content. Any BOM/EOL
        // encoding — restoring the view OR converting to a new coding — makes the
        // saved file a different size; the new Original is then in the TARGET
        // coding, and the view strips it back to the same logical text, so the
        // char/line summary is reused with the saved file's byte length.
        debug_assert!(
            !(self.coding.is_plain() && self.view_coding.is_plain())
                || bytes_len == self.total_bytes(),
            "rebase: a fully-plain saved file's size must equal the content"
        );
        let summary = self.root.summary();
        // The saved file is now in the TARGET coding; its BOM (if any) precedes
        // the Original piece, which therefore starts at byte 3.
        let bom_off = if self.coding.had_bom { 3 } else { 0 };
        let root = single_original_root(bom_off, bytes_len - bom_off, summary.chars, summary.lines);
        self.original = Rc::new(Original::Paged(PagedFile::open(
            file,
            bytes_len,
            stamp.clone(),
        )));
        self.add = Arc::new(Vec::new());
        self.root = root;
        self.stamp = Some(stamp);
        self.view_coding = self.coding; // the saved file is in the target coding now
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

    /// Like [`for_bytes`](Self::for_bytes), but yields the NORMALIZED VIEW bytes:
    /// for a non-plain Original it elides a leading BOM and the `\r` of each
    /// `\r\n` (keeping the `\n`). The read paths (`full_text`, `collect_range`)
    /// use this; `write_to` uses raw `for_bytes` so untouched regions save
    /// byte-exact. `start`/`len` are RAW byte offsets into the source.
    fn for_view_bytes(
        &self,
        source: Source,
        start: usize,
        len: usize,
        mut f: impl FnMut(&[u8]) -> bool,
    ) {
        if !self.strips_crlf(source) {
            self.for_bytes(source, start, len, f);
            return;
        }
        let mut out: Vec<u8> = Vec::new();
        let mut pending_cr = false; // a `\r` held at a chunk boundary
        let mut stop = false;
        self.for_bytes(source, start, len, |chunk| {
            out.clear();
            if pending_cr {
                pending_cr = false;
                if chunk.first() != Some(&b'\n') {
                    out.push(b'\r'); // the held `\r` was a lone CR
                }
            }
            for (j, &b) in chunk.iter().enumerate() {
                if b == b'\r' {
                    match chunk.get(j + 1) {
                        Some(&b'\n') => continue,   // drop the `\r` of `\r\n`
                        Some(_) => out.push(b'\r'), // lone CR
                        None => {
                            pending_cr = true; // decide at the next chunk
                            continue;
                        }
                    }
                } else {
                    out.push(b);
                }
            }
            if !out.is_empty() {
                stop = !f(&out);
            }
            !stop
        });
        if pending_cr && !stop {
            f(b"\r");
        }
    }

    /// `true` when reads of `source` strip CRLF → LF for the normalized view. The
    /// BOM is NOT handled here: its bytes are excluded from every piece's range at
    /// open (the Original starts after them) and re-emitted on save, so it survives
    /// edits at the buffer start. Only the paged Original of a DOS file is stripped;
    /// the add buffer (inserted text) is always plain LF. `write_to` bypasses this
    /// and emits raw Original bytes, so untouched regions save byte-exact.
    fn strips_crlf(&self, source: Source) -> bool {
        source == Source::Original && self.view_coding.eol == crate::coding::Eol::Dos
    }

    /// Walk the chars of `piece` in order, one chunked byte pass, calling
    /// `f(byte_offset_of_char_start, char_index, newlines_before_char)`; `f`
    /// returns `false` to stop. Char starts are the non-continuation bytes, so no
    /// UTF-8 decode is needed — chunk splits inside a multi-byte char are
    /// invisible to the counts. The unifying primitive for every within-piece
    /// char→byte seek (locate, summary_before, the insert/delete leaf cuts).
    ///
    /// Under a normalized DOS view (see [`strips_crlf`](Self::strips_crlf)) the
    /// byte offsets stay RAW (file offsets) while char indices are LOGICAL: a
    /// `\r\n` is one newline char whose byte-start is the `\r` (the `\n` is
    /// absorbed, so a split at a char boundary never lands between them).
    fn for_each_char_mark(&self, piece: &Piece, mut f: impl FnMut(usize, usize, usize) -> bool) {
        let mut bp = 0usize; // byte offset within the piece
        let mut ci = 0usize; // chars seen so far
        let mut nl = 0usize; // newlines seen so far
        let mut prev_cr = false; // previous byte was a `\r` (carried across chunks)
        if self.strips_crlf(piece.source) {
            self.for_bytes(piece.source, piece.start, piece.len, |chunk| {
                for &b in chunk {
                    let absorbed_lf = b == b'\n' && prev_cr;
                    if (b & 0xC0) != 0x80 && !absorbed_lf {
                        if !f(bp, ci, nl) {
                            return false;
                        }
                        ci += 1;
                    }
                    if b == b'\n' {
                        nl += 1;
                    }
                    prev_cr = b == b'\r';
                    bp += 1;
                }
                true
            });
            return;
        }
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
    /// across chunk splits). Under a normalized DOS view, counts are LOGICAL: a
    /// `\r\n` is one char + one line.
    fn count_range(&self, source: Source, start: usize, len: usize) -> (usize, usize) {
        let (mut chars, mut lines) = (0usize, 0usize);
        if self.strips_crlf(source) {
            let mut prev_cr = false;
            self.for_bytes(source, start, len, |chunk| {
                for &b in chunk {
                    let absorbed_lf = b == b'\n' && prev_cr;
                    chars += usize::from((b & 0xC0) != 0x80 && !absorbed_lf);
                    lines += usize::from(b == b'\n');
                    prev_cr = b == b'\r';
                }
                true
            });
            return (chars, lines);
        }
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
        // Under a DOS view this byte starts a newline char iff it is the `\r` of
        // a `\r\n` (then the logical char is `\n`); a `\r` not followed by `\n`
        // is a real lone-CR char. (Splits never separate `\r\n`, so a `\r` at the
        // piece's end is a lone CR.)
        if self.strips_crlf(piece.source) && buf[0] == b'\r' {
            return Some(if i >= 2 && buf[1] == b'\n' {
                '\n'
            } else {
                '\r'
            });
        }
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
                    // `target` ran past the last child → end of document.
                    node = next?;
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
                self.for_view_bytes(piece.source, piece.start, piece.len, |chunk| {
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

    /// Stream the bytes of the absolute char range `[lo, hi)` (1-based) to `f`,
    /// in document order, page-bounded chunks — never materializing the range.
    /// `f` returns `false` to stop early (e.g. a search that found its match). A
    /// chunk may end mid-char, so it's raw bytes. O(streamed prefix + log n).
    fn for_range_bytes(&self, lo: usize, hi: usize, mut f: impl FnMut(&[u8]) -> bool) {
        let lo = lo.clamp(1, self.total_chars() + 1);
        let hi = hi.clamp(lo, self.total_chars() + 1);
        if lo == hi {
            return;
        }
        let mut pos = 1usize; // 1-based char position at the start of `piece`
        self.for_each_piece(|piece| {
            let piece_end = pos + piece.chars; // exclusive
            let mut keep_going = true;
            if piece_end > lo && pos < hi {
                let from = lo.saturating_sub(pos); // chars to skip in this piece
                let to = (hi - pos).min(piece.chars); // chars to take (exclusive)
                let bstart = self.scan_prefix(piece, from).0;
                let bend = self.scan_prefix(piece, to).0;
                self.for_view_bytes(piece.source, piece.start + bstart, bend - bstart, |chunk| {
                    keep_going = f(chunk);
                    keep_going
                });
            }
            pos = piece_end;
            keep_going && pos < hi
        });
    }

    /// Materialize the absolute char range `[lo, hi)` (1-based) into an owned
    /// `String`. O(range + log n) — only the requested span is copied.
    fn collect_range(&self, lo: usize, hi: usize) -> String {
        let mut out: Vec<u8> = Vec::new();
        self.for_range_bytes(lo, hi, |chunk| {
            out.extend_from_slice(chunk);
            true
        });
        // SAFETY: a char range is char-boundary-aligned, so the concatenation of
        // its byte chunks is valid UTF-8.
        unsafe { String::from_utf8_unchecked(out) }
    }

    /// First occurrence of the literal `needle` in char range `[from, to)`,
    /// returned as `(char_start, char_end)` (1-based, `end` exclusive), or
    /// `None`. Streams the range a chunk at a time, carrying the last
    /// `needle.len()-1` bytes across chunk boundaries to catch a match that
    /// straddles one, and stops at the first hit — so it never materializes the
    /// (possibly whole-document) window the way `collect_range` + `find` did.
    fn find_forward(&self, needle: &str, from: usize, to: usize) -> Option<(usize, usize)> {
        let nb = needle.as_bytes();
        let nlen = nb.len();
        if nlen == 0 {
            return None; // callers handle the empty needle
        }
        // memmem's two-way searcher (SIMD prefilter) — built once, reused per
        // chunk; the naive windows().position() scan was the residual cost on
        // long or common-prefix needles.
        let finder = memchr::memmem::Finder::new(nb);
        let mut buf: Vec<u8> = Vec::new();
        let mut buf_start_chars = 0usize; // chars in [from, start-of-buf)
        let mut start: Option<usize> = None;
        self.for_range_bytes(from, to, |chunk| {
            buf.extend_from_slice(chunk);
            if let Some(o) = finder.find(&buf) {
                start = Some(from + buf_start_chars + count_chars_lines(&buf[..o]).0);
                return false; // found — stop streaming
            }
            // Keep only the last needle-1 bytes: the longest prefix of a match
            // that could still complete in the next chunk. Account the dropped
            // bytes' chars so positions stay exact across the boundary.
            let keep = (nlen - 1).min(buf.len());
            let drop = buf.len() - keep;
            buf_start_chars += count_chars_lines(&buf[..drop]).0;
            buf.drain(..drop);
            true
        });
        start.map(|s| (s, s + needle.chars().count()))
    }

    /// LAST occurrence of the literal `needle` in char range `[from, to)`,
    /// streamed BACKWARD chunk by chunk (with a needle-sized overlap reaching
    /// down across each boundary), stopping at the first — i.e. latest —
    /// hit. Locating an anchor just above point no longer materializes the
    /// whole `[bound, point)` window the way `collect_range` + `rfind` did.
    fn find_backward(&self, needle: &str, from: usize, to: usize) -> Option<(usize, usize)> {
        let nb = needle.as_bytes();
        if nb.is_empty() {
            return None; // callers handle the empty needle
        }
        let nchars = needle.chars().count();
        let finder = memchr::memmem::FinderRev::new(nb);
        const CHUNK_CHARS: usize = 16 * 1024;
        let mut hi = to;
        while hi > from {
            let lo = hi.saturating_sub(CHUNK_CHARS).max(from);
            // A match may straddle the lower edge: extend the window down by
            // needle-1 chars so it is seen here (the chunk below would only
            // see its prefix).
            let scan_lo = lo.saturating_sub(nchars - 1).max(from);
            let window = self.collect_range(scan_lo, hi);
            if let Some(b) = finder.rfind(window.as_bytes()) {
                let start = scan_lo + window[..b].chars().count();
                return Some((start, start + nchars));
            }
            hi = lo;
        }
        None
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
    /// Adaptively windowed: materialize a window growing from `ctx_from` (one char
    /// of context before point, so `^`/`\b` judge the real boundary at a mid-line
    /// point — never crossing point-min, a real line beginning) and run the regex,
    /// stopping the moment the match ends strictly inside the window — so a hit
    /// near point never copies the document tail. A match ending exactly at a
    /// non-EOF boundary is re-judged against a larger window (it may extend, or be
    /// a `$`/`\b` artifact of the cut), so the result is identical to scanning the
    /// whole `[point, bound)` window at once (including the `$`-at-bound divergence
    /// the oracle documents). A genuine *miss* still grows to the bound — the
    /// regex must see every byte to say "no", the residual a true streaming DFA
    /// over the piece tree would close. Capturing `replace_match`'s groups needs
    /// the matched substrings regardless.
    fn re_search_forward(&mut self, re: &regex::Regex, bound: Option<usize>) -> Option<usize> {
        let from = self.point;
        let to = bound.unwrap_or_else(|| self.point_max());
        if from > to {
            return None;
        }
        let ctx_from = from.saturating_sub(1).max(self.point_min().min(from));
        let mut span = SEARCH_WINDOW_START;
        loop {
            let hi = (ctx_from + span).min(to);
            let at_eof = hi >= to;
            let window = self.collect_range(ctx_from, hi);
            let skip = if ctx_from < from {
                window.chars().next().map_or(0, char::len_utf8)
            } else {
                0
            };
            let caps = re.captures_at(&window, skip);
            // A match ending inside the window is final; one ending at a non-EOF
            // boundary might extend or be a cut artifact — grow and re-judge.
            let settled = match &caps {
                Some(c) => c.get(0).is_none_or(|m| m.end() < window.len()) || at_eof,
                None => at_eof,
            };
            if settled {
                let caps = caps?; // a settled `None` is a genuine miss
                let whole = caps.get(0)?;
                let groups: Vec<Option<String>> = caps
                    .iter()
                    .map(|g| g.map(|m| m.as_str().to_string()))
                    .collect();
                // Byte offsets in `window` → char offsets → absolute positions.
                let start = ctx_from + window[..whole.start()].chars().count();
                let end = ctx_from + window[..whole.end()].chars().count();
                self.last_match = Some(MatchData { start, end, groups });
                self.point = end;
                return Some(end);
            }
            span = span.saturating_mul(2);
        }
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
        // Anchor a regex at point. The scan window runs to the *document* end
        // (narrowing is ignored here), with one char of context before point so
        // `^`/`\b` judge the real boundary — but never from before point-min, a
        // real line beginning (see re_search_forward). Adaptively windowed: a
        // match anchored at point and ending inside the window settles `true`
        // without copying the tail; growth only happens when no in-window match
        // starts at point yet (it may appear with more text) or the match ends at
        // a non-EOF boundary (a possible `$`/`\b` cut artifact). A genuine miss
        // still grows to EOF — the residual a streaming DFA would close.
        let ctx_from = self
            .point
            .saturating_sub(1)
            .max(self.point_min().min(self.point));
        let limit = self.total_chars() + 1;
        let mut span = SEARCH_WINDOW_START;
        loop {
            let hi = (ctx_from + span).min(limit);
            let at_eof = hi >= limit;
            let window = self.collect_range(ctx_from, hi);
            let skip = if ctx_from < self.point {
                window.chars().next().map_or(0, char::len_utf8)
            } else {
                0
            };
            match re.find_at(&window, skip) {
                Some(m) if m.start() == skip && (m.end() < window.len() || at_eof) => return true,
                _ if at_eof => return false,
                _ => span = span.saturating_mul(2),
            }
        }
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
                        let clamped = self.point_min();
                        let moved = self.point != clamped;
                        self.point = clamped;
                        // Same genuine-line-beginning rule as the oracle (see
                        // Buffer::forward_line): the buffer start, or a
                        // restriction starting just after a newline, is a
                        // real line beginning — reaching it completes the
                        // move; a mid-line restriction start stays short.
                        let genuine =
                            clamped == min && (min == 1 || self.char_at(min - 1) == Some('\n'));
                        if genuine && moved {
                            left -= 1;
                            continue;
                        }
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
        let (start, end) = self.find_forward(needle, from, to)?;
        self.last_match = Some(MatchData {
            start,
            end,
            groups: vec![Some(needle.to_string())],
        });
        self.point = end;
        Some(end)
    }
    /// Regex search backward from point (bounded below): the latest-starting
    /// match wholly inside `[bound|point-min, point)`. On a hit: record
    /// match-data, move point to the match START, return it. Like the exact
    /// backward search, this still materializes the whole window (the
    /// streaming-search todo covers both); the window edges count as
    /// boundaries for `^`/`$`/`\b` — the documented cut divergence.
    fn re_search_backward(&mut self, re: &regex::Regex, bound: Option<usize>) -> Option<usize> {
        let lo = bound.unwrap_or_else(|| self.point_min()).min(self.point);
        let hi = self.point;
        let window = self.collect_range(lo, hi);
        let (ms, me, groups) = crate::store::latest_match_in(re, &window)?;
        let start = lo + window[..ms].chars().count();
        let end = lo + window[..me].chars().count();
        self.last_match = Some(MatchData { start, end, groups });
        self.point = start;
        Some(start)
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
        let (start, end) = self.find_backward(needle, lo, hi)?;
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
    fn re_search_backward(&mut self, re: &regex::Regex, bound: Option<usize>) -> Option<usize> {
        Quire::re_search_backward(self, re, bound)
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
    fn drifted(&self) -> bool {
        self.original.drifted()
    }
    fn rebase_to_file(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        Quire::rebase_to(self, path)
    }
    fn write_to(&self, w: &mut dyn std::io::Write) -> std::io::Result<usize> {
        // The target coding still matches the file the Original came from: save
        // byte-exact. The BOM (excluded from the pieces) is re-emitted first;
        // Original pieces then emit RAW (their on-disk CRLF preserved, so
        // untouched regions — even mixed line endings — round-trip exactly), and
        // inserted text (the LF add buffer) is encoded to the target EOL.
        if self.coding == self.view_coding {
            let dos = self.coding.eol == crate::coding::Eol::Dos;
            let mut written = 0usize;
            let mut err = None;
            if self.coding.had_bom {
                w.write_all(&crate::coding::BOM)?;
                written += crate::coding::BOM.len();
            }
            self.for_each_piece(|p| {
                let encode = dos && p.source == Source::Add;
                let mut ok = true;
                self.for_bytes(p.source, p.start, p.len, |chunk| {
                    let res = if encode {
                        crate::coding::write_lf_as_crlf(w, chunk).map(|n| written += n)
                    } else {
                        w.write_all(chunk).map(|()| written += chunk.len())
                    };
                    match res {
                        Ok(()) => true,
                        Err(e) => {
                            err = Some(e);
                            ok = false;
                            false
                        }
                    }
                });
                ok
            });
            return err.map_or(Ok(written), Err);
        }
        // The target coding was changed (e.g. "re-save as UTF-8" / convert to
        // CRLF): re-encode the whole NORMALIZED view to the new coding. Streams
        // the logical LF/no-BOM bytes through the CodingWriter — still no
        // whole-document materialization — at the cost of byte-exactness (the
        // user asked to change the format).
        let mut cw = crate::coding::CodingWriter::new(w, self.coding);
        let mut err = None;
        self.for_each_piece(|p| {
            let mut ok = true;
            self.for_view_bytes(p.source, p.start, p.len, |chunk| {
                if let Err(e) = std::io::Write::write_all(&mut cw, chunk) {
                    err = Some(e);
                    ok = false;
                    return false;
                }
                true
            });
            ok
        });
        if let Some(e) = err {
            return Err(e);
        }
        cw.finish()
    }
    fn coding(&self) -> crate::coding::FileCoding {
        self.coding
    }
    fn set_coding(&mut self, coding: crate::coding::FileCoding) {
        if self.coding != coding {
            self.coding = coding;
            // A coding change alters the on-disk bytes the next save writes, so
            // it must count as a modification — otherwise the conversion reads as
            // unsaved:false and an auto-revert could silently discard it.
            self.version = crate::store::next_version();
        }
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
    fn parallel_fused_scan_matches_sequential() {
        // A file past PARALLEL_INDEX_THRESHOLD, multibyte THROUGHOUT, so
        // wherever the per-core partition lands its seams, split chars are
        // exercised — the parallel pass must agree with the sequential one
        // bit-for-bit, and both with the naive count.
        let path = tmp_path("parscan");
        let unit = "é½‸a\n"; // 5 chars, 9 bytes
        let text = unit.repeat((PARALLEL_INDEX_THRESHOLD / unit.len()) + 1024);
        std::fs::write(&path, &text).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let len = file.metadata().unwrap().len() as usize;
        let stamp = crate::safety::FileStamp::capture(&path).unwrap();
        let paged = PagedFile::open(file, len, stamp);
        assert!(
            len >= PARALLEL_INDEX_THRESHOLD,
            "exercises the parallel path"
        );
        let par = paged.validate_and_count().expect("valid");
        let seq = paged.validate_and_count_seq().expect("valid");
        assert_eq!(par, seq);
        assert_eq!(par, (text.chars().count(), text.matches('\n').count()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn backward_search_chunking_matches_the_oracle() {
        // Multibyte text spanning several backward 16K-char chunks, with one
        // needle deep in the bottom chunk and one straddling the first chunk
        // edge below point — the streamed backward search must agree with
        // the oracle on both hits (latest first), the match data, and the
        // final miss.
        let mut chars: Vec<char> = std::iter::repeat_n('é', 40_000).collect();
        let plant = |chars: &mut Vec<char>, at: usize| {
            for (i, c) in "NEEDLE".chars().enumerate() {
                chars[at + i] = c;
            }
        };
        plant(&mut chars, 4_999);
        plant(&mut chars, 40_000 - 16_384 - 3); // spans the first chunk edge
        let text: String = chars.iter().collect();

        let mut q = Quire::from_string("t", &text);
        let mut b = crate::buffer::Buffer::from_string("t", &text);
        TextStore::goto_char(&mut q, chars.len() + 1);
        b.goto_char(chars.len() + 1);
        for round in 0..3 {
            let qq = TextStore::search_backward(&mut q, "NEEDLE", None);
            let bb = b.search_backward("NEEDLE", None);
            assert_eq!(qq, bb, "round {round}");
            assert_eq!(TextStore::point(&q), b.point(), "round {round}");
        }
        assert!(
            TextStore::search_backward(&mut q, "NEEDLE", None).is_none(),
            "no third match"
        );
    }

    #[test]
    fn fused_open_counts_a_char_straddling_the_page_boundary() {
        // 'é' (2 bytes) sits across the first page edge: the fused
        // validate-and-count pass must carry the split char and still
        // count it exactly once.
        let path = tmp_path("straddle");
        let mut text = "a".repeat(PAGE - 1);
        text.push('é');
        text.push_str("tail\n");
        std::fs::write(&path, &text).unwrap();
        let q = Quire::open(&path).unwrap();
        assert_eq!(Quire::char_len(&q), text.chars().count());
        assert_eq!(TextStore::char_after(&q, PAGE), Some('é'));
        assert_eq!(
            q.substring(PAGE, PAGE + 5),
            "étail",
            "reads across the boundary"
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

    #[test]
    fn paged_fresh_read_after_drift_latches_sticky_stale() {
        let path = tmp_path("drift");
        std::fs::write(&path, "a".repeat(3 * PAGE)).unwrap();
        let orig_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        let q = Quire::open(&path).unwrap();
        // Read page 0 → clean (the open just stamped the file).
        assert_eq!(TextStore::char_after(&q, 1), Some('a'));
        assert!(!TextStore::drifted(&q), "clean right after open");

        // External in-place change (size + mtime differ → the stamp drifts).
        std::fs::write(&path, "b".repeat(3 * PAGE + 7)).unwrap();
        // A read of a not-yet-cached page is a FRESH read: it detects and
        // latches the drift.
        let _ = TextStore::char_after(&q, 2 * PAGE + 100);
        assert!(
            TextStore::drifted(&q),
            "a fresh read of a changed file must latch stale"
        );

        // Sticky: restore the exact original bytes AND mtime, so a bare stat
        // would now read clean — the latched flag must still report stale.
        std::fs::write(&path, "a".repeat(3 * PAGE)).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(orig_mtime))
            .unwrap();
        assert_eq!(
            q.stamp.as_ref().unwrap().check(),
            None,
            "a bare stat reads clean again after the reset"
        );
        assert!(
            TextStore::drifted(&q),
            "the drift latch survives an mtime/size reset"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn paged_streaming_search_finds_a_match_across_a_page_edge() {
        // A literal needle deliberately straddling the PAGE boundary, then a
        // second copy two pages later — exercises the cross-chunk carry and the
        // continue-from-point loop against the in-memory oracle.
        let path = tmp_path("search");
        let needle = "NEEDLE-straddles-the-edge";
        let mut content = String::new();
        content.push_str(&"a".repeat(PAGE - 5)); // needle starts before, ends after PAGE
        content.push_str(needle);
        content.push_str(&"b".repeat(2 * PAGE));
        content.push_str(needle); // a later occurrence
        content.push('\n');
        std::fs::write(&path, &content).unwrap();

        let mut q = Quire::open(&path).unwrap();
        let mut oracle = Buffer::from_string("oracle", &content);
        TextStore::goto_char(&mut q, 1);
        TextStore::goto_char(&mut oracle, 1);
        // First (straddling) match, then the second, then a miss — all matching
        // the oracle, including where point lands.
        for _ in 0..2 {
            assert_eq!(
                TextStore::search_forward(&mut q, needle, None),
                TextStore::search_forward(&mut oracle, needle, None),
            );
            assert_eq!(TextStore::point(&q), TextStore::point(&oracle));
        }
        assert_eq!(
            TextStore::search_forward(&mut q, "no-such-token", None),
            TextStore::search_forward(&mut oracle, "no-such-token", None),
        );
        // A needle longer than a single page still matches across chunks.
        let big = "Z".repeat(PAGE + 17);
        let mut content2 = format!("prefix\n{big}\nsuffix");
        content2.push('\n');
        let path2 = tmp_path("search-big");
        std::fs::write(&path2, &content2).unwrap();
        let mut q2 = Quire::open(&path2).unwrap();
        let mut o2 = Buffer::from_string("oracle2", &content2);
        assert_eq!(
            TextStore::search_forward(&mut q2, &big, None),
            TextStore::search_forward(&mut o2, &big, None),
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&path2);
    }

    #[test]
    fn paged_search_carries_a_multibyte_char_across_the_chunk_boundary() {
        // The needle is PAST a 3-byte char that straddles the PAGE edge, so
        // find_forward's keep/drop carry runs across a multibyte split BEFORE the
        // match — exercising the char-position accounting through a dropped
        // multibyte prefix (the all-ASCII straddle test doesn't reach this path).
        let path = tmp_path("search-mb");
        let mut content = String::new();
        content.push_str(&"a".repeat(PAGE - 1));
        content.push('€'); // bytes [PAGE-1, PAGE+2): straddles the page edge
        content.push_str("bcMARKERdef\n");
        std::fs::write(&path, &content).unwrap();

        let mut q = Quire::open(&path).unwrap();
        let mut oracle = Buffer::from_string("oracle", &content);
        // A needle after the straddle, and one that itself spans the straddle.
        for n in ["MARKER", "a€bc"] {
            TextStore::goto_char(&mut q, 1);
            TextStore::goto_char(&mut oracle, 1);
            assert_eq!(
                TextStore::search_forward(&mut q, n, None),
                TextStore::search_forward(&mut oracle, n, None),
                "needle {n:?}"
            );
            assert_eq!(TextStore::point(&q), TextStore::point(&oracle));
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn adaptive_regex_search_grows_the_window_and_matches_the_oracle() {
        // A buffer larger than the initial search window so the window must grow;
        // the match and a looking_at land PAST it. Every result is checked against
        // the in-memory oracle, so the adaptive growth must be exact.
        let mut content = "a".repeat(SEARCH_WINDOW_START + 2000);
        content.push_str("NEEDLE then end\nmore\n");
        content.push_str(&"b".repeat(500));
        let mut q = Quire::from_string("q", &content);
        let mut o = Buffer::from_string("o", &content);

        // `a$` matches at the INITIAL all-`a` window's cut (end-of-window looks
        // like end-of-text) but NOT in the full text (the a's are followed by
        // `NEEDLE`, not a line end) — the adaptive search must grow past the cut
        // artifact and settle to the oracle's `None`, not report the artifact.
        for pat in ["NEEDLE", "end$", "x?NEEDLE", "no-such-token", "a$"] {
            let re = regex::Regex::new(pat).unwrap();
            TextStore::goto_char(&mut q, 1);
            TextStore::goto_char(&mut o, 1);
            assert_eq!(
                TextStore::re_search_forward(&mut q, &re, None),
                TextStore::re_search_forward(&mut o, &re, None),
                "re_search past the window must agree with the oracle for {pat:?}"
            );
        }

        // looking_at a match anchored at point that extends PAST the window.
        let long = regex::Regex::new("a+NEEDLE").unwrap();
        assert_eq!(
            TextStore::looking_at(&q, &long),
            TextStore::looking_at(&o, &long),
        );
        assert!(
            TextStore::looking_at(&q, &long),
            "a+NEEDLE matches at point 1"
        );
        // A non-match that the regex can only settle by reading to EOF.
        let nope = regex::Regex::new("a+ZZZ").unwrap();
        assert_eq!(
            TextStore::looking_at(&q, &nope),
            TextStore::looking_at(&o, &nope)
        );
        assert!(!TextStore::looking_at(&q, &nope));
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

    #[test]
    fn differential_crlf_view_paged() {
        // The proof the stripped-paged view is behaviourally identical to an LF
        // buffer: open CRLF (and BOM+CRLF) files and run the full random-op stress
        // against a Buffer holding the decoded LF text. Every text/position/read/
        // search/insert/delete/snapshot step must stay in lockstep.
        let crlf = SNAP_INITIAL.replace('\n', "\r\n");
        let bom_crlf = {
            let mut v = crate::coding::BOM.to_vec();
            v.extend_from_slice(crlf.as_bytes());
            v
        };
        for (label, bytes) in [("crlf", crlf.into_bytes()), ("bom-crlf", bom_crlf)] {
            let path = std::env::temp_dir().join(format!(
                "mime-crlf-{}-{}.txt",
                label,
                std::process::id()
            ));
            std::fs::write(&path, &bytes).unwrap();
            for seed in SNAP_SEEDS {
                run_diff_snap(seed, 4000, SNAP_INITIAL, Quire::open(&path).unwrap());
            }
            std::fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn crlf_straddling_a_page_boundary() {
        // The pending-`\r` carry in the view primitives must work when a `\r\n`
        // splits across a 64 KiB page read (the `\r` ends page 0, the `\n` starts
        // page 1) — text, char_at across the seam, and a save round-trip.
        let mut bytes = vec![b'x'; PAGE - 1]; // `\r` lands at offset PAGE-1
        bytes.extend_from_slice(b"\r\ntail\r\n");
        let path = std::env::temp_dir().join(format!("mime-pageb-{}.txt", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();

        let q = Quire::open(&path).unwrap();
        let mut want = String::from_utf8(vec![b'x'; PAGE - 1]).unwrap();
        want.push_str("\ntail\n");
        assert_eq!(TextStore::text(&q), want, "view across the page seam");
        assert_eq!(TextStore::char_len(&q), want.chars().count());
        // The newline char sits right at the seam: char (PAGE-1)+1 is '\n'.
        assert_eq!(TextStore::char_after(&q, PAGE), Some('\n'));
        assert_eq!(TextStore::char_after(&q, PAGE + 1), Some('t'));

        let mut buf: Vec<u8> = Vec::new();
        TextStore::write_to(&q, &mut buf).unwrap();
        assert_eq!(
            buf, bytes,
            "save restores the exact CRLF bytes across the seam"
        );
        std::fs::remove_file(&path).ok();
    }
}
