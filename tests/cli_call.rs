//! `mime call`: MCP tool calls from the shell, driven through the binary.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// A fresh directory for one test, removed when the guard drops.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let p = std::env::temp_dir().join(format!("mime-call-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("create temp dir");
        TempDir(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run `mime ARGS` confined to `root`, with `stdin` piped in.
fn mime(root: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mime"))
        .args(args)
        .env("MIME_ROOTS", root)
        .env_remove("MIME_EXEC")
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mime");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().expect("mime runs")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn one_call_prints_its_text_and_saves_like_the_server() {
    let dir = TempDir::new("one");
    std::fs::write(dir.0.join("a.txt"), "one\ntwo\n").unwrap();

    let out = mime(
        &dir.0,
        &[
            "call",
            "replace_text",
            r#"{"path": "a.txt", "old": "two", "new": "TWO"}"#,
        ],
        "",
    );
    assert!(out.status.success(), "stderr: {}", text(&out.stderr));
    // `old`/`new` are the server's aliases: the same dispatch, not a copy.
    assert!(text(&out.stdout).contains("replaced 1 occurrence at line 2"));
    assert_eq!(
        std::fs::read_to_string(dir.0.join("a.txt")).unwrap(),
        "one\nTWO\n"
    );
}

#[test]
fn a_failed_call_goes_to_stderr_and_exits_1() {
    let dir = TempDir::new("fail");
    std::fs::write(dir.0.join("a.txt"), "one\n").unwrap();

    let out = mime(
        &dir.0,
        &["call", "replace_text", "-"],
        r#"{"path": "a.txt", "pattern": "nope", "replacement": "x"}"#,
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stdout).is_empty());
    assert!(text(&out.stderr).contains("no match for the pattern \"nope\""));

    // Outside the roots, and a tool stdio does not list, fail the same way.
    let out = mime(
        &dir.0,
        &["call", "view", r#"{"path": "/etc/hostname"}"#],
        "",
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("outside the allowed roots"));
    let out = mime(&dir.0, &["call", "open_workspace"], "");
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("unknown tool: open_workspace"));
}

#[test]
fn json_prints_the_whole_result() {
    let dir = TempDir::new("json");
    std::fs::write(dir.0.join("a.txt"), "one\n").unwrap();
    let out = mime(
        &dir.0,
        &[
            "call",
            "run_program",
            r#"{"path": "a.txt", "program": "(+ 1 2)"}"#,
            "--json",
        ],
        "",
    );
    assert!(out.status.success(), "stderr: {}", text(&out.stderr));
    let result: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["isError"], false);
    assert_eq!(result["structuredContent"]["value"], "3");
}

#[test]
fn a_script_runs_every_call_in_one_warm_workspace() {
    let dir = TempDir::new("script");
    std::fs::write(dir.0.join("a.txt"), "one\n").unwrap();
    let script = dir.0.join("repro.jsonl");
    std::fs::write(
        &script,
        r#"# an unsaved edit, seen by the next call
{"name": "replace_text", "arguments": {"path": "a.txt", "pattern": "one", "replacement": "ONE", "save": false}}

{"name": "unsaved_diff", "arguments": {"path": "a.txt"}}
{"name": "undo_last", "arguments": {"path": "a.txt", "save": false}}
{"name": "undo_last", "arguments": {"path": "a.txt"}}
"#,
    )
    .unwrap();

    let out = mime(&dir.0, &["call", "--script", "repro.jsonl"], "");
    let stdout = text(&out.stdout);
    // The last undo has nothing left to undo: the script runs to the end and
    // exits 1.
    assert_eq!(out.status.code(), Some(1), "stdout: {stdout}");
    let headers: Vec<&str> = stdout.lines().filter(|l| l.starts_with("--- ")).collect();
    assert_eq!(
        headers,
        [
            "--- replace_text",
            "--- unsaved_diff",
            "--- undo_last",
            "--- undo_last (error)"
        ]
    );
    assert!(
        stdout.contains("+ONE"),
        "the diff sees the held edit: {stdout}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.0.join("a.txt")).unwrap(),
        "one\n"
    );

    // --json: one result per line.
    let out = mime(
        &dir.0,
        &["call", "--script", "-", "--json"],
        r#"{"name": "help"}"#,
    );
    assert!(out.status.success());
    let lines: Vec<&str> = std::str::from_utf8(&out.stdout).unwrap().lines().collect();
    assert_eq!(lines.len(), 1);
    let result: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(result["isError"], false);
}

#[test]
fn an_edit_held_to_the_end_is_reported_as_discarded() {
    let dir = TempDir::new("held");
    std::fs::write(dir.0.join("a.txt"), "one\n").unwrap();
    let held = r#"{"path": "a.txt", "pattern": "one", "replacement": "ONE", "save": false}"#;

    let out = mime(&dir.0, &["call", "replace_text", held], "");
    assert!(out.status.success());
    assert!(
        text(&out.stderr).contains("a.txt were discarded on exit"),
        "stderr: {}",
        text(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(dir.0.join("a.txt")).unwrap(),
        "one\n"
    );

    // Saved later in the same script: nothing is lost, nothing to report.
    let script = format!(
        "{{\"name\": \"replace_text\", \"arguments\": {held}}}\n\
         {{\"name\": \"save_buffer\", \"arguments\": {{\"path\": \"a.txt\"}}}}\n"
    );
    let out = mime(&dir.0, &["call", "--script", "-"], &script);
    assert!(out.status.success());
    assert!(
        text(&out.stderr).is_empty(),
        "stderr: {}",
        text(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(dir.0.join("a.txt")).unwrap(),
        "ONE\n"
    );

    // Two sessions holding edits to one file: the file is named once.
    let script = [
        r#"{"name": "open_file", "arguments": {"path": "a.txt", "session": "s1"}}"#,
        r#"{"name": "open_file", "arguments": {"path": "a.txt", "session": "s2"}}"#,
        r#"{"name": "replace_text", "arguments": {"session": "s1", "pattern": "ONE", "replacement": "1", "save": false}}"#,
        r#"{"name": "replace_text", "arguments": {"session": "s2", "pattern": "ONE", "replacement": "2", "save": false}}"#,
    ]
    .join("\n");
    let out = mime(&dir.0, &["call", "--script", "-"], &script);
    assert!(out.status.success());
    assert_eq!(
        text(&out.stderr).matches("a.txt").count(),
        1,
        "stderr: {}",
        text(&out.stderr)
    );
}

#[test]
fn a_bad_script_line_runs_nothing() {
    let dir = TempDir::new("bad-script");
    std::fs::write(dir.0.join("a.txt"), "one\n").unwrap();
    let out = mime(
        &dir.0,
        &["call", "--script", "-"],
        "{\"name\": \"replace_text\", \"arguments\": {\"path\": \"a.txt\", \"pattern\": \"one\", \"replacement\": \"ONE\"}}\n{\"arguments\": {}}\n",
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("script line 2: no \"name\""));
    assert_eq!(
        std::fs::read_to_string(dir.0.join("a.txt")).unwrap(),
        "one\n"
    );
}

#[test]
fn usage_errors_exit_2() {
    let dir = TempDir::new("usage");
    for args in [
        &["call"][..],
        &["call", "view", "[1]"],
        &["call", "view", "not json"],
        &["call", "--frob", "view"],
        &["call", "--script"],
    ] {
        let out = mime(&dir.0, args, "");
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(text(&out.stderr).contains("usage: mime call"), "{args:?}");
    }
}
