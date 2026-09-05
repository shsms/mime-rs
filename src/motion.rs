//! Foundation motions: the `skip-chars` character-set parser and the
//! position walkers (skip, word/symbol unit, paragraph) the motion builtins
//! are written on. Every walker is a pure function over `&dyn TextStore`:
//! it reads through `char_after` / `char_before` only (never `text()`, so a
//! file-backed buffer never materializes) and returns a position; the
//! builtin does the `goto_char` / `set_mark`.

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
}
