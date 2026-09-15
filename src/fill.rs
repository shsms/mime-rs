//! Paragraph filling: the pure text reflow behind `fill-paragraph` /
//! `fill-region` and the `fill_text` tool.

/// Layout knobs for one fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fill {
    /// The last column a line may reach, prefix included (Emacs
    /// `fill-column`).
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

/// Does `word` end a sentence: `.`, `?` or `!`, optionally followed by
/// closing punctuation (`)`, `]`, `}`, `"`, `'`), as Emacs's `sentence-end`.
fn ends_sentence(word: &str) -> bool {
    let core = word.trim_end_matches(['\'', '"', ')', ']', '}']);
    core.ends_with(['.', '?', '!'])
}

/// The words of `body` with the gap each one wants after it: two spaces
/// after a sentence end that the source marked with a line break or two
/// spaces (Emacs's `sentence-end-double-space`), one otherwise. A single
/// space after a period ("e.g. x") stays single, since the source did not
/// treat it as a sentence end.
fn words(body: &str, double_space: bool) -> Vec<(&str, usize)> {
    // A no-break space (and its narrow and figure forms) is part of its
    // word; every other whitespace separates words.
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

/// Reflow `body` — prose whose prefix is already stripped — into lines that
/// fit `opts.column` once `first` is put in front of the first line and
/// `rest` in front of every other. A word wider than the room left stands
/// on its own line, whole: a URL never breaks.
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
}
