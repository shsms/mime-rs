//! Paragraph filling: the pure text reflow behind `fill-paragraph` /
//! `fill-region` and the `fill_text` tool.

use crate::syntax::{ProseKind, SexpRule};

/// Layout knobs for one fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fill {
    /// The last column a line may reach, prefix included (Emacs `fill-column`).
    pub column: usize,
    /// Two spaces after a sentence end when joining lines (Emacs
    /// `sentence-end-double-space`).
    pub double_space: bool,
}

/// Display width of `s`: chars, with a tab counting to the next multiple of
/// eight.
fn width(s: &str) -> usize {
    s.chars()
        .fold(0, |w, c| if c == '\t' { w + 8 - w % 8 } else { w + 1 })
}

/// Does `word` end a sentence: `.`, `?` or `!`, optionally followed by closing
/// punctuation (`)`, `]`, `}`, `"`, `'`), as Emacs's `sentence-end`.
fn ends_sentence(word: &str) -> bool {
    let core = word.trim_end_matches(['\'', '"', ')', ']', '}']);
    core.ends_with(['.', '?', '!'])
}

/// The words of `body` with the gap each one wants after it: two spaces after a
/// sentence end that the source marked with a line break or two spaces (Emacs's
/// `sentence-end-double-space`), one otherwise. A single space after a period
/// ("e.g. x") stays single, since the source did not treat it as a sentence
/// end.
fn words(body: &str, double_space: bool) -> Vec<(&str, usize)> {
    // A no-break space (and its narrow and figure forms) is part of its word;
    // every other whitespace separates words.
    let gap = |c: char| c.is_whitespace() && !matches!(c, '\u{a0}' | '\u{202f}' | '\u{2007}');
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        let start = rest.trim_start_matches(gap);
        if start.is_empty() {
            break;
        }
        let end = start.find(gap).unwrap_or(start.len());
        let (word, after) = start.split_at(end);
        let gap_len = after.len() - after.trim_start_matches(gap).len();
        let gap = &after[..gap_len];
        let wide = double_space
            && ends_sentence(word)
            && (gap.contains('\n') || gap.chars().filter(|c| *c == ' ').count() >= 2);
        out.push((word, if wide { 2 } else { 1 }));
        rest = after;
    }
    out
}

/// Reflow `body` — prose whose prefix is already stripped — into lines that fit
/// `opts.column` once `first` is put in front of the first line and `rest` in
/// front of every other. A word wider than the room left stands on its own
/// line, whole: a URL never breaks.
pub fn fill_paragraph(body: &str, first: &str, rest: &str, opts: &Fill) -> String {
    let mut out = String::new();
    let mut line = String::from(first);
    let mut line_w = width(first);
    let mut gap = 0;
    for (word, next_gap) in words(body, opts.double_space) {
        let word_w = width(word);
        if gap > 0 && line_w + gap + word_w > opts.column {
            out.push_str(&line);
            out.push('\n');
            line = String::from(rest);
            line_w = width(rest);
            gap = 0;
        }
        line.extend(std::iter::repeat_n(' ', gap));
        line.push_str(word);
        line_w += gap + word_w;
        gap = next_gap;
    }
    out.push_str(&line);
    out
}

/// The text that leads every line of a unit and is not prose: a comment marker
/// with its indentation, a docstring's opening quotes. `first` leads the first
/// line, `rest` every other one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Frame {
    pub first: String,
    pub rest: String,
}

impl Frame {
    pub fn new(first: &str, rest: &str) -> Frame {
        Frame {
            first: first.to_string(),
            rest: rest.to_string(),
        }
    }

    /// The same prefix on every line.
    pub fn uniform(prefix: &str) -> Frame {
        Frame::new(prefix, prefix)
    }

    /// This frame placed inside `outer`, for a block whose first line is output
    /// line `n` of the unit.
    fn under(&self, outer: &Frame, n: usize) -> Frame {
        Frame::new(
            &format!("{}{}", outer.prefix(n), self.first),
            &format!("{}{}", outer.rest, self.rest),
        )
    }

    /// The prefix leading output line `n` of the unit.
    fn prefix(&self, n: usize) -> &str {
        if n == 0 { &self.first } else { &self.rest }
    }

    /// `line` with `prefix` taken off: the marker must match exactly, then up
    /// to the prefix's own trailing whitespace is dropped, so text indented
    /// past the marker keeps its extra indent. A line that does not carry the
    /// marker is body as it stands.
    fn strip<'a>(prefix: &str, line: &'a str) -> &'a str {
        let core = prefix.trim_end();
        let Some(mut after) = line.strip_prefix(core) else {
            return line;
        };
        for _ in core.len()..prefix.len() {
            match after.strip_prefix([' ', '\t']) {
                Some(a) => after = a,
                None => break,
            }
        }
        after
    }
}

/// The Emacs `adaptive-fill` guess for a run of comment lines: the shared
/// indentation plus the shared run of marker punctuation (`//`, `///`, `//!`,
/// `#`, `;;`), and one space when the first line has one after it.  The marker
/// must begin with one of the language's comment `openers`, so a Markdown `#`
/// heading or `-` bullet is never taken for a comment marker. Lines that do not
/// share the first line's marker shrink it to what every line has; an empty
/// string means "no comment marker".
pub fn detect_frame(text: &str, openers: &[&str]) -> String {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let Some(line0) = lines.next() else {
        return String::new();
    };
    let marker_chars: Vec<char> = openers
        .iter()
        .flat_map(|o| o.chars())
        .chain(['!'])
        .collect();
    let (indent, after) = split_indent(line0);
    // A marker run stops at the first non-marker char; the run of the first
    // line seeds it and every later line can only shorten it.
    let run_of = |s: &str| -> usize {
        s.chars()
            .take_while(|c| marker_chars.contains(c))
            .map(char::len_utf8)
            .sum()
    };
    let mut marker = &after[..run_of(after)];
    let mut indent = indent;
    for line in lines {
        let (ind, aft) = split_indent(line);
        indent = common_prefix(indent, ind);
        marker = common_prefix(marker, &aft[..run_of(aft)]);
    }
    if !openers.iter().any(|o| marker.starts_with(o)) {
        return String::new();
    }
    // One space after the marker unless every line runs straight on from it
    // (`//a`), so a `/// a` + `// b` pair still frames as `// `.
    let bare = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .all(|l| !split_indent(l).1[marker.len()..].starts_with([' ', '\t']));
    format!("{indent}{marker}{}", if bare { "" } else { " " })
}

fn common_prefix<'a>(a: &'a str, b: &str) -> &'a str {
    let n = a
        .char_indices()
        .zip(b.chars())
        .take_while(|((_, x), y)| x == y)
        .last()
        .map(|((i, x), _)| i + x.len_utf8())
        .unwrap_or(0);
    &a[..n]
}

/// `line` as (leading whitespace, the rest).
fn split_indent(line: &str) -> (&str, &str) {
    let n = line.len() - line.trim_start_matches([' ', '\t']).len();
    line.split_at(n)
}

/// Length of a Markdown list marker (`- `, `* `, `+ `, `1. `, `1) `) with the
/// whitespace after it at the start of `s`, or `None`.
fn list_marker(s: &str) -> Option<usize> {
    let digits = s.chars().take_while(char::is_ascii_digit).count();
    let mark = if digits > 0 && digits <= 9 {
        let b = s.as_bytes();
        if matches!(b.get(digits), Some(b'.' | b')')) {
            digits + 1
        } else {
            return None;
        }
    } else if s.starts_with(['-', '*', '+']) {
        1
    } else {
        return None;
    };
    let after = &s[mark..];
    if !after.is_empty() && !after.starts_with([' ', '\t']) {
        return None;
    }
    Some(mark + (after.len() - after.trim_start_matches([' ', '\t']).len()))
}

fn is_fence(s: &str) -> bool {
    s.starts_with("```") || s.starts_with("~~~")
}

fn is_heading(s: &str) -> bool {
    let hashes = s.chars().take_while(|c| *c == '#').count();
    let spaced = s[hashes..].starts_with([' ', '\t']);
    ((1..=6).contains(&hashes) && spaced) || (hashes > 0 && s.len() == hashes)
}

/// A setext underline or thematic break: three or more of one of `-=*_`, spaces
/// allowed between.
fn is_rule(s: &str) -> bool {
    let mut chars = s.chars().filter(|c| *c != ' ');
    let Some(first) = chars.next() else {
        return false;
    };
    "-=*_".contains(first) && chars.clone().all(|c| c == first) && chars.count() >= 2
}

/// The hard line break `line` ends with — a backslash, or two spaces — as the
/// text to put back after filling.
fn hard_break(line: &str) -> Option<&'static str> {
    if line.ends_with('\\') {
        Some("\\")
    } else if line.ends_with("  ") {
        Some("  ")
    } else {
        None
    }
}

/// A run of spaces as wide as `s`: the hanging indent under a list marker.
fn spaces_as_wide_as(s: &str) -> String {
    " ".repeat(width(s))
}

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    Blank,
    /// Copied through unchanged; the text says what it is, for a refusal.
    Verbatim(&'static str),
    /// Prose behind its own frame: the indent, a list marker with the hanging
    /// indent under it.
    Prose(Frame),
    /// A block quote, behind its `>` marker: its lines, less one `>` each, are
    /// a body of their own, split by the same rules.
    Quote(Frame),
}

struct Block {
    start: usize,
    end: usize,
    kind: Kind,
}

/// Cut `body` (frame-stripped lines) into blocks by Markdown's block rules:
/// blank lines separate; fences, headings, tables, rules and indented code are
/// verbatim; a list item or block quote is its own prose block with its marker
/// as the frame; anything else is a paragraph.
fn split_blocks(body: &[&str]) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut i = 0;
    let starts_block = |s: &str| {
        is_fence(s)
            || is_heading(s)
            || s.starts_with('|')
            || is_rule(s)
            || list_marker(s).is_some()
            || s.starts_with('>')
    };
    while i < body.len() {
        let line = body[i];
        let (indent, rest) = split_indent(line);
        let start = i;
        let kind = if rest.is_empty() {
            i += 1;
            Kind::Blank
        } else if is_fence(rest) {
            let fence = &rest[..3];
            i += 1;
            while i < body.len() {
                let done = split_indent(body[i]).1.starts_with(fence);
                i += 1;
                if done {
                    break;
                }
            }
            Kind::Verbatim("inside a fenced code block")
        } else if is_heading(rest) {
            i += 1;
            Kind::Verbatim("a heading")
        } else if is_rule(rest) {
            i += 1;
            Kind::Verbatim("a rule line")
        } else if rest.starts_with('|') {
            while i < body.len() && split_indent(body[i]).1.starts_with('|') {
                i += 1;
            }
            Kind::Verbatim("a table")
        } else if width(indent) >= 4 {
            while i < body.len() && width(split_indent(body[i]).0) >= 4 {
                i += 1;
            }
            Kind::Verbatim("an indented code block")
        } else if let Some(after) = rest.strip_prefix('>') {
            // The quote runs over the lines that start with `>` (up to three
            // spaces in); what follows each line's own `>` is a body of its
            // own, so nested quotes, lists and fences inside it follow the same
            // rules. The marker is the first line's `>` and the one space (or
            // tab) after it, which `unquote` then takes off every line along
            // with the line's own indent; text glued to the `>` makes a bare
            // marker, and then no line loses a space. A bare `>` first line
            // frames as `> `.
            let space = after.chars().next().filter(|c| matches!(c, ' ' | '\t'));
            let marker = match space {
                Some(c) => format!("{indent}>{c}"),
                None if after.is_empty() => format!("{indent}> "),
                None => format!("{indent}>"),
            };
            i += 1;
            while i < body.len() && {
                let (ind, r) = split_indent(body[i]);
                width(ind) < 4 && r.starts_with('>')
            } {
                i += 1;
            }
            Kind::Quote(Frame::uniform(&marker))
        } else if let Some(n) = list_marker(rest) {
            let first = format!("{indent}{}", &rest[..n]);
            i += 1;
            while i < body.len() {
                let (ind, r) = split_indent(body[i]);
                // A block starter indented past the marker is still the item's
                // text.
                if r.is_empty() || (starts_block(r) && width(ind) < 4) {
                    break;
                }
                i += 1;
            }
            // The hanging indent is the width of the marker as it is put back —
            // a bare one without its trailing whitespace — plus one space when
            // the marker carries none.
            let core = if rest[n..].is_empty() {
                first.trim_end()
            } else {
                first.as_str()
            };
            let mut hang = spaces_as_wide_as(core);
            if !core.ends_with([' ', '\t']) {
                hang.push(' ');
            }
            Kind::Prose(Frame::new(&first, &hang))
        } else {
            i += 1;
            while i < body.len() {
                let r = split_indent(body[i]).1;
                if r.is_empty() || starts_block(r) {
                    break;
                }
                i += 1;
            }
            Kind::Prose(Frame::uniform(indent))
        };
        blocks.push(Block {
            start,
            end: i,
            kind,
        });
    }
    blocks
}

/// The prose content of one body line of a block: its frame prefix removed, a
/// trailing hard break kept aside.
fn content<'a>(prefix: &str, line: &'a str) -> (&'a str, Option<&'static str>) {
    let s = Frame::strip(prefix, line).trim_start();
    let brk = hard_break(s);
    let s = match brk {
        Some(b) => s[..s.len() - b.len()].trim_end(),
        None => s.trim_end(),
    };
    (s, brk)
}

/// Fill one prose block: its lines, in segments a hard break ends, each
/// stripped of the block's own frame and refilled behind `out_frame`, the
/// unit's frame and the block's composed.
fn fill_block(
    lines: &[&str],
    block: &Frame,
    out_frame: &Frame,
    opts: &Fill,
    out: &mut Vec<String>,
) {
    let (first, rest) = (out_frame.first.as_str(), out_frame.rest.as_str());
    let mut segment = String::new();
    let mut lead = first;
    let before = out.len();
    let flush = |segment: &mut String, lead: &str, brk: Option<&str>, out: &mut Vec<String>| {
        let mut filled = fill_paragraph(segment, lead, rest, opts);
        if let Some(b) = brk {
            filled.push_str(b);
        }
        out.extend(filled.split('\n').map(str::to_string));
        segment.clear();
    };
    for (i, line) in lines.iter().enumerate() {
        let (text, brk) = content(block.prefix(i), line);
        // A bare marker with text on the lines below keeps its own line.
        if i == 0 && text.is_empty() && brk.is_none() && lines.len() > 1 {
            out.push(first.trim_end().to_string());
            lead = rest;
            continue;
        }
        if !segment.is_empty() {
            segment.push('\n');
        }
        segment.push_str(text);
        if brk.is_some() {
            flush(&mut segment, lead, brk, out);
            lead = rest;
        }
    }
    if !segment.is_empty() {
        flush(&mut segment, lead, None, out);
    } else if out.len() == before {
        // A block with no words (a bare list marker or `>`) keeps its line.
        out.push(first.trim_end().to_string());
    }
}

/// Refill the prose of one unit — a comment run, a docstring, a Markdown
/// paragraph — whose every line `frame` leads. Blocks inside it follow
/// Markdown: blank lines separate paragraphs, list items hang, quotes keep
/// their `>`, and fenced code, headings, tables, rules and indented code pass
/// through untouched. `only_line` (0-based, within the unit) refills just the
/// block holding that line — the one after it when the line is blank — and
/// errors when that line is not prose; `None` refills every block.
pub fn fill_unit(
    text: &str,
    frame: &Frame,
    only_line: Option<usize>,
    opts: &Fill,
) -> Result<String, String> {
    let (text, crlf) = lf_only(text);
    fill_unit_lf(&text, frame, only_line, opts).map(|out| with_crlf(out, crlf))
}

/// [`fill_unit`] on text whose line ends are `\n`.
fn fill_unit_lf(
    text: &str,
    frame: &Frame,
    only_line: Option<usize>,
    opts: &Fill,
) -> Result<String, String> {
    let raw: Vec<&str> = text
        .strip_suffix('\n')
        .unwrap_or(text)
        .split('\n')
        .collect();
    // A line's own `\r` (a mixed unit) is a line end, not body.
    let body: Vec<&str> = raw
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let b = Frame::strip(frame.prefix(i), l);
            b.strip_suffix('\r').unwrap_or(b)
        })
        .collect();
    let out = fill_body(&raw, &body, frame, only_line, opts, 0)?;
    let mut joined = out.join("\n");
    if text.ends_with('\n') {
        joined.push('\n');
    }
    Ok(joined)
}

/// The output lines for `body` — `raw` with `frame` stripped — every one led by
/// `frame`. A quote block recurses: its lines, less one `>` each, are a body
/// under the quote's marker composed onto `frame`.
fn fill_body(
    raw: &[&str],
    body: &[&str],
    frame: &Frame,
    only_line: Option<usize>,
    opts: &Fill,
    depth: usize,
) -> Result<Vec<String>, String> {
    let blocks = split_blocks(body);
    let target = match only_line {
        None => None,
        Some(t) => {
            let mut at = blocks
                .iter()
                .position(|b| b.start <= t && t < b.end)
                .unwrap_or(blocks.len());
            while at < blocks.len() && blocks[at].kind == Kind::Blank {
                at += 1;
            }
            match blocks.get(at) {
                None => return Err("no paragraph to fill after that line".to_string()),
                Some(Block {
                    kind: Kind::Verbatim(what),
                    ..
                }) => return Err(format!("that line is {what}, not prose")),
                Some(_) => Some(at),
            }
        }
    };
    let mut out: Vec<String> = Vec::new();
    for (bi, block) in blocks.iter().enumerate() {
        let lines = &body[block.start..block.end];
        let raw_lines = &raw[block.start..block.end];
        match &block.kind {
            Kind::Prose(block) if target.is_none_or(|t| t == bi) => {
                // The unit's frame is part of every line's width: the block
                // fills against the composed prefixes.
                let composed = block.under(frame, out.len());
                fill_block(lines, block, &composed, opts, &mut out);
            }
            // Past the depth cap a quote is left as it is, like any block the
            // filler does not touch; asking for it by line is an error.
            Kind::Quote(_) if depth >= MAX_QUOTE_DEPTH && target == Some(bi) => {
                return Err("block quotes nested too deep to fill".to_string());
            }
            Kind::Quote(marker) if depth < MAX_QUOTE_DEPTH && target.is_none_or(|t| t == bi) => {
                let spaced = marker.first.ends_with([' ', '\t']);
                let inner: Vec<&str> = lines.iter().map(|l| unquote(l, spaced)).collect();
                let composed = marker.under(frame, out.len());
                // A target line before the block (a blank line) means its first
                // block.
                let inner_line = only_line.map(|t| t.saturating_sub(block.start));
                out.extend(fill_body(
                    raw_lines,
                    &inner,
                    &composed,
                    inner_line,
                    opts,
                    depth + 1,
                )?);
            }
            // A blank line is its frame prefix less trailing spaces: plain
            // indent goes entirely, a marker keeps the tab that holds it off
            // the text, so the next fill re-derives the same marker (while the
            // tab survives). One outside the one block being filled is left as
            // it is.
            Kind::Blank if target.is_none() => {
                let p = frame.prefix(out.len());
                let kept = if p.trim_end().is_empty() {
                    ""
                } else {
                    p.trim_end_matches(' ')
                };
                out.push(kept.to_string());
            }
            Kind::Blank | Kind::Verbatim(_) | Kind::Prose(_) | Kind::Quote(_) => {
                out.extend(raw_lines.iter().map(|l| l.to_string()));
            }
        }
    }
    Ok(out)
}

/// Quotes nested deeper than this are not filled: each level is a recursion of
/// [`fill_body`].
const MAX_QUOTE_DEPTH: usize = 64;

/// A quote line less its indent, its own `>` and, when the block's marker has
/// one, the one space (or tab) after it.
fn unquote(line: &str, spaced: bool) -> &str {
    let r = split_indent(line).1;
    let r = r.strip_prefix('>').unwrap_or(r);
    if spaced {
        r.strip_prefix([' ', '\t']).unwrap_or(r)
    } else {
        r
    }
}

/// `text` with `\r\n` line ends made `\n`, and whether every line end was one.
fn lf_only(text: &str) -> (std::borrow::Cow<'_, str>, bool) {
    // Only a unit whose every line end is `\r\n` is a CRLF unit. In a mixed
    // one, verbatim lines keep their ending and refilled lines end with `\n`.
    let crlf = text.contains("\r\n") && text.matches('\n').count() == text.matches("\r\n").count();
    if crlf {
        (text.replace("\r\n", "\n").into(), true)
    } else {
        (text.into(), false)
    }
}

/// The line ends [`lf_only`] took out, put back.
fn with_crlf(text: String, crlf: bool) -> String {
    if crlf {
        text.replace('\n', "\r\n")
    } else {
        text
    }
}

/// The shared leading whitespace of the non-blank `lines`.
fn common_indent<'a>(lines: &[&'a str]) -> &'a str {
    let mut it = lines.iter().filter(|l| !l.trim().is_empty());
    let Some(first) = it.next() else {
        return "";
    };
    it.fold(split_indent(first).0, |acc, l| {
        common_prefix(acc, split_indent(l).0)
    })
}

/// Where a unit's closer (`*/`, `-->`, `"""`) sits, so it can be put back after
/// the fill.
enum Closer<'a> {
    /// On its own line, copied through as it stands.
    Line(&'a str),
    /// At the end of the last text line, after `sep` (its whitespace).
    Inline { sep: &'a str, closer: &'a str },
}

/// Refill a prose unit of `kind` — the whole-line text
/// [`crate::syntax::Syntax::prose_unit_at`] found — framing it by what it is: a
/// comment run by its adaptive marker, a block comment or docstring by its
/// opener on the first line and the margin its later lines share, with the
/// closer kept where it was; a Markdown paragraph has no frame.
pub fn fill_prose(
    text: &str,
    kind: ProseKind,
    rule: &SexpRule,
    only_line: Option<usize>,
    opts: &Fill,
) -> Result<String, String> {
    let (text, crlf) = lf_only(text);
    fill_prose_lf(&text, kind, rule, only_line, opts).map(|out| with_crlf(out, crlf))
}

/// [`fill_prose`] on text whose line ends are `\n`.
fn fill_prose_lf(
    text: &str,
    kind: ProseKind,
    rule: &SexpRule,
    only_line: Option<usize>,
    opts: &Fill,
) -> Result<String, String> {
    match kind {
        ProseKind::Paragraph => return fill_unit_lf(text, &Frame::default(), only_line, opts),
        ProseKind::LineComments => {
            let frame = Frame::uniform(&detect_frame(text, rule.line_comments));
            return fill_unit_lf(text, &frame, only_line, opts);
        }
        ProseKind::BlockComment | ProseKind::TripleString => {}
    }
    let had_nl = text.ends_with('\n');
    let body = text.strip_suffix('\n').unwrap_or(text);
    let mut lines: Vec<&str> = body.split('\n').collect();
    let (indent, after) = split_indent(lines[0]);
    let (opener, closer) = if kind == ProseKind::BlockComment {
        rule.block_comments
            .iter()
            .find(|(o, _)| after.starts_with(o))
            .map(|(o, c)| (*o, *c))
            .ok_or_else(|| "not a block comment".to_string())?
    } else {
        // `r"""` / `b'''`: the string prefix letters lead the quotes.
        let k = after.chars().take_while(char::is_ascii_alphabetic).count();
        match after.get(k..k + 3) {
            Some(q @ ("\"\"\"" | "'''")) => (&after[..k + 3], q),
            _ => return Err("not a triple-quoted string".to_string()),
        }
    };
    let ws = &after[opener.len()..];
    let ws = &ws[..ws.len() - ws.trim_start_matches([' ', '\t']).len()];
    let first = format!("{indent}{opener}{ws}");
    // Detach the closer: alone on the last line it is copied through; ending
    // the last text line it goes back after the fill.
    let last = *lines.last().expect("split yields one line");
    let closer_at = if lines.len() > 1 && last.trim() == closer {
        lines.pop();
        Some(Closer::Line(last))
    } else if let Some(stripped) = last.trim_end().strip_suffix(closer) {
        let kept = stripped.trim_end();
        let n = lines.len() - 1;
        lines[n] = kept;
        Some(Closer::Inline {
            sep: &stripped[kept.len()..],
            closer,
        })
    } else {
        None
    };
    let rest = if lines.len() > 1 {
        let tail = lines[1..].join("\n");
        let margin = if kind == ProseKind::BlockComment {
            detect_frame(&tail, &["*"])
        } else {
            String::new()
        };
        if margin.is_empty() {
            common_indent(&lines[1..]).to_string()
        } else {
            margin
        }
    } else if kind == ProseKind::BlockComment {
        spaces_as_wide_as(&first)
    } else {
        indent.to_string()
    };
    // A line on the detached closer fills the paragraph before it.
    let only_line = only_line.map(|l| l.min(lines.len() - 1));
    let frame = Frame::new(&first, &rest);
    let mut out = fill_unit_lf(&lines.join("\n"), &frame, only_line, opts)?;
    match closer_at {
        Some(Closer::Line(l)) => {
            out.push('\n');
            out.push_str(l);
        }
        Some(Closer::Inline { sep, closer }) => {
            out.push_str(sep);
            out.push_str(closer);
        }
        None => {}
    }
    if had_nl {
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(column: usize) -> Fill {
        Fill {
            column,
            double_space: true,
        }
    }

    #[test]
    fn wraps_words_at_the_column() {
        let out = fill_paragraph("aaa bbb ccc ddd eee", "", "", &opts(11));
        assert_eq!(out, "aaa bbb ccc\nddd eee");
    }
    #[test]
    fn a_line_may_reach_the_column_exactly() {
        assert_eq!(fill_paragraph("aaa bbb", "", "", &opts(7)), "aaa bbb");
    }

    #[test]
    fn prefixes_count_against_the_column() {
        let out = fill_paragraph("aaa bbb ccc", "- ", "  ", &opts(9));
        assert_eq!(
            out,
            "- aaa bbb
  ccc"
        );
    }

    #[test]
    fn a_word_wider_than_the_room_stands_alone() {
        let out = fill_paragraph(
            "see https://example.com/very/long/path now",
            "",
            "",
            &opts(10),
        );
        assert_eq!(out, "see\nhttps://example.com/very/long/path\nnow");
    }

    #[test]
    fn sentence_ends_get_two_spaces_when_double_space_is_on() {
        // A line break or two spaces after `.` marks a sentence end; a single
        // space ("e.g. x") does not.
        let out = fill_paragraph("One.\nTwo.  Three, e.g. four", "", "", &opts(80));
        assert_eq!(out, "One.  Two.  Three, e.g. four");
    }

    #[test]
    fn sentence_ends_get_one_space_when_double_space_is_off() {
        let opts = Fill {
            column: 80,
            double_space: false,
        };
        let out = fill_paragraph("One.\nTwo.  Three", "", "", &opts);
        assert_eq!(out, "One. Two. Three");
    }

    #[test]
    fn closing_punctuation_after_a_sentence_end_still_counts() {
        let out = fill_paragraph("(One.)\nTwo?\nThree!\nfour", "", "", &opts(80));
        assert_eq!(out, "(One.)  Two?  Three!  four");
    }

    #[test]
    fn a_tab_in_the_prefix_is_eight_wide() {
        let out = fill_paragraph("aa bb", "\t", "\t", &opts(12));
        assert_eq!(out, "\taa\n\tbb");
    }

    // ---- fill_unit: blocks inside one framed unit ----

    fn unit(text: &str, first: &str, rest: &str, only: Option<usize>, column: usize) -> String {
        fill_unit(text, &Frame::new(first, rest), only, &opts(column)).unwrap()
    }

    #[test]
    fn fills_every_paragraph_of_an_unframed_body() {
        let out = unit("aa bb\ncc\n\ndd ee\nff\n", "", "", None, 80);
        assert_eq!(out, "aa bb cc\n\ndd ee ff\n");
    }

    #[test]
    fn only_line_fills_just_the_block_holding_that_line() {
        let out = unit("aa\nbb\n\ncc\ndd\n", "", "", Some(3), 80);
        assert_eq!(out, "aa\nbb\n\ncc dd\n");
    }

    #[test]
    fn only_line_on_a_blank_line_fills_the_block_after_it() {
        let out = unit("aa\nbb\n\ncc\ndd\n", "", "", Some(2), 80);
        assert_eq!(out, "aa\nbb\n\ncc dd\n");
    }

    #[test]
    fn the_frame_is_stripped_and_put_back() {
        let out = unit(
            "/// Summary that is\n/// short.\n///\n/// Body one\n/// two\n",
            "/// ",
            "/// ",
            None,
            80,
        );
        assert_eq!(out, "/// Summary that is short.\n///\n/// Body one two\n");
    }

    #[test]
    fn a_blank_framed_line_keeps_no_trailing_space() {
        let out = unit("// a\n// \n// b\n", "// ", "// ", None, 80);
        assert_eq!(out, "// a\n//\n// b\n");
    }

    #[test]
    fn a_list_item_gets_a_hanging_indent() {
        let out = unit("- aaa bbb ccc\n  ddd\n- eee fff ggg\n", "", "", None, 9);
        assert_eq!(out, "- aaa bbb\n  ccc ddd\n- eee fff\n  ggg\n");
    }

    #[test]
    fn a_numbered_item_gets_a_hanging_indent() {
        let out = unit("1. aaa bbb\nccc\n", "", "", None, 10);
        assert_eq!(out, "1. aaa bbb\n   ccc\n");
    }

    #[test]
    fn a_block_quote_keeps_its_marker_on_every_line() {
        let out = unit("> aaa bbb\n> ccc\n", "", "", None, 80);
        assert_eq!(out, "> aaa bbb ccc\n");
    }

    #[test]
    fn fenced_code_is_left_verbatim() {
        let text = "aa\nbb\n\n```\nlet x   = 1;\nlet y = 2;\n```\n\ncc\ndd\n";
        let out = unit(text, "", "", None, 80);
        assert_eq!(
            out,
            "aa bb\n\n```\nlet x   = 1;\nlet y = 2;\n```\n\ncc dd\n"
        );
    }

    #[test]
    fn fenced_code_inside_a_doc_comment_is_left_verbatim() {
        let text = "/// Example:\n/// ```\n/// let x = 1;\n/// let y = 2;\n/// ```\n";
        let out = unit(text, "/// ", "/// ", None, 80);
        assert_eq!(out, text);
    }

    #[test]
    fn headings_and_tables_are_left_verbatim() {
        let text = "# Title\n\n| a | b |\n|---|---|\n\naa\nbb\n";
        let out = unit(text, "", "", None, 80);
        assert_eq!(out, "# Title\n\n| a | b |\n|---|---|\n\naa bb\n");
    }

    #[test]
    fn indented_code_after_a_blank_line_is_left_verbatim() {
        let text = "aa\nbb\n\n    code  here\n    more\n\ncc\ndd\n";
        let out = unit(text, "", "", None, 80);
        assert_eq!(out, "aa bb\n\n    code  here\n    more\n\ncc dd\n");
    }

    #[test]
    fn a_bare_quote_line_separates_quoted_paragraphs() {
        let out = unit("> aaa\n> bbb\n>\n> ccc\n", "", "", None, 80);
        assert_eq!(out, "> aaa bbb\n>\n> ccc\n");
    }

    #[test]
    fn a_nested_quote_keeps_its_markers_and_its_blank_line() {
        let out = unit("> > aaa\n> > bbb\n> > \n> > ccc\n", "", "", None, 80);
        assert_eq!(out, "> > aaa bbb\n> >\n> > ccc\n");
        // A bare marker line with trailing space is still bare.
        assert_eq!(
            unit("> aaa\n> \n> bbb\n", "", "", None, 80),
            "> aaa\n>\n> bbb\n"
        );
    }

    #[test]
    fn a_quote_continues_over_differently_spaced_markers() {
        assert_eq!(
            unit("> aaa\n>  bbb\n > ccc\n", "", "", None, 80),
            "> aaa bbb ccc\n"
        );
        // A deeper line is a quote inside the quote.
        assert_eq!(
            unit("> aaa\n> > bbb\n", "", "", None, 80),
            "> aaa\n> > bbb\n"
        );
    }

    #[test]
    fn blocks_inside_a_quote_follow_the_block_rules() {
        // A quoted list keeps its items and hanging indents.
        let text =
            "> - open the file and make sure it is\n>   actually there\n> - close it again\n";
        assert_eq!(
            unit(text, "", "", None, 80),
            "> - open the file and make sure it is actually there\n> - close it again\n"
        );
        // A quoted fence and a quoted indented line stay verbatim.
        let text = "> Note:\n> ```\n> let x   = 1;\n> ```\n> Then\n> check.\n";
        assert_eq!(
            unit(text, "", "", None, 80),
            "> Note:\n> ```\n> let x   = 1;\n> ```\n> Then check.\n"
        );
        // An indented line right after a paragraph line continues it
        // (CommonMark: indented code cannot interrupt a paragraph).
        assert_eq!(unit("> aa\n>     bb\n", "", "", None, 80), "> aa bb\n");
        assert_eq!(
            unit("> aa\n>\n>     bb\n", "", "", None, 80),
            "> aa\n>\n>     bb\n"
        );
        // Inside a doc comment too.
        let text = "/// > - a b\n/// >   c\n/// > - d\n";
        assert_eq!(
            unit(text, "/// ", "/// ", None, 80),
            "/// > - a b c\n/// > - d\n"
        );
    }

    #[test]
    fn only_line_reaches_into_a_quote() {
        let text = "> aa\n> bb\n>\n> cc\n> dd\n";
        assert_eq!(unit(text, "", "", Some(3), 80), "> aa\n> bb\n>\n> cc dd\n");
        // From a blank line before the quote: its first block.
        assert_eq!(
            unit("aa\n\n> bb\n> cc\n", "", "", Some(1), 80),
            "aa\n\n> bb cc\n"
        );
        // A line inside a quote that does not start the unit.
        assert_eq!(
            unit("aa\n\n> bb\n>\n> cc\n> dd\n", "", "", Some(4), 80),
            "aa\n\n> bb\n>\n> cc dd\n"
        );
    }

    #[test]
    fn a_quote_fill_is_idempotent_and_keeps_its_spacing() {
        assert_eq!(unit(">  aa\n>  bb\n", "", "", None, 80), ">  aa bb\n");
        assert_eq!(unit(">\n> aa\n> bb\n", "", "", None, 80), ">\n> aa bb\n");
        assert_eq!(unit(">\taa\n>\tbb\n", "", "", None, 80), ">\taa bb\n");
        assert_eq!(
            unit(">\t\n>\taa\n>\tbb\n", "", "", None, 80),
            ">\t\n>\taa bb\n"
        );
        assert_eq!(unit(">\n>\taa\n>\tbb\n", "", "", None, 80), ">\n> aa bb\n");
        // Text glued to the first `>` keeps every line's spacing.
        assert_eq!(
            unit(">aa\n>\n>  bb cc\n", "", "", None, 80),
            ">aa\n>\n>  bb cc\n"
        );
        // Four columns of indent before `>` is code, not the quote.
        assert_eq!(
            unit("> aa\n\n    > bb\n", "", "", None, 80),
            "> aa\n\n    > bb\n"
        );
    }

    #[test]
    fn a_quote_below_the_first_line_of_a_docstring_frame() {
        // The frame's first line is the quotes; the quote block sits under the
        // rest prefix.
        let text = "    \"\"\"Summary.\n\n    > aa\n    > bb\n";
        assert_eq!(
            unit(text, "    \"\"\"", "    ", None, 80),
            "    \"\"\"Summary.\n\n    > aa bb\n"
        );
    }

    #[test]
    fn quotes_nested_too_deep_are_left_alone_or_refused_by_line() {
        let ok = format!("{}x y\n{}z\n", "> ".repeat(64), "> ".repeat(64));
        assert_eq!(
            unit(&ok, "", "", None, 400),
            format!("{}x y z\n", "> ".repeat(64))
        );
        let deep = format!("aa\nbb\n\n{0}x\n{0}y\n", "> ".repeat(65));
        assert_eq!(
            unit(&deep, "", "", None, 400),
            format!("aa bb\n\n{0}x\n{0}y\n", "> ".repeat(65))
        );
        // A paragraph beside a too-deep quote still fills by line.
        let beside = format!("{0}aa\n{0}cc\n{0}\n{0}> deep\n", "> ".repeat(64));
        assert_eq!(
            unit(&beside, "", "", Some(0), 400),
            format!("{0}aa cc\n{0}\n{0}> deep\n", "> ".repeat(64))
        );
        let err = fill_unit(&deep, &Frame::default(), Some(3), &opts(80)).unwrap_err();
        assert!(err.contains("too deep"), "{err}");
    }

    #[test]
    fn a_blank_line_in_a_tab_indented_frame_is_empty() {
        let text = "\t\t\"\"\"aa\n\n\t\tbb\n\t\t\"\"\"\n";
        assert_eq!(
            prose(text, ProseKind::TripleString, Lang::Python, None, 80),
            text
        );
        assert_eq!(unit("aa\n\nbb\n", "\t", "\t", None, 80), "\taa\n\n\tbb\n");
    }

    #[test]
    fn a_blank_line_outside_the_filled_block_keeps_its_ending() {
        assert_eq!(
            unit("aa\nbb\n\r\ncc\r\ndd\n", "", "", Some(0), 80),
            "aa bb\n\r\ncc\r\ndd\n"
        );
    }

    #[test]
    fn a_marker_with_no_text_keeps_its_line() {
        assert_eq!(unit("- \n- a\n", "", "", None, 80), "-\n- a\n");
        // With text on the lines below it, the marker still stands alone, and
        // its trailing whitespace does not widen the hanging indent.
        assert_eq!(unit("aa\n-\n b\n c\n", "", "", None, 40), "aa\n-\n  b c\n");
        assert_eq!(unit("- \n b\n", "", "", None, 40), "-\n  b\n");
        assert_eq!(unit("-\t\n b\n", "", "", None, 40), "-\n  b\n");
        // A marker with text keeps the width of its own trailing whitespace.
        assert_eq!(
            unit("*\taa bb cc\n", "", "", None, 12),
            "*\taa\n        bb\n        cc\n"
        );
        // A first line that is only a hard break is not a bare marker.
        assert_eq!(unit("\\\nb\n", "/// ", "/// ", None, 40), "/// \\\n/// b\n");
        assert_eq!(
            unit("/// Items:\n/// -\n/// - a\n", "/// ", "/// ", None, 80),
            "/// Items:\n/// -\n/// - a\n"
        );
    }

    #[test]
    fn crlf_line_ends_survive() {
        assert_eq!(unit("aa\r\nbb\r\n", "", "", None, 80), "aa bb\r\n");
        let text = "// aaa\r\n//\r\n// bbb\r\n";
        assert_eq!(unit(text, "// ", "// ", None, 80), text);
    }

    #[test]
    fn mixed_line_ends_are_not_made_uniform() {
        // One CRLF line among LF ones: the refilled text ends with `\n`, and a
        // verbatim line keeps its own ending.
        let text = "aa\r\nbb\n\n```\nx\r\n```\n";
        assert_eq!(unit(text, "", "", None, 80), "aa bb\n\n```\nx\r\n```\n");
        // A `\r`-ended blank line still separates paragraphs.
        let text = "// aaa\n// bbb\r\n//\r\n// ccc\n";
        assert_eq!(
            unit(text, "// ", "// ", None, 80),
            "// aaa bbb\n//\n// ccc\n"
        );
    }

    #[test]
    fn other_unicode_spaces_still_separate_words() {
        assert_eq!(
            fill_paragraph("aaa\u{3000}bbb\u{2003}ccc", "", "", &opts(7)),
            "aaa bbb\nccc"
        );
    }

    #[test]
    fn a_no_break_space_stays_inside_its_word() {
        assert_eq!(
            fill_paragraph("10\u{a0}kg of\napples", "", "", &opts(80)),
            "10\u{a0}kg of apples"
        );
        assert_eq!(
            fill_paragraph("1\u{202f}000\u{2007}kg", "", "", &opts(3)),
            "1\u{202f}000\u{2007}kg"
        );
    }

    #[test]
    fn a_hard_line_break_is_kept() {
        let out = unit("aa  \nbb\ncc\\\ndd\n", "", "", None, 80);
        assert_eq!(out, "aa  \nbb cc\\\ndd\n");
    }

    #[test]
    fn an_indented_paragraph_keeps_its_indent() {
        let out = unit("  aa\n  bb\n", "", "", None, 80);
        assert_eq!(out, "  aa bb\n");
    }

    #[test]
    fn only_line_inside_a_fence_is_refused() {
        let text = "```\ncode\n```\n";
        let err = fill_unit(text, &Frame::new("", ""), Some(1), &opts(80)).unwrap_err();
        assert!(err.contains("fenced code"), "{err}");
    }

    #[test]
    fn a_unit_without_a_trailing_newline_stays_without() {
        assert_eq!(unit("aa\nbb", "", "", None, 80), "aa bb");
    }

    #[test]
    fn first_and_rest_frames_differ() {
        // A docstring: the opening quotes lead the first line only.
        let out = unit(
            "    \"\"\"Summary line\n    that wraps.\n",
            "    \"\"\"",
            "    ",
            None,
            80,
        );
        assert_eq!(out, "    \"\"\"Summary line that wraps.\n");
    }

    // ---- detect_frame: the adaptive comment prefix ----

    #[test]
    fn detects_a_line_comment_prefix_with_its_indent() {
        assert_eq!(detect_frame("  // a\n  // b\n", &["//"]), "  // ");
    }

    #[test]
    fn detects_doc_comment_markers() {
        assert_eq!(detect_frame("/// a\n/// b\n", &["//"]), "/// ");
        assert_eq!(detect_frame("//! a\n//! b\n", &["//"]), "//! ");
        assert_eq!(detect_frame("# a\n# b\n", &["#"]), "# ");
        assert_eq!(detect_frame(";; a\n;; b\n", &[";"]), ";; ");
    }

    #[test]
    fn a_marker_not_shared_by_every_line_shrinks_to_what_is() {
        assert_eq!(detect_frame("/// a\n// b\n", &["//"]), "// ");
    }

    #[test]
    fn a_marker_must_start_with_an_opener() {
        // Markdown: `#` and `-` are structure, not comment markers.
        assert_eq!(detect_frame("# a\n# b\n", &["<!--"]), "");
        assert_eq!(detect_frame("- a\n- b\n", &["<!--"]), "");
    }

    #[test]
    fn a_marker_with_no_space_after_it_stays_bare() {
        assert_eq!(detect_frame("//a\n//b\n", &["//"]), "//");
    }

    // ---- fill_prose: framing by unit kind ----

    use crate::syntax::{Lang, ProseKind};

    fn prose(
        text: &str,
        kind: ProseKind,
        lang: Lang,
        only: Option<usize>,
        column: usize,
    ) -> String {
        fill_prose(text, kind, lang.sexp_rule(), only, &opts(column)).unwrap()
    }

    #[test]
    fn a_doc_comment_run_keeps_its_marker_and_indent() {
        let out = prose(
            "    /// aaa bbb\n    /// ccc\n",
            ProseKind::LineComments,
            Lang::Rust,
            None,
            80,
        );
        assert_eq!(out, "    /// aaa bbb ccc\n");
    }

    #[test]
    fn only_line_in_a_comment_run_fills_one_paragraph() {
        let text = "// aaa\n// bbb\n//\n// ccc\n// ddd\n";
        let out = prose(text, ProseKind::LineComments, Lang::Rust, Some(4), 80);
        assert_eq!(out, "// aaa\n// bbb\n//\n// ccc ddd\n");
    }

    #[test]
    fn a_docstring_keeps_its_closing_quotes_on_the_text() {
        let text = "    \"\"\"Summary that\n    wraps here.\"\"\"\n";
        let out = prose(text, ProseKind::TripleString, Lang::Python, None, 80);
        assert_eq!(out, "    \"\"\"Summary that wraps here.\"\"\"\n");
    }

    #[test]
    fn a_docstring_closer_on_its_own_line_stays_there() {
        let text = "    \"\"\"Doc\n    more\n    \"\"\"\n";
        let out = prose(text, ProseKind::TripleString, Lang::Python, None, 80);
        assert_eq!(out, "    \"\"\"Doc more\n    \"\"\"\n");
    }

    #[test]
    fn a_docstring_opener_on_its_own_line_stays_there() {
        let text = "    \"\"\"\n    aaa\n    bbb\n    \"\"\"\n";
        let out = prose(text, ProseKind::TripleString, Lang::Python, None, 80);
        assert_eq!(out, "    \"\"\"\n    aaa bbb\n    \"\"\"\n");
    }

    #[test]
    fn a_block_comment_keeps_its_star_margin_and_closer() {
        let text = "  /* aaa\n   * bbb\n   */\n";
        let out = prose(text, ProseKind::BlockComment, Lang::Rust, None, 80);
        assert_eq!(out, "  /* aaa bbb\n   */\n");
        let out = prose(
            "  /* aaa bbb ccc\n   * ddd\n   */\n",
            ProseKind::BlockComment,
            Lang::Rust,
            None,
            12,
        );
        assert_eq!(out, "  /* aaa bbb\n   * ccc ddd\n   */\n");
    }

    #[test]
    fn a_one_line_block_comment_wraps_under_its_text() {
        let out = prose(
            "/* aaa bbb */\n",
            ProseKind::BlockComment,
            Lang::Go,
            None,
            8,
        );
        assert_eq!(out, "/* aaa\n   bbb */\n");
    }

    #[test]
    fn an_html_comment_is_a_block_comment() {
        let out = prose(
            "<!-- aaa\n     bbb -->\n",
            ProseKind::BlockComment,
            Lang::Html,
            None,
            80,
        );
        assert_eq!(out, "<!-- aaa bbb -->\n");
    }

    #[test]
    fn a_crlf_docstring_keeps_its_line_ends() {
        let text = "    \"\"\"Summary that\r\n    wraps.\"\"\"\r\n";
        let out = prose(text, ProseKind::TripleString, Lang::Python, None, 80);
        assert_eq!(out, "    \"\"\"Summary that wraps.\"\"\"\r\n");
    }

    #[test]
    fn a_markdown_paragraph_has_no_frame() {
        let out = prose(
            "- aaa\n  bbb\n",
            ProseKind::Paragraph,
            Lang::Markdown,
            None,
            80,
        );
        assert_eq!(out, "- aaa bbb\n");
    }
}
