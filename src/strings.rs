//! String library (a mime-rs module, RE2-backed) — Emacs-Lisp string functions,
//! pure string→string with no buffer state. This is the user's key insight: the
//! Emacs string/regex layer is a mime-rs module, not a tulisp change. M0 seed;
//! subagents extend (`string-match`/`match-string`, `split-string` keeping
//! separators, `string-trim`, `number-to-string`, …).
use tulisp::{Error, TulispContext};

pub fn register(ctx: &mut TulispContext) {
    // (replace-regexp-in-string REGEXP REP STRING) — REP is a template with
    // `\N` (group) / `\&` (whole match) backrefs.
    ctx.defun(
        "replace-regexp-in-string",
        |regexp: String, rep: String, s: String| -> Result<String, Error> {
            let rx = regex::Regex::new(&regexp)
                .map_err(|e| Error::lisp_error(format!("Invalid regexp: {e}")))?;
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
                Some(re) => match regex::Regex::new(&re) {
                    Ok(rx) => rx.split(&s).map(str::to_string).collect(),
                    Err(_) => vec![s],
                },
                None => s.split_whitespace().map(str::to_string).collect(),
            }
        },
    );
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
