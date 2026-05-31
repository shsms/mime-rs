//! mimectl — the CLI client.
//!
//! Two modes share one parser:
//!
//! * One-shot, embedded (no daemon):
//!   `mimectl run --local PROG.tl [--file FILE] [--write]` runs a tulisp edit
//!   program against FILE (or stdin) in-process and prints the structured JSON
//!   result; `--write` saves the edited text back to FILE.
//!
//! * Daemon-backed (talks to `mimed` over its unix socket — `$MIME_SOCKET` or
//!   `/tmp/mimed.sock`):
//!     - `mimectl --session S open --file FILE` / `--text STR`
//!     - `mimectl --session S run PROG.tl`
//!     - `mimectl --session S save --path PATH`
//!     - `mimectl --session S close`
//!     - `mimectl status` — live sessions, the allowed roots, and audit state
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
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let args = parse(&argv);

    let Some(verb) = args.verb.clone() else {
        usage();
        exit(2);
    };

    if args.local {
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
            "run" | "rehearse" | "open" | "status" | "save" | "close" if a.verb.is_none() => {
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
    let store: Box<dyn mime_rs::TextStore> = match &args.file {
        Some(f) => {
            let path = match mime_rs::safety::check_path(std::path::Path::new(f)) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("{e}");
                    exit(2);
                }
            };
            match mime_rs::Quire::open(&path) {
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
            Box::new(mime_rs::Buffer::from_string("*stdin*", s))
        }
    };

    // `rehearse` rolls back inside the workspace, so the result is identical to
    // discarding the report; we just never write it back. `run` may persist.
    let mut ws = mime_rs::Workspace::new(store);
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
                        let path = match mime_rs::safety::check_path(std::path::Path::new(f)) {
                            Ok(p) => p,
                            Err(e) => {
                                eprintln!("{e}");
                                exit(1);
                            }
                        };
                        if let Err(e) =
                            mime_rs::safety::write_atomic(&path, report.final_text.as_bytes())
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

/// The daemon-backed path: build the JSON request for `verb`, send it to
/// `mimed`, and print the response line.
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
                eprintln!("{verb} needs a program path: mimectl --session S {verb} PROG.tl");
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

/// Send one JSON request line to `mimed` and return its one response line.
fn request(req: &serde_json::Value) -> String {
    let path = std::env::var("MIME_SOCKET").unwrap_or_else(|_| DEFAULT_SOCKET.to_string());
    let stream = UnixStream::connect(&path).unwrap_or_else(|e| {
        eprintln!("cannot connect to mimed at {path}: {e} (is mimed running?)");
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
         mimectl run --local PROG.tl [--file FILE] [--write]   (embedded one-shot)\n  \
         mimectl rehearse --local PROG.tl [--file FILE]        (embedded dry-run; never writes)\n  \
         mimectl --session S open --file FILE | --text STR     (daemon)\n  \
         mimectl --session S run PROG.tl                       (daemon)\n  \
         mimectl --session S rehearse PROG.tl                  (daemon dry-run; nothing persists)\n  \
         mimectl --session S save --path PATH                  (daemon)\n  \
         mimectl --session S close                             (daemon)\n  \
         mimectl status                                        (daemon)"
    );
}

fn read_or_die(path: &str, what: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("cannot read {what} {path}: {e}");
        exit(2);
    })
}
