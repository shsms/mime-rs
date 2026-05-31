//! mimectl — the CLI client.
//!
//! M0: `mimectl run --local PROG.tl [--file FILE] [--write]` runs a tulisp edit
//! program against FILE (or stdin) in an in-memory buffer and prints the
//! structured JSON result. `--write` saves the edited text back to FILE. The
//! daemon-backed forms (`mimectl run` against `mimed`, checkpoints, …) come later.
use std::io::Read;
use std::process::exit;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut prog_path: Option<String> = None;
    let mut file_path: Option<String> = None;
    let mut write_back = false;
    let mut saw_run = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "run" => saw_run = true,
            "--local" => {}
            "--write" => write_back = true,
            "--file" => {
                i += 1;
                file_path = args.get(i).cloned();
            }
            other if prog_path.is_none() => prog_path = Some(other.to_string()),
            other => {
                eprintln!("unexpected argument: {other}");
                exit(2);
            }
        }
        i += 1;
    }

    let Some(prog_path) = prog_path.filter(|_| saw_run) else {
        eprintln!("usage: mimectl run --local PROG.tl [--file FILE] [--write]");
        exit(2);
    };

    let program = read_or_die(&prog_path, "program");
    let (name, text) = match &file_path {
        Some(f) => (f.clone(), read_or_die(f, "file")),
        None => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s).ok();
            ("*stdin*".to_string(), s)
        }
    };

    let buffer = mime_rs::Buffer::from_string(name, text);
    match mime_rs::run_program(buffer, &program) {
        Ok(report) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report.to_json()).unwrap()
            );
            if write_back {
                match &file_path {
                    Some(f) => {
                        if let Err(e) = std::fs::write(f, &report.final_text) {
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

fn read_or_die(path: &str, what: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("cannot read {what} {path}: {e}");
        exit(2);
    })
}
