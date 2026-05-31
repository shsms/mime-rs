//! The CLI front-end (the `mime` default mode).
//!
//! Three modes share one parser:
//!
//! * One-shot, embedded (no daemon):
//!   `mime run --local PROG.tl [--file FILE] [--write]` runs a tulisp edit
//!   program against FILE (or stdin) in-process and prints the structured JSON
//!   result; `--write` saves the edited text back to FILE.
//!
//! * Interactive, embedded (no daemon):
//!   `mime repl [--file FILE]` opens a warm in-process session and reads
//!   tulisp from stdin a form at a time, printing the diff + reports + value for
//!   each. State (buffer, kill-ring, checkpoints, `defun`s) persists across
//!   lines; nothing is ever written back to disk.
//!
//! * Daemon-backed (talks to `mime --daemon` over its unix socket —
//!   `$MIME_SOCKET` or `/tmp/mimed.sock`):
//!     - `mime --session S open --file FILE` / `--text STR`
//!     - `mime --session S run PROG.tl`
//!     - `mime --session S save --path PATH`
//!     - `mime --session S close`
//!     - `mime status` — live sessions, the allowed roots, and audit state
//!
//! The session comes from `--session` or `$MIME_SESSION`. Each verb is sent as
//! one JSON request line; the daemon's JSON response line is printed verbatim.
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::exit;

const DEFAULT_SOCKET: &str = "/tmp/mimed.sock";

#[derive(Default)]
struct Args {
    verb: Option<String>,
    local: bool,
    write_back: bool,
    session: Option<String>,
    file: Option<String>,
    text: Option<String>,
    path: Option<String>,
    prog_path: Option<String>,
    /// Extra CLI arguments that are not known mime flags — passed through to the
    /// trusted program as `(arg "KEY")` values. `--KEY VALUE` becomes
    /// `(KEY, VALUE)`; a bare `--KEY` (next is another `--…` or the end) becomes
    /// `(KEY, "t")`. Only the local trusted run path forwards these on.
    prog_args: Vec<(String, String)>,
}

pub fn run() {
    let argv: Vec<String> = std::env::args().collect();
    let args = parse(&argv);

    let Some(verb) = args.verb.clone() else {
        usage();
        exit(2);
    };

    // `repl` is its own thing: an interactive, warm, in-process session that
    // reads tulisp from stdin (no daemon, no `--local` needed).
    if verb == "repl" {
        run_repl(&args);
    } else if args.local {
        run_local(&args, &verb);
    } else {
        run_daemon(&args, &verb);
    }
}

/// Parse argv into [`Args`]. The verb is the first bare word among the known
/// verbs; any other bare word is the program path (for `run`).
fn parse(argv: &[String]) -> Args {
    let mut a = Args::default();
    let mut i = 1;
    while i < argv.len() {
        match argv[i].as_str() {
            "run" | "rehearse" | "open" | "status" | "save" | "close" | "repl"
                if a.verb.is_none() =>
            {
                a.verb = Some(argv[i].clone());
            }
            "--local" => a.local = true,
            "--write" => a.write_back = true,
            "--session" => {
                i += 1;
                a.session = argv.get(i).cloned();
            }
            "--file" => {
                i += 1;
                a.file = argv.get(i).cloned();
            }
            "--text" => {
                i += 1;
                a.text = argv.get(i).cloned();
            }
            "--path" => {
                i += 1;
                a.path = argv.get(i).cloned();
            }
            // Any other `--KEY` is a program argument forwarded to the trusted
            // program: `--KEY VALUE` → (KEY, VALUE) when the next arg is a plain
            // value; a bare `--KEY` (next is another `--…` or the end) → (KEY,
            // "t"), so a flag reads back as "t".
            flag if flag.starts_with("--") => {
                let key = flag.trim_start_matches('-').to_string();
                match argv.get(i + 1) {
                    Some(v) if !v.starts_with("--") => {
                        a.prog_args.push((key, v.clone()));
                        i += 1;
                    }
                    _ => a.prog_args.push((key, "t".to_string())),
                }
            }
            other if a.prog_path.is_none() => a.prog_path = Some(other.to_string()),
            other => {
                eprintln!("unexpected argument: {other}");
                exit(2);
            }
        }
        i += 1;
    }
    a
}

/// The embedded one-shot path (`--local`). Supports `run` (persisting, honours
/// `--write`) and `rehearse` (dry-run: report only, NEVER writes — even if a
/// stray `--write` is passed).
fn run_local(args: &Args, verb: &str) {
    let rehearse = match verb {
        "run" => false,
        "rehearse" => true,
        _ => {
            eprintln!("--local supports only `run` and `rehearse` (got `{verb}`)");
            exit(2);
        }
    };
    let Some(prog_path) = args.prog_path.as_deref() else {
        usage();
        exit(2);
    };
    let program = read_or_die(prog_path, "program");

    // A file is opened through Quire — the mmap-backed piece-table store (the
    // production path, GB-capable); stdin uses the in-memory Buffer.
    let store: Box<dyn crate::TextStore> = match &args.file {
        Some(f) => {
            let path = match crate::safety::check_path(std::path::Path::new(f)) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("{e}");
                    exit(2);
                }
            };
            match crate::Quire::open(&path) {
                Ok(q) => Box::new(q),
                Err(e) => {
                    eprintln!("cannot open file {f}: {e}");
                    exit(2);
                }
            }
        }
        None => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s).ok();
            Box::new(crate::Buffer::from_string("*stdin*", s))
        }
    };

    // `rehearse` rolls back inside the workspace, so the result is identical to
    // discarding the report; we just never write it back. `run` may persist.
    // Trusted tier: the local `mime` CLI also gets the orchestration group
    // (multiple buffers, file I/O, args) — see Workspace::new_trusted / Capabilities.
    let mut ws = crate::Workspace::new_trusted(store);
    // Forward the extra CLI args (everything that is not a known mime flag) to
    // the trusted program, readable via `(arg "KEY")`. This is what parameterizes
    // a program like add-anno (--date, --anno_path, --infile, …).
    ws.set_program_args(args.prog_args.clone());
    let result = if rehearse {
        ws.rehearse(&program)
    } else {
        ws.run(&program)
    };
    match result {
        Ok(report) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report.to_json()).unwrap()
            );
            // A rehearsal is a dry-run by definition: never touch the file, even
            // if `--write` was passed by mistake.
            if args.write_back && rehearse {
                eprintln!("note: --write ignored for a rehearsal (a rehearsal never writes)");
            } else if args.write_back {
                match &args.file {
                    Some(f) => {
                        let path = match crate::safety::check_path(std::path::Path::new(f)) {
                            Ok(p) => p,
                            Err(e) => {
                                eprintln!("{e}");
                                exit(1);
                            }
                        };
                        if let Err(e) =
                            crate::safety::write_atomic(&path, report.final_text.as_bytes())
                        {
                            eprintln!("cannot write {f}: {e}");
                            exit(1);
                        }
                    }
                    None => eprintln!("--write needs --file (stdin has nowhere to go)"),
                }
            }
        }
        Err(err) => {
            eprintln!("{}", serde_json::json!({ "ok": false, "error": err }));
            exit(1);
        }
    }
}

/// The interactive REPL (`mime repl [--file FILE]`): a single warm,
/// in-process [`Workspace`] that survives across lines, so the buffer, the
/// kill-ring, checkpoints, and any `defun`s persist between expressions. Reads
/// tulisp from stdin a form at a time and prints, for each, the diff (if the
/// buffer changed), any `(report ...)`/`(message ...)` output, and the value.
///
/// `--file FILE` loads FILE into the session first (via Quire, the production
/// store); without it the session starts on an empty in-memory buffer. The REPL
/// never writes anything back to disk — it is a scratchpad for *trying* edits;
/// use `mime --session … save` (daemon) or `run --local … --write` to persist.
fn run_repl(args: &Args) {
    let store: Box<dyn crate::TextStore> = match &args.file {
        Some(f) => {
            let path = match crate::safety::check_path(std::path::Path::new(f)) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("{e}");
                    exit(2);
                }
            };
            match crate::Quire::open(&path) {
                Ok(q) => Box::new(q),
                Err(e) => {
                    eprintln!("cannot open file {f}: {e}");
                    exit(2);
                }
            }
        }
        None => Box::new(crate::Buffer::from_string("*repl*", String::new())),
    };

    // Trusted tier: the local `mime` CLI also gets the orchestration group
    // (multiple buffers, file I/O, args) — see Workspace::new_trusted / Capabilities.
    let mut ws = crate::Workspace::new_trusted(store);
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    repl_loop(&mut ws, stdin.lock(), &mut stdout.lock());
}

/// The REPL read-eval-print loop, factored out over generic `BufRead`/`Write`
/// so it can be driven by canned stdin in tests. Reads lines, accumulates them
/// until a complete (paren-balanced, not mid-string) form is in hand — or a
/// blank line forces a submit — evaluates the form against `ws`, and writes the
/// formatted result. Returns cleanly on EOF.
fn repl_loop<R: BufRead, W: Write>(ws: &mut crate::Workspace, mut input: R, out: &mut W) {
    let prompt = |out: &mut W, first: bool| {
        // A continuation prompt ("..") signals "I'm waiting for the rest of the
        // form"; the primary prompt is "mime>".
        let _ = write!(out, "{}", if first { "mime> " } else { "...   " });
        let _ = out.flush();
    };

    let mut pending = String::new();
    prompt(out, true);
    let mut line = String::new();
    loop {
        line.clear();
        match input.read_line(&mut line) {
            Ok(0) => {
                // EOF. Flush a trailing partial form (so a file piped without a
                // final newline still runs), then leave.
                if !pending.trim().is_empty() {
                    eval_and_print(ws, &pending, out);
                }
                let _ = writeln!(out);
                let _ = out.flush();
                return;
            }
            Ok(_) => {}
            Err(e) => {
                let _ = writeln!(out, "input error: {e}");
                return;
            }
        }
        let blank = line.trim().is_empty();
        pending.push_str(&line);

        // A blank line submits whatever has accumulated (and is the way to force
        // an incomplete form to run, or to clear a stray one). Otherwise we wait
        // until the form is balanced.
        if pending.trim().is_empty() {
            // Nothing buffered yet — a blank line at the primary prompt is a no-op.
            pending.clear();
            prompt(out, true);
            continue;
        }
        if blank || form_complete(&pending) {
            eval_and_print(ws, &pending, out);
            pending.clear();
            prompt(out, true);
        } else {
            prompt(out, false);
        }
    }
}

/// Evaluate one REPL form against `ws` and write the formatted result: the diff
/// when the buffer changed, each `(message …)` line, each `(report K V)` pair,
/// and the value (tulisp-printed) after `=>`. A tulisp error prints as
/// `error: …` and does not abort the loop.
fn eval_and_print<W: Write>(ws: &mut crate::Workspace, form: &str, out: &mut W) {
    match ws.run_value(form) {
        Ok((report, value)) => {
            // The diff is the agent-facing payload — show it only when the edit
            // actually changed the buffer.
            if report.dirty && !report.diff.is_empty() {
                let _ = write!(out, "{}", report.diff);
            }
            for line in &report.log {
                let _ = writeln!(out, "{line}");
            }
            for (k, v) in &report.reports {
                let _ = writeln!(out, "{k}: {v}");
            }
            let _ = writeln!(out, "=> {value}");
        }
        Err(e) => {
            let _ = writeln!(out, "error: {e}");
        }
    }
    let _ = out.flush();
}

/// Whether `src` holds at least one complete top-level form: parentheses
/// balanced and not currently inside a string. Tracks `\` escapes and `?c`
/// character literals so a `(` / `"` inside a string or a `?(` char literal
/// doesn't throw the count off. A `)` with depth already 0 also counts as
/// "complete" (a stray close still submits, so the error surfaces).
fn form_complete(src: &str) -> bool {
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut char_literal = false;
    let mut in_comment = false;
    let mut saw_atom = false;
    for c in src.chars() {
        if in_comment {
            // A `;` comment runs to end of line; nothing on it counts.
            if c == '\n' {
                in_comment = false;
            }
            continue;
        }
        if escaped {
            // The previous char was a backslash; this one is consumed literally.
            escaped = false;
            continue;
        }
        if char_literal {
            // `?c` — c is a single literal char that moves no paren depth. A
            // `?\(` / `?\n` escape spends one more char: hand the backslash to
            // the `escaped` branch so the *next* char is the literal, not a paren.
            char_literal = false;
            saw_atom = true;
            if c == '\\' {
                escaped = true;
            }
            continue;
        }
        if in_string {
            match c {
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' => {
                in_string = true;
                saw_atom = true;
            }
            '?' => char_literal = true,
            ';' => in_comment = true,
            '(' | '[' => {
                depth += 1;
                saw_atom = true;
            }
            ')' | ']' => {
                depth -= 1;
                saw_atom = true;
                if depth <= 0 {
                    return true;
                }
            }
            c if !c.is_whitespace() => saw_atom = true,
            _ => {}
        }
    }
    // A bare atom on its own (a parenthesized form is caught above; `42` or a
    // symbol is complete once we've seen non-whitespace and aren't mid-string,
    // -paren, or -comment).
    depth == 0 && !in_string && !in_comment && saw_atom
}

/// The daemon-backed path: build the JSON request for `verb`, send it to
/// the daemon, and print the response line.
fn run_daemon(args: &Args, verb: &str) {
    let req = match verb {
        "status" => serde_json::json!({ "op": "status" }),
        "open" => {
            let session = require_session(args);
            match (&args.file, &args.text) {
                (Some(f), _) => {
                    serde_json::json!({ "op": "open", "session": session, "file": f })
                }
                (None, Some(t)) => {
                    serde_json::json!({ "op": "open", "session": session, "text": t })
                }
                (None, None) => {
                    eprintln!("open needs --file FILE or --text STR");
                    exit(2);
                }
            }
        }
        "run" | "rehearse" => {
            let session = require_session(args);
            let Some(prog_path) = args.prog_path.as_deref() else {
                eprintln!("{verb} needs a program path: mime --session S {verb} PROG.tl");
                exit(2);
            };
            let program = read_or_die(prog_path, "program");
            // `rehearse` is the dry-run twin of `run`: same request shape, the
            // daemon rolls the session back and never persists.
            serde_json::json!({ "op": verb, "session": session, "program": program })
        }
        "save" => {
            let session = require_session(args);
            let Some(path) = args.path.as_deref() else {
                eprintln!("save needs --path PATH");
                exit(2);
            };
            serde_json::json!({ "op": "save", "session": session, "path": path })
        }
        "close" => {
            let session = require_session(args);
            serde_json::json!({ "op": "close", "session": session })
        }
        other => {
            eprintln!("unknown verb: {other}");
            exit(2);
        }
    };

    let response = request(&req);
    // Pretty-print if the daemon returned JSON (it always does); fall back to raw.
    match serde_json::from_str::<serde_json::Value>(&response) {
        Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap()),
        Err(_) => println!("{response}"),
    }
    // Exit non-zero when the daemon reported failure, so scripts can branch on it.
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&response)
        && v.get("ok").and_then(serde_json::Value::as_bool) == Some(false)
    {
        exit(1);
    }
}

/// Send one JSON request line to the daemon and return its one response line.
fn request(req: &serde_json::Value) -> String {
    let path = std::env::var("MIME_SOCKET").unwrap_or_else(|_| DEFAULT_SOCKET.to_string());
    let stream = UnixStream::connect(&path).unwrap_or_else(|e| {
        eprintln!("cannot connect to mimed at {path}: {e} (is `mime --daemon` running?)");
        exit(2);
    });
    let mut writer = stream.try_clone().unwrap_or_else(|e| {
        eprintln!("socket error: {e}");
        exit(2);
    });
    writeln!(writer, "{req}").unwrap_or_else(|e| {
        eprintln!("write error: {e}");
        exit(2);
    });
    writer.flush().ok();

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        eprintln!("mimed closed the connection without responding");
        exit(2);
    }
    line.trim_end().to_string()
}

fn require_session(args: &Args) -> String {
    args.session
        .clone()
        .or_else(|| std::env::var("MIME_SESSION").ok())
        .unwrap_or_else(|| {
            eprintln!("no session: pass --session S or set $MIME_SESSION");
            exit(2);
        })
}

fn usage() {
    eprintln!(
        "usage:\n  \
         mime run --local PROG.tl [--file FILE] [--write]      (embedded one-shot)\n  \
         mime rehearse --local PROG.tl [--file FILE]           (embedded dry-run; never writes)\n  \
         mime repl [--file FILE]                               (interactive warm session; never writes)\n  \
         mime --session S open --file FILE | --text STR        (daemon)\n  \
         mime --session S run PROG.tl                          (daemon)\n  \
         mime --session S rehearse PROG.tl                     (daemon dry-run; nothing persists)\n  \
         mime --session S save --path PATH                     (daemon)\n  \
         mime --session S close                                (daemon)\n  \
         mime status                                           (daemon)"
    );
}

fn read_or_die(path: &str, what: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("cannot read {what} {path}: {e}");
        exit(2);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive `repl_loop` with canned stdin against a fresh workspace seeded with
    /// `text`, and return everything it wrote to stdout as a String.
    fn repl(text: &str, stdin: &str) -> String {
        let mut ws = crate::Workspace::new(Box::new(crate::Buffer::from_string("*t*", text)));
        let mut out: Vec<u8> = Vec::new();
        repl_loop(&mut ws, std::io::Cursor::new(stdin.to_string()), &mut out);
        String::from_utf8(out).unwrap()
    }

    /// argv as `parse` expects it: argv[0] is the program name, ignored.
    fn argv(rest: &[&str]) -> Vec<String> {
        std::iter::once("mime".to_string())
            .chain(rest.iter().map(|s| s.to_string()))
            .collect()
    }

    #[test]
    fn parse_collects_unknown_flags_as_program_args() {
        // Known flags (--local, --file, --write) and the program path are still
        // consumed normally; every other `--KEY` is forwarded as a program arg.
        let a = parse(&argv(&[
            "run",
            "--local",
            "prog.tl",
            "--file",
            "doc.md",
            "--write",
            "--date",
            "May 1",
            "--anno_path",
            "anno.json",
            "--with_badges",
        ]));
        assert!(a.local);
        assert!(a.write_back);
        assert_eq!(a.file.as_deref(), Some("doc.md"));
        assert_eq!(a.prog_path.as_deref(), Some("prog.tl"));
        // `--KEY VALUE` → (KEY, VALUE); a trailing bare `--KEY` → (KEY, "t").
        assert_eq!(
            a.prog_args,
            vec![
                ("date".to_string(), "May 1".to_string()),
                ("anno_path".to_string(), "anno.json".to_string()),
                ("with_badges".to_string(), "t".to_string()),
            ]
        );
    }

    #[test]
    fn parse_treats_a_bare_flag_before_another_flag_as_t() {
        // `--with_badges` followed by another `--KEY` (not a value) is a bare flag.
        let a = parse(&argv(&["run", "--with_badges", "--date", "May 1"]));
        assert_eq!(
            a.prog_args,
            vec![
                ("with_badges".to_string(), "t".to_string()),
                ("date".to_string(), "May 1".to_string()),
            ]
        );
    }

    #[test]
    fn form_complete_balances_parens_strings_and_atoms() {
        assert!(form_complete("(point)"));
        assert!(form_complete("42"));
        assert!(form_complete("(insert \"a)b\")")); // close paren inside a string
        assert!(!form_complete("(insert \"unclosed")); // mid-string
        assert!(!form_complete("(+ 1 2")); // unbalanced
        assert!(!form_complete("   ")); // nothing but whitespace
        assert!(!form_complete("")); // empty
        // A `?(` char literal must not be counted as an open paren.
        assert!(form_complete("(insert (char-to-string ?\\())"));
        // A `;` comment to end of line is ignored, so the trailing quote/paren
        // on the comment doesn't open a string or a list.
        assert!(form_complete("(point) ; a \"comment\" with ( parens"));
    }

    #[test]
    fn repl_prints_value_with_arrow() {
        // A pure expression: no diff, value printed after `=>`, then EOF.
        let out = repl("", "(+ 1 2)\n");
        assert!(out.contains("=> 3"), "got: {out:?}");
        // A string value renders quoted, the way tulisp prints it.
        let out = repl("hello", "(buffer-string)\n");
        assert!(out.contains("=> \"hello\""), "got: {out:?}");
    }

    #[test]
    fn repl_shows_diff_reports_and_messages_for_an_edit() {
        let out = repl(
            "hello",
            "(goto-char (point-max)) (insert \" world\") (message \"did it\") (report \"len\" (point-max))\n",
        );
        // The diff (buffer changed)...
        assert!(out.contains("-hello"), "missing diff: {out:?}");
        assert!(out.contains("+hello world"), "missing diff: {out:?}");
        // ...the message line, the report pair, and the value.
        assert!(out.contains("did it"), "missing message: {out:?}");
        assert!(out.contains("len: 12"), "missing report: {out:?}");
        assert!(out.contains("=> "), "missing value: {out:?}");
    }

    #[test]
    fn repl_state_persists_across_lines() {
        // Two separate submissions: the first edits the buffer, the second sees
        // the edit — proving the workspace is warm across lines.
        let out = repl("", "(insert \"abc\")\n(upcase-region 1 4)\n");
        assert!(out.contains("+ABC") || out.contains("ABC"), "got: {out:?}");
        // A defun defined on one line is callable on the next.
        let out = repl(
            "",
            "(defun greet () (insert \"hi\"))\n(greet)\n(buffer-string)\n",
        );
        assert!(out.contains("=> \"hi\""), "defun did not persist: {out:?}");
    }

    #[test]
    fn repl_accumulates_a_multi_line_form() {
        // A form split across lines submits only when balanced.
        let out = repl("", "(+ 1\n   2\n   3)\n");
        assert!(out.contains("=> 6"), "multi-line form failed: {out:?}");
    }

    #[test]
    fn repl_reports_errors_without_aborting() {
        // A bad form prints `error:` and the loop keeps going to the next form.
        let out = repl("", "(no-such-fn)\n(+ 2 2)\n");
        assert!(out.contains("error:"), "missing error: {out:?}");
        assert!(out.contains("=> 4"), "loop did not continue: {out:?}");
    }

    #[test]
    fn repl_handles_eof_on_a_trailing_partial_form() {
        // stdin ends without a newline mid-input: the buffered form still runs.
        let out = repl("", "(+ 10 20)");
        assert!(out.contains("=> 30"), "trailing form not flushed: {out:?}");
    }
}
