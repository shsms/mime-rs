//! Foundation motions: the `skip-chars` character-set parser and the
//! position walkers (skip, word/symbol unit, paragraph) the motion builtins
//! are written on. Every walker is a pure function over `&dyn TextStore`:
//! it reads through `char_after` / `char_before` only (never `text()`, so a
//! file-backed buffer never materializes) and returns a position; the
//! builtin does the `goto_char` / `set_mark`.

use crate::store::TextStore;

/// One `[:name:]` class of the Emacs `skip-chars` spec syntax. Membership
/// uses Rust's Unicode predicates, except for `[:digit:]`, which is ASCII
/// `0`-`9` as in Emacs, so a `[:digit:]` skip is narrower than a
/// `forward-word` hop. `[:alnum:]` uses `is_alphanumeric`, which also
/// accepts numeric forms such as `½` that Emacs's `[:alnum:]` rejects (and
/// `[:punct:]` therefore excludes them where Emacs includes them), because
/// the standard library has no decimal-digit category test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Alpha,
    Alnum,
    Digit,
    Space,
    Blank,
    Upper,
    Lower,
    Punct,
}

impl Class {
    fn parse(name: &str) -> Option<Class> {
        Some(match name {
            "alpha" => Class::Alpha,
            "alnum" => Class::Alnum,
            "digit" => Class::Digit,
            "space" => Class::Space,
            "blank" => Class::Blank,
            "upper" => Class::Upper,
            "lower" => Class::Lower,
            "punct" => Class::Punct,
            _ => return None,
        })
    }

    fn contains(self, c: char) -> bool {
        match self {
            Class::Alpha => c.is_alphabetic(),
            Class::Alnum => c.is_alphanumeric(),
            Class::Digit => c.is_ascii_digit(),
            Class::Space => c.is_whitespace(),
            Class::Blank => c == ' ' || c == '\t',
            Class::Upper => c.is_uppercase(),
            Class::Lower => c.is_lowercase(),
            Class::Punct => {
                c.is_ascii_punctuation()
                    || (!c.is_alphanumeric()
                        && !c.is_whitespace()
                        && !c.is_control()
                        && !c.is_ascii())
            }
        }
    }
}

/// A parsed `skip-chars` SPEC: literal characters, `a-z` ranges, `[:class:]`
/// names, optionally negated by a leading `^`. Backslash quotes the next
/// character; a `-` first or last (or right after a range) is literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharSet {
    negated: bool,
    chars: Vec<char>,
    ranges: Vec<(char, char)>,
    classes: Vec<Class>,
}

impl CharSet {
    /// Parse an Emacs `skip-chars` spec. A malformed spec — an unterminated
    /// `[:`, an unknown class name, a range whose end precedes its start, a
    /// trailing lone backslash — is an error naming the offending part; it is
    /// never silently reinterpreted.
    pub fn parse(spec: &str) -> Result<CharSet, String> {
        let mut set = CharSet {
            negated: false,
            chars: Vec::new(),
            ranges: Vec::new(),
            classes: Vec::new(),
        };
        let chars: Vec<char> = spec.chars().collect();
        let mut i = 0;
        if chars.first() == Some(&'^') {
            set.negated = true;
            i = 1;
        }
        // One literal character at `i` (after backslash quoting); advances `i`.
        fn literal(chars: &[char], i: &mut usize) -> Result<char, String> {
            let c = chars[*i];
            if c == '\\' {
                *i += 1;
                let Some(&q) = chars.get(*i) else {
                    return Err("skip-chars: spec ends in a lone backslash".to_string());
                };
                *i += 1;
                Ok(q)
            } else {
                *i += 1;
                Ok(c)
            }
        }
        while i < chars.len() {
            // A class: `[:name:]`.
            if chars[i] == '[' && chars.get(i + 1) == Some(&':') {
                let start = i + 2;
                let Some(len) = chars[start..].windows(2).position(|w| w == [':', ']']) else {
                    return Err(format!(
                        "skip-chars: unterminated character class in {spec:?}"
                    ));
                };
                let name: String = chars[start..start + len].iter().collect();
                let class = Class::parse(&name)
                    .ok_or_else(|| format!("skip-chars: unknown character class [:{name}:]"))?;
                set.classes.push(class);
                i = start + len + 2;
                continue;
            }
            let lo = literal(&chars, &mut i)?;
            // A range: `lo-hi`, only when a `-` is followed by another char.
            if chars.get(i) == Some(&'-') && i + 1 < chars.len() {
                i += 1;
                let hi = literal(&chars, &mut i)?;
                if hi < lo {
                    return Err(format!(
                        "skip-chars: range {lo}-{hi} runs backwards (end precedes start)"
                    ));
                }
                set.ranges.push((lo, hi));
            } else {
                set.chars.push(lo);
            }
        }
        Ok(set)
    }

    /// Whether `c` is in the set (honouring negation).
    pub fn contains(&self, c: char) -> bool {
        let hit = self.chars.contains(&c)
            || self.ranges.iter().any(|&(lo, hi)| (lo..=hi).contains(&c))
            || self.classes.iter().any(|k| k.contains(c));
        hit != self.negated
    }
}

/// The first position in `[from, bound]` whose char fails `pred`, or `bound`.
pub fn skip_forward(
    store: &dyn TextStore,
    from: usize,
    bound: usize,
    pred: &dyn Fn(char) -> bool,
) -> usize {
    let mut p = from;
    while p < bound && store.char_after(p).is_some_and(pred) {
        p += 1;
    }
    p
}

/// The mirror of [`skip_forward`]: walks `char_before` down to `bound`.
pub fn skip_backward(
    store: &dyn TextStore,
    from: usize,
    bound: usize,
    pred: &dyn Fn(char) -> bool,
) -> usize {
    let mut p = from;
    while p > bound && store.char_before(p).is_some_and(pred) {
        p -= 1;
    }
    p
}

/// `forward-word`'s notion of a word character. `_` is deliberately not one:
/// in Emacs it has symbol syntax, not word syntax, in every programming mode.
pub fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
}

/// One forward hop over a unit (word, symbol): skip non-constituents, then
/// constituents. Returns the position after the unit, or `bound`.
pub fn unit_forward(
    store: &dyn TextStore,
    from: usize,
    bound: usize,
    is_constituent: &dyn Fn(char) -> bool,
) -> usize {
    let p = skip_forward(store, from, bound, &|c| !is_constituent(c));
    skip_forward(store, p, bound, is_constituent)
}

/// The mirror of [`unit_forward`]: the position before the previous unit.
pub fn unit_backward(
    store: &dyn TextStore,
    from: usize,
    bound: usize,
    is_constituent: &dyn Fn(char) -> bool,
) -> usize {
    let p = skip_backward(store, from, bound, &|c| !is_constituent(c));
    skip_backward(store, p, bound, is_constituent)
}

/// Start of the line containing `p`, not below `min`.
fn bol(store: &dyn TextStore, p: usize, min: usize) -> usize {
    skip_backward(store, p, min, &|c| c != '\n')
}

/// Start of the line before the one starting at `line`, or `min`.
fn prev_bol(store: &dyn TextStore, line: usize, min: usize) -> usize {
    if line <= min {
        return min;
    }
    // `line - 1` is the newline ending the previous line; scan past it.
    bol(store, line - 1, min)
}

/// One pass over the line starting at `line`: whether it holds only spaces
/// and tabs (an empty line, and the empty line at `bound`, count as blank),
/// and the start of the line after it, or `bound`.
///
/// The blank run and the run to the newline are one walk — the second skip
/// starts where the first stopped — so a paragraph walk reads each line once.
fn scan_line(store: &dyn TextStore, line: usize, bound: usize) -> (bool, usize) {
    let p = skip_forward(store, line, bound, &|c| c == ' ' || c == '\t');
    let blank = p >= bound || store.char_after(p) == Some('\n');
    let eol = skip_forward(store, p, bound, &|c| c != '\n');
    (blank, if eol < bound { eol + 1 } else { bound })
}

/// Forward paragraph hop: skip any blank lines point is on, then run to the
/// start of the next blank line, or `bound`. Never mutates the store.
pub fn paragraph_forward(store: &dyn TextStore, from: usize, bound: usize) -> usize {
    let mut l = bol(store, from, store.point_min());
    while l < bound {
        let (blank, next) = scan_line(store, l, bound);
        if !blank {
            break;
        }
        l = next;
    }
    while l < bound {
        let (blank, next) = scan_line(store, l, bound);
        if blank {
            break;
        }
        l = next;
    }
    l
}

/// Backward paragraph hop: from a blank line, step up into the paragraph
/// above; then run to the paragraph's first line and land on the blank line
/// before it, or `bound`. Never mutates the store.
pub fn paragraph_backward(store: &dyn TextStore, from: usize, bound: usize) -> usize {
    let max = store.point_max();
    let mut l = bol(store, from, bound);
    while l > bound && scan_line(store, l, max).0 {
        l = prev_bol(store, l, bound);
    }
    // One `prev_bol` per line: the line above is both the landing place, when
    // it is blank, and the next line to step to when it is not.
    loop {
        if l <= bound {
            return bound;
        }
        let prev = prev_bol(store, l, bound);
        if scan_line(store, prev, max).0 {
            return prev;
        }
        l = prev;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(spec: &str) -> CharSet {
        CharSet::parse(spec).unwrap()
    }

    #[test]
    fn literal_characters() {
        let s = set("abc");
        assert!(s.contains('a') && s.contains('c'));
        assert!(!s.contains('d'));
    }

    #[test]
    fn ranges_and_a_literal_dash_at_either_end() {
        let s = set("a-cx");
        assert!(s.contains('b') && s.contains('x'));
        assert!(!s.contains('d'));
        assert!(set("-a").contains('-'));
        assert!(set("a-").contains('-'));
        assert!(!set("a-").contains('b'));
    }

    #[test]
    fn leading_caret_negates_and_an_inner_caret_is_literal() {
        let s = set("^a-z");
        assert!(s.contains('A') && s.contains(' '));
        assert!(!s.contains('m'));
        assert!(set("a^").contains('^'));
        assert!(!set("a^").contains('b'));
    }

    #[test]
    fn backslash_quotes_the_next_character() {
        let s = set("\\^\\-\\\\");
        assert!(s.contains('^') && s.contains('-') && s.contains('\\'));
        assert!(!s.contains('a'));
    }

    #[test]
    fn classes_mix_with_literals() {
        let s = set("[:digit:]_");
        assert!(s.contains('7') && s.contains('_'));
        assert!(!s.contains('a'));
        assert!(!set("[:digit:]").contains('½'));
        let alpha = set("[:alpha:]");
        assert!(alpha.contains('é') && !alpha.contains('1'));
        let blank = set("[:blank:]");
        assert!(blank.contains(' ') && blank.contains('\t') && !blank.contains('\n'));
        let space = set("[:space:]");
        assert!(space.contains('\n'));
        assert!(set("[:upper:]").contains('Q') && !set("[:upper:]").contains('q'));
        assert!(set("[:lower:]").contains('q') && !set("[:lower:]").contains('Q'));
        assert!(set("[:punct:]").contains(',') && !set("[:punct:]").contains('a'));
        let alnum = set("[:alnum:]");
        assert!(alnum.contains('z') && alnum.contains('9') && !alnum.contains('_'));
    }

    #[test]
    fn malformed_specs_are_errors_that_name_the_problem() {
        assert!(
            CharSet::parse("[:alpha")
                .unwrap_err()
                .contains("unterminated")
        );
        assert!(CharSet::parse("[:bogus:]").unwrap_err().contains("bogus"));
        assert!(CharSet::parse("z-a").unwrap_err().contains("z-a"));
        assert!(CharSet::parse("ab\\").unwrap_err().contains("backslash"));
    }

    #[test]
    fn empty_spec_matches_nothing_and_negated_empty_matches_everything() {
        assert!(!set("").contains('a'));
        assert!(set("^").contains('a'));
    }

    use crate::buffer::Buffer;

    // `Buffer` has inherent `point_min` / `point_max` / `narrow_to_region`,
    // and `&Buffer` coerces to `&dyn TextStore` at the call sites, so the
    // trait needs no import here (an unused import fails the clippy gate).
    fn buf(text: &str) -> Buffer {
        Buffer::from_string("t", text)
    }

    #[test]
    fn skip_forward_stops_at_the_first_non_member_or_the_bound() {
        //          123456789
        let b = buf("aaab  cde");
        let a = CharSet::parse("a").unwrap();
        assert_eq!(skip_forward(&b, 1, b.point_max(), &|c| a.contains(c)), 4);
        // Already on a non-member: no movement.
        assert_eq!(skip_forward(&b, 4, b.point_max(), &|c| a.contains(c)), 4);
        // The bound caps the walk.
        assert_eq!(skip_forward(&b, 1, 3, &|c| a.contains(c)), 3);
        // Everything matches: lands on point_max.
        assert_eq!(skip_forward(&b, 1, b.point_max(), &|_| true), b.point_max());
    }

    #[test]
    fn skip_backward_mirrors_over_char_before() {
        //          123456789
        let b = buf("aaab  cde");
        let a = CharSet::parse("a").unwrap();
        assert_eq!(skip_backward(&b, 4, b.point_min(), &|c| a.contains(c)), 1);
        assert_eq!(skip_backward(&b, 4, 2, &|c| a.contains(c)), 2);
        assert_eq!(skip_backward(&b, 1, b.point_min(), &|_| true), 1);
    }

    #[test]
    fn unit_forward_skips_separators_then_constituents() {
        //          1234567890123
        let b = buf("  foo_bar baz");
        let max = b.point_max();
        // Word: `_` is a separator, so the first hop ends after `foo`.
        assert_eq!(unit_forward(&b, 1, max, &is_word_char), 6);
        assert_eq!(unit_forward(&b, 6, max, &is_word_char), 10);
        // Symbol: `_` is a constituent.
        let sym = |c: char| c.is_alphanumeric() || c == '_';
        assert_eq!(unit_forward(&b, 1, max, &sym), 10);
        // At the end there is nothing to skip: stays at the bound.
        assert_eq!(unit_forward(&b, max, max, &is_word_char), max);
    }

    #[test]
    fn unit_backward_mirrors() {
        //          1234567890123
        let b = buf("  foo_bar baz");
        let min = b.point_min();
        assert_eq!(unit_backward(&b, 14, min, &is_word_char), 11);
        assert_eq!(unit_backward(&b, 11, min, &is_word_char), 7);
        assert_eq!(unit_backward(&b, 7, min, &is_word_char), 3);
        assert_eq!(unit_backward(&b, 3, min, &is_word_char), 1);
    }

    #[test]
    fn paragraph_forward_lands_on_the_next_blank_line_or_the_bound() {
        // "one\n" = 1-4, "two\n" = 5-8, "\n" = 9, "three\n" = 10-15,
        // "  \n" = 16-18, "four" = 19-22, point_max = 23.
        let b = buf("one\ntwo\n\nthree\n  \nfour");
        let max = b.point_max();
        assert_eq!(paragraph_forward(&b, 1, max), 9);
        // From mid-paragraph, same answer.
        assert_eq!(paragraph_forward(&b, 6, max), 9);
        // Starting ON the blank line: skip it, then run to the next one.
        // The whitespace-only line starts at 16.
        assert_eq!(paragraph_forward(&b, 9, max), 16);
        // The last paragraph has no separator after it: the bound.
        assert_eq!(paragraph_forward(&b, 19, max), max);
        // A bound inside the text caps the walk.
        assert_eq!(paragraph_forward(&b, 1, 7), 7);
    }

    #[test]
    fn paragraph_backward_lands_on_the_previous_blank_line_or_the_bound() {
        let b = buf("one\ntwo\n\nthree\n  \nfour");
        let min = b.point_min();
        let max = b.point_max();
        // From inside `four`: the whitespace-only separator at 16.
        assert_eq!(paragraph_backward(&b, max, min), 16);
        assert_eq!(paragraph_backward(&b, 21, min), 16);
        // From ON that separator: into `three`, whose separator is at 9.
        assert_eq!(paragraph_backward(&b, 16, min), 9);
        // From the start of `three` (line start, not a blank line): 9.
        assert_eq!(paragraph_backward(&b, 10, min), 9);
        // The first paragraph has no separator before it: the bound.
        assert_eq!(paragraph_backward(&b, 6, min), min);
        // A bound inside the text caps the walk.
        assert_eq!(paragraph_backward(&b, 6, 3), 3);
    }

    #[test]
    fn paragraph_walks_respect_a_narrowing() {
        let mut b = buf("one\ntwo\n\nthree\n  \nfour");
        b.narrow_to_region(5, 15); // "two\n\nthree\n"
        assert_eq!(paragraph_forward(&b, 5, b.point_max()), 9);
        assert_eq!(paragraph_forward(&b, 10, b.point_max()), b.point_max());
        assert_eq!(paragraph_backward(&b, 12, b.point_min()), 9);
        assert_eq!(paragraph_backward(&b, 7, b.point_min()), b.point_min());
    }
}
