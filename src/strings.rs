//! String library (a mime-rs module, RE2-backed) — Emacs-Lisp string functions,
//! pure string→string with no buffer state. This is the user's key insight: the
//! Emacs string/regex layer is a mime-rs module, not a tulisp change. M0 seed;
//! subagents extend (`string-match`/`match-string`, `split-string` keeping
//! separators, `string-trim`, `number-to-string`, …).
use tulisp::{Error, TulispContext, TulispObject};

pub fn register(ctx: &mut TulispContext) {
    // (replace-regexp-in-string REGEXP REP STRING) — REP is a template with
    // `\N` (group) / `\&` (whole match) backrefs. Compiled via `cached_regex`,
    // so `^` / `$` anchor lines (Emacs semantics), here as in the buffer
    // searches.
    ctx.defun(
        "replace-regexp-in-string",
        |regexp: String, rep: String, s: String| -> Result<String, Error> {
            let rx = crate::builtins::cached_regex(&regexp)?;
            Ok(rx
                .replace_all(&s, |caps: &regex::Captures| expand(&rep, caps))
                .into_owned())
        },
    );

    // (substring STRING FROM &optional TO) — char-based, non-negative (M0).
    ctx.defun(
        "substring",
        |s: String, from: i64, to: Option<i64>| -> String {
            let chars: Vec<char> = s.chars().collect();
            let n = chars.len() as i64;
            let f = from.clamp(0, n) as usize;
            let t = to.unwrap_or(n).clamp(0, n) as usize;
            if f >= t {
                String::new()
            } else {
                chars[f..t].iter().collect()
            }
        },
    );

    // (split-string STRING &optional SEPARATORS) — SEP is a regex; default splits
    // on whitespace runs (dropping empties), like Emacs's default.
    ctx.defun(
        "split-string",
        |s: String, sep: Option<String>| -> Vec<String> {
            match sep {
                Some(re) => match crate::builtins::cached_regex(&re) {
                    Ok(rx) => rx.split(&s).map(str::to_string).collect(),
                    Err(_) => vec![s],
                },
                None => s.split_whitespace().map(str::to_string).collect(),
            }
        },
    );

    // (string-trim STRING) — drop leading and trailing whitespace.
    ctx.defun("string-trim", |s: String| -> String {
        s.trim_matches(is_ws).to_string()
    });
    // (string-trim-left STRING) — drop leading whitespace only.
    ctx.defun("string-trim-left", |s: String| -> String {
        s.trim_start_matches(is_ws).to_string()
    });
    // (string-trim-right STRING) — drop trailing whitespace only.
    ctx.defun("string-trim-right", |s: String| -> String {
        s.trim_end_matches(is_ws).to_string()
    });

    // (string-prefix-p PREFIX STRING) — t iff STRING starts with PREFIX.
    ctx.defun("string-prefix-p", |prefix: String, s: String| -> bool {
        s.starts_with(&prefix)
    });
    // (string-suffix-p SUFFIX STRING) — t iff STRING ends with SUFFIX.
    ctx.defun("string-suffix-p", |suffix: String, s: String| -> bool {
        s.ends_with(&suffix)
    });

    // (string-search NEEDLE HAYSTACK &optional START) — 0-based *char* index
    // of the first match at or after START, or nil. START out of range errors,
    // matching Emacs (`args-out-of-range`).
    ctx.defun(
        "string-search",
        |needle: String, haystack: String, start: Option<i64>| -> Result<TulispObject, Error> {
            let chars: Vec<char> = haystack.chars().collect();
            let start = start.unwrap_or(0);
            if start < 0 || start > chars.len() as i64 {
                return Err(Error::lisp_error(format!(
                    "Args out of range: {haystack:?}, {start}"
                )));
            }
            // Search within the byte slice starting at the START-th char.
            let byte_start: usize = chars[..start as usize].iter().map(|c| c.len_utf8()).sum();
            match haystack[byte_start..].find(&needle) {
                Some(byte_off) => {
                    // Convert the absolute byte offset back to a char index.
                    let abs_byte = byte_start + byte_off;
                    let char_idx = haystack[..abs_byte].chars().count() as i64;
                    Ok(TulispObject::from(char_idx))
                }
                None => Ok(TulispObject::nil()),
            }
        },
    );

    // (string-replace FROM-STRING TO-STRING IN-STRING) — literal replace-all.
    // Empty FROM is an error in Emacs (`wrong-length-argument`).
    ctx.defun(
        "string-replace",
        |from: String, to: String, s: String| -> Result<String, Error> {
            if from.is_empty() {
                return Err(Error::lisp_error(
                    "Wrong length argument: empty FROM-STRING",
                ));
            }
            Ok(s.replace(&from, &to))
        },
    );

    // (number-to-string N) — int or float to its printed form.
    ctx.defun(
        "number-to-string",
        |n: TulispObject| -> Result<String, Error> { number_to_string(&n) },
    );
    // (string-to-number STRING &optional BASE) — leading int/float; 0 if none.
    // BASE (other than 10) applies to integer parsing only, like Emacs.
    ctx.defun(
        "string-to-number",
        |s: String, base: Option<i64>| -> TulispObject { string_to_number(&s, base) },
    );

    // (upcase STRING) / (downcase STRING) — Unicode-aware case mapping.
    ctx.defun("upcase", |s: String| -> String { s.to_uppercase() });
    ctx.defun("downcase", |s: String| -> String { s.to_lowercase() });
    // (capitalize STRING) — upcase the first letter of each word, downcase the
    // rest. A "word" is a maximal run of alphanumerics (digits included).
    ctx.defun("capitalize", |s: String| -> String { capitalize(&s) });

    // (char-to-string CHAR) — a 1-char string from a codepoint.
    ctx.defun("char-to-string", |c: i64| -> Result<String, Error> {
        match u32::try_from(c).ok().and_then(char::from_u32) {
            Some(ch) => Ok(ch.to_string()),
            None => Err(Error::lisp_error(format!("Invalid character: {c}"))),
        }
    });
    // (string-to-char STRING) — codepoint of the first char, or 0 if empty.
    ctx.defun("string-to-char", |s: String| -> i64 {
        s.chars().next().map_or(0, |c| c as i64)
    });

    // (string-join LIST &optional SEPARATOR) — concatenate with SEPARATOR.
    ctx.defun(
        "string-join",
        |parts: Vec<String>, sep: Option<String>| -> String {
            parts.join(sep.as_deref().unwrap_or(""))
        },
    );

    // (string-empty-p STRING) — t iff STRING is "".
    ctx.defun("string-empty-p", |s: String| -> bool { s.is_empty() });
}

/// Emacs's default whitespace for `string-trim`: space, tab, newline,
/// carriage return, form feed, vertical tab.
fn is_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c' | '\x0b')
}

/// Upcase the first letter of each alphanumeric run; downcase the rest.
fn capitalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_word = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            if in_word {
                out.extend(c.to_lowercase());
            } else {
                out.extend(c.to_uppercase());
            }
            in_word = true;
        } else {
            out.push(c);
            in_word = false;
        }
    }
    out
}

/// Print N (int or float) like Emacs's `number-to-string`.
fn number_to_string(n: &TulispObject) -> Result<String, Error> {
    if let Ok(i) = i64::try_from(n) {
        Ok(i.to_string())
    } else if let Ok(f) = f64::try_from(n) {
        Ok(format_float(f))
    } else {
        Err(Error::lisp_error(format!(
            "Wrong type argument: numberp, {n}"
        )))
    }
}

/// Format a float the way Emacs's `number-to-string` does: the shortest
/// round-tripping decimal, always carrying a decimal point or exponent (so 3.0
/// prints "3.0", not "3"). Emacs follows C `%g`'s decimal-vs-exponential rule
/// keyed on the shortest digit string: with D significant digits and the
/// leading digit at decimal exponent X, it prints in exponent form when
/// X < -4 or X >= max(15, D) — e.g. 1e15 -> "1e+15" but 1234567890123456.0
/// stays decimal — and pads the exponent to at least two digits ("1e-05").
fn format_float(f: f64) -> String {
    if f.is_nan() {
        // Emacs prints these as 0.0e+NaN / N.Ne+INF; we won't hit them from
        // ordinary arithmetic, but keep a sane fallback.
        return "0.0e+NaN".to_string();
    }
    if f.is_infinite() {
        return if f < 0.0 {
            "-1.0e+INF".to_string()
        } else {
            "1.0e+INF".to_string()
        };
    }
    // Rust's "{:e}" is the shortest round-tripping form, always "d[.ddd]eX",
    // e.g. "1.5e15", "1e0", "-3e-5". Pull the sign, digits, and exponent out.
    let sci = format!("{f:e}");
    let (sign, rest) = match sci.strip_prefix('-') {
        Some(r) => ("-", r),
        None => ("", sci.as_str()),
    };
    let (mantissa, exp_str) = rest.split_once('e').unwrap_or((rest, "0"));
    let exp: i32 = exp_str.parse().unwrap_or(0);
    // Significant digits, with the decimal point removed.
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let d = digits.len() as i32;

    // Emacs's %g-style cutoff on the shortest representation.
    if exp < -4 || exp >= 15.max(d) {
        // Exponential form: a single significant digit stays bare ("1e+15"),
        // multiple digits carry the point ("1.5e+15").
        let mant = if digits.len() == 1 {
            digits.clone()
        } else {
            format!("{}.{}", &digits[..1], &digits[1..])
        };
        let esign = if exp < 0 { '-' } else { '+' };
        format!("{sign}{mant}e{esign}{:02}", exp.abs())
    } else if exp >= 0 {
        // Decimal form, value >= 1. Place the point after exp+1 digits.
        let point = (exp + 1) as usize;
        if point >= digits.len() {
            // Pad integer part with trailing zeros; fraction is ".0".
            let zeros = "0".repeat(point - digits.len());
            format!("{sign}{digits}{zeros}.0")
        } else {
            format!("{sign}{}.{}", &digits[..point], &digits[point..])
        }
    } else {
        // Decimal form, 0 < |value| < 1: "0.00ddd" with -exp-1 leading zeros.
        let zeros = "0".repeat((-exp - 1) as usize);
        format!("{sign}0.{zeros}{digits}")
    }
}

/// Parse a leading number out of STRING like Emacs's `string-to-number`,
/// returning an integer or float `TulispObject` (0 when nothing parses).
fn string_to_number(s: &str, base: Option<i64>) -> TulispObject {
    let body = s.trim_start_matches([' ', '\t']);
    match base {
        // A non-decimal base parses an integer only, stopping at the first
        // digit invalid for that radix.
        Some(b) if b != 10 => {
            let radix = b.clamp(2, 36) as u32;
            TulispObject::from(parse_int_radix(body, radix))
        }
        // No base (or base 10): parse a leading int or float.
        _ => match parse_decimal(body) {
            Some(Number::Int(i)) => TulispObject::from(i),
            Some(Number::Float(f)) => TulispObject::from(f),
            None => TulispObject::from(0i64),
        },
    }
}

/// An int-or-float result from decimal parsing.
enum Number {
    Int(i64),
    Float(f64),
}

/// Parse a leading signed integer in `radix`, stopping at the first invalid
/// digit (Emacs behavior, e.g. "17.5" base 8 -> 15).
fn parse_int_radix(s: &str, radix: u32) -> i64 {
    let mut chars = s.chars().peekable();
    let mut neg = false;
    match chars.peek() {
        Some('+') => {
            chars.next();
        }
        Some('-') => {
            neg = true;
            chars.next();
        }
        _ => {}
    }
    let mut acc: i64 = 0;
    let mut any = false;
    for c in chars {
        match c.to_digit(radix) {
            Some(d) => {
                acc = acc.saturating_mul(radix as i64).saturating_add(d as i64);
                any = true;
            }
            None => break,
        }
    }
    if !any {
        return 0;
    }
    if neg { -acc } else { acc }
}

/// Parse a leading decimal int/float (sign, digits, optional `.`fraction,
/// optional `e`exponent), stopping at the first character that doesn't fit.
/// Returns `Int` when no fraction/exponent was consumed, else `Float`.
fn parse_decimal(s: &str) -> Option<Number> {
    let bytes = s.as_bytes();
    let mut i = 0;
    let n = bytes.len();
    let start = i;
    if i < n && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    let int_start = i;
    while i < n && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let has_int = i > int_start;
    let mut is_float = false;
    // Fractional part.
    if i < n && bytes[i] == b'.' {
        let dot = i;
        i += 1;
        let frac_start = i;
        while i < n && bytes[i].is_ascii_digit() {
            i += 1;
        }
        // Need a digit somewhere (before or after the dot) for this to be a
        // number. A lone "." (or sign + ".") is not.
        if !has_int && i == frac_start {
            return None;
        }
        // Only count the dot as making it a float if a fraction follows;
        // "1." parses as the integer 1 in Emacs.
        if i > frac_start {
            is_float = true;
        } else {
            i = dot; // back up: don't consume a trailing bare dot
        }
    } else if !has_int {
        return None;
    }
    // Exponent: only valid if we have a mantissa and a digit follows e/E.
    if i < n && (bytes[i] == b'e' || bytes[i] == b'E') {
        let mut j = i + 1;
        if j < n && (bytes[j] == b'+' || bytes[j] == b'-') {
            j += 1;
        }
        let exp_digits = j;
        while j < n && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j > exp_digits {
            is_float = true;
            i = j;
        }
    }
    let tok = &s[start..i];
    if is_float {
        tok.parse::<f64>().ok().map(Number::Float)
    } else {
        // Integer token; on overflow Emacs returns a float, so fall back.
        match tok.parse::<i64>() {
            Ok(v) => Some(Number::Int(v)),
            Err(_) => tok.parse::<f64>().ok().map(Number::Float),
        }
    }
}

/// Expand `\N` / `\&` backrefs in a replacement template against `caps`.
fn expand(rep: &str, caps: &regex::Captures) -> String {
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
                out.push_str(caps.get(0).map_or("", |m| m.as_str()));
            }
            Some(d) if d.is_ascii_digit() => {
                let n = it.next().unwrap().to_digit(10).unwrap() as usize;
                out.push_str(caps.get(n).map_or("", |m| m.as_str()));
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
    use tulisp::TulispContext;

    /// Build a context with the string library registered.
    fn ctx() -> TulispContext {
        let mut ctx = TulispContext::new();
        crate::strings::register(&mut ctx);
        ctx
    }

    /// Eval a program and extract its result as a String.
    fn s(prog: &str) -> String {
        ctx().eval_string(prog).unwrap().try_into().unwrap()
    }

    /// Eval a program and extract its result as an i64.
    fn i(prog: &str) -> i64 {
        ctx().eval_string(prog).unwrap().try_into().unwrap()
    }

    /// Eval a program and report whether the result is non-nil.
    fn truthy(prog: &str) -> bool {
        !ctx().eval_string(prog).unwrap().null()
    }

    #[test]
    fn trim() {
        assert_eq!(s(r#"(string-trim "  hi ")"#), "hi");
        // Tabs / newlines / CR / form feed are whitespace too.
        assert_eq!(s("(string-trim \"\t\n  hi \n\")"), "hi");
        assert_eq!(s(r#"(string-trim-left "  hi  ")"#), "hi  ");
        assert_eq!(s(r#"(string-trim-right "  hi  ")"#), "  hi");
        assert_eq!(s(r#"(string-trim "")"#), "");
    }

    #[test]
    fn prefix_suffix() {
        assert!(truthy(r#"(string-prefix-p "he" "hello")"#));
        assert!(!truthy(r#"(string-prefix-p "x" "hello")"#));
        assert!(truthy(r#"(string-suffix-p "lo" "hello")"#));
        assert!(!truthy(r#"(string-suffix-p "x" "hello")"#));
        assert!(truthy(r#"(string-prefix-p "" "hello")"#));
    }

    #[test]
    fn search() {
        assert_eq!(i(r#"(string-search "lo" "hello")"#), 3);
        assert_eq!(i(r#"(string-search "l" "hello" 3)"#), 3);
        assert!(truthy(r#"(string-search "h" "hello")"#)); // index 0 is non-nil
        assert!(!truthy(r#"(string-search "z" "hello")"#));
        // Empty needle returns START (default 0).
        assert_eq!(i(r#"(string-search "" "abc")"#), 0);
        assert_eq!(i(r#"(string-search "" "abc" 3)"#), 3);
        // Char index, not byte index, around multibyte chars.
        assert_eq!(i(r#"(string-search "x" "éx")"#), 1);
        // START out of range is an error (like Emacs).
        assert!(
            ctx()
                .eval_string(r#"(string-search "a" "abc" 10)"#)
                .is_err()
        );
    }

    #[test]
    fn replace() {
        assert_eq!(s(r#"(string-replace "o" "0" "foo boo")"#), "f00 b00");
        assert_eq!(s(r#"(string-replace "x" "y" "abc")"#), "abc");
        // Literal, not regex: a "." only matches a literal dot.
        assert_eq!(s(r#"(string-replace "." "!" "a.b.c")"#), "a!b!c");
        // Empty FROM-STRING is an error.
        assert!(
            ctx()
                .eval_string(r#"(string-replace "" "x" "abc")"#)
                .is_err()
        );
    }

    #[test]
    fn number_string_roundtrip() {
        assert_eq!(s("(number-to-string 42)"), "42");
        assert_eq!(s("(number-to-string -7)"), "-7");
        // Floats keep a decimal point; integral floats print as "N.0".
        assert_eq!(s("(number-to-string 3.14)"), "3.14");
        assert_eq!(s("(number-to-string 3.0)"), "3.0");
        assert_eq!(s("(number-to-string -0.5)"), "-0.5");

        assert_eq!(i(r#"(string-to-number "12")"#), 12);
        // Leading integer, trailing garbage ignored.
        assert_eq!(i(r#"(string-to-number "12abc")"#), 12);
        // Leading whitespace skipped.
        assert_eq!(i(r#"(string-to-number "  12")"#), 12);
        assert_eq!(i(r#"(string-to-number "-5")"#), -5);
        assert_eq!(i(r#"(string-to-number "+5")"#), 5);
        // "1." is the integer 1; unparseable is 0.
        assert_eq!(i(r#"(string-to-number "1.")"#), 1);
        assert_eq!(i(r#"(string-to-number "abc")"#), 0);
        // Floats / exponents.
        assert_eq!(s(r#"(number-to-string (string-to-number "3.14"))"#), "3.14");
        assert_eq!(s(r#"(number-to-string (string-to-number ".5"))"#), "0.5");
        assert_eq!(
            s(r#"(number-to-string (string-to-number "1e3"))"#),
            "1000.0"
        );
        assert_eq!(
            s(r#"(number-to-string (string-to-number "1.5e2"))"#),
            "150.0"
        );
        // BASE applies to integers, stopping at the first invalid digit.
        assert_eq!(i(r#"(string-to-number "ff" 16)"#), 255);
        assert_eq!(i(r#"(string-to-number "FF" 16)"#), 255);
        assert_eq!(i(r#"(string-to-number "101" 2)"#), 5);
        assert_eq!(i(r#"(string-to-number "17.5" 8)"#), 15);
        assert_eq!(i(r#"(string-to-number "-ff" 16)"#), -255);
    }

    #[test]
    fn float_printing_matches_emacs() {
        // Integral floats keep ".0"; ordinary decimals print shortest.
        assert_eq!(s("(number-to-string 3.0)"), "3.0");
        assert_eq!(s("(number-to-string 0.1)"), "0.1");
        assert_eq!(s("(number-to-string -0.5)"), "-0.5");
        assert_eq!(s("(number-to-string 1234.5678)"), "1234.5678");
        // Large magnitudes stay decimal until the %g-style cutoff at 1e15.
        assert_eq!(s("(number-to-string 100000.0)"), "100000.0");
        assert_eq!(s("(number-to-string 9e14)"), "900000000000000.0");
        // A long mantissa stays decimal even past 1e15 (cutoff is exp >= D).
        assert_eq!(
            s("(number-to-string 1234567890123456.0)"),
            "1234567890123456.0"
        );
        // Round, large/small powers switch to exponent form: single-digit
        // mantissa bare, exponent signed and zero-padded to two digits.
        assert_eq!(s("(number-to-string 1e15)"), "1e+15");
        assert_eq!(s("(number-to-string 1.5e15)"), "1.5e+15");
        assert_eq!(s("(number-to-string 1e20)"), "1e+20");
        assert_eq!(s("(number-to-string 6.022e23)"), "6.022e+23");
        assert_eq!(s("(number-to-string 1e100)"), "1e+100");
        // Lower cutoff is exponent < -4.
        assert_eq!(s("(number-to-string 1e-4)"), "0.0001");
        assert_eq!(s("(number-to-string 9e-4)"), "0.0009");
        assert_eq!(s("(number-to-string 1e-5)"), "1e-05");
        assert_eq!(s("(number-to-string 9e-5)"), "9e-05");
        assert_eq!(s("(number-to-string 1e-100)"), "1e-100");
    }

    #[test]
    fn case_funcs() {
        assert_eq!(s(r#"(upcase "abc")"#), "ABC");
        assert_eq!(s(r#"(downcase "ABC")"#), "abc");
        // Unicode-aware.
        assert_eq!(s(r#"(upcase "café")"#), "CAFÉ");
        assert_eq!(s(r#"(downcase "CAFÉ")"#), "café");
        // capitalize: first letter of each word up, rest down; digits are
        // word constituents so "foo2bar" stays one word.
        assert_eq!(s(r#"(capitalize "hello WORLD")"#), "Hello World");
        assert_eq!(s(r#"(capitalize "foo-bar baz")"#), "Foo-Bar Baz");
        assert_eq!(s(r#"(capitalize "foo2bar")"#), "Foo2bar");
        assert_eq!(s(r#"(capitalize "")"#), "");
    }

    #[test]
    fn chars() {
        assert_eq!(s("(char-to-string 65)"), "A");
        assert_eq!(s("(char-to-string 233)"), "é");
        assert_eq!(i(r#"(string-to-char "A")"#), 65);
        assert_eq!(i(r#"(string-to-char "é")"#), 233);
        // Empty string -> 0.
        assert_eq!(i(r#"(string-to-char "")"#), 0);
    }

    #[test]
    fn join() {
        assert_eq!(s(r#"(string-join (list "a" "b" "c") "-")"#), "a-b-c");
        // No separator concatenates.
        assert_eq!(s(r#"(string-join (list "a" "b" "c"))"#), "abc");
        assert_eq!(s(r#"(string-join (list))"#), "");
    }

    #[test]
    fn empty_p() {
        assert!(truthy(r#"(string-empty-p "")"#));
        assert!(!truthy(r#"(string-empty-p "x")"#));
    }
}
