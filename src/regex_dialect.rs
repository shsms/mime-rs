//! Translate Emacs regexp syntax into the `regex` crate's (RE2) syntax.
//!
//! mime exposes Emacs-Lisp, so patterns should read like Emacs regexps: groups
//! are `\(...\)`, alternation is `\|`, intervals are `\{n,m\}`, shy groups are
//! `\(?:...\)`, and a bare `(`, `|` or `{` is a literal. The `regex` crate
//! inverts all of that (`(...)`, `|`, `{n,m}` are the operators; `\(` is a
//! literal paren). Rather than swap the engine — and lose RE2's linear-time
//! guarantee on agent-supplied patterns over huge files — we translate the
//! surface syntax here, at the single compile chokepoint
//! ([`crate::builtins::cached_regex`]).
//!
//! One concession to RE2: a standalone inline-flag token like `(?i)` or `(?s)`
//! passes through unchanged. Emacs has no such syntax (it uses
//! `case-fold-search`), but mime itself prepends `(?i)` for case-insensitive
//! occur/grep, and the flags are genuinely useful. Ordinary bare parens are
//! still literals; shy groups use the Emacs spelling `\(?:...\)`.
//!
//! Replacement strings (`\1`, `\&`) are already Emacs-style and are expanded
//! elsewhere; this module only touches the *pattern*.
//!
//! Features RE2 cannot express are rejected with a clear error instead of
//! silently mismatching: backreferences in a pattern (`\1`), `\=` (point),
//! `\_<` / `\_>` (symbol boundaries), and `\s` / `\S` (syntax-class escapes,
//! which take an Emacs syntax-code argument RE2 has no equivalent for). Inside
//! `[...]`, Emacs has no backslash escapes — `\` is a literal member — so we
//! double it for RE2 (`[\d]` ⇒ the set `{\, d}`, not a digit class). One
//! convenience divergence from strict Emacs: C-style escapes outside a class
//! (`\t \n \r \f \v \a`) keep their RE2 meaning (the control char), not Emacs's
//! "backslash before an ordinary letter is just that letter".

/// Convert an Emacs-dialect pattern to an equivalent RE2 pattern.
///
/// Returns `Err(message)` for constructs RE2 cannot represent.
pub(crate) fn translate(pat: &str) -> Result<String, String> {
    let cs: Vec<char> = pat.chars().collect();
    let mut out = String::with_capacity(pat.len() + 8);
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        match c {
            '\\' => {
                let Some(&n) = cs.get(i + 1) else {
                    return Err("trailing backslash in regexp".to_string());
                };
                i += 1;
                match n {
                    // Emacs-special -> RE2-special: drop the backslash.
                    '(' | ')' | '|' | '{' | '}' => out.push(n),
                    // Buffer-edge anchors.
                    '`' => out.push_str("\\A"),
                    '\'' => out.push_str("\\z"),
                    // Not representable in RE2.
                    '1'..='9' => {
                        return Err(format!(
                            "backreference \\{n} is not supported (RE2 engine has no backrefs)"
                        ));
                    }
                    '=' => return Err("\\= (match point) is not supported".to_string()),
                    '_' => {
                        return Err("\\_< / \\_> (symbol boundaries) are not supported; use \\b"
                            .to_string());
                    }
                    // Emacs `\s`/`\S` take a syntax-code argument (\sw, \s-, …)
                    // that RE2 can't express; passing them through would silently
                    // leak the code char as a literal, so reject them outright.
                    's' | 'S' => {
                        return Err(format!(
                            "\\{n} (syntax classes) are not supported; use a character class \
                             like [[:space:]] / [[:word:]]"
                        ));
                    }
                    // \w \W \b \B \< \> \A \z and metachar escapes (\. \* \\ …)
                    // mean the same in both dialects. C-style escapes (\t \n \r
                    // \f \v \a …) follow RE2 — they are the control char, NOT the
                    // strict-Emacs "backslash before an ordinary letter is that
                    // letter". Kept verbatim either way.
                    _ => {
                        out.push('\\');
                        out.push(n);
                    }
                }
            }
            // A standalone inline-flag group — `(?i)`, `(?s)`, `(?im-s)` — is the
            // one RE2 form we keep verbatim (see module docs); any other `(` is
            // an Emacs literal and gets escaped for RE2.
            '(' => {
                if let Some(end) = inline_flag_token(&cs, i) {
                    out.extend(&cs[i..=end]);
                    i = end;
                } else {
                    out.push('\\');
                    out.push('(');
                }
            }
            // Emacs-literal -> RE2-literal: add a backslash.
            ')' | '|' | '{' | '}' => {
                out.push('\\');
                out.push(c);
            }
            '[' => {
                i = copy_class(&cs, i, &mut out)?;
            }
            _ => out.push(c),
        }
        i += 1;
    }
    Ok(out)
}

/// Emacs-style `regexp-quote`: backslash the characters that begin a regexp
/// construct in the *Emacs* dialect, so the result — once run through
/// [`translate`] — matches `s` literally. Bare `( ) { } |` are deliberately
/// left alone here; `translate` is what escapes them for RE2. Producing RE2
/// escaping directly (e.g. `regex::escape`) would be wrong: its `\(` would be
/// read back by `translate` as a group opener.
pub(crate) fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '.' | '*' | '+' | '?' | '[' | ']' | '^' | '$' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// If `cs[at]` begins an inline-flag token `(?<flags>)` with at least one flag
/// char from `imsxuU-`, return the index of its closing `)`. Otherwise `None`
/// (so `(?:`, `(?P<name>`, a bare `(`, etc. fall through to literal handling).
fn inline_flag_token(cs: &[char], at: usize) -> Option<usize> {
    if cs.get(at) != Some(&'(') || cs.get(at + 1) != Some(&'?') {
        return None;
    }
    let mut j = at + 2;
    while matches!(cs.get(j), Some('i' | 'm' | 's' | 'x' | 'u' | 'U' | '-')) {
        j += 1;
    }
    if j > at + 2 && cs.get(j) == Some(&')') {
        Some(j)
    } else {
        None
    }
}

/// Copy a `[...]` character class, translating Emacs class semantics to RE2.
/// Emacs has NO backslash escapes inside a class — `\` is a literal member —
/// so each interior `\` is doubled to `\\` (RE2's literal backslash); otherwise
/// `[\d]` (Emacs: the set `{\, d}`) would become an RE2 digit class. Handles a
/// leading `^`, a `]` as the first member, and POSIX classes like `[:alpha:]`
/// whose inner `:]` must not be mistaken for the class close. The POSIX-class
/// body is copied verbatim (no `\` there). Returns the index of the closing `]`.
fn copy_class(cs: &[char], open: usize, out: &mut String) -> Result<usize, String> {
    out.push('[');
    let mut i = open + 1;
    if cs.get(i) == Some(&'^') {
        out.push('^');
        i += 1;
    }
    if cs.get(i) == Some(&']') {
        // A `]` immediately after `[` (or `[^`) is a literal member.
        out.push(']');
        i += 1;
    }
    while i < cs.len() {
        let c = cs[i];
        if c == ']' {
            out.push(']');
            return Ok(i);
        }
        if c == '[' && cs.get(i + 1) == Some(&':') {
            // POSIX class [:name:] — copy through its own `:]` terminator.
            out.push('[');
            out.push(':');
            i += 2;
            while i < cs.len() {
                out.push(cs[i]);
                if cs[i] == ':' && cs.get(i + 1) == Some(&']') {
                    out.push(']');
                    i += 1;
                    break;
                }
                i += 1;
            }
        } else {
            // A literal backslash in an Emacs class → RE2 literal `\\`.
            if c == '\\' {
                out.push('\\');
            }
            out.push(c);
        }
        i += 1;
    }
    Err("unterminated [...] in regexp".to_string())
}

#[cfg(test)]
mod tests {
    use super::translate;

    fn t(p: &str) -> String {
        translate(p).unwrap()
    }

    #[test]
    fn groups_alternation_intervals_swap_escaping() {
        assert_eq!(t("\\(ab\\)"), "(ab)");
        assert_eq!(t("a\\|b"), "a|b");
        assert_eq!(t("a\\{2,3\\}"), "a{2,3}");
        assert_eq!(t("\\(?:ab\\)"), "(?:ab)"); // Emacs-spelled shy group
    }

    #[test]
    fn bare_metachars_become_literals() {
        assert_eq!(t("(a)"), "\\(a\\)");
        assert_eq!(t("a|b"), "a\\|b");
        assert_eq!(t("{\\an8}"), "\\{\\an8\\}");
    }

    #[test]
    fn inline_flag_tokens_pass_through() {
        assert_eq!(t("(?i)abc"), "(?i)abc");
        assert_eq!(t("(?s)a.b"), "(?s)a.b");
        assert_eq!(t("(?im-s)x"), "(?im-s)x");
        // a `)` after the flag group is still a literal
        assert_eq!(t("(?i)a)b"), "(?i)a\\)b");
        // not a flag token -> the paren is a literal
        assert_eq!(t("(?x"), "\\(?x");
    }

    #[test]
    fn the_renumber_pattern_that_started_this() {
        // The SRT renumber: capture the timecode prefix after the cue number.
        assert_eq!(
            t("^[0-9]+\n\\([0-9][0-9:,]*[ ]*-->\\)"),
            "^[0-9]+\n([0-9][0-9:,]*[ ]*-->)"
        );
    }

    #[test]
    fn anchors_and_classes_and_escapes() {
        assert_eq!(t("\\`start"), "\\Astart");
        assert_eq!(t("end\\'"), "end\\z");
        assert_eq!(t("[][:alpha:]]"), "[][:alpha:]]");
        assert_eq!(t("[^]a]"), "[^]a]");
        assert_eq!(t("\\bfoo\\b"), "\\bfoo\\b");
        assert_eq!(t("a\\.b\\*c"), "a\\.b\\*c");
        assert_eq!(t("a\\{2\\}?"), "a{2}?"); // non-greedy interval
    }

    #[test]
    fn class_backslash_is_a_literal_member() {
        // Emacs class has no escapes: `\` is literal, so double it for RE2.
        assert_eq!(t("[\\d]"), "[\\\\d]"); // the set {\, d}, NOT a digit class
        assert_eq!(t("[a\\]"), "[a\\\\]"); // {a, \}
        let re = regex::Regex::new(&t("[\\d]")).unwrap();
        assert!(re.is_match("\\") && re.is_match("d") && !re.is_match("7"));
    }

    #[test]
    fn syntax_class_escapes_are_rejected() {
        assert!(translate("\\sw").unwrap_err().contains("syntax classes"));
        assert!(translate("a\\S-b").unwrap_err().contains("syntax classes"));
    }

    #[test]
    fn patterns_without_metachars_are_unchanged() {
        for p in ["plain text", "a.b*c+d?", "^line$", "[a-z0-9]", "x\ny"] {
            assert_eq!(translate(p).unwrap(), p, "should pass through: {p:?}");
        }
    }

    #[test]
    fn quote_then_translate_matches_literally() {
        let raw = "a.b*c(d)[e]{f}|g\\h^i$";
        let pat = translate(&super::quote(raw)).unwrap();
        let re = regex::Regex::new(&pat).unwrap();
        assert!(re.is_match(raw));
        // It is a literal match, not the metacharacters interpreted.
        assert!(!re.is_match("abc"));
    }

    #[test]
    fn unsupported_features_error_clearly() {
        assert!(
            translate("\\(a\\)\\1")
                .unwrap_err()
                .contains("backreference")
        );
        assert!(translate("foo\\=").unwrap_err().contains("point"));
        assert!(translate("\\_<word\\_>").unwrap_err().contains("symbol"));
        assert!(translate("ab\\").unwrap_err().contains("trailing"));
        assert!(translate("[abc").unwrap_err().contains("unterminated"));
    }
}
