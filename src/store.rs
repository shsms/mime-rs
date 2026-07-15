//! `TextStore` — the editing surface the engine drives, and the seam between
//! the M0 in-memory [`crate::buffer::Buffer`] (the differential-test oracle) and
//! `Quire`, the piece-tree-over-mmap store (M1). All positions are 1-based char
//! positions, Emacs-style; `point_min`/`point_max` honor the narrowing.
pub trait TextStore {
    fn name(&self) -> &str;
    /// Rename the buffer — `find-file` uniquifies a colliding basename
    /// (`doc.txt<2>`) before installing the store.
    fn set_name(&mut self, name: &str);
    /// Content version: a globally unique stamp (see [`next_version`]) taken
    /// at creation and again on every text mutation. Equal versions imply
    /// equal text (a snapshot keeps its source's version; divergent edits get
    /// fresh stamps), so caches keyed on it — the per-session tree-sitter
    /// parse — invalidate exactly when the text changes.
    fn version(&self) -> u64;
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
    fn re_search_backward(&mut self, re: &regex::Regex, bound: Option<usize>) -> Option<usize>;
    fn search_forward(&mut self, needle: &str, bound: Option<usize>) -> Option<usize>;
    fn search_backward(&mut self, needle: &str, bound: Option<usize>) -> Option<usize>;
    fn replace_match(&mut self, replacement: &str) -> Result<(), String>;
    fn looking_at(&self, re: &regex::Regex) -> bool;

    // Line motion honors the narrowing, like Emacs: results clamp into
    // [point_min, point_max], so point never escapes the accessible region
    // even when the restriction starts or ends mid-line.
    /// Move point to the first char of its line, raised to `point_min`.
    fn beginning_of_line(&mut self);
    /// Move point to the end of its line, lowered to `point_max`.
    fn end_of_line(&mut self);
    /// Move point `n` lines forward, to a line beginning; returns the count of
    /// lines that could not be moved (0 on full success), like Emacs. A line
    /// beginning outside the narrowing is unreachable: point clamps to the
    /// boundary and the move counts as short.
    fn forward_line(&mut self, n: i64) -> i64;
    /// 1-based line number of position `p`, counted from the start of the
    /// accessible region (Emacs `line-number-at-pos` default) — so displayed
    /// line labels round-trip through `goto-line` under narrowing.
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
    /// Number of marker slots ever issued (live or detached) — `revert-buffer`
    /// pads a fresh store's registry to this length so old ids stay detached
    /// instead of aliasing newly created markers.
    fn marker_count(&self) -> usize;
    /// The marker's current absolute 1-based position, or `None` if detached.
    fn marker_position(&self, id: usize) -> Option<usize>;
    /// Point marker `id` at absolute `pos`, or detach it with `None`.
    fn marker_set(&mut self, id: usize, pos: Option<usize>);

    /// After the buffer has been saved to `path`, re-base the store onto that
    /// file — for `Quire`, re-mmap the new file as one fresh original and drop the
    /// pre-save backing (the pinned old mmap inode) and the add buffer; a no-op for
    /// the in-memory `Buffer`. Content and point/mark/narrowing are unchanged.
    fn rebase_to_file(&mut self, path: &std::path::Path) -> std::io::Result<()>;

    /// The identity stamp of the visited file, captured at open/rebase time —
    /// the basis for external-change detection. `None` (the default) for a
    /// store with no backing file, like the in-memory `Buffer`.
    fn file_stamp(&self) -> Option<&crate::safety::FileStamp> {
        None
    }

    /// `true` once a read has observed the visited file drifted on disk since
    /// open — a *sticky* signal a lazy backing sets when it fetches fresh bytes
    /// from a changed file (so staleness survives an mtime reset that a bare
    /// stat would miss). `false` by default; only a file-backed store sets it.
    fn drifted(&self) -> bool {
        false
    }

    /// Stream the buffer's bytes into `w` and return the byte count written. The
    /// streaming atomic save uses this so a multi-GB `Quire` is written piece by
    /// piece, never materialized into one allocation; `Buffer` writes its string.
    /// File-backed stores re-apply their [`coding`](Self::coding) (BOM/EOL) here.
    fn write_to(&self, w: &mut dyn std::io::Write) -> std::io::Result<usize>;

    /// The visited file's BOM/EOL convention, restored by `write_to` on save.
    /// `default()` (utf-8-unix) for stores with no file, like the in-memory
    /// `Buffer` — they need no round-trip.
    fn coding(&self) -> crate::coding::FileCoding {
        crate::coding::FileCoding::default()
    }
    /// Change the coding (Emacs `set-buffer-file-coding-system`); a no-op for
    /// stores that don't round-trip a file.
    fn set_coding(&mut self, _coding: crate::coding::FileCoding) {}
}

/// The `regex-automata` twin of a compiled `regex::Regex`, for span-bounded
/// searches. Compiled with `multi_line(true)`, mirroring `build_regex` — the
/// single compile path every buffer-search regex flows through. Thread-local
/// cache keyed by pattern (bounded, like the builtin pattern cache); `None`
/// when the twin fails to build (it shouldn't — the pattern already compiled
/// under the same syntax), which callers answer with a hard-cut fallback.
fn meta_twin(re: &regex::Regex) -> Option<std::rc::Rc<regex_automata::meta::Regex>> {
    use std::{cell::RefCell, collections::HashMap, rc::Rc};
    thread_local! {
        static CACHE: RefCell<HashMap<String, Option<Rc<regex_automata::meta::Regex>>>> =
            RefCell::new(HashMap::new());
    }
    CACHE.with(|c| {
        let mut map = c.borrow_mut();
        if map.len() >= 128 && !map.contains_key(re.as_str()) {
            map.clear();
        }
        map.entry(re.as_str().to_string())
            .or_insert_with(|| {
                regex_automata::meta::Regex::builder()
                    .syntax(regex_automata::util::syntax::Config::new().multi_line(true))
                    .build(re.as_str())
                    .ok()
                    .map(Rc::new)
            })
            .clone()
    })
}

/// Leftmost match of `re` confined to `span` of `hay`, with `^`/`$`/`\b`
/// consulting the context OUTSIDE the span — the Emacs bounded-search
/// semantics (a match must fit the bound, assertions read the real buffer
/// past it). Byte offsets into `hay`, plus the capture-group texts. Like
/// `captures_at`, the match may start anywhere at or after `span.start`.
pub(crate) fn span_captures(
    re: &regex::Regex,
    hay: &str,
    span: std::ops::Range<usize>,
) -> Option<(usize, usize, Vec<Option<String>>)> {
    match meta_twin(re) {
        Some(meta) => {
            let mut caps = meta.create_captures();
            meta.captures(regex_automata::Input::new(hay).span(span), &mut caps);
            let whole = caps.get_match()?;
            let groups = (0..caps.group_len())
                .map(|i| caps.get_group(i).map(|g| hay[g.start..g.end].to_string()))
                .collect();
            Some((whole.start(), whole.end(), groups))
        }
        // Fallback: search a haystack hard-cut at the span end (assertions
        // see the cut, not the buffer).
        None => {
            let caps = re.captures_at(&hay[..span.end], span.start)?;
            let whole = caps.get(0)?;
            let groups = caps
                .iter()
                .map(|g| g.map(|m| m.as_str().to_string()))
                .collect();
            Some((whole.start(), whole.end(), groups))
        }
    }
}

/// `span_captures` without the groups — the cheap probe for the backward sweep.
fn span_find(re: &regex::Regex, hay: &str, span: std::ops::Range<usize>) -> Option<(usize, usize)> {
    match meta_twin(re) {
        Some(meta) => meta
            .find(regex_automata::Input::new(hay).span(span))
            .map(|m| (m.start(), m.end())),
        None => re
            .find_at(&hay[..span.end], span.start)
            .map(|m| (m.start(), m.end())),
    }
}

/// The latest-starting match of `re` confined to `span` of `hay` (byte
/// offsets), plus its capture-group texts — the backward-search primitive
/// shared by both stores. `^`/`$`/`\b` consult the context outside the span
/// (see [`span_captures`]). Implemented the way Emacs does it, as repeated
/// forward probes from successively later starts, so with OVERLAPPING matches
/// the latest start wins where a plain `find_iter`-take-last would be
/// leftmost-biased ("aa" in "aaa" must yield the match at 2, not 1). Each
/// probe strictly advances, so the sweep terminates; an empty-pattern match
/// degenerates to the span end, like Emacs.
pub(crate) fn latest_match_in_span(
    re: &regex::Regex,
    hay: &str,
    span: std::ops::Range<usize>,
) -> Option<(usize, usize, Vec<Option<String>>)> {
    let mut best: Option<(usize, usize)> = None;
    let mut at = span.start;
    while at <= span.end {
        let Some((s, e)) = span_find(re, hay, at..span.end) else {
            break;
        };
        best = Some((s, e));
        at = s + 1;
        while at < span.end && !hay.is_char_boundary(at) {
            at += 1;
        }
    }
    let (s, e) = best?;
    // Re-run from the winning start to collect the groups; the leftmost match
    // from `s` is the same span the probe found.
    let got = span_captures(re, hay, s..span.end)?;
    debug_assert_eq!((got.0, got.1), (s, e));
    Some(got)
}

/// The next content-version stamp (see [`TextStore::version`]): a process-wide
/// monotonic counter, so no two distinct text states ever share a value.
pub(crate) fn next_version() -> u64 {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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
