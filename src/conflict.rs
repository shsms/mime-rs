//! Merge-conflict hunks — parse VCS conflict markers out of a `TextStore` and
//! resolve them. Recognizes the standard git shape (also emitted by svn/hg),
//! including the diff3/zdiff3 base section and longer marker runs (git's
//! `conflict-marker-size` attribute), with a consistent run length per hunk:
//!
//! ```text
//! <<<<<<< ours-label
//! ours…
//! ||||||| base-label        (diff3 only)
//! base…
//! =======
//! theirs…
//! >>>>>>> theirs-label
//! ```
//!
//! Scanning is stateless — every call re-scans the accessible region (so it
//! composes with narrowing, and there is no cached hunk list to invalidate
//! after an edit). Malformed or unterminated marker runs are not hunks; they
//! stay plain text for the ordinary editing vocabulary, and the overview
//! reports them as stray opener lines so they are never silent. A *nested*
//! conflict of the same marker size surfaces innermost-first: the outer parse
//! is rejected (its separator/closer would be ambiguous), the inner hunk is
//! well-formed in isolation, and once it is resolved a re-scan sees the
//! outer. CRLF lines are tolerated (the `\r` stays inside the side spans,
//! content-faithful).

use crate::store::TextStore;

/// One conflict hunk, in 1-based char positions, half-open spans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// Whole hunk: start of the `<<<<<<<` line to just past the `>>>>>>>`
    /// line's newline (or `point-max` when the file ends without one).
    pub start: usize,
    pub end: usize,
    /// The "ours" side, marker lines excluded; a non-empty side ends in `\n`.
    pub ours: (usize, usize),
    /// The diff3 base section, when present.
    pub base: Option<(usize, usize)>,
    /// The "theirs" side, marker lines excluded.
    pub theirs: (usize, usize),
    pub ours_label: String,
    pub theirs_label: String,
}

/// Scan the accessible region for well-formed conflict hunks, in buffer order.
/// Point is preserved; match data is clobbered (the scan searches).
pub fn scan(b: &mut dyn TextStore) -> Vec<Hunk> {
    scan_with_strays(b).0
}

/// Like [`scan`], but also counts the STRAY opener lines in the same pass:
/// well-formed `<<<<<<<` marker lines that did not parse into a hunk
/// (malformed, unterminated, or nested conflicts — an opener glued to its
/// label, like `<<<<<<<x`, is plain text, not a stray). The overview surfaces
/// the count so an agent never mistakes "0 parsed hunks" for "nothing
/// conflict-shaped present".
pub fn scan_with_strays(b: &mut dyn TextStore) -> (Vec<Hunk>, usize) {
    let saved = b.point();
    let mut hunks = Vec::new();
    let mut strays = 0;
    b.goto_char(b.point_min());
    while let Some(after) = b.search_forward("<<<<<<<", None) {
        let start = after - 7;
        // Only a run at the start of a line opens a hunk.
        if start > b.point_min() && b.char_before(start) != Some('\n') {
            continue;
        }
        match parse_hunk_at(b, start) {
            Some(h) => {
                b.goto_char(h.end);
                hunks.push(h);
            }
            // Malformed: count the opener as a stray when it is marker-form,
            // then resume the search just after the run.
            None => {
                let (line, _) = line_at(b, start);
                let mlen = run_len(line.strip_suffix('\r').unwrap_or(&line), '<');
                if marker_label(&line, '<', mlen).is_some() {
                    strays += 1;
                }
                b.goto_char(after);
            }
        }
    }
    b.goto_char(saved);
    (hunks, strays)
}

/// The line starting at `pos`: its text and the position just past its
/// end-of-line (the next line's start, or `point-max` on the last line).
fn line_at(b: &mut dyn TextStore, pos: usize) -> (String, usize) {
    b.goto_char(pos);
    b.end_of_line();
    let le = b.point();
    let text = b.substring(pos, le);
    let next = if le < b.point_max() { le + 1 } else { le };
    (text, next)
}

/// Length of the run of `marker` chars at the start of `line`.
fn run_len(line: &str, marker: char) -> usize {
    line.chars().take_while(|&c| c == marker).count()
}

/// For a candidate marker line: `Some(label)` if, after a run of exactly
/// `mlen` `marker` chars, the line is empty or carries a space-separated
/// label; `None` otherwise (not a marker line). CR-tolerant.
fn marker_label(line: &str, marker: char, mlen: usize) -> Option<String> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    if run_len(line, marker) != mlen {
        return None;
    }
    let rest = &line[mlen..]; // the run is ASCII, so byte index == char index
    if rest.is_empty() {
        Some(String::new())
    } else {
        rest.strip_prefix(' ').map(|l| l.trim().to_string())
    }
}

/// Parse one hunk whose `<<<<<<<` line begins at `start`; `None` unless a
/// well-formed opener → (base?) → `=======` → `>>>>>>>` sequence completes
/// before the end of the accessible region.
fn parse_hunk_at(b: &mut dyn TextStore, start: usize) -> Option<Hunk> {
    let pmax = b.point_max();
    let (opener, ours_start) = line_at(b, start);
    let mlen = run_len(opener.strip_suffix('\r').unwrap_or(&opener), '<');
    if mlen < 7 {
        return None;
    }
    let ours_label = marker_label(&opener, '<', mlen)?;

    // Walk the sections; each `Some` marks a boundary already seen.
    let mut ours_end: Option<usize> = None;
    let mut base_span: Option<(usize, usize)> = None; // (start, end)
    let mut theirs_start: Option<usize> = None;
    let mut pos = ours_start;
    while pos < pmax {
        let line_pos = pos;
        let (line, next) = line_at(b, line_pos);
        pos = next;
        // A nested opener of the SAME run length (zdiff3 criss-cross merges)
        // would make this hunk's separator/closer ambiguous — reject the outer
        // parse. The scan then resumes past this opener and surfaces the
        // *inner* conflict, which is well-formed in isolation: nested hunks
        // resolve innermost-first, and re-scanning sees the outer once its
        // inside is clean. (A nested opener of a *different* length is plain
        // content — its separator/closer can't match `mlen` — so it parses
        // through correctly without this guard.)
        if marker_label(&line, '<', mlen).is_some() {
            return None;
        }
        if let Some(ts) = theirs_start {
            // In theirs: only the closer ends the hunk.
            if let Some(theirs_label) = marker_label(&line, '>', mlen) {
                return Some(Hunk {
                    start,
                    end: next,
                    ours: (ours_start, ours_end.unwrap()),
                    base: base_span,
                    theirs: (ts, line_pos),
                    ours_label,
                    theirs_label,
                });
            }
        } else if let Some((bs, _)) = base_span {
            // In base: only the separator ends it.
            if marker_label(&line, '=', mlen) == Some(String::new()) {
                base_span = Some((bs, line_pos));
                theirs_start = Some(next);
            }
        } else {
            // In ours: a diff3 base marker or the separator ends it.
            if marker_label(&line, '|', mlen).is_some() {
                ours_end = Some(line_pos);
                base_span = Some((next, next)); // end patched at the separator
            } else if marker_label(&line, '=', mlen) == Some(String::new()) {
                ours_end = Some(line_pos);
                theirs_start = Some(next);
            }
        }
    }
    None // unterminated
}

/// The hunk a program addressed: 1-based index `n`, or — with `None` — the
/// hunk containing `point` (how `smerge-keep-current` addresses). The `Err`
/// is a ready error message.
pub fn pick(hunks: &[Hunk], n: Option<i64>, point: usize) -> Result<&Hunk, String> {
    match n {
        Some(n) => usize::try_from(n)
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| hunks.get(i))
            .ok_or_else(|| format!("no conflict {n} ({} in the buffer)", hunks.len())),
        None => hunks
            .iter()
            .find(|h| h.start <= point && point < h.end)
            .ok_or_else(|| "point is not inside a conflict".to_string()),
    }
}

/// The text a resolution side denotes. `ours`/`theirs`/`base` are the plain
/// sections (`base` errs on a non-diff3 hunk); `both` keeps ours then theirs
/// (smerge has no name for it); `all` keeps ours, base, theirs in order
/// (`smerge-keep-all`).
pub fn side_text(b: &dyn TextStore, h: &Hunk, side: &str) -> Result<String, String> {
    // Lazy per side: only the sections the request names are materialized.
    let ours = || b.substring(h.ours.0, h.ours.1);
    let theirs = || b.substring(h.theirs.0, h.theirs.1);
    let base = || h.base.map(|(s, e)| b.substring(s, e));
    match side {
        "ours" => Ok(ours()),
        "theirs" => Ok(theirs()),
        "base" => base().ok_or_else(|| "no base section (not a diff3 conflict)".to_string()),
        "both" => Ok(format!("{}{}", ours(), theirs())),
        "all" => Ok(format!(
            "{}{}{}",
            ours(),
            base().unwrap_or_default(),
            theirs()
        )),
        other => Err(format!("unknown side: {other} (ours|theirs|base|both|all)")),
    }
}

/// Render the `conflict-hunks` overview: one line per hunk with its number,
/// char position + line (goto-char-able), labels, and side sizes; plus a
/// trailing warning when `strays` opener lines were left unparsed.
pub fn render(b: &dyn TextStore, hunks: &[Hunk], strays: usize) -> String {
    let name = b.name();
    let lines_of = |span: (usize, usize)| b.substring(span.0, span.1).lines().count();
    let mut out = if hunks.is_empty() {
        format!("— no conflicts in {name} —\n")
    } else {
        format!(
            "— {} conflict{} in {name} —\n",
            hunks.len(),
            if hunks.len() == 1 { "" } else { "s" },
        )
    };
    for (i, h) in hunks.iter().enumerate() {
        let sides = match h.base {
            Some(span) => format!(
                "ours {} / base {} / theirs {}",
                lines_of(h.ours),
                lines_of(span),
                lines_of(h.theirs)
            ),
            None => format!("ours {} / theirs {}", lines_of(h.ours), lines_of(h.theirs)),
        };
        out.push_str(&format!(
            "{:>5} @{} L{}: {} ↔ {} ({} lines)\n",
            i + 1,
            h.start,
            b.line_number_at_pos(h.start),
            h.ours_label,
            h.theirs_label,
            sides,
        ));
    }
    if strays > 0 {
        // No `^` anchor in the suggestion: the engine's windowed search treats
        // `^` as a haystack anchor, not a line anchor, so an anchored pattern
        // would miss these very lines.
        out.push_str(&format!(
            "  ! {strays} unparsed '<<<<<<<' marker line{} — malformed or nested \
             conflict text; resolve the hunks above and re-list, or inspect \
             with (occur \"<<<<<<<\")\n",
            if strays == 1 { "" } else { "s" }
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Buffer;

    fn hunks_of(text: &str) -> (Vec<Hunk>, Buffer) {
        let mut b = Buffer::from_string("merge.txt", text);
        let hunks = scan(&mut b);
        (hunks, b)
    }

    #[test]
    fn parses_a_two_way_hunk_with_labels_and_spans() {
        let text = "keep\n<<<<<<< HEAD\nours line\n=======\ntheirs line\n>>>>>>> feature/x\ntail\n";
        let (hunks, b) = hunks_of(text);
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        assert_eq!(h.ours_label, "HEAD");
        assert_eq!(h.theirs_label, "feature/x");
        assert_eq!(b.substring(h.ours.0, h.ours.1), "ours line\n");
        assert_eq!(b.substring(h.theirs.0, h.theirs.1), "theirs line\n");
        assert_eq!(h.base, None);
        // The whole-hunk span covers opener line through closer newline.
        assert_eq!(b.substring(h.start, h.end), &text[5..text.len() - 5]);
        // Point was preserved by the scan.
        assert_eq!(b.point(), 1);
    }

    #[test]
    fn parses_diff3_base_and_empty_sides() {
        let text = "<<<<<<< ours\n=======\nt1\nt2\n>>>>>>> theirs\n";
        let (hunks, b) = hunks_of(text);
        assert_eq!(hunks.len(), 1);
        assert_eq!(b.substring(hunks[0].ours.0, hunks[0].ours.1), "");
        assert_eq!(
            b.substring(hunks[0].theirs.0, hunks[0].theirs.1),
            "t1\nt2\n"
        );

        let text = "<<<<<<< a\no\n||||||| merged common ancestor\nb1\nb2\n=======\nt\n>>>>>>> b\n";
        let (hunks, b) = hunks_of(text);
        assert_eq!(hunks.len(), 1);
        let (bs, be) = hunks[0].base.expect("diff3 base parsed");
        assert_eq!(b.substring(bs, be), "b1\nb2\n");
        assert_eq!(b.substring(hunks[0].ours.0, hunks[0].ours.1), "o\n");
        assert_eq!(b.substring(hunks[0].theirs.0, hunks[0].theirs.1), "t\n");
    }

    #[test]
    fn multiple_hunks_longer_markers_crlf_and_no_trailing_newline() {
        // Two hunks; the second uses git's longer markers (size 9), CRLF
        // line endings, and the file ends without a newline.
        let text = "<<<<<<< A\no1\n=======\nt1\n>>>>>>> B\nmid\n<<<<<<<<< A\r\no2\r\n=========\r\nt2\r\n>>>>>>>>> B";
        let (hunks, b) = hunks_of(text);
        assert_eq!(hunks.len(), 2);
        assert_eq!(b.substring(hunks[1].ours.0, hunks[1].ours.1), "o2\r\n");
        assert_eq!(b.substring(hunks[1].theirs.0, hunks[1].theirs.1), "t2\r\n");
        assert_eq!(hunks[1].end, b.char_len() + 1, "closer at EOF, no newline");
    }

    #[test]
    fn rejects_malformed_runs_as_plain_text() {
        for text in [
            // Unterminated: opener and separator but no closer.
            "<<<<<<< A\nours\n=======\ntheirs\n",
            // No separator at all.
            "<<<<<<< A\nours\n>>>>>>> B\n",
            // Marker not at line start.
            "x <<<<<<< A\nours\n=======\ntheirs\n>>>>>>> B is fine though\n",
            // Run-length mismatch: a size-9 opener with a size-7 closer.
            "<<<<<<<<< A\nours\n=========\ntheirs\n>>>>>>> B\n",
            // Opener glued to a label without a space (heredoc-ish).
            "<<<<<<<EOF\nours\n=======\ntheirs\n>>>>>>> B\n",
        ] {
            let (hunks, _) = hunks_of(text);
            // "Marker not at line start" still has a *valid* opener later? No:
            // every case above must yield zero hunks.
            assert_eq!(hunks.len(), 0, "expected no hunks in: {text:?}");
        }
        // A separator line with trailing junk does not end ours; the hunk
        // completes at the real separator.
        let (hunks, b) =
            hunks_of("<<<<<<< A\nours\n======= not a separator\n=======\ntheirs\n>>>>>>> B\n");
        assert_eq!(hunks.len(), 1);
        assert_eq!(
            b.substring(hunks[0].ours.0, hunks[0].ours.1),
            "ours\n======= not a separator\n"
        );
    }

    #[test]
    fn nested_same_size_conflicts_surface_innermost_first() {
        // zdiff3 criss-cross shape: an inner conflict inside the outer ours.
        let text = "<<<<<<< A\nout-ours\n<<<<<<< inner\nin-ours\n=======\nin-theirs\n\
                    >>>>>>> inner\n=======\nout-theirs\n>>>>>>> B\n";
        let (hunks, mut b) = hunks_of(text);
        // The outer parse is rejected (ambiguous); the inner hunk surfaces.
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].ours_label, "inner");
        assert_eq!(b.substring(hunks[0].ours.0, hunks[0].ours.1), "in-ours\n");
        // The outer opener is reported as a stray, not silently dropped.
        assert_eq!(scan_with_strays(&mut b).1, 1);
        // Resolving the inner hunk makes the outer well-formed on re-scan.
        b.delete_region(hunks[0].start, hunks[0].end);
        b.goto_char(hunks[0].start);
        b.insert("in-resolved\n");
        let hunks = scan(&mut b);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].ours_label, "A");
        assert_eq!(
            b.substring(hunks[0].ours.0, hunks[0].ours.1),
            "out-ours\nin-resolved\n"
        );
        assert_eq!(scan_with_strays(&mut b).1, 0);
    }

    #[test]
    fn nested_different_size_markers_parse_as_outer_content() {
        // A 9-char inner marker block inside a 7-char hunk is plain content:
        // its separator/closer can't match the outer run length.
        let text = "<<<<<<< A\no1\n<<<<<<<<< X\nfixture\n=========\nfix2\n>>>>>>>>> Y\n\
                    =======\nt1\n>>>>>>> B\n";
        let (hunks, b) = hunks_of(text);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].ours_label, "A");
        assert!(
            b.substring(hunks[0].ours.0, hunks[0].ours.1)
                .contains("<<<<<<<<< X"),
            "the size-9 block stays inside ours"
        );
    }

    #[test]
    fn stray_openers_counted_for_malformed_runs() {
        // An unterminated conflict is no hunk, but its opener is not silent.
        let (hunks, mut b) = hunks_of("<<<<<<< A\nours\n=======\ntheirs, no closer\n");
        assert_eq!(hunks.len(), 0);
        assert_eq!(scan_with_strays(&mut b).1, 1);
        let out = render(&b, &hunks, 1);
        assert!(out.contains("no conflicts"), "got: {out}");
        assert!(
            out.contains("1 unparsed '<<<<<<<' marker line"),
            "got: {out}"
        );
    }

    #[test]
    fn scan_respects_narrowing() {
        let text = "<<<<<<< A\no\n=======\nt\n>>>>>>> B\n<<<<<<< A\no2\n=======\nt2\n>>>>>>> B\n";
        let mut b = Buffer::from_string("m", text);
        let all = scan(&mut b);
        assert_eq!(all.len(), 2);
        // Narrow to the second hunk only.
        b.narrow_to_region(all[1].start, all[1].end);
        let narrowed = scan(&mut b);
        assert_eq!(narrowed.len(), 1);
        assert_eq!(narrowed[0].start, all[1].start);
    }
}
