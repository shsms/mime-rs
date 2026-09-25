//! Integration test for the MCP server (`mime --mcp`).
//!
//! Spawns the built `mime` binary in MCP mode as a subprocess with piped
//! stdin/stdout and drives it with real JSON-RPC 2.0 lines, asserting on the
//! responses. This exercises the full stdio protocol path — handshake,
//! `tools/list`, `tools/call` for a real edit program, and the checkpoint →
//! mutate → restore round-trip — exactly as an MCP client would.
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

/// A live `mime --mcp` subprocess with line-buffered stdin/stdout handles.
struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Server {
    fn spawn() -> Server {
        Server::spawn_with_env(&[])
    }

    /// Spawn `mime --mcp` with extra environment variables (e.g. `MIME_ROOTS`,
    /// `MIME_AUDIT`) — used by the safety tests.
    fn spawn_with_env(env: &[(&str, &std::path::Path)]) -> Server {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mime"));
        cmd.arg("--mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            // Tests opt into MIME_EXEC explicitly; never inherit the caller's.
            .env_remove("MIME_EXEC");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn mime --mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Server {
            child,
            stdin,
            stdout,
        }
    }

    /// Write one JSON-RPC request line and read exactly one response line back.
    /// Use for *requests* (which always reply).
    fn request(&mut self, req: Value) -> Value {
        self.send(&req);
        self.read_line()
    }

    /// Write a notification (no response is expected, so we don't read).
    fn notify(&mut self, req: Value) {
        self.send(&req);
    }

    fn send(&mut self, req: &Value) {
        writeln!(self.stdin, "{req}").expect("write request");
        self.stdin.flush().expect("flush");
    }

    fn read_line(&mut self) -> Value {
        let mut line = String::new();
        let n = self.stdout.read_line(&mut line).expect("read response");
        assert!(n > 0, "server closed stdout unexpectedly");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad JSON response {line:?}: {e}"))
    }

    /// Call a tool and return the text of its first content block, asserting it
    /// did not error.
    fn call_ok(&mut self, id: i64, name: &str, arguments: Value) -> String {
        let resp = self.request(json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        }));
        assert_eq!(resp["id"], id);
        let result = &resp["result"];
        assert_eq!(
            result["isError"], false,
            "tool {name} unexpectedly errored: {result}"
        );
        result["content"][0]["text"]
            .as_str()
            .expect("text content")
            .to_string()
    }

    /// Call a tool expecting a tool-level failure; assert `isError` and return
    /// the error text.
    fn call_err(&mut self, id: i64, name: &str, arguments: Value) -> String {
        let resp = self.request(json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        }));
        assert_eq!(resp["id"], id);
        let result = &resp["result"];
        assert_eq!(
            result["isError"], true,
            "tool {name} unexpectedly succeeded: {result}"
        );
        result["content"][0]["text"]
            .as_str()
            .expect("text content")
            .to_string()
    }

    /// The buffer text of `start`/`end` (the chars [start, end)) or `lines: [a,
    /// b]` (whole lines, inclusive), read through run_program with `save:
    /// false`. `args` also carries `path` or `session` when the call needs one.
    fn read_text(&mut self, id: i64, args: Value) -> String {
        let mut call = serde_json::Map::new();
        for key in ["path", "session"] {
            if let Some(v) = args.get(key) {
                call.insert(key.to_string(), v.clone());
            }
        }
        let program = match args.get("lines") {
            Some(l) => format!(
                "(save-excursion (goto-line {}) (beginning-of-line) \
                 (let ((s (point))) (goto-line {}) (end-of-line) \
                 (buffer-substring s (point))))",
                l[0], l[1]
            ),
            None => format!("(buffer-substring {} {})", args["start"], args["end"]),
        };
        call.insert("program".to_string(), json!(program));
        call.insert("save".to_string(), json!(false));
        let out = self.call_ok(id, "run_program", Value::Object(call));
        let report: Value = serde_json::from_str(&out).expect("run_program answers JSON");
        report["value"].as_str().unwrap_or("").to_string()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `view`'s numbered body for `text` from line `first`: the gutter `window`
/// draws, without the focus mark, one trailing newline ending the last line.
fn numbered(first: usize, text: &str) -> String {
    let body = text.strip_suffix('\n').unwrap_or(text);
    body.split('\n')
        .enumerate()
        .map(|(i, line)| format!("{:>5}   {line}\n", first + i))
        .collect()
}

#[test]
fn full_session_round_trip_over_stdio() {
    let mut s = Server::spawn();

    // --- handshake: initialize (request) ---
    let init = s.request(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2024-11-05", "capabilities": {} },
    }));
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["serverInfo"]["name"], "mime-rs");
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
    assert!(init["result"]["capabilities"]["tools"].is_object());

    // notifications/initialized is a notification — it must NOT produce a
    // response. We send it, then immediately issue tools/list and confirm the
    // *next* line we read is the tools/list reply (id 2), proving no stray
    // notification response was emitted.
    s.notify(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));

    // --- tools/list contains run_program (and the rest) ---
    let list = s.request(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
    assert_eq!(list["id"], 2);
    let tools = list["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    for expected in [
        "open_file",
        "open_text",
        "run_program",
        "view",
        "replace_text",
        "occur",
        "conflicts",
        "undo_last",
        "save_buffer",
        "session_status",
        "outline",
        "close_session",
        "help",
    ] {
        assert!(names.contains(&expected), "tools/list missing {expected}");
    }
    // Each tool advertises an object inputSchema.
    let run_tool = tools
        .iter()
        .find(|t| t["name"] == "run_program")
        .expect("run_program tool");
    assert_eq!(run_tool["inputSchema"]["type"], "object");
    assert_eq!(run_tool["inputSchema"]["required"][0], "program");

    // --- open_text ---
    let opened = s.call_ok(3, "open_text", json!({ "text": "hello world" }));
    assert!(opened.contains("11 chars"), "open_text said: {opened}");

    // --- run_program with a real edit; assert the diff/report comes back ---
    let report_text = s.call_ok(
        4,
        "run_program",
        json!({ "program": r#"(while (re-search-forward "world" nil t) (replace-match "mime")) (report "done" 1)"# }),
    );
    let report: Value = serde_json::from_str(&report_text).expect("RunReport is JSON");
    assert_eq!(report["ok"], true);
    assert_eq!(report["dirty"], true);
    assert!(
        report["diff"].as_str().unwrap().contains("+hello mime"),
        "diff was: {}",
        report["diff"]
    );
    assert!(
        report["diff"].as_str().unwrap().contains("-hello world"),
        "diff was: {}",
        report["diff"]
    );
    assert_eq!(report["reports"]["done"], "1");

    // --- read_text pulls text on demand without mutating ---
    let region = s.read_text(5, json!({ "start": 1, "end": 6 }));
    assert_eq!(region, "hello");

    // --- Lisp checkpoint, mutate, restore-checkpoint, confirm the revert ---
    s.call_ok(
        6,
        "run_program",
        json!({ "program": r#"(checkpoint "before")"# }),
    );
    let mutated = s.call_ok(
        7,
        "run_program",
        json!({ "program": r#"(erase-buffer) (insert "DESTROYED")"# }),
    );
    let mutated: Value = serde_json::from_str(&mutated).unwrap();
    assert_eq!(mutated["len_after"], 9);
    let cps = s.call_ok(8, "session_status", json!({}));
    assert!(cps.contains("before"), "session_status said: {cps}");
    s.call_ok(
        9,
        "run_program",
        json!({ "program": r#"(restore-checkpoint "before")"# }),
    );

    // Confirm the revert via a fresh run_program: buffer is back to the
    // post-edit "hello mime", unchanged by this read-only program.
    let confirm = s.call_ok(
        10,
        "run_program",
        json!({ "program": r#"(goto-char (point-min)) (report "text" (buffer-string))"# }),
    );
    let confirm: Value = serde_json::from_str(&confirm).unwrap();
    assert_eq!(confirm["reports"]["text"], "\"hello mime\"");
    assert_eq!(confirm["dirty"], false);
    assert_eq!(confirm["len_after"], 10);
}

#[test]
fn rehearse_previews_an_edit_then_rolls_back_over_stdio() {
    let mut s = Server::spawn();
    s.request(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }));
    s.call_ok(2, "open_text", json!({ "text": "hello world" }));

    // rehearse the same replace the run_program round-trip does — but it must
    // NOT stick.
    let report_text = s.call_ok(
        3,
        "run_program",
        json!({ "program": r#"(while (re-search-forward "world" nil t) (replace-match "mime")) (report "done" 1)"#, "rehearse": true }),
    );
    let report: Value = serde_json::from_str(&report_text).expect("RunReport is JSON");
    // The report shows the hypothetical edit, flagged as a rehearsal.
    assert_eq!(report["ok"], true);
    assert_eq!(report["rehearsed"], true);
    assert_eq!(report["dirty"], true);
    assert!(
        report["diff"].as_str().unwrap().contains("+hello mime"),
        "diff was: {}",
        report["diff"]
    );
    assert_eq!(report["reports"]["done"], "1");

    // But the live buffer is untouched: a follow-up read still sees "hello
    // world".
    let region = s.read_text(4, json!({ "start": 1, "end": 12 }));
    assert_eq!(region, "hello world");

    // And a real run_program afterwards persists normally, proving rehearse
    // left the session fully usable.
    let applied = s.call_ok(
        5,
        "run_program",
        json!({ "program": r#"(while (re-search-forward "world" nil t) (replace-match "mime"))"# }),
    );
    let applied: Value = serde_json::from_str(&applied).unwrap();
    assert_eq!(applied["rehearsed"], false);
    let confirm = s.read_text(6, json!({ "start": 1, "end": 11 }));
    assert_eq!(confirm, "hello mime");
}

#[test]
fn unknown_method_is_jsonrpc_error_and_tool_error_sets_is_error() {
    let mut s = Server::spawn();

    // Unknown method -> JSON-RPC error -32601 (not a tool result).
    let resp = s.request(json!({ "jsonrpc": "2.0", "id": 1, "method": "no/such/method" }));
    assert_eq!(resp["id"], 1);
    assert_eq!(resp["error"]["code"], -32601);
    assert!(resp.get("result").is_none());

    // A tool-level failure (running against a session that was never opened) is
    // a *successful* JSON-RPC call with isError=true.
    let resp = s.request(json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": { "name": "run_program", "arguments": { "program": "(insert \"x\")", "session": "ghost" } },
    }));
    assert_eq!(resp["id"], 2);
    assert_eq!(resp["result"]["isError"], true);
    assert!(
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("no such session"),
        "got: {}",
        resp["result"]["content"][0]["text"]
    );
}

#[test]
fn sessions_are_isolated_and_warm() {
    let mut s = Server::spawn();
    s.request(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }));

    // Two independent sessions.
    s.call_ok(2, "open_text", json!({ "text": "aaa", "session": "one" }));
    s.call_ok(3, "open_text", json!({ "text": "bbb", "session": "two" }));

    // A defun defined in session "one" persists (warmth) and only affects
    // "one".
    s.call_ok(
        4,
        "run_program",
        json!({ "program": r#"(defun tag () (goto-char (point-max)) (insert "!"))"#, "session": "one" }),
    );
    let r = s.call_ok(
        5,
        "run_program",
        json!({ "program": "(tag)", "session": "one" }),
    );
    let r: Value = serde_json::from_str(&r).unwrap();
    assert_eq!(r["len_after"], 4); // "aaa!"

    // session_status lists both sessions and advertises the sandbox: at least
    // one allowed root (defaults to the subprocess cwd here) and audit off (no
    // MIME_AUDIT in this server's env).
    let status = s.call_ok(6, "session_status", json!({}));
    let status: Value = serde_json::from_str(&status).unwrap();
    let ids = status["sessions"].as_array().unwrap();
    assert!(ids.iter().any(|v| v["id"] == "one"));
    assert!(ids.iter().any(|v| v["id"] == "two"));
    // Per-session visibility: buffer, visited file, narrowing, staleness.
    let one = ids.iter().find(|v| v["id"] == "one").unwrap();
    assert_eq!(one["file"], Value::Null, "open_text buffers visit no file");
    assert_eq!(one["narrowed"], false);
    assert_eq!(one["stale"], false);
    let roots = status["roots"].as_array().expect("roots array");
    assert!(!roots.is_empty(), "expected at least one root: {status}");
    assert!(roots.iter().all(|r| r.is_string()), "roots are strings");
    assert_eq!(status["audit"], false, "audit should be off: {status}");

    // "two" is untouched and does not know `tag`.
    let two = s.request(json!({
        "jsonrpc": "2.0", "id": 7, "method": "tools/call",
        "params": { "name": "run_program", "arguments": { "program": "(tag)", "session": "two" } },
    }));
    assert_eq!(
        two["result"]["isError"], true,
        "calling tag in session two should fail: {}",
        two["result"]
    );
}

/// A unique temp directory for one safety test, used as the `MIME_ROOTS` root.
#[test]
fn conflicts_overview_and_resolution_round_trip() {
    let mut s = Server::spawn();
    s.call_ok(
        1,
        "open_text",
        json!({ "text": "intro\n<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> branch\ntail\n" }),
    );

    // The read-only overview names the hunk and its labels.
    let out = s.call_ok(2, "conflicts", json!({}));
    assert!(out.contains("1 conflict"), "got: {out}");
    assert!(out.contains("HEAD ↔ branch"), "got: {out}");

    // Resolution through run_program; the report carries the remaining count.
    let report = s.call_ok(
        3,
        "run_program",
        json!({ "program": r#"(report "left" (conflict-keep "theirs" 1))"# }),
    );
    let report: Value = serde_json::from_str(&report).expect("RunReport is JSON");
    assert_eq!(report["reports"]["left"], "0");

    let out = s.call_ok(4, "conflicts", json!({}));
    assert!(out.contains("no conflicts"), "got: {out}");
    let text = s.read_text(5, json!({ "start": 1, "end": 19 }));
    assert_eq!(text, "intro\ntheirs\ntail\n");
}

#[test]
fn replace_text_is_literal_counted_and_quote_safe() {
    let mut s = Server::spawn();
    s.call_ok(
        1,
        "open_text",
        json!({ "text": "a = b;\na = b;\na = b;\n" }),
    );

    // Single replace: first occurrence only, with a remaining-match hint.
    let out = s.call_ok(
        2,
        "replace_text",
        json!({ "pattern": "a = b;", "replacement": "a = c;" }),
    );
    assert!(out.contains("replaced 1 occurrence"), "got: {out}");
    assert!(out.contains("2 more match(es) remain"), "got: {out}");

    // all:true sweeps the rest.
    let out = s.call_ok(
        3,
        "replace_text",
        json!({ "pattern": "a = b;", "replacement": "a = c;", "all": true }),
    );
    assert!(out.contains("replaced 2 occurrence(s)"), "got: {out}");
    let text = s.read_text(4, json!({ "start": 1, "end": 22 }));
    assert_eq!(text, "a = c;\na = c;\na = c;\n");

    // The friction case the tool exists for: patterns/replacements full of
    // quotes, backslashes, and \u-style escapes — all literal, including
    // backrefs that replace-match would have expanded.
    s.call_ok(
        5,
        "open_text",
        json!({ "text": "format!(\"\\u{2014} occur\")", "session": "q" }),
    );
    let out = s.call_ok(
        6,
        "replace_text",
        json!({
            "pattern": "format!(\"\\u{2014} occur\")",
            "replacement": "write!(w, \"\\u{2026} occur \\1\")",
            "session": "q"
        }),
    );
    assert!(out.contains("replaced 1 occurrence"), "got: {out}");
    let text = s.read_text(7, json!({ "start": 1, "end": 31, "session": "q" }));
    assert_eq!(text, "write!(w, \"\\u{2026} occur \\1\")");

    // No match is a proper error that names the pattern — and a true no-op: the
    // buffer and point are exactly as before.
    let report = s.call_ok(
        8,
        "run_program",
        json!({ "program": r#"(goto-char 4) (report "p" (point))"#, "session": "q" }),
    );
    let report: Value = serde_json::from_str(&report).unwrap();
    assert_eq!(report["reports"]["p"], "4");
    let err = s.call_err(
        9,
        "replace_text",
        json!({ "pattern": "absent", "replacement": "x", "session": "q" }),
    );
    assert!(err.contains("no match"), "got: {err}");
    assert!(err.contains("absent"), "the error names the pattern: {err}");
    let report = s.call_ok(
        10,
        "run_program",
        json!({ "program": r#"(report "p" (point))"#, "session": "q" }),
    );
    let report: Value = serde_json::from_str(&report).unwrap();
    assert_eq!(
        report["reports"]["p"], "4",
        "failed replace left point alone"
    );
}

#[test]
fn insert_text_appends_at_eob_and_anchors_on_a_unique_line() {
    let mut s = Server::spawn();
    s.call_ok(1, "open_text", json!({ "text": "alpha\nbeta\ngamma\n" }));

    // Append at the end of the buffer.
    s.call_ok(2, "insert_text", json!({ "text": "omega\n", "pos": "eob" }));
    let out = s.read_text(3, json!({ "lines": [1, 4] }));
    assert_eq!(out, "alpha\nbeta\ngamma\nomega");

    // Anchor on a literal LINE: insert after the line matching "beta".
    s.call_ok(
        4,
        "insert_text",
        json!({ "text": "\nbeta-note", "anchor": { "pattern": "beta" } }),
    );
    let out = s.read_text(5, json!({ "lines": [1, 5] }));
    assert_eq!(out, "alpha\nbeta\nbeta-note\ngamma\nomega");
    // ...and before it.
    s.call_ok(
        6,
        "insert_text",
        json!({ "text": "pre-gamma\n", "anchor": { "pattern": "gamma", "where": "before" } }),
    );
    let out = s.read_text(7, json!({ "lines": [1, 6] }));
    assert_eq!(out, "alpha\nbeta\nbeta-note\npre-gamma\ngamma\nomega");

    // Ambiguity is an error listing the match lines; a miss names the pattern.
    let err = s.call_err(
        8,
        "insert_text",
        json!({ "text": "x", "anchor": { "pattern": "beta" } }),
    );
    assert!(err.contains("unique") && err.contains("lines"), "{err}");
    let err = s.call_err(
        9,
        "insert_text",
        json!({ "text": "x", "anchor": { "pattern": "absent" } }),
    );
    assert!(err.contains("no line matches"), "{err}");
    // A bad pos shape is loud.
    let err = s.call_err(
        10,
        "insert_text",
        json!({ "text": "x", "pos": "somewhere" }),
    );
    assert!(err.contains("eob"), "{err}");
}

#[test]
fn insert_text_accepts_expect_unique_and_keeps_anchors_unique() {
    let mut s = Server::spawn();
    s.call_ok(
        1,
        "open_text",
        json!({ "text": "a\nb\na\n", "session": "u" }),
    );

    s.call_ok(
        2,
        "insert_text",
        json!({ "session": "u", "text": "\nx", "anchor": { "pattern": "b" }, "expect_unique": true }),
    );
    assert_eq!(
        s.read_text(3, json!({ "session": "u", "start": 1, "end": 9 })),
        "a\nb\nx\na\n"
    );

    let err = s.call_err(
        4,
        "insert_text",
        json!({ "session": "u", "text": "y", "anchor": { "pattern": "a" }, "expect_unique": true }),
    );
    assert!(err.contains("must be unique"), "{err}");

    let err = s.call_err(
        5,
        "insert_text",
        json!({ "session": "u", "text": "y", "anchor": { "pattern": "b" }, "expect_unique": false }),
    );
    assert!(err.contains("always match exactly one line"), "{err}");
}

#[test]
fn guessed_names_reach_the_tools() {
    let mut s = Server::spawn();
    s.call_ok(1, "open_text", json!({ "text": "a a a", "session": "g" }));
    s.call_ok(
        2,
        "replace_text",
        json!({ "session": "g", "pattern": "a", "replacement": "z", "replace_all": true }),
    );
    assert_eq!(
        s.read_text(3, json!({ "session": "g", "start": 1, "end": 6 })),
        "z z z"
    );

    let git = s.call_ok(4, "help", json!({ "topic": ["git"] }));
    assert!(
        git.starts_with("— git history workflow —"),
        "the one-item list reads as the topic: {git}"
    );

    s.call_ok(5, "open_text", json!({ "text": "x", "session": "h" }));
    s.call_ok(6, "close_session", json!({ "session": "*" }));
    let status: Value = serde_json::from_str(&s.call_ok(7, "session_status", json!({}))).unwrap();
    assert_eq!(
        status["sessions"],
        json!([]),
        "session \"*\" closed them all: {status}"
    );
}

#[test]
fn view_reads_each_form_as_numbered_lines_without_moving_point() {
    let mut s = Server::spawn();
    s.call_ok(
        1,
        "open_text",
        json!({ "text": "l1\nl2\nl3\nl4\nl5\n", "name": "t.txt" }),
    );

    // A line range, 1-based inclusive. The trailing newline puts point-max on
    // line 6, as in Emacs.
    let out = s.call_ok(2, "view", json!({ "lines": [2, 4] }));
    assert_eq!(
        out,
        format!(
            "\u{2014} t.txt  lines 2-4 of 6 \u{2014}\n{}",
            numbered(2, "l2\nl3\nl4")
        )
    );
    // start_line/end_line are the same range under guessed names.
    assert_eq!(
        s.call_ok(3, "view", json!({ "start_line": 2, "end_line": 4 })),
        out
    );

    // Char positions [4, 9) are "l2\nl3".
    let out = s.call_ok(4, "view", json!({ "start": 4, "end": 9 }));
    assert_eq!(
        out,
        format!(
            "\u{2014} t.txt  @4-9 (lines 2-3) \u{2014}\n{}",
            numbered(2, "l2\nl3")
        )
    );

    // Around a line: `context` lines each side, the focus line marked.
    let out = s.call_ok(5, "view", json!({ "line": 3, "context": 1 }));
    assert!(
        out.contains("    2   l2\n")
            && out.contains("    3 > \u{2038}l3\n")
            && out.contains("    4   l4\n"),
        "{out}"
    );
    assert!(!out.contains("l1") && !out.contains("l5"), "{out}");
    // `count` is `context`; a bare number in `lines` is the old view's count.
    assert_eq!(s.call_ok(6, "view", json!({ "line": 3, "count": 1 })), out);
    let around = s.call_ok(7, "view", json!({ "lines": 1 }));
    assert!(
        around.contains("    1 > \u{2038}l1\n")
            && around.contains("    2   l2\n")
            && !around.contains("l3"),
        "{around}"
    );

    // Reading never moves point.
    let pt = s.call_ok(
        8,
        "run_program",
        json!({ "program": "(point)", "save": false }),
    );
    let pt: Value = serde_json::from_str(&pt).unwrap();
    assert_eq!(pt["value"], "1", "{pt}");
}

#[test]
fn view_names_its_four_forms_when_the_call_is_ambiguous() {
    let mut s = Server::spawn();
    s.call_ok(1, "open_text", json!({ "text": "aaa\nbbb\n" }));

    let err = s.call_err(2, "view", json!({ "lines": [1, 2], "pos": 3 }));
    assert!(
        err.contains("lines and line/pos given together") && err.contains("thing:"),
        "{err}"
    );
    let err = s.call_err(3, "view", json!({ "lines": "2-4", "start": 1 }));
    assert!(
        err.contains("given together"),
        "the clash is named before the bad shape: {err}"
    );
    let err = s.call_err(4, "view", json!({ "line": 2, "pos": 3 }));
    assert!(err.contains("pass one"), "{err}");
    let err = s.call_err(5, "view", json!({ "lines": [1, 2], "context": 2 }));
    assert!(err.contains("context applies"), "{err}");
    let err = s.call_err(6, "view", json!({ "start": 1 }));
    assert!(err.contains("\"end\""), "{err}");
    let err = s.call_err(7, "view", json!({ "lines": "2-4" }));
    assert!(err.contains("[start, end]"), "{err}");
    let err = s.call_err(8, "view", json!({ "end_line": 3 }));
    assert!(err.contains("start_line"), "{err}");
    let err = s.call_err(9, "view", json!({ "lines": 3, "context": 2 }));
    assert!(err.contains("pass one"), "{err}");
    let err = s.call_err(10, "read_region", json!({ "start": 1, "end": 2 }));
    assert!(err.contains("unknown tool"), "read_region is gone: {err}");
}

#[test]
fn view_counts_lines_within_a_narrowing_and_clamps_past_the_end() {
    let mut s = Server::spawn();
    s.call_ok(
        1,
        "open_text",
        json!({ "text": "l1\nl2\nl3\n", "name": "n.txt", "session": "n" }),
    );
    s.call_ok(
        2,
        "run_program",
        json!({ "session": "n", "program": "(narrow-to-region 4 9)", "save": false }),
    );
    // Line 1 is the first ACCESSIBLE line, and the header says so.
    let out = s.call_ok(3, "view", json!({ "session": "n", "lines": [1, 1] }));
    assert_eq!(
        out,
        format!(
            "\u{2014} n.txt  lines 1-1 of 2  Narrow \u{2014}\n{}",
            numbered(1, "l2")
        )
    );

    // An end past the buffer reads to the end instead of failing.
    s.call_ok(
        4,
        "open_text",
        json!({ "text": "a\nb\n", "name": "p.txt", "session": "p" }),
    );
    let out = s.call_ok(5, "view", json!({ "session": "p", "start": 1, "end": 999 }));
    assert!(out.ends_with(&numbered(1, "a\nb\n")), "{out}");
}

#[test]
fn view_range_forms_flag_unsaved_edits() {
    let dir = temp_dir("view-unsaved");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let p = file.to_string_lossy().into_owned();
    s.call_ok(
        1,
        "run_program",
        json!({ "path": p, "program": "(goto-char (point-max)) (insert \"beta\\n\")", "save": false }),
    );
    let out = s.call_ok(2, "view", json!({ "path": p, "lines": [1, 2] }));
    assert!(out.contains("    2   beta\n"), "{out}");
    assert!(
        out.contains("unsaved edits"),
        "a range read of a dirty buffer says so: {out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn view_resolves_a_thing_by_position_or_anchor_line() {
    let mut s = Server::spawn();
    let text =
        "fn main() {\n    let v = vec![1, (2 + 3)];\n    foo(v);\n}\n\nfn foo(v: Vec<i32>) {}\n";
    s.call_ok(1, "open_text", json!({ "text": text, "name": "m.rs" }));

    // The block after a line: `{` is at 11 and the block is 45 chars.
    let block = "{\n    let v = vec![1, (2 + 3)];\n    foo(v);\n}";
    let out = s.call_ok(
        2,
        "view",
        json!({ "thing": { "kind": "list", "after": "fn main() {" } }),
    );
    assert_eq!(
        out,
        format!(
            "list @11-{} (lines 1-4):\n{}",
            11 + block.chars().count(),
            numbered(1, block)
        )
    );

    // By position: 30 is the `1` inside `[1, (2 + 3)]`, which spans 29-41.
    let out = s.call_ok(3, "view", json!({ "thing": { "kind": "list", "at": 30 } }));
    assert_eq!(
        out,
        format!("list @29-41 (lines 2-2):\n{}", numbered(2, "[1, (2 + 3)]"))
    );
    let out = s.call_ok(
        4,
        "view",
        json!({ "thing": { "kind": "list", "at": 30, "up": 1 } }),
    );
    assert!(out.starts_with("list @11-56"), "{out}");
    let out = s.call_ok(5, "view", json!({ "thing": { "kind": "sexp", "at": 34 } }));
    assert_eq!(
        out,
        format!("sexp @34-35 (lines 2-2):\n{}", numbered(2, "2"))
    );
    let out = s.call_ok(
        6,
        "view",
        json!({ "thing": { "kind": "list", "before": "foo(v);" } }),
    );
    assert_eq!(
        out,
        format!("list @29-41 (lines 2-2):\n{}", numbered(2, "[1, (2 + 3)]"))
    );
    let out = s.call_ok(
        7,
        "view",
        json!({ "thing": { "kind": "defun", "after": "fn foo" } }),
    );
    assert!(
        out.starts_with("defun @") && out.ends_with("fn foo(v: Vec<i32>) {}\n"),
        "{out}"
    );
    // The anchor line sits inside the block, so the walk crosses the `}` that
    // closes it at depth zero — but `(v)` began on the line, so it wins.
    let out = s.call_ok(
        17,
        "view",
        json!({ "thing": { "kind": "list", "after": "foo(v);" } }),
    );
    assert_eq!(
        out,
        format!("list @50-53 (lines 3-3):\n{}", numbered(3, "(v)"))
    );

    // Errors name the problem.
    let err = s.call_err(
        7,
        "view",
        json!({ "thing": { "kind": "string", "at": 30 } }),
    );
    assert!(err.contains("no string at 30"), "{err}");
    let err = s.call_err(
        9,
        "view",
        json!({ "thing": { "kind": "list", "after": "fn" } }),
    );
    assert!(err.contains("unique") && err.contains("lines"), "{err}");
    let err = s.call_err(
        10,
        "view",
        json!({ "thing": { "kind": "list", "after": "absent" } }),
    );
    assert!(err.contains("no line matches"), "{err}");
    let err = s.call_err(11, "view", json!({ "thing": { "at": 30 } }));
    assert!(err.contains("kind"), "{err}");
    let err = s.call_err(
        12,
        "view",
        json!({ "thing": { "kind": "list", "at": 30, "after": "x" } }),
    );
    assert!(err.contains("one of"), "{err}");
    let err = s.call_err(
        13,
        "view",
        json!({ "thing": { "kind": "string", "at": 30, "up": 1 } }),
    );
    assert!(err.contains("sexp and list"), "{err}");
    let err = s.call_err(
        14,
        "view",
        json!({ "thing": { "kind": "list", "at": 30 }, "lines": [1, 2] }),
    );
    assert!(err.contains("given together"), "{err}");
    s.call_ok(15, "open_text", json!({ "text": "(a b", "name": "u.rs" }));
    let err = s.call_err(16, "view", json!({ "thing": { "kind": "list", "at": 2 } }));
    assert!(err.contains("Unbalanced parentheses at 1"), "{err}");
}

/// `after:` reads the anchor line differently per kind: a `list` is the block
/// the line OPENS (the last list beginning on it), while every other kind —
/// `sexp` included — is the FIRST thing at or after the line's start. Without
/// that split, `{kind: "sexp", after: "old(1, 2);"}` named the trailing `;`.
#[test]
fn a_sexp_after_a_line_is_the_first_one_a_list_the_last_the_line_opens() {
    let mut s = Server::spawn();
    // 1-8 "fn f() {", 9 newline, 10-13 the indent, 14-16 "old", 17 "(", 18 "1",
    // 19 ",", 20 " ", 21 "2", 22 ")", 23 ";", 24 newline, 25 "}".
    s.call_ok(
        1,
        "open_text",
        json!({ "text": "fn f() {\n    old(1, 2);\n}\n", "name": "m.rs" }),
    );

    // The first sexp at or after the line start is the call's name.
    let out = s.call_ok(
        2,
        "view",
        json!({ "thing": { "kind": "sexp", "after": "old(1, 2);" } }),
    );
    assert_eq!(
        out,
        format!("sexp @14-17 (lines 2-2):\n{}", numbered(2, "old"))
    );
    // The last list beginning on the same line is its argument list.
    let out = s.call_ok(
        3,
        "view",
        json!({ "thing": { "kind": "list", "after": "old(1, 2);" } }),
    );
    assert_eq!(
        out,
        format!("list @17-23 (lines 2-2):\n{}", numbered(2, "(1, 2)"))
    );

    // On the defun's own line the two diverge the other way: the sexp is the
    // `fn` keyword, the list the block the line opens.
    let out = s.call_ok(
        4,
        "view",
        json!({ "thing": { "kind": "sexp", "after": "fn f() {" } }),
    );
    assert_eq!(
        out,
        format!("sexp @1-3 (lines 1-1):\n{}", numbered(1, "fn"))
    );
    let out = s.call_ok(
        5,
        "view",
        json!({ "thing": { "kind": "list", "after": "fn f() {" } }),
    );
    assert_eq!(
        out,
        format!(
            "list @8-26 (lines 1-3):\n{}",
            numbered(1, "{\n    old(1, 2);\n}")
        )
    );
}

#[test]
fn view_thing_after_with_nothing_left_to_find_is_an_error() {
    let mut s = Server::spawn();
    s.call_ok(
        1,
        "open_text",
        json!({ "text": "zz\n---\n", "name": "n.md" }),
    );

    // Nothing follows the anchor line, so there is no word to read — the lookup
    // must say so, not hand back an empty span.
    let err = s.call_err(
        2,
        "view",
        json!({ "thing": { "kind": "word", "after": "---" } }),
    );
    assert!(err.contains("no word after the line"), "{err}");
    let err = s.call_err(
        3,
        "view",
        json!({ "thing": { "kind": "symbol", "after": "---" } }),
    );
    assert!(err.contains("no symbol after the line"), "{err}");
}

#[test]
fn replace_text_and_insert_text_take_a_thing() {
    let mut s = Server::spawn();
    s.call_ok(
        1,
        "open_text",
        json!({ "text": "fn f() {\n    old(1, 2);\n}\n", "name": "m.rs" }),
    );

    let out = s.call_ok(
        2,
        "replace_text",
        json!({ "thing": { "kind": "list", "after": "old(1, 2);" }, "replacement": "(3)" }),
    );
    assert!(
        out.starts_with("replaced the list @17-23 (line 2) with 3 chars"),
        "{out}"
    );
    assert_eq!(
        s.read_text(3, json!({ "lines": [1, 3] })),
        "fn f() {\n    old(3);\n}"
    );

    // By position: 14 is inside `old`.
    let out = s.call_ok(
        4,
        "replace_text",
        json!({ "thing": { "kind": "sexp", "at": 14 }, "replacement": "new" }),
    );
    assert!(out.starts_with("replaced the sexp @14-17"), "{out}");
    assert_eq!(
        s.read_text(5, json!({ "lines": [1, 3] })),
        "fn f() {\n    new(3);\n}"
    );

    // insert_text: before the line containing the anchor, and after a list.
    let out = s.call_ok(
        6,
        "insert_text",
        json!({ "text": "    pre();\n", "thing": { "kind": "line", "after": "new(3);" }, "where": "before" }),
    );
    assert!(out.contains("before the line @10-"), "{out}");
    let out = s.call_ok(
        7,
        "insert_text",
        json!({ "text": " // done", "thing": { "kind": "list", "after": "new(3);" } }),
    );
    assert!(out.contains("after the list @"), "{out}");
    assert_eq!(
        s.read_text(8, json!({ "lines": [1, 4] })),
        "fn f() {\n    pre();\n    new(3) // done;\n}"
    );

    // Exclusivity and bad shapes.
    let err = s.call_err(
        9,
        "replace_text",
        json!({ "thing": { "kind": "list", "at": 9 }, "pattern": "x", "replacement": "y" }),
    );
    assert!(err.contains("not both"), "{err}");
    let err = s.call_err(
        10,
        "insert_text",
        json!({ "text": "x", "thing": { "kind": "list", "at": 9 }, "pos": 1 }),
    );
    assert!(err.contains("not both"), "{err}");
    let err = s.call_err(
        11,
        "insert_text",
        json!({ "text": "x", "thing": { "kind": "list", "at": 9 }, "where": "middle" }),
    );
    assert!(err.contains("after") && err.contains("before"), "{err}");
    let err = s.call_err(
        12,
        "replace_text",
        json!({ "thing": { "kind": "string", "at": 3 }, "replacement": "y" }),
    );
    assert!(err.contains("no string at 3"), "{err}");

    // A top-level `where` with no `thing` names nothing: the anchor form keeps
    // its own `where` inside the anchor object, so this is an error rather than
    // an insert silently landing at the other end.
    let err = s.call_err(
        13,
        "insert_text",
        json!({ "text": "x", "anchor": { "pattern": "fn f() {", "where": "before" }, "where": "before" }),
    );
    assert!(err.contains("anchor") && err.contains("where"), "{err}");

    // A non-string `where` is a mistyped argument, not a silent default: schema
    // validation checks key names, not value types.
    let err = s.call_err(
        14,
        "insert_text",
        json!({ "text": "x", "thing": { "kind": "list", "at": 9 }, "where": 1 }),
    );
    assert!(
        err.contains("must be \"after\" or \"before\"") && err.contains("got 1"),
        "{err}"
    );
    let err = s.call_err(
        15,
        "insert_text",
        json!({ "text": "x", "thing": { "kind": "list", "at": 9 }, "where": true }),
    );
    assert!(err.contains("got true"), "{err}");

    // An `at` past the end of the buffer clamps inside the scanner, which would
    // name the LAST thing in the file and splice over it. It is an error
    // instead — but point-max itself stays a valid probe point.
    let err = s.call_err(
        16,
        "replace_text",
        json!({ "thing": { "kind": "sexp", "at": 999999 }, "replacement": "y" }),
    );
    assert!(
        err.contains("999999 is outside the accessible region"),
        "{err}"
    );
    let err = s.call_err(
        17,
        "view",
        json!({ "thing": { "kind": "sexp", "at": 999999 } }),
    );
    assert!(err.contains("outside the accessible region"), "{err}");
    // "fn f() {\n pre();\n new(3) // done;\n}\n" is 42 chars, so point-max is
    // 43 and the thing there is the last one in the buffer.
    let out = s.call_ok(18, "view", json!({ "thing": { "kind": "line", "at": 43 } }));
    assert_eq!(
        out,
        format!("line @41-43 (lines 4-4):\n{}", numbered(4, "}\n"))
    );
}

#[test]
fn replace_text_regex_mode_expands_backrefs() {
    let mut s = Server::spawn();
    s.call_ok(
        1,
        "open_text",
        json!({ "text": "Doe, John\nRoe, Jane\nplain line\n" }),
    );

    // mode:"regex" + all: the one-call form of the re-search-forward /
    // replace-match loop, \1 backrefs and all.
    let out = s.call_ok(
        2,
        "replace_text",
        json!({
            "pattern": "\\([A-Za-z]+\\), \\([A-Za-z]+\\)",
            "replacement": "\\2 \\1",
            "mode": "regex",
            "all": true
        }),
    );
    assert!(out.contains("replaced 2 occurrence(s)"), "got: {out}");
    let text = s.read_text(3, json!({ "start": 1, "end": 31 }));
    assert_eq!(text, "John Doe\nJane Roe\nplain line\n");

    // Single regex replace reports the remaining matches.
    s.call_ok(
        4,
        "open_text",
        json!({ "text": "x1 x2 x3\n", "session": "q" }),
    );
    let out = s.call_ok(
        5,
        "replace_text",
        json!({ "pattern": "x[0-9]", "replacement": "y\\&", "mode": "regex", "session": "q" }),
    );
    assert!(out.contains("replaced 1 occurrence"), "got: {out}");
    assert!(out.contains("2 more match(es) remain"), "got: {out}");
    let text = s.read_text(6, json!({ "start": 1, "end": 10, "session": "q" }));
    assert_eq!(text, "yx1 x2 x3");

    // expect_unique keeps its semantics per pattern: an ambiguous regex is an
    // error listing the match lines, and nothing is replaced.
    let err = s.call_err(
        7,
        "replace_text",
        json!({
            "pattern": "x[0-9]",
            "replacement": "z",
            "mode": "regex",
            "expect_unique": true,
            "session": "q"
        }),
    );
    assert!(err.contains("expect_unique"), "got: {err}");
    let text = s.read_text(8, json!({ "start": 1, "end": 10, "session": "q" }));
    assert_eq!(text, "yx1 x2 x3", "ambiguity replaced nothing");

    // A regex miss errors like the literal one.
    let err = s.call_err(
        9,
        "replace_text",
        json!({ "pattern": "q[0-9]+", "replacement": "z", "mode": "regex", "session": "q" }),
    );
    assert!(err.contains("no match"), "got: {err}");

    // In edits[] batches a top-level mode is the default per entry.
    let out = s.call_ok(
        10,
        "replace_text",
        json!({
            "mode": "regex",
            "edits": [
                { "pattern": "y\\(x1\\)", "replacement": "\\1" },
                { "pattern": "x3", "replacement": "done", "mode": "exact" },
            ],
            "session": "q"
        }),
    );
    assert!(out.contains("applied 2 edit(s)"), "got: {out}");
    let text = s.read_text(11, json!({ "start": 1, "end": 9, "session": "q" }));
    assert_eq!(text, "x1 x2 do");

    // An unknown mode is a loud error.
    let err = s.call_err(
        12,
        "replace_text",
        json!({ "pattern": "a", "replacement": "b", "mode": "fuzzy", "session": "q" }),
    );
    assert!(err.contains("mode"), "got: {err}");
}

/// Every literal-taking tool escapes user strings into generated tulisp on the
/// server (lisp_literal; occur adds regexp-quote / regex_dialect::quote
/// underneath); one missed path is a silent wrong edit or a false miss.
/// Round-trip a gauntlet of hostile strings through insert_text → read_text,
/// occur, replace_text, and the edits batch, requiring byte-exact results
/// everywhere.
#[test]
fn literal_tools_round_trip_hostile_strings() {
    let mut s = Server::spawn();
    let cases: &[&str] = &[
        "back\\slash and trailing \\",
        "dou\"ble \"quo\"tes",
        "real\nnewlines\nand\ttabs",
        "crlf\r\nline",
        ".*+?[](){}|^$ regex metachars",
        "emacs \\(group\\) \\| alt x\\{2,3\\}",
        "mixed \"\\\" quote-slash \\\" edge",
        "unicode αβγ 🦀 ñé",
        "\\n literal backslash-n (not a newline)",
        "semi;colons 'single' `backtick` $dollar (report \"k\" 1)",
    ];
    let mut id = 0;
    for (i, case) in cases.iter().enumerate() {
        let sess = format!("h{i}");
        id += 1;
        s.call_ok(id, "open_text", json!({ "text": "AB", "session": sess }));
        // Insert between A and B; the buffer must read back byte-exact.
        id += 1;
        s.call_ok(
            id,
            "insert_text",
            json!({ "session": sess, "text": case, "pos": 2 }),
        );
        let n = case.chars().count() as i64;
        id += 1;
        let txt = s.read_text(id, json!({ "session": sess, "start": 1, "end": n + 3 }));
        assert_eq!(txt, format!("A{case}B"), "insert round trip, case {i}");
        // occur (exact mode) must find the literal — single-line cases only
        // (occur is line-oriented).
        if !case.contains('\n') && !case.contains('\r') {
            id += 1;
            let out = s.call_ok(id, "occur", json!({ "session": sess, "pattern": case }));
            assert!(out.contains(case), "occur finds case {i}: {out}");
        }
        // Replace the literal with another hostile literal, byte-exact.
        let repl = format!("R\"\\{case}\\\"R");
        id += 1;
        let ok = s.call_ok(
            id,
            "replace_text",
            json!({ "session": sess, "pattern": case, "replacement": repl, "expect_unique": true }),
        );
        assert!(ok.contains("replaced 1 occurrence"), "case {i}: {ok}");
        let m = repl.chars().count() as i64;
        id += 1;
        let txt = s.read_text(id, json!({ "session": sess, "start": 1, "end": m + 3 }));
        assert_eq!(txt, format!("A{repl}B"), "replace round trip, case {i}");
    }

    // The edits batch rides the same escaping inside with-transaction.
    id += 1;
    s.call_ok(
        id,
        "open_text",
        json!({ "text": "x \"q\\s\" y", "session": "batch" }),
    );
    id += 1;
    s.call_ok(
        id,
        "replace_text",
        json!({
            "session": "batch",
            "edits": [
                { "pattern": "\"q\\s\"", "replacement": "\"Q\\S\"", "expect_unique": true },
                { "pattern": "x ", "replacement": "x\t\n" },
            ],
        }),
    );
    id += 1;
    let txt = s.read_text(id, json!({ "session": "batch", "start": 1, "end": 11 }));
    assert_eq!(txt, "x\t\n\"Q\\S\" y", "batch round trip");
}

/// grep compiles the exact-mode pattern with RE2 escaping (a separate path from
/// occur's in-engine regexp-quote) — hostile literals must match there too, and
/// the file's rendered line must survive clamping intact.
#[test]
fn grep_exact_matches_hostile_literals() {
    let dir = temp_dir("grep-hostile");
    let cases = [
        "we\\d+ \"quo\" \\(x\\) .*+?[]{}|^$",
        "tab\there $dollar `tick`",
    ];
    std::fs::write(dir.join("h.txt"), cases.join("\n")).unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    for (i, c) in cases.iter().enumerate() {
        let out = s.call_ok(i as i64 + 1, "grep", json!({ "pattern": c }));
        assert!(
            out.contains("1 match") && out.contains("h.txt"),
            "case {i}: {out}"
        );
    }
}

#[test]
fn failed_run_carries_the_programs_reports_and_log() {
    let mut s = Server::spawn();
    s.call_ok(1, "open_text", json!({ "text": "hello" }));
    // The error content is the failure JSON: the program's own diagnostics
    // (reports + log) ride along with the error string.
    let err = s.call_err(
        2,
        "run_program",
        json!({ "program": r#"(report "saw" (point-max)) (message "diag") (error "boom")"# }),
    );
    let failure: Value = serde_json::from_str(&err).expect("failure content is JSON");
    assert_eq!(failure["ok"], false);
    assert!(
        failure["error"].as_str().unwrap().contains("boom"),
        "got: {failure}"
    );
    assert_eq!(failure["reports"]["saw"], "6");
    assert_eq!(failure["log"][0], "diag");
    // A navigate-and-report program left no edits behind.
    assert_eq!(failure["dirty"], false);

    // A program that edits and THEN dies is transactional by default: the
    // pre-error edit is rolled back and the failure says so.
    let err = s.call_err(
        3,
        "run_program",
        json!({ "program": r#"(insert "partial ") (error "late boom")"# }),
    );
    let failure: Value = serde_json::from_str(&err).expect("failure content is JSON");
    assert_eq!(failure["dirty"], false);
    assert_eq!(failure["rolled_back"], true);
    assert!(
        failure["error"].as_str().unwrap().contains("rolled back"),
        "got: {failure}"
    );
    let text = s.read_text(4, json!({ "start": 1, "end": 6 }));
    assert_eq!(text, "hello", "the pre-error edit was rolled back");

    // keep_partial:true opts out: the edit persists and the error says how to
    // revert it.
    let err = s.call_err(
        5,
        "run_program",
        json!({ "program": r#"(goto-char (point-min)) (insert "partial ") (error "late boom")"#, "keep_partial": true }),
    );
    let failure: Value = serde_json::from_str(&err).expect("failure content is JSON");
    assert_eq!(failure["dirty"], true);
    assert!(failure.get("rolled_back").is_none());
    assert!(
        failure["error"].as_str().unwrap().contains("undo_last"),
        "got: {failure}"
    );
    let text = s.read_text(6, json!({ "start": 1, "end": 14 }));
    assert_eq!(
        text, "partial hello",
        "keep_partial kept the pre-error edit"
    );
}

#[test]
fn occur_overviews_matches_without_moving_point() {
    let mut s = Server::spawn();
    s.call_ok(
        1,
        "open_text",
        json!({ "text": "alpha beta\nbeta beta\ngamma\nbeta\n" }),
    );

    // Exact mode: every matching line, with line number, position, and a
    // per-line count for the double hit.
    let out = s.call_ok(2, "occur", json!({ "pattern": "beta" }));
    assert!(out.contains("4 matches on 3 lines"), "got: {out}");
    assert!(out.contains("    2 @12 ×2: beta beta"), "got: {out}");

    // Point did not move: occur is read-only, so the cursor stays at point-min.
    let view = s.call_ok(3, "view", json!({}));
    assert!(view.contains("line 1 col 0"), "got: {view}");

    // Regex mode and the limit tail both pass through to the builtin.
    let out = s.call_ok(4, "occur", json!({ "pattern": "g.mma", "mode": "regex" }));
    assert!(out.contains("1 match on 1 line"), "got: {out}");
    let out = s.call_ok(5, "occur", json!({ "pattern": "beta", "limit": 1 }));
    assert!(out.contains("… and 2 more matching lines"), "got: {out}");

    // case_insensitive is plumbed through the MCP arg (exact + folding branch).
    let ci = s.call_ok(
        6,
        "occur",
        json!({ "pattern": "BETA", "case_insensitive": true }),
    );
    assert!(ci.contains("4 matches on 3 lines"), "got: {ci}");
}

/// The `fill` default reaches the git tools end to end: a long body committed
/// through the server comes back at 72 columns, and `fill: false` keeps it.
#[test]
fn git_tools_fill_message_bodies_by_default() {
    let dir = temp_dir("msg-fill");
    let repo = git2::Repository::init(&dir).unwrap();
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "T").unwrap();
    config.set_str("user.email", "t@example.invalid").unwrap();
    config.set_bool("commit.gpgsign", false).unwrap();
    drop(config);
    std::fs::write(dir.join("f.txt"), "1\n").unwrap();
    let long = "subject\n\nThis body line is written well past the fill column and should wrap when the tool fills it.\n";
    let filled = "subject\n\nThis body line is written well past the fill column and should wrap when\nthe tool fills it.\n";
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let repo_arg = dir.to_string_lossy().into_owned();

    let out = s.call_ok(
        1,
        "git_commit",
        json!({ "repo": repo_arg, "paths": ["f.txt"], "message": long }),
    );
    assert!(out.contains("body filled at 72 columns"), "{out}");
    let tip = repo.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(tip.message(), Some(filled));

    let out = s.call_ok(
        2,
        "git_reword",
        json!({ "repo": repo_arg, "commit": "HEAD", "message": long, "fill": false }),
    );
    assert!(!out.contains("filled"), "{out}");
    let tip = repo.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(tip.message(), Some(long));

    let err = s.call_err(
        3,
        "git_reword",
        json!({ "repo": repo_arg, "commit": "HEAD", "fill": 0 }),
    );
    assert!(err.contains("`fill` must be a positive integer"), "{err}");
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("mime-mcp-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create temp dir");
    p
}

#[test]
fn path_reuses_a_session_already_visiting_the_file() {
    let dir = temp_dir("one-copy");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let p = file.to_string_lossy().into_owned();

    // Open under a custom name, then address by path: ONE warm buffer, not a
    // divergent second copy of the same document.
    s.call_ok(1, "open_file", json!({ "path": p, "session": "custom" }));
    s.call_ok(
        2,
        "replace_text",
        json!({ "path": p, "pattern": "alpha", "replacement": "beta", "save": false }),
    );
    let status = s.call_ok(3, "session_status", json!({}));
    let status: Value = serde_json::from_str(&status).unwrap();
    assert_eq!(
        status["sessions"].as_array().unwrap().len(),
        1,
        "one warm copy: {status}"
    );
    let txt = s.read_text(4, json!({ "session": "custom", "start": 1, "end": 5 }));
    assert_eq!(
        txt, "beta",
        "the custom session sees the path-addressed edit"
    );

    // TWO sessions deliberately visiting the same file: {path} addressing
    // refuses to guess between divergent copies.
    s.call_ok(6, "open_file", json!({ "path": p, "session": "second" }));
    let err = s.call_err(
        7,
        "replace_text",
        json!({ "path": p, "pattern": "x", "replacement": "y" }),
    );
    assert!(err.contains("ambiguous"), "got: {err}");
    assert!(
        err.contains("custom") && err.contains("second"),
        "names both sessions: {err}"
    );

    // A refused (stale) save names the warm session that still holds the edit.
    std::fs::write(&file, "external change\n").unwrap();
    let err = s.call_err(
        8,
        "replace_text",
        json!({ "session": "custom", "pattern": "beta", "replacement": "gamma", "save": true }),
    );
    assert!(
        err.contains("preserved in warm session \"custom\""),
        "got: {err}"
    );
}

#[test]
fn auto_revert_refreshes_clean_reads_while_modified_reads_warn() {
    let dir = temp_dir("stale-read");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let p = file.to_string_lossy().into_owned();

    s.call_ok(1, "open_file", json!({ "path": p }));
    let view = s.call_ok(2, "view", json!({ "path": p }));
    assert!(
        !view.contains("WARNING"),
        "clean read must not warn: {view}"
    );

    // An external writer lands while the warm buffer has NO unsaved edits:
    // auto-revert-mode silently re-reads the file on the next read, so the read
    // sees the current content with NO drift warning.
    std::fs::write(&file, "ALPHA external\n").unwrap();
    let txt = s.read_text(3, json!({ "path": p, "start": 1, "end": 6 }));
    assert_eq!(
        txt, "ALPHA",
        "clean+stale buffer auto-reverted to the new file"
    );
    let view = s.call_ok(4, "view", json!({ "path": p }));
    assert!(
        !view.contains("WARNING"),
        "auto-reverted read does not warn: {view}"
    );
    let out = s.call_ok(5, "run_program", json!({ "path": p, "program": "(point)" }));
    let out: Value = serde_json::from_str(&out).unwrap();
    assert!(
        out.get("stale").is_none(),
        "no drift flag after auto-revert: {out}"
    );

    // Now MODIFY the buffer, then drift the file again. A modified buffer is
    // the genuine conflict — it is NOT auto-reverted, so reads carry the
    // warning.
    s.call_ok(
        6,
        "run_program",
        json!({ "path": p, "program": "(goto-char (point-max)) (insert \"mine\\n\")", "save": false }),
    );
    std::fs::write(&file, "THIRD external, a different length\n").unwrap();
    let view = s.call_ok(7, "view", json!({ "path": p }));
    assert!(
        view.contains("WARNING") && view.contains("revert-buffer"),
        "a modified + drifted read warns: {view}"
    );
    let out = s.call_ok(8, "run_program", json!({ "path": p, "program": "(point)" }));
    let out: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        out["stale"], true,
        "structured drift flag on a modified buffer: {out}"
    );

    // Explicit revert-buffer still recovers (discarding the edit) and clears.
    s.call_ok(
        9,
        "run_program",
        json!({ "path": p, "program": "(revert-buffer)" }),
    );
    let txt = s.read_text(10, json!({ "path": p, "start": 1, "end": 6 }));
    assert_eq!(txt, "THIRD", "fresh content after explicit revert");
    let view = s.call_ok(11, "view", json!({ "path": p }));
    assert!(!view.contains("WARNING"), "stamp re-armed: {view}");
}

#[test]
fn edit_tools_flag_a_stale_dirty_buffer_on_results_and_misses() {
    let dir = temp_dir("stale-edit");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "alpha\nbeta\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let p = file.to_string_lossy().into_owned();

    // Dirty the warm buffer, then drift the file: the genuine conflict that
    // auto-revert deliberately leaves alone.
    s.call_ok(
        1,
        "replace_text",
        json!({ "path": p, "pattern": "alpha", "replacement": "ALPHA", "save": false }),
    );
    std::fs::write(&file, "rewritten externally, different length\n").unwrap();

    // The read tools warned already; the EDIT tools are where staleness
    // actually bites, and a miss is the moment the diagnosis is needed: the
    // pattern exists on disk but the match ran against the warm buffer.
    let err = s.call_err(
        2,
        "replace_text",
        json!({ "path": p, "pattern": "rewritten externally", "replacement": "x" }),
    );
    assert!(err.contains("no match"), "got: {err}");
    assert!(
        err.contains("changed on disk"),
        "the miss must carry the stale note: {err}"
    );

    // A successful edit's result carries it too — the edit landed in a warm
    // buffer that no longer matches the file it came from.
    let ok = s.call_ok(
        3,
        "replace_text",
        json!({ "path": p, "pattern": "beta", "replacement": "BETA", "save": false }),
    );
    assert!(
        ok.contains("changed on disk"),
        "the edit result must carry the stale note: {ok}"
    );
    let ok = s.call_ok(
        4,
        "insert_text",
        json!({ "path": p, "text": "tail\n", "save": false }),
    );
    assert!(ok.contains("changed on disk"), "insert_text too: {ok}");
}

#[test]
fn rehearse_auto_reverts_a_clean_drifted_buffer_like_a_run_would() {
    // A clean drifted buffer auto-reverts BEFORE the rehearse snapshot: the
    // revert discards nothing, and without it the preview would run against
    // bytes the committing run — which does revert — won't use, so preview and
    // commit could legitimately disagree.
    let dir = temp_dir("rehearse-revert");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "original\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let p = file.to_string_lossy().into_owned();
    s.call_ok(1, "open_file", json!({ "path": p }));

    // Drift the file (different length) while the buffer is clean.
    std::fs::write(&file, "changed on disk and longer\n").unwrap();
    let preview = s.call_ok(
        2,
        "run_program",
        json!({ "path": p, "program": r#"(search-forward "changed" nil t) (replace-match "previewed")"#, "rehearse": true }),
    );
    assert!(
        preview.contains("previewed"),
        "the preview sees the CURRENT file: {preview}"
    );

    // The rehearsal's rollback lands on the reverted (fresh) state: not stale,
    // text matching the disk.
    let status = s.call_ok(3, "session_status", json!({}));
    let status: Value = serde_json::from_str(&status).unwrap();
    assert!(
        status["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|sess| sess["stale"] == false),
        "the clean buffer was re-read: {status}"
    );
    let txt = s.read_text(4, json!({ "path": p, "start": 1, "end": 8 }));
    assert_eq!(txt, "changed", "rollback kept the reverted text");
}

#[test]
fn unsaved_edits_are_flagged_until_saved() {
    let dir = temp_dir("unsaved");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let p = file.to_string_lossy().into_owned();
    s.call_ok(1, "open_file", json!({ "path": p }));

    // A freshly opened buffer is clean — not flagged unsaved.
    let st: Value = serde_json::from_str(&s.call_ok(2, "session_status", json!({}))).unwrap();
    assert!(
        st["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|x| x["unsaved"] == false),
        "fresh buffer is not unsaved: {st}"
    );

    // Edit WITHOUT save → the run result flags it, and so does session_status.
    let out: Value = serde_json::from_str(&s.call_ok(
        3,
        "run_program",
        json!({ "path": p, "program": "(goto-char (point-max)) (insert \"beta\\n\")", "save": false }),
    ))
    .unwrap();
    assert_eq!(out["unsaved"], true, "an unsaved edit is flagged: {out}");
    let st: Value = serde_json::from_str(&s.call_ok(4, "session_status", json!({}))).unwrap();
    assert!(
        st["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["unsaved"] == true),
        "session_status flags the unsaved buffer: {st}"
    );

    // An edit tool's text message carries the reminder when it doesn't save.
    let msg = s.call_ok(
        5,
        "replace_text",
        json!({ "path": p, "pattern": "alpha", "replacement": "ALPHA", "save": false }),
    );
    assert!(msg.contains("unsaved"), "edit-tool message reminds: {msg}");

    // The read side is authoritative too: grep (disk) flags the file, view
    // (buffer) says which state it shows, and unsaved_diff answers "what
    // exactly have I not saved".
    let hits = s.call_ok(50, "grep", json!({ "pattern": "alpha" }));
    assert!(
        hits.contains("UNSAVED"),
        "grep flags a file whose warm buffer differs from disk: {hits}"
    );
    let vp = s.call_ok(51, "view", json!({ "path": p }));
    assert!(
        vp.contains("unsaved"),
        "view says it shows the warm buffer: {vp}"
    );
    let d = s.call_ok(52, "unsaved_diff", json!({ "path": p }));
    assert!(
        d.contains("+ALPHA") && d.contains("-alpha") && d.contains("+beta"),
        "the diff is disk → buffer: {d}"
    );

    // Saving clears the flag everywhere.
    s.call_ok(
        6,
        "run_program",
        json!({ "path": p, "program": "(point)", "save": true }),
    );
    let out: Value = serde_json::from_str(&s.call_ok(
        7,
        "run_program",
        json!({ "path": p, "program": "(point)" }),
    ))
    .unwrap();
    assert!(
        out.get("unsaved").is_none(),
        "saved → no unsaved flag: {out}"
    );
    let st: Value = serde_json::from_str(&s.call_ok(8, "session_status", json!({}))).unwrap();
    assert!(
        st["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|x| x["unsaved"] == false),
        "saved → not unsaved: {st}"
    );
}

#[test]
fn unsaved_flag_covers_insert_rehearse_and_lisp_restore() {
    let dir = temp_dir("unsaved2");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let p = file.to_string_lossy().into_owned();
    // Explicit session id so the Lisp checkpoint forms (keyed by session, not
    // path) address the same buffer.
    s.call_ok(1, "open_file", json!({ "session": "s", "path": p }));

    // insert_text without save → its text message carries the reminder.
    let msg = s.call_ok(
        2,
        "insert_text",
        json!({ "session": "s", "text": "X", "save": false }),
    );
    assert!(msg.contains("unsaved"), "insert_text reminds: {msg}");

    // Save to a clean baseline, then rehearse a would-be edit: it rolls back,
    // so it must NOT report unsaved.
    s.call_ok(
        3,
        "run_program",
        json!({ "session": "s", "program": "(point)", "save": true }),
    );
    let out: Value = serde_json::from_str(&s.call_ok(
        4,
        "run_program",
        json!({ "session": "s", "program": "(goto-char (point-max)) (insert \"Z\")", "rehearse": true }),
    ))
    .unwrap();
    assert!(
        out.get("unsaved").is_none(),
        "rehearse rolls back → not unsaved: {out}"
    );

    // Checkpoint at the saved state, save a DIFFERENT content to disk, then
    // restore the checkpoint — the buffer now differs from disk → unsaved.
    s.call_ok(
        5,
        "run_program",
        json!({ "session": "s", "program": r#"(checkpoint "cp0")"# }),
    );
    s.call_ok(
        6,
        "run_program",
        json!({ "session": "s", "program": "(erase-buffer) (insert \"BETA\\n\")" }),
    );
    let restored: Value = serde_json::from_str(&s.call_ok(
        7,
        "run_program",
        json!({ "session": "s", "program": r#"(restore-checkpoint "cp0")"#, "save": false }),
    ))
    .unwrap();
    assert_eq!(
        restored["unsaved"], true,
        "a restore that diverges from disk flags unsaved: {restored}"
    );
}

#[test]
fn one_call_editing_with_path_save_and_batches() {
    let dir = temp_dir("one-call");
    let file = dir.join("prog.rs");
    std::fs::write(&file, "fn main() {\n    let a = 1;\n    let b = 1;\n}\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let p = file.to_string_lossy().into_owned();

    // ONE call: open by path, replace, save back to disk atomically.
    let out = s.call_ok(
        1,
        "replace_text",
        json!({ "path": p, "pattern": "let a = 1;", "replacement": "let a = 2;", "save": true }),
    );
    assert!(out.contains("replaced 1 occurrence"), "got: {out}");
    assert!(out.contains("saved"), "got: {out}");
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .contains("let a = 2;")
    );

    // The session is warm, keyed by the canonical path, and visible.
    let status = s.call_ok(2, "session_status", json!({}));
    let status: Value = serde_json::from_str(&status).unwrap();
    let sess = &status["sessions"][0];
    assert!(
        sess["file"].as_str().unwrap().contains("prog.rs"),
        "got: {status}"
    );
    assert_eq!(sess["stale"], false);

    // Batch edits are transactional: the second misses, the first rolls back.
    let err = s.call_err(
        3,
        "replace_text",
        json!({ "path": p, "edits": [
            { "pattern": "let b = 1;", "replacement": "let b = 2;" },
            { "pattern": "absent", "replacement": "x" },
        ] }),
    );
    assert!(err.contains("edit 2"), "the error names the edit: {err}");
    assert!(err.contains("absent"), "got: {err}");
    let out = s.call_ok(4, "occur", json!({ "path": p, "pattern": "let b = 1;" }));
    assert!(out.contains("1 match"), "rollback kept b = 1, got: {out}");

    // A good batch applies in order and saves in the same call.
    let out = s.call_ok(
        5,
        "replace_text",
        json!({ "path": p, "save": true, "edits": [
            { "pattern": "let a = 2;", "replacement": "let a = 3;" },
            { "pattern": "let b = 1;", "replacement": "let b = 3;" },
        ] }),
    );
    assert!(
        out.contains("applied 2 edit(s), 2 replacement(s)"),
        "got: {out}"
    );
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .contains("let b = 3;")
    );

    // Saving a .rs buffer that no longer parses warns (never blocks).
    let out = s.call_ok(
        6,
        "replace_text",
        json!({ "path": p, "pattern": "}", "replacement": "", "save": true }),
    );
    assert!(out.contains("WARNING"), "got: {out}");
    assert!(out.contains("saved"), "warns but still saves: {out}");

    // path + session together is ambiguous and rejected.
    let err = s.call_err(
        7,
        "replace_text",
        json!({ "path": p, "session": "x", "pattern": "a", "replacement": "b" }),
    );
    assert!(err.contains("not both"), "got: {err}");

    // save:true on a no-file buffer is a clear error.
    s.call_ok(8, "open_text", json!({ "text": "scratch" }));
    let err = s.call_err(
        9,
        "replace_text",
        json!({ "pattern": "scratch", "replacement": "x", "save": true }),
    );
    assert!(err.contains("no visited file"), "got: {err}");
}

#[test]
fn open_file_and_save_buffer_reject_out_of_root_paths() {
    // The agent is granted exactly one root; everything else is off-limits.
    let root = temp_dir("reject");
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", root.as_path())]);
    s.request(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }));

    // open_file on an absolute path outside the root is refused.
    let err = s.call_err(2, "open_file", json!({ "path": "/etc/passwd" }));
    assert!(
        err.contains("outside the allowed roots"),
        "open_file error was: {err}"
    );

    // A `..` escape that climbs out of the root is refused too.
    let escape = root.join("..").join("escape.txt");
    let err = s.call_err(3, "open_file", json!({ "path": escape.to_str().unwrap() }));
    assert!(
        err.contains("outside the allowed roots"),
        "open_file ../ error was: {err}"
    );

    // Open an in-memory buffer (no FS) and try to save it outside the root.
    s.call_ok(4, "open_text", json!({ "text": "secret" }));
    let err = s.call_err(
        5,
        "save_buffer",
        json!({ "to": "/tmp/mime-escape-should-fail.txt" }),
    );
    assert!(
        err.contains("outside the allowed roots"),
        "save_buffer error was: {err}"
    );
    assert!(
        !std::path::Path::new("/tmp/mime-escape-should-fail.txt").exists(),
        "save_buffer must not have written outside the root"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn save_buffer_writes_inside_root() {
    // A save to a new file *inside* the granted root is permitted.
    let root = temp_dir("allow");
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", root.as_path())]);
    s.request(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }));

    s.call_ok(2, "open_text", json!({ "text": "hello world" }));
    let dest = root.join("out.txt");
    // `to` is the save-as destination; `path` would address a session.
    let msg = s.call_ok(3, "save_buffer", json!({ "to": dest.to_str().unwrap() }));
    assert!(msg.contains("wrote"), "save said: {msg}");
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "hello world");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn open_file_create_visits_a_new_file_written_by_the_first_save() {
    let root = temp_dir("create");
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", root.as_path())]);
    s.request(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }));
    let dest = root.join("new.rs");
    let p = dest.to_str().unwrap();
    // Without `create`, a missing file is an error that names the way in — from
    // open_file and from any tool's auto-opening `path` alike.
    let err = s.call_err(2, "open_file", json!({ "path": p }));
    assert!(err.contains("create: true"), "got: {err}");
    let err = s.call_err(3, "insert_text", json!({ "path": p, "text": "x" }));
    assert!(err.contains("create: true"), "got: {err}");
    assert!(!dest.exists(), "a refusal creates nothing");

    let ok = s.call_ok(4, "open_file", json!({ "path": p, "create": true }));
    assert!(
        ok.contains("NEW file") && ok.contains("first save"),
        "got: {ok}"
    );
    assert!(!dest.exists(), "opening creates nothing on disk");
    // Later calls address the buffer by path, and save:true creates the file.
    let ok = s.call_ok(
        5,
        "insert_text",
        json!({ "path": p, "text": "fn main() {}\n", "save": true }),
    );
    assert!(ok.contains("saved"), "got: {ok}");
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "fn main() {}\n");
    // The buffer now visits a real file: a second save passes the stale guard.
    let ok = s.call_ok(
        6,
        "replace_text",
        json!({ "path": p, "pattern": "main", "replacement": "run", "save": true }),
    );
    assert!(ok.contains("saved"), "got: {ok}");
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "fn run() {}\n");
    // An existing file opens normally under `create`.
    let ok = s.call_ok(
        7,
        "open_file",
        json!({ "path": p, "create": true, "session": "again" }),
    );
    assert!(ok.contains("12 chars"), "got: {ok}");

    // Another writer creating the file first: the stale guard refuses to
    // overwrite it, exactly as it refuses a modified one.
    let dest2 = root.join("raced.txt");
    let p2 = dest2.to_str().unwrap();
    s.call_ok(
        8,
        "open_file",
        json!({ "path": p2, "create": true, "session": "raced" }),
    );
    s.call_ok(
        9,
        "insert_text",
        json!({ "session": "raced", "text": "mine\n", "save": false }),
    );
    std::fs::write(&dest2, "theirs\n").unwrap();
    let err = s.call_err(10, "save_buffer", json!({ "session": "raced" }));
    assert!(err.contains("created on disk"), "got: {err}");
    assert_eq!(std::fs::read_to_string(&dest2).unwrap(), "theirs\n");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn read_only_session_rejects_mutation_over_stdio() {
    let mut s = Server::spawn();
    s.request(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }));

    // Attach reference material unwritable.
    let opened = s.call_ok(
        2,
        "open_text",
        json!({ "text": "reference", "read_only": true }),
    );
    assert!(opened.contains("read-only"), "open_text said: {opened}");

    // A mutating program is rejected as a tool-level error...
    let err = s.call_err(
        3,
        "run_program",
        json!({ "program": r#"(goto-char (point-max)) (insert "!")"# }),
    );
    assert!(err.contains("read-only"), "run_program error was: {err}");

    // ...and the buffer is untouched (a read-only report still works).
    let region = s.read_text(4, json!({ "start": 1, "end": 10 }));
    assert_eq!(region, "reference");
}

#[test]
fn audit_journal_records_one_line_per_run() {
    // With MIME_AUDIT set, each run_program appends one JSON line.
    let root = temp_dir("audit");
    let log = root.join("audit.jsonl");
    let mut s = Server::spawn_with_env(&[
        ("MIME_ROOTS", root.as_path()),
        ("MIME_AUDIT", log.as_path()),
    ]);
    s.request(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }));

    s.call_ok(
        2,
        "open_text",
        json!({ "text": "abc", "session": "audited" }),
    );
    s.call_ok(
        3,
        "run_program",
        json!({ "program": r#"(goto-char (point-max)) (insert "d")"#, "session": "audited" }),
    );
    s.call_ok(
        4,
        "run_program",
        json!({ "program": r#"(goto-char (point-min))"#, "session": "audited" }),
    );

    let contents = std::fs::read_to_string(&log).expect("audit log exists");
    let lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 2, "expected one line per run, got: {contents}");

    let first: Value = serde_json::from_str(lines[0]).expect("audit line is JSON");
    assert_eq!(first["session"], "audited");
    assert_eq!(first["dirty"], true);
    assert_eq!(first["len_before"], 3);
    assert_eq!(first["len_after"], 4);
    assert!(
        first["time"].as_u64().is_some(),
        "time should be a unix secs int"
    );
    assert!(first["program"].as_str().unwrap().contains("insert"));

    // The second run was a pure navigation — not dirty.
    let second: Value = serde_json::from_str(lines[1]).expect("audit line is JSON");
    assert_eq!(second["dirty"], false);

    // A rehearsed edit persists nothing, so it is not dirty either.
    s.call_ok(
        5,
        "replace_text",
        json!({ "pattern": "abc", "replacement": "xyz", "session": "audited", "rehearse": true }),
    );
    let file = root.join("doc.txt");
    std::fs::write(&file, "abc\n").unwrap();
    s.call_ok(
        6,
        "insert_text",
        json!({ "text": "uvw", "session": "audited", "rehearse": true }),
    );
    s.call_ok(
        7,
        "replace_in_files",
        json!({ "files": [file], "pattern": "abc", "replacement": "rst", "rehearse": true }),
    );
    let contents = std::fs::read_to_string(&log).expect("audit log exists");
    let rehearsed: Vec<Value> = contents
        .lines()
        .skip(2)
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(rehearsed.len(), 3, "{contents}");
    for (line, text) in rehearsed.iter().zip(["xyz", "uvw", "rst"]) {
        assert!(line["program"].as_str().unwrap().contains(text), "{line}");
        assert_eq!(line["dirty"], false, "{line}");
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn save_buffer_addresses_by_path_and_saves_as() {
    let dir = temp_dir("uniform-addr");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let p = file.to_string_lossy().into_owned();

    // save_buffer without `to` writes back to the visited file — addressed by
    // path, like every other tool, no need to know the canonical-path session
    // id.
    s.call_ok(
        6,
        "replace_text",
        json!({ "path": p, "pattern": "alpha", "replacement": "gamma", "save": false }),
    );
    let saved = s.call_ok(7, "save_buffer", json!({ "path": p }));
    assert!(saved.contains("saved"), "save said: {saved}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "gamma\n");

    // save_buffer with `to` is save-as.
    let copy = dir.join("copy.txt");
    let to = copy.to_string_lossy().into_owned();
    s.call_ok(8, "save_buffer", json!({ "path": p, "to": to }));
    assert_eq!(std::fs::read_to_string(&copy).unwrap(), "gamma\n");
}

#[test]
fn session_miss_error_names_the_warm_sessions() {
    let mut s = Server::spawn();
    s.call_ok(1, "open_text", json!({ "text": "x", "session": "alpha" }));
    s.call_ok(2, "open_text", json!({ "text": "y", "session": "beta" }));
    let err = s.call_err(
        3,
        "run_program",
        json!({ "session": "nope", "program": "(point)" }),
    );
    assert!(
        err.contains("alpha") && err.contains("beta"),
        "the miss should list the warm sessions: {err}"
    );
}

#[test]
fn undo_last_rewinds_one_mutating_call_at_a_time() {
    let mut s = Server::spawn();
    s.call_ok(1, "open_text", json!({ "text": "v0" }));

    // Two separate mutating calls, then a read (which must not consume an undo
    // step).
    s.call_ok(
        2,
        "replace_text",
        json!({ "pattern": "v0", "replacement": "v1" }),
    );
    s.call_ok(
        3,
        "replace_text",
        json!({ "pattern": "v1", "replacement": "v2" }),
    );
    let txt = s.read_text(4, json!({ "start": 1, "end": 3 }));
    assert_eq!(txt, "v2");

    // First undo: back to v1. Second: back to v0. Then the ring is dry.
    let u1 = s.call_ok(5, "undo_last", json!({}));
    assert!(u1.contains("rewound"), "undo said: {u1}");
    let txt = s.read_text(6, json!({ "start": 1, "end": 3 }));
    assert_eq!(txt, "v1");
    s.call_ok(7, "undo_last", json!({}));
    let txt = s.read_text(8, json!({ "start": 1, "end": 3 }));
    assert_eq!(txt, "v0");
    let err = s.call_err(9, "undo_last", json!({}));
    assert!(err.contains("nothing to undo"), "got: {err}");
}

#[test]
fn expect_unique_makes_ambiguous_anchors_an_error() {
    let mut s = Server::spawn();
    s.call_ok(
        1,
        "open_text",
        json!({ "text": "use a;\nuse b;\nuse a;\n" }),
    );

    // Two occurrences: the unique replace refuses and lists the lines, and
    // nothing changes.
    let err = s.call_err(
        2,
        "replace_text",
        json!({ "pattern": "use a;", "replacement": "use z;", "expect_unique": true }),
    );
    assert!(err.contains("matches at lines 1, 3"), "got: {err}");
    let txt = s.read_text(3, json!({ "start": 1, "end": 8 }));
    assert_eq!(txt, "use a;\n", "nothing replaced: {txt}");

    // A genuinely unique anchor goes through and reports its line.
    let ok = s.call_ok(
        4,
        "replace_text",
        json!({ "pattern": "use b;", "replacement": "use y;", "expect_unique": true }),
    );
    assert!(ok.contains("at line 2"), "got: {ok}");

    // expect_unique + all is a contradiction.
    let err = s.call_err(
        5,
        "replace_text",
        json!({ "pattern": "use", "replacement": "USE", "expect_unique": true, "all": true }),
    );
    assert!(err.contains("contradicts"), "got: {err}");

    // In a batch, a failed uniqueness check rolls the whole batch back.
    let err = s.call_err(
        6,
        "replace_text",
        json!({ "edits": [
            { "pattern": "use y;", "replacement": "use x;" },
            { "pattern": "use a;", "replacement": "use w;", "expect_unique": true },
        ] }),
    );
    assert!(
        err.contains("edit 2") && err.contains("expect_unique"),
        "got: {err}"
    );
    let txt = s.read_text(7, json!({ "start": 8, "end": 15 }));
    assert_eq!(txt, "use y;\n", "batch rolled back: {txt}");
}

#[test]
fn close_session_releases_and_guards_unsaved_edits() {
    let dir = temp_dir("close");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let p = file.to_string_lossy().into_owned();

    // Closing a clean session by path works and empties the status.
    s.call_ok(1, "occur", json!({ "path": p, "pattern": "alpha" }));
    let closed = s.call_ok(2, "close_session", json!({ "path": p }));
    assert!(closed.contains("closed"), "got: {closed}");
    let status: Value = serde_json::from_str(&s.call_ok(3, "session_status", json!({}))).unwrap();
    assert_eq!(status["sessions"].as_array().unwrap().len(), 0);

    // An unsaved session refuses without force, closes with it.
    s.call_ok(
        4,
        "replace_text",
        json!({ "path": p, "pattern": "alpha", "replacement": "beta", "save": false }),
    );
    let err = s.call_err(5, "close_session", json!({ "path": p }));
    assert!(err.contains("unsaved"), "got: {err}");
    let closed = s.call_ok(6, "close_session", json!({ "path": p, "force": true }));
    assert!(closed.contains("discarded"), "got: {closed}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\n");

    // Closing something that isn't warm is a clean error, not an open.
    let err = s.call_err(7, "close_session", json!({ "path": p }));
    assert!(err.contains("no warm session"), "got: {err}");
}

#[test]
fn close_session_closes_several_targets_at_once() {
    let dir = temp_dir("close-multi");
    let a = dir.join("a.txt");
    let b = dir.join("b.txt");
    std::fs::write(&a, "alpha\n").unwrap();
    std::fs::write(&b, "beta\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let pa = a.to_string_lossy().into_owned();
    let pb = b.to_string_lossy().into_owned();

    // Two warm files plus a named in-memory buffer; paths and sessions mix.
    s.call_ok(1, "occur", json!({ "path": pa, "pattern": "alpha" }));
    s.call_ok(2, "occur", json!({ "path": pb, "pattern": "beta" }));
    s.call_ok(
        3,
        "open_text",
        json!({ "text": "x", "name": "scratch", "session": "mem" }),
    );
    let closed = s.call_ok(
        4,
        "close_session",
        json!({ "paths": [pa, pb], "sessions": ["mem"] }),
    );
    assert!(closed.contains("closed 3 sessions"), "got: {closed}");
    assert!(
        closed.contains(&pa) && closed.contains(&pb) && closed.contains("mem"),
        "got: {closed}"
    );
    let status: Value = serde_json::from_str(&s.call_ok(5, "session_status", json!({}))).unwrap();
    assert_eq!(status["sessions"].as_array().unwrap().len(), 0);
}

#[test]
fn close_session_multi_is_all_or_nothing_without_force() {
    let dir = temp_dir("close-atomic");
    let a = dir.join("a.txt");
    let b = dir.join("b.txt");
    std::fs::write(&a, "alpha\n").unwrap();
    std::fs::write(&b, "beta\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let pa = a.to_string_lossy().into_owned();
    let pb = b.to_string_lossy().into_owned();

    s.call_ok(1, "occur", json!({ "path": pa, "pattern": "alpha" }));
    s.call_ok(
        2,
        "replace_text",
        json!({ "path": pb, "pattern": "beta", "replacement": "gamma", "save": false }),
    );
    // b is unsaved: nothing closes, and the error names the offender only.
    let err = s.call_err(3, "close_session", json!({ "paths": [pa, pb] }));
    assert!(err.contains("unsaved") && err.contains(&pb), "got: {err}");
    assert!(!err.contains(&pa), "clean session named as unsaved: {err}");
    let status: Value = serde_json::from_str(&s.call_ok(4, "session_status", json!({}))).unwrap();
    assert_eq!(
        status["sessions"].as_array().unwrap().len(),
        2,
        "partial close"
    );

    // A miss anywhere in the list is also a whole-call error.
    let err = s.call_err(
        5,
        "close_session",
        json!({ "paths": [pa, dir.join("nope.txt")] }),
    );
    assert!(err.contains("no warm session"), "got: {err}");
    let status: Value = serde_json::from_str(&s.call_ok(6, "session_status", json!({}))).unwrap();
    assert_eq!(
        status["sessions"].as_array().unwrap().len(),
        2,
        "partial close"
    );

    // force discards and reports which one lost edits.
    let closed = s.call_ok(
        7,
        "close_session",
        json!({ "paths": [pa, pb], "force": true }),
    );
    assert_eq!(
        closed,
        format!("closed 2 sessions:\n  \"{pa}\"\n  \"{pb}\" (unsaved edits discarded)")
    );
    assert_eq!(std::fs::read_to_string(&b).unwrap(), "beta\n");
}

#[test]
fn close_session_plural_forms_never_touch_the_default_session() {
    let dir = temp_dir("close-plural");
    let a = dir.join("a.txt");
    std::fs::write(&a, "alpha\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let pa = a.to_string_lossy().into_owned();

    // A warm "default" is what a stray fallback would close.
    s.call_ok(1, "open_text", json!({ "text": "x" }));
    s.call_ok(3, "occur", json!({ "path": pa, "pattern": "alpha" }));

    // Empty lists close nothing — and do not fall back to "default".
    let closed = s.call_ok(4, "close_session", json!({ "paths": [] }));
    assert!(closed.contains("closed 0 sessions"), "got: {closed}");
    let closed = s.call_ok(5, "close_session", json!({ "sessions": [] }));
    assert!(closed.contains("closed 0 sessions"), "got: {closed}");

    // A mistyped `all` or a malformed list is an error, not a fallback; `all`
    // refuses even an empty explicit list.
    let err = s.call_err(6, "close_session", json!({ "all": "true" }));
    assert!(err.contains("\"all\" must be a boolean"), "got: {err}");
    let err = s.call_err(7, "close_session", json!({ "paths": [1] }));
    assert!(err.contains("list of strings"), "got: {err}");
    let err = s.call_err(8, "close_session", json!({ "paths": "not-a-list" }));
    assert!(err.contains("non-array"), "got: {err}");
    let err = s.call_err(13, "close_session", json!({ "all": true, "paths": [] }));
    assert!(err.contains("all"), "got: {err}");

    // The same session via path and paths is closed once; the singular form
    // mixes with the plural one.
    let closed = s.call_ok(9, "close_session", json!({ "path": pa, "paths": [pa] }));
    assert!(closed.starts_with("closed session "), "got: {closed}");
    s.call_ok(10, "occur", json!({ "path": pa, "pattern": "alpha" }));
    let closed = s.call_ok(
        11,
        "close_session",
        json!({ "session": "default", "paths": [pa], "force": true }),
    );
    // Exact: one line per target, in the order given (not sorted).
    assert_eq!(
        closed,
        format!("closed 2 sessions:\n  \"default\"\n  \"{pa}\"")
    );
    let status: Value = serde_json::from_str(&s.call_ok(12, "session_status", json!({}))).unwrap();
    assert_eq!(status["sessions"].as_array().unwrap().len(), 0);
}

#[test]
fn close_session_all_drops_every_warm_session() {
    let dir = temp_dir("close-all");
    let a = dir.join("a.txt");
    std::fs::write(&a, "alpha\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let pa = a.to_string_lossy().into_owned();

    // Nothing warm: a clean no-op, not an error.
    let closed = s.call_ok(1, "close_session", json!({ "all": true }));
    assert!(closed.contains("closed 0 sessions"), "got: {closed}");

    // Exactly one warm session reads as a single close, not "1 sessions".
    s.call_ok(11, "occur", json!({ "path": pa, "pattern": "alpha" }));
    let closed = s.call_ok(12, "close_session", json!({ "all": true }));
    assert_eq!(closed, format!("closed session \"{pa}\""));

    s.call_ok(2, "occur", json!({ "path": pa, "pattern": "alpha" }));
    s.call_ok(
        3,
        "open_text",
        json!({ "text": "x", "name": "scratch", "session": "mem" }),
    );
    let b = dir.join("b.txt");
    std::fs::write(&b, "beta\n").unwrap();
    let pb = b.to_string_lossy().into_owned();
    s.call_ok(
        4,
        "replace_text",
        json!({ "path": pb, "pattern": "beta", "replacement": "gamma", "save": false }),
    );
    let err = s.call_err(6, "close_session", json!({ "all": true }));
    assert!(err.contains("unsaved") && err.contains(&pb), "got: {err}");
    let status: Value = serde_json::from_str(&s.call_ok(7, "session_status", json!({}))).unwrap();
    assert_eq!(
        status["sessions"].as_array().unwrap().len(),
        3,
        "partial close"
    );

    let closed = s.call_ok(8, "close_session", json!({ "all": true, "force": true }));
    assert!(closed.contains("closed 3 sessions"), "got: {closed}");
    let status: Value = serde_json::from_str(&s.call_ok(9, "session_status", json!({}))).unwrap();
    assert_eq!(status["sessions"].as_array().unwrap().len(), 0);

    // all combined with an explicit target is ambiguous.
    let err = s.call_err(
        10,
        "close_session",
        json!({ "all": true, "session": "mem" }),
    );
    assert!(err.contains("all"), "got: {err}");
}

#[test]
fn outline_scope_and_anchor_drive_structural_edits() {
    let mut s = Server::spawn();
    let src = "fn alpha() -> i64 {\n    let x = 1;\n    x\n}\n\nfn beta() -> i64 {\n    let x = 1;\n    x\n}\n";
    s.call_ok(1, "open_text", json!({ "text": src, "name": "x.rs" }));

    let outline = s.call_ok(2, "outline", json!({}));
    assert!(
        outline.contains("alpha") && outline.contains("beta") && outline.contains("rust"),
        "outline: {outline}"
    );

    // Scoped replace touches only beta's copy of the shared line.
    let ok = s.call_ok(
        3,
        "replace_text",
        json!({
            "pattern": "let x = 1;", "replacement": "let x = 2;",
            "scope": { "defun": "beta" }
        }),
    );
    assert!(ok.contains("replaced 1"), "got: {ok}");
    let end = src.chars().count() + 1;
    let txt = s.read_text(4, json!({ "start": 1, "end": end }));
    assert!(
        txt.contains("alpha() -> i64 {\n    let x = 1;"),
        "alpha untouched: {txt}"
    );
    assert!(
        txt.contains("beta() -> i64 {\n    let x = 2;"),
        "beta edited: {txt}"
    );

    // A scope miss names the defuns that exist.
    let err = s.call_err(
        5,
        "replace_text",
        json!({ "pattern": "x", "replacement": "y", "scope": { "defun": "gamma" } }),
    );
    assert!(err.contains("gamma") && err.contains("alpha"), "got: {err}");

    // Anchored insert lands between alpha and beta.
    s.call_ok(
        6,
        "insert_text",
        json!({ "text": "\n\nfn mid() -> i64 {\n    3\n}", "anchor": { "defun": "alpha" } }),
    );
    let outline = s.call_ok(7, "outline", json!({}));
    let a = outline.find("alpha").expect("alpha");
    let m = outline.find("mid").expect("mid");
    let b = outline.find("beta").expect("beta");
    assert!(a < m && m < b, "mid sits between alpha and beta: {outline}");
}

#[test]
fn view_echo_appends_a_viewport_to_edit_results() {
    let mut s = Server::spawn();
    s.call_ok(
        1,
        "open_text",
        json!({ "text": "one\ntwo\nthree\nfour\nfive\n" }),
    );
    let ok = s.call_ok(
        2,
        "replace_text",
        json!({ "pattern": "three", "replacement": "THREE", "view": 1 }),
    );
    assert!(ok.contains("— view —"), "got: {ok}");
    assert!(ok.contains("THREE"), "the viewport shows the edit: {ok}");
    // Not requested → not present.
    let ok = s.call_ok(
        3,
        "replace_text",
        json!({ "pattern": "four", "replacement": "FOUR" }),
    );
    assert!(!ok.contains("— view —"), "got: {ok}");
}

#[test]
fn diff_echo_appends_the_edits_diff_to_edit_results() {
    let mut s = Server::spawn();
    s.call_ok(
        1,
        "open_text",
        json!({ "text": "one\ntwo\nthree\nfour\nfive\n" }),
    );
    let ok = s.call_ok(
        2,
        "replace_text",
        json!({ "pattern": "three", "replacement": "THREE", "diff": true }),
    );
    assert!(ok.contains("— diff —"), "got: {ok}");
    assert!(
        ok.contains("-three\n+THREE\n"),
        "the diff shows the edit: {ok}"
    );
    // Every edit path carries it: the batch form, the thing form, insert_text.
    let ok = s.call_ok(
        3,
        "replace_text",
        json!({ "edits": [{ "pattern": "one", "replacement": "1" }, { "pattern": "two", "replacement": "2" }], "diff": true }),
    );
    assert!(
        ok.contains("-one\n") && ok.contains("+1\n") && ok.contains("+2\n"),
        "got: {ok}"
    );
    let ok = s.call_ok(
        4,
        "insert_text",
        json!({ "text": "six\n", "pos": "eob", "diff": true }),
    );
    assert!(ok.contains("+six\n"), "got: {ok}");
    let ok = s.call_ok(
        5,
        "replace_text",
        json!({ "thing": { "kind": "line", "after": "four" }, "replacement": "FOUR", "diff": true }),
    );
    assert!(ok.contains("-four\n") && ok.contains("+FOUR"), "got: {ok}");
    // Not requested → not present.
    let ok = s.call_ok(
        6,
        "replace_text",
        json!({ "pattern": "five", "replacement": "FIVE" }),
    );
    assert!(!ok.contains("— diff —"), "got: {ok}");

    // A big diff is clamped like run_program's; full_diff:true lifts the clamp.
    let text: String = (1..=300).map(|i| format!("row {i}\n")).collect();
    s.call_ok(7, "open_text", json!({ "text": text, "session": "big" }));
    let ok = s.call_ok(
        8,
        "replace_text",
        json!({ "session": "big", "pattern": "row", "replacement": "ROW", "all": true, "diff": true }),
    );
    assert!(
        ok.contains("lines elided"),
        "got: {}",
        &ok[..200.min(ok.len())]
    );
    let ok = s.call_ok(
        9,
        "replace_text",
        json!({ "session": "big", "pattern": "ROW", "replacement": "row", "all": true, "diff": true, "full_diff": true }),
    );
    assert!(
        !ok.contains("lines elided"),
        "got: {}",
        &ok[..200.min(ok.len())]
    );
    assert!(ok.contains("+row 300\n"), "the whole diff is there");
}

#[test]
fn help_serves_topics_and_lists_them_on_a_miss() {
    let mut s = Server::spawn();
    let index = s.call_ok(1, "help", json!({}));
    assert!(
        index.contains("regex") && index.contains("recipes"),
        "got: {index}"
    );
    let regex = s.call_ok(2, "help", json!({ "topic": "regex" }));
    assert!(
        regex.contains("RE2") && regex.contains("replace-regexp"),
        "got: {regex}"
    );
    let err = s.call_err(3, "help", json!({ "topic": "nope" }));
    assert!(
        err.contains("unknown help topic") && err.contains("treesit"),
        "got: {err}"
    );
}

#[test]
fn multi_file_replace_is_atomic_across_the_set() {
    let dir = temp_dir("multi-file");
    let a = dir.join("a.txt");
    let b = dir.join("b.txt");
    std::fs::write(&a, "old_name here\n").unwrap();
    std::fs::write(&b, "calls old_name twice: old_name\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let (pa, pb) = (
        a.to_string_lossy().into_owned(),
        b.to_string_lossy().into_owned(),
    );

    // The cross-file rename: one call, saved only after both succeeded.
    let ok = s.call_ok(
        1,
        "replace_in_files",
        json!({
            "files": [pa.clone(), pb.clone()],
            "pattern": "old_name", "replacement": "new_name", "all": true,
            "save": true,
        }),
    );
    assert!(ok.contains("2 file(s), 3 replacement(s)"), "got: {ok}");
    assert!(ok.contains("saved 2 file(s)"), "got: {ok}");
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "new_name here\n");
    assert_eq!(
        std::fs::read_to_string(&b).unwrap(),
        "calls new_name twice: new_name\n"
    );

    // A miss in the SECOND file rolls the first back: nothing changes anywhere
    // (warm buffers included).
    let err = s.call_err(
        2,
        "replace_in_files",
        json!({
            "files": [pa.clone(), pb.clone()],
            "pattern": "new_name here", "replacement": "X",
        }),
    );
    assert!(err.contains("rolled back"), "got: {err}");

    // The files form no longer rides on replace_text — the argument guard
    // points at what IS accepted.
    let err = s.call_err(
        21,
        "replace_text",
        json!({ "files": [pa.clone()], "pattern": "x", "replacement": "y" }),
    );
    assert!(err.contains("unknown argument \"files\""), "got: {err}");
    let txt = s.read_text(3, json!({ "path": pa, "start": 1, "end": 9 }));
    assert_eq!(txt, "new_name", "file a's warm buffer was rolled back");
}

#[test]
fn warm_sessions_are_bounded_with_clean_lru_eviction() {
    let mut s = Server::spawn();
    // Fill to the cap with clean scratch sessions; edit one so it holds
    // un-persisted content (eviction must never take it).
    for i in 0..16 {
        s.call_ok(
            i + 1,
            "open_text",
            json!({ "text": "x", "session": format!("s{i}") }),
        );
    }
    s.call_ok(
        100,
        "replace_text",
        json!({ "session": "s3", "pattern": "x", "replacement": "edited" }),
    );

    // Opening more sessions evicts clean LRU entries, never s3.
    for i in 16..20 {
        s.call_ok(
            i as i64 + 1,
            "open_text",
            json!({ "text": "x", "session": format!("s{i}") }),
        );
    }
    let status: Value = serde_json::from_str(&s.call_ok(200, "session_status", json!({}))).unwrap();
    let ids: Vec<String> = status["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["id"].as_str().unwrap().to_string())
        .collect();
    assert!(ids.len() <= 16, "bounded: {ids:?}");
    assert!(
        ids.contains(&"s3".to_string()),
        "the edited session survives"
    );
    assert!(
        !ids.contains(&"s0".to_string()),
        "an idle clean session was evicted: {ids:?}"
    );
}

/// A program's final form value is surfaced as `value` — a string RAW
/// (unquoted, unescaped), other types tulisp-printed — so a read-only inspector
/// like `(conflict-diff N)` is readable without wrapping it in `(message …)`; a
/// `nil` value is omitted, like `stale`/`unsaved`.
#[test]
fn run_program_surfaces_the_final_form_value() {
    let mut s = Server::spawn();
    s.request(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }));
    s.call_ok(2, "open_text", json!({ "text": "hello" }));

    // A non-nil string value comes back raw (no quotes, escapes undone), riding
    // alongside an empty read-only `diff`.
    let out: Value = serde_json::from_str(&s.call_ok(
        3,
        "run_program",
        json!({ "program": r#"(concat (upcase "hi") "\n2nd")"# }),
    ))
    .unwrap();
    assert_eq!(out["value"], json!("HI\n2nd"));
    assert_eq!(out["diff"], json!(""));

    // A non-string value keeps its printed form.
    let out: Value =
        serde_json::from_str(&s.call_ok(31, "run_program", json!({ "program": "(+ 40 2)" })))
            .unwrap();
    assert_eq!(out["value"], json!("42"));

    // A final nil omits the field entirely.
    let out: Value = serde_json::from_str(&s.call_ok(
        4,
        "run_program",
        json!({ "program": "(goto-char (point-min)) nil" }),
    ))
    .unwrap();
    assert!(out.get("value").is_none(), "nil value is omitted: {out}");
}

/// An argument the tool doesn't declare is rejected (naming it and the valid
/// arguments), not silently dropped — e.g. `view {offset}` for its `pos`.
#[test]
fn unknown_argument_is_rejected_not_ignored() {
    let mut s = Server::spawn();
    s.request(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }));
    s.call_ok(
        2,
        "open_text",
        json!({ "text": "line one\nline two\nline three" }),
    );

    let err = s.call_err(3, "view", json!({ "offset": 2 }));
    assert!(err.contains("offset"), "names the offender: {err}");
    assert!(err.contains("pos"), "lists valid arguments: {err}");

    // A declared argument still works.
    s.call_ok(4, "view", json!({ "pos": 1 }));
}

/// `rehearse` shares run_program's handler and reads `full_diff`/`view`, so its
/// schema must declare them — otherwise arg-validation rejects valid rehearse
/// calls. A rehearsed bulk edit is exactly when the full preview diff matters.
/// Regression guard for the schema/handler mismatch.
#[test]
fn rehearse_accepts_full_diff_and_view() {
    let mut s = Server::spawn();
    s.request(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }));
    s.call_ok(2, "open_text", json!({ "text": "alpha\nbeta\ngamma" }));

    // full_diff + view are run_program args; rehearse: true must accept them,
    // not reject them as unknown.
    let out: Value = serde_json::from_str(&s.call_ok(
        3,
        "run_program",
        json!({
            "program": r#"(goto-char (point-min)) (while (re-search-forward "a" nil t) (replace-match "A"))"#,
            "full_diff": true,
            "view": 4,
            "rehearse": true,
        }),
    ))
    .unwrap();
    assert_eq!(out["rehearsed"], json!(true), "rehearse rolls back: {out}");
    assert!(
        out["diff"].as_str().is_some_and(|d| !d.is_empty()),
        "full_diff returns the preview: {out}"
    );
}

/// The modern era's per-request `_meta`: protocol version + client capabilities
/// on every call, in place of the `initialize` handshake.
fn meta() -> Value {
    json!({
        mime_rs::rpc::META_PROTOCOL_VERSION: mime_rs::rpc::PROTOCOL_VERSION,
        mime_rs::rpc::META_CLIENT_CAPABILITIES: {},
        "io.modelcontextprotocol/clientInfo": {"name": "e2e", "version": "0"},
    })
}

/// A 2026-07-28 client never handshakes: it discovers, lists and calls, and
/// every reply comes back shaped (`resultType` + `_meta.serverInfo`).
#[test]
fn modern_stdio_conversation_needs_no_handshake() {
    let mut s = Server::spawn();
    let d = s.request(
        json!({"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta": meta()}}),
    );
    assert_eq!(d["result"]["resultType"], "complete");
    assert_eq!(d["result"]["supportedVersions"][0], "2026-07-28");
    assert_eq!(
        d["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "mime-rs"
    );
    assert_eq!(d["result"]["serverInfo"]["name"], "mime-rs");

    let l =
        s.request(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{"_meta": meta()}}));
    assert_eq!(l["result"]["cacheScope"], "public");
    assert!(
        !l["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "open_workspace"),
        "open_workspace only lists on HTTP"
    );

    let o = s.request(
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"open_text","arguments":{"text":"a\nb\n","session":"m"},"_meta": meta()}}),
    );
    assert_eq!(o["result"]["isError"], false);
    assert_eq!(o["result"]["resultType"], "complete");
    assert!(
        o["result"].get("structuredContent").is_none(),
        "a prose tool carries no structured value: {}",
        o["result"]
    );

    let v = s.request(
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"view","arguments":{"session":"m"},"_meta": meta()}}),
    );
    assert!(
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("a")
    );

    let bad = s.request(
        json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"view","arguments":{"session":"m","workspace":"0000000000000000ffffffffffffffff"},"_meta": meta()}}),
    );
    assert_eq!(bad["result"]["isError"], true);
    assert!(
        bad["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown workspace")
    );

    let unsupported = s.request(
        json!({"jsonrpc":"2.0","id":6,"method":"ping","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2031-01-01","io.modelcontextprotocol/clientCapabilities":{}}}}),
    );
    assert_eq!(unsupported["error"]["code"], -32022);
}

/// One process serves both eras at once: a dual-era client can probe the modern
/// way, fall back to the handshake, and keep using either shape.
#[test]
fn both_eras_interleave_on_one_process() {
    let mut s = Server::spawn();
    // A dual-era client probes, then falls back to the handshake.
    let d = s.request(
        json!({"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta": meta()}}),
    );
    assert_eq!(d["result"]["resultType"], "complete");
    let i = s.request(
        json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"e2e","version":"0"}}}),
    );
    assert_eq!(i["result"]["protocolVersion"], "2025-11-25");
    assert!(i["result"].get("resultType").is_none());
    s.notify(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    // Modern and legacy calls share stdio's one implicit workspace.
    let m = s.request(
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"open_text","arguments":{"text":"shared\n","session":"x"},"_meta": meta()}}),
    );
    assert_eq!(m["result"]["resultType"], "complete");
    let text = s.call_ok(4, "view", json!({"session":"x"}));
    assert!(text.contains("shared"));
}

/// The legacy (initialize-based) wire format is frozen: every response below
/// must stay byte-identical (after `normalise`) across the dual-era work.
/// Regenerate deliberately with `MIME_UPDATE_FIXTURES=1 cargo test --test mcp
/// legacy_wire_format_is_frozen` and review the diff.
#[test]
fn legacy_wire_format_is_frozen() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/legacy");
    let update = std::env::var_os("MIME_UPDATE_FIXTURES").is_some();
    let mut s = Server::spawn();

    let script: Vec<(&str, Value)> = vec![
        (
            "initialize-2024-11-05",
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"golden","version":"0"}}}),
        ),
        (
            "initialize-2025-03-26",
            json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"golden","version":"0"}}}),
        ),
        (
            "initialize-2025-06-18",
            json!({"jsonrpc":"2.0","id":3,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"golden","version":"0"}}}),
        ),
        (
            "initialize-2025-11-25",
            json!({"jsonrpc":"2.0","id":4,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"golden","version":"0"}}}),
        ),
        ("ping", json!({"jsonrpc":"2.0","id":5,"method":"ping"})),
        (
            "tools-list",
            json!({"jsonrpc":"2.0","id":6,"method":"tools/list"}),
        ),
        (
            "open-text",
            json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"open_text","arguments":{"text":"hello\nworld\n","session":"g"}}}),
        ),
        (
            "view",
            json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"view","arguments":{"session":"g"}}}),
        ),
        (
            "session-status",
            json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"session_status","arguments":{}}}),
        ),
        (
            "unknown-method",
            json!({"jsonrpc":"2.0","id":10,"method":"no/such"}),
        ),
    ];

    let mut failures = Vec::new();
    for (name, req) in script {
        let mut got = s.request(req);
        normalise(&mut got);
        let path = dir.join(format!("{name}.json"));
        let rendered = serde_json::to_string_pretty(&got).unwrap() + "\n";
        if update {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(&path, &rendered).unwrap();
            continue;
        }
        let want = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "missing fixture {}: {e} (MIME_UPDATE_FIXTURES=1 to create)",
                path.display()
            )
        });
        if want != rendered {
            failures.push(format!(
                "{name}: fixture differs\n--- want\n{want}\n--- got\n{rendered}"
            ));
        }
    }
    drop(s); // the Drop impl kills and reaps the child
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// Strip the parts of a response that legitimately vary between runs and
/// machines: `session_status` reports the allowed roots and audit flag, which
/// depend on the environment, and the workspace handle, which is random.
/// Everything else is compared verbatim.
fn normalise(v: &mut Value) {
    let Some(text) = v["result"]["content"][0]["text"].as_str() else {
        return;
    };
    let Ok(mut inner) = serde_json::from_str::<Value>(text) else {
        return;
    };
    let mut touched = false;
    if inner.get("roots").is_some() {
        inner["roots"] = json!([]);
        inner["audit"] = json!(false);
        touched = true;
    }
    // The workspace handle is freshly minted per run. A null one is stable —
    // and says something (the handle is not echoed off modern HTTP) — so it is
    // frozen as it stands rather than masked.
    if inner["workspace"].is_string() {
        inner["workspace"] = json!("<handle>");
        touched = true;
    }
    if touched {
        v["result"]["content"][0]["text"] = Value::String(inner.to_string());
    }
    // session_status also answers as `structuredContent`; blank the same
    // per-run fields there, or the fixture would freeze this machine's roots.
    if v["result"]["structuredContent"].get("roots").is_some() {
        v["result"]["structuredContent"]["roots"] = json!([]);
        v["result"]["structuredContent"]["audit"] = json!(false);
        if v["result"]["structuredContent"]["workspace"].is_string() {
            v["result"]["structuredContent"]["workspace"] = json!("<handle>");
        }
    }
}

/// `fill_text` reflows the prose unit an anchor, a position, a line range or
/// `all` names, to `column`, and refuses code by naming it.
#[test]
fn fill_text_reflows_comments_and_paragraphs_and_refuses_code() {
    let dir = temp_dir("fill-text");
    let rs = dir.join("lib.rs");
    std::fs::write(
        &rs,
        "/// aaa bbb\n/// ccc\nfn f() {\n    // ddd\n    // eee\n}\n",
    )
    .unwrap();
    let md = dir.join("README.md");
    std::fs::write(
        &md,
        "aaa bbb ccc ddd\n\n- eee fff ggg\n\n```\ncode  here\n```\n",
    )
    .unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let rs_p = rs.to_string_lossy().into_owned();
    let md_p = md.to_string_lossy().into_owned();

    // The anchor form: the unique line containing the text picks the unit.
    let out = s.call_ok(
        1,
        "fill_text",
        json!({ "path": rs_p, "anchor": { "pattern": "/// aaa" } }),
    );
    assert!(out.contains("filled comment @1-17 (lines 1-1)"), "{out}");
    let txt = s.read_text(2, json!({ "path": rs_p, "lines": [1, 1] }));
    assert_eq!(txt, "/// aaa bbb ccc");

    // Code is refused, naming what the position is in.
    let err = s.call_err(3, "fill_text", json!({ "path": rs_p, "pos": 20 }));
    assert!(err.contains("function_item"), "{err}");

    // A line range fills every unit it touches and leaves the code alone.
    let out = s.call_ok(
        4,
        "fill_text",
        json!({ "path": rs_p, "lines": [3, 5], "column": 20 }),
    );
    assert!(out.contains("filled 1 of 1"), "{out}");
    let txt = s.read_text(5, json!({ "path": rs_p, "lines": [2, 4] }));
    assert_eq!(txt, "fn f() {\n    // ddd eee\n}");

    // `all` over a Markdown file, saved: fences survive, paragraphs wrap.
    let out = s.call_ok(
        6,
        "fill_text",
        json!({ "path": md_p, "all": true, "column": 9, "save": true }),
    );
    assert!(out.contains("filled 2 of 2"), "{out}");
    assert_eq!(
        std::fs::read_to_string(&md).unwrap(),
        "aaa bbb\nccc ddd\n\n- eee fff\n  ggg\n\n```\ncode  here\n```\n"
    );

    // An explicit prefix overrides detection: no grammar calls `;;` a comment
    // marker in a text file, and the prefix bounds the paragraph by the lines
    // that carry it.
    let txt_f = dir.join("notes.txt");
    std::fs::write(&txt_f, ";; aaa\n;; bbb\nplain\n").unwrap();
    let txt_p = txt_f.to_string_lossy().into_owned();
    let out = s.call_ok(
        7,
        "fill_text",
        json!({ "path": txt_p, "pos": 1, "prefix": ";; " }),
    );
    assert!(out.contains("filled paragraph @1-12"), "{out}");
    let txt = s.read_text(8, json!({ "path": txt_p, "lines": [1, 2] }));
    assert_eq!(txt, ";; aaa bbb\nplain");

    // Two target forms at once is an error, not a guess.
    let err = s.call_err(
        9,
        "fill_text",
        json!({ "path": md_p, "pos": 1, "all": true }),
    );
    assert!(err.contains("one of"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Non-ASCII text through every target form, and `prefix` over a region.
#[test]
fn fill_text_survives_non_ascii_and_fills_prefixed_regions() {
    let dir = temp_dir("fill-text-utf8");
    let rs = dir.join("lib.rs");
    std::fs::write(&rs, "// café au\n// lait\nfn f() {}\n").unwrap();
    let txt = dir.join("notes.txt");
    std::fs::write(&txt, ";; aaa bbb\n;; ccc\nplain\n;; ddd\n;; eee\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let rs_p = rs.to_string_lossy().into_owned();
    let txt_p = txt.to_string_lossy().into_owned();

    // A range ending after a multi-byte char must not take the server down.
    let out = s.call_ok(1, "fill_text", json!({ "path": rs_p, "lines": [1, 1] }));
    assert!(out.contains("filled 1 of 1"), "{out}");
    let txt_out = s.read_text(2, json!({ "path": rs_p, "lines": [1, 1] }));
    assert_eq!(txt_out, "// café au lait");

    let out = s.call_ok(
        3,
        "fill_text",
        json!({ "path": txt_p, "all": true, "prefix": ";; " }),
    );
    assert!(out.contains("filled 2 of 2"), "{out}");
    let txt_out = s.read_text(4, json!({ "path": txt_p, "lines": [1, 3] }));
    assert_eq!(txt_out, ";; aaa bbb ccc\nplain\n;; ddd eee");

    // `all` must be a boolean: a string is an error, not a silent point fill.
    let err = s.call_err(5, "fill_text", json!({ "path": txt_p, "all": "yes" }));
    assert!(err.contains("must be a boolean"), "{err}");

    // An error that merely mentions an anchor sentinel is not an anchor error
    // when the call had no anchor.
    let err = s.call_err(
        6,
        "fill_text",
        json!({ "path": txt_p, "pos": 1, "prefix": "__no_anchor__ " }),
    );
    assert!(err.contains("does not carry the fill-prefix"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn edit_tools_save_by_default() {
    let dir = temp_dir("save-default");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let p = file.to_string_lossy().into_owned();

    s.call_ok(
        1,
        "replace_text",
        json!({ "path": p, "pattern": "alpha", "replacement": "beta" }),
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "beta\n");
    s.call_ok(
        2,
        "insert_text",
        json!({ "path": p, "pos": "eob", "text": "gamma\n" }),
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "beta\ngamma\n");
    s.call_ok(
        3,
        "run_program",
        json!({ "path": p, "program": "(goto-char (point-min)) (insert \"# \")" }),
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "# beta\ngamma\n");

    let other = dir.join("other.txt");
    std::fs::write(&other, "one\n").unwrap();
    s.call_ok(
        4,
        "replace_in_files",
        json!({ "files": [other.to_string_lossy()], "pattern": "one", "replacement": "two" }),
    );
    assert_eq!(std::fs::read_to_string(&other).unwrap(), "two\n");

    let prose = dir.join("prose.md");
    std::fs::write(&prose, "aaa bbb ccc ddd\n").unwrap();
    s.call_ok(
        5,
        "fill_text",
        json!({ "path": prose.to_string_lossy(), "all": true, "column": 8 }),
    );
    assert_eq!(
        std::fs::read_to_string(&prose).unwrap(),
        "aaa bbb\nccc ddd\n"
    );
}

#[test]
fn save_false_holds_the_edit_in_the_buffer() {
    let dir = temp_dir("save-false");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let p = file.to_string_lossy().into_owned();

    let msg = s.call_ok(
        1,
        "replace_text",
        json!({ "path": p, "pattern": "alpha", "replacement": "beta", "save": false }),
    );
    assert!(
        msg.contains("unsaved"),
        "the reminder names the held edit: {msg}"
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\n");
    s.call_ok(2, "save_buffer", json!({ "path": p }));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "beta\n");
}

#[test]
fn rehearse_flag_previews_without_touching_buffer_or_disk() {
    let dir = temp_dir("rehearse-flag");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let prose = dir.join("prose.md");
    std::fs::write(&prose, "aaa bbb ccc ddd\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let p = file.to_string_lossy().into_owned();
    let pm = prose.to_string_lossy().into_owned();

    let out = s.call_ok(
        1,
        "replace_text",
        json!({ "path": p, "pattern": "alpha", "replacement": "beta", "rehearse": true }),
    );
    assert!(
        out.starts_with("rehearsed"),
        "flagged as a rehearsal: {out}"
    );
    assert!(out.contains("+beta"), "the preview carries the diff: {out}");
    assert!(!out.contains("unsaved"), "a rehearsal holds nothing: {out}");

    let out = s.call_ok(
        2,
        "insert_text",
        json!({ "path": p, "pos": "eob", "text": "X\n", "rehearse": true }),
    );
    assert!(out.contains("+X"), "insert preview: {out}");

    let out = s.call_ok(
        3,
        "fill_text",
        json!({ "path": pm, "all": true, "column": 8, "rehearse": true }),
    );
    assert!(out.contains("+aaa bbb"), "fill preview: {out}");

    let out: Value = serde_json::from_str(&s.call_ok(
        4,
        "run_program",
        json!({ "path": p, "program": "(insert \"Z\")", "rehearse": true }),
    ))
    .unwrap();
    assert_eq!(out["rehearsed"], true);
    assert!(out["diff"].as_str().unwrap().contains("+Zalpha"), "{out}");
    assert!(
        out.get("saved").is_none(),
        "a rehearsal saves nothing: {out}"
    );

    let out = s.call_ok(
        5,
        "replace_in_files",
        json!({ "files": [p], "pattern": "alpha", "replacement": "beta", "rehearse": true }),
    );
    assert!(out.starts_with("rehearsed"), "{out}");

    // Nothing reached the disk, and no buffer holds an edit.
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\n");
    assert_eq!(
        std::fs::read_to_string(&prose).unwrap(),
        "aaa bbb ccc ddd\n"
    );
    let st: Value = serde_json::from_str(&s.call_ok(6, "session_status", json!({}))).unwrap();
    assert!(
        st["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|x| x["unsaved"] == false),
        "{st}"
    );

    let err = s.call_err(
        7,
        "replace_text",
        json!({ "path": p, "pattern": "alpha", "replacement": "beta", "rehearse": true, "save": true }),
    );
    assert!(err.contains("drop save: true"), "got: {err}");

    // A flag that is not a boolean is refused before anything runs.
    for (id, flag) in [(9, "rehearse"), (10, "save")] {
        let err = s.call_err(
            id,
            "replace_text",
            json!({ "path": p, "pattern": "alpha", "replacement": "beta", flag: "true" }),
        );
        assert!(err.contains("must be a boolean"), "got: {err}");
    }
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\n");

    // A real edit after the rehearsals starts from the untouched text.
    s.call_ok(
        8,
        "replace_text",
        json!({ "path": p, "pattern": "alpha", "replacement": "gamma" }),
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "gamma\n");
}

/// A rehearsal with `view: true` runs a SECOND program afterwards
/// (`view_echo`'s follow-up read), which also pushes onto the undo ring. On a
/// ring already at capacity that push (and the edit's own) evict the oldest
/// entries without changing the ring's LENGTH — so a rollback that only
/// restores the length, not the ring's contents, would leave the rehearsed edit
/// on top for a later undo_last to apply and save.
#[test]
fn rehearse_on_a_full_undo_ring_does_not_leak_onto_it() {
    let dir = temp_dir("rehearse-undo-ring");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let p = file.to_string_lossy().into_owned();

    // Nine real, saved edits — one more than the undo ring's capacity (8) — so
    // the ring is already full and has evicted its very first state.
    for i in 0..9 {
        s.call_ok(
            i + 1,
            "insert_text",
            json!({ "path": p, "pos": "eob", "text": format!("l{i}\n") }),
        );
    }
    let before = std::fs::read_to_string(&file).unwrap();
    assert!(before.ends_with("l8\n"));

    let out = s.call_ok(
        10,
        "replace_text",
        json!({
            "path": p, "pattern": "alpha", "replacement": "REHEARSED",
            "rehearse": true, "view": true,
        }),
    );
    assert!(out.starts_with("rehearsed"), "{out}");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        before,
        "a rehearsal never touches disk, full ring or not"
    );

    // undo_last must rewind the last REAL edit (dropping "l8\n"), never the
    // rehearsed one — and never save the rehearsed text.
    let out = s.call_ok(11, "undo_last", json!({ "path": p }));
    assert!(!out.contains("REHEARSED"), "got: {out}");
    let after_undo = std::fs::read_to_string(&file).unwrap();
    assert!(!after_undo.contains("REHEARSED"), "got: {after_undo}");
    assert_eq!(
        after_undo,
        before.strip_suffix("l8\n").unwrap(),
        "undo rewinds the last real edit, not the rehearsed one"
    );

    // The rehearsal did not cost the ring an extra real step either: draining
    // it the rest of the way (each real edit's own save triggers a syntax check
    // that pushes its post-edit state too, so undo_last's "the top may already
    // be the current state" skip retires one extra entry for free on the first
    // call above) reaches the same depth a run without any rehearsal would,
    // then errors with nothing left — never more, never fewer.
    let mut drained = 0;
    loop {
        let resp = s.request(json!({
            "jsonrpc": "2.0", "id": 12, "method": "tools/call",
            "params": { "name": "undo_last", "arguments": { "path": p } },
        }));
        if resp["result"]["isError"] == true {
            break;
        }
        drained += 1;
    }
    assert_eq!(
        drained, 6,
        "the ring's remaining real depth after the first undo"
    );
}

#[test]
fn a_rehearsed_replace_in_files_keeps_a_full_undo_ring() {
    let dir = temp_dir("rif-rehearse-ring");
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    // Two files with the same history, more edits than the undo ring holds;
    // only the first then takes a rehearsed replace_in_files. The edits stay
    // unsaved so the ring's top is not the current text (a save's follow-up
    // read would push it), and a push at the rehearsal evicts an entry.
    let paths: Vec<String> = ["rehearsed.txt", "control.txt"]
        .iter()
        .map(|name| {
            let file = dir.join(name);
            std::fs::write(&file, "v0\n").unwrap();
            file.to_string_lossy().into_owned()
        })
        .collect();
    let mut id = 0;
    for p in &paths {
        for i in 0..9 {
            id += 1;
            s.call_ok(
                id,
                "replace_text",
                json!({ "path": p, "pattern": format!("v{i}"), "replacement": format!("v{}", i + 1), "save": false }),
            );
        }
    }
    let out = s.call_ok(
        100,
        "replace_in_files",
        json!({ "files": [paths[0]], "pattern": "v9", "replacement": "zz", "rehearse": true }),
    );
    assert!(out.starts_with("rehearsed"), "{out}");

    // The rehearsal cost no undo step: both files rewind equally far.
    let mut reached = Vec::new();
    for (k, p) in paths.iter().enumerate() {
        let mut steps = 0;
        loop {
            let id = 200 + 100 * k as i64 + steps;
            let resp = s.request(json!({
                "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": { "name": "undo_last", "arguments": { "path": p, "save": false } },
            }));
            if resp["result"]["isError"] == true {
                break;
            }
            steps += 1;
        }
        let text = s.read_text(500 + k as i64, json!({ "path": p, "start": 1, "end": 3 }));
        reached.push((steps, text));
    }
    assert_eq!(reached[0], reached[1], "rehearsed vs control");
    assert_eq!(reached[0], (8, "v1".to_string()), "a full ring's worth");
}

#[test]
fn a_rehearsal_leaves_a_clean_buffer_clean() {
    let dir = temp_dir("rehearse-clean");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "one\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let p = file.to_string_lossy().into_owned();
    s.call_ok(1, "open_file", json!({ "path": p }));

    // A rehearsed re-read, then a rehearsed edit of a buffer whose file changed
    // on disk: each re-reads the file and is rolled back.
    s.call_ok(
        2,
        "run_program",
        json!({ "path": p, "program": "(revert-buffer)", "rehearse": true }),
    );
    std::fs::write(&file, "two, longer\n").unwrap();
    s.call_ok(
        3,
        "replace_text",
        json!({ "path": p, "pattern": "two", "replacement": "three", "rehearse": true }),
    );

    let st: Value = serde_json::from_str(&s.call_ok(4, "session_status", json!({}))).unwrap();
    assert!(
        st["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|x| x["unsaved"] == false),
        "{st}"
    );
    s.call_ok(
        5,
        "replace_text",
        json!({ "path": p, "pattern": "two", "replacement": "four" }),
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "four, longer\n");
}

#[test]
fn a_read_only_program_does_not_rewrite_the_file() {
    let dir = temp_dir("no-rewrite");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(946_684_800);
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let p = file.to_string_lossy().into_owned();

    let out: Value = serde_json::from_str(&s.call_ok(
        1,
        "run_program",
        json!({ "path": p, "program": "(point)" }),
    ))
    .unwrap();
    assert!(out.get("saved").is_none(), "nothing to save: {out}");
    assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), old);
}

#[test]
fn a_read_only_call_does_not_save_held_edits() {
    let dir = temp_dir("held-edits");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let p = file.to_string_lossy().into_owned();

    s.call_ok(
        1,
        "replace_text",
        json!({ "path": p, "pattern": "alpha", "replacement": "beta", "save": false }),
    );
    // A read-only program changes nothing, so the default save leaves the held
    // edit alone.
    s.call_ok(2, "run_program", json!({ "path": p, "program": "(point)" }));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\n");
    // An explicit save: true writes whatever the buffer holds.
    s.call_ok(
        3,
        "run_program",
        json!({ "path": p, "program": "(point)", "save": true }),
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "beta\n");
}

#[test]
fn an_edit_that_changes_no_text_leaves_the_buffer_clean() {
    let dir = temp_dir("identity-edit");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let p = file.to_string_lossy().into_owned();
    let unsaved = |s: &mut Server, id| -> bool {
        let st: Value = serde_json::from_str(&s.call_ok(id, "session_status", json!({}))).unwrap();
        st["sessions"][0]["unsaved"] == true
    };

    let out = s.call_ok(
        1,
        "replace_text",
        json!({ "path": p, "pattern": "alpha", "replacement": "alpha" }),
    );
    assert!(!out.contains("unsaved"), "{out}");
    assert!(!unsaved(&mut s, 2));

    // Held edits stay held: the same identity edit neither saves them nor marks
    // the buffer clean.
    s.call_ok(
        3,
        "replace_text",
        json!({ "path": p, "pattern": "alpha", "replacement": "beta", "save": false }),
    );
    s.call_ok(
        4,
        "replace_text",
        json!({ "path": p, "pattern": "beta", "replacement": "beta" }),
    );
    assert!(unsaved(&mut s, 5));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\n");
}

#[test]
fn replace_in_files_writes_nothing_when_one_file_is_stale() {
    let dir = temp_dir("rif-stale");
    let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
    std::fs::write(&a, "foo a\n").unwrap();
    std::fs::write(&b, "foo b\nzzz\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let (pa, pb) = (
        a.to_string_lossy().into_owned(),
        b.to_string_lossy().into_owned(),
    );

    // b holds an unsaved edit when an outside writer changes its file.
    s.call_ok(
        1,
        "replace_text",
        json!({ "path": pb, "pattern": "zzz", "replacement": "yyy", "save": false }),
    );
    std::fs::write(&b, "foo b, written outside\n").unwrap();

    let err = s.call_err(
        2,
        "replace_in_files",
        json!({ "files": [pa, pb], "pattern": "foo", "replacement": "bar" }),
    );
    assert!(err.contains("no file was written"), "{err}");
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "foo a\n");
    assert_eq!(
        std::fs::read_to_string(&b).unwrap(),
        "foo b, written outside\n"
    );

    // a's edit rolled back too, so the same edit on a alone lands once.
    s.call_ok(
        3,
        "replace_in_files",
        json!({ "files": [pa], "pattern": "foo", "replacement": "bar" }),
    );
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "bar a\n");
}

#[test]
fn replace_in_files_saves_past_a_session_it_evicted() {
    // More files than the session cap: opening the later ones evicts the
    // earliest clean session, here one whose edit left its text as it was.
    let dir = temp_dir("rif-evict");
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let files: Vec<String> = (0..18)
        .map(|i| {
            let file = dir.join(format!("f{i}.txt"));
            std::fs::write(&file, if i == 1 { "Bar\n" } else { "Foo\n" }).unwrap();
            file.to_string_lossy().into_owned()
        })
        .collect();
    s.call_ok(
        1,
        "replace_in_files",
        json!({ "files": files, "pattern": "Foo\\|Bar", "replacement": "Bar", "mode": "regex" }),
    );
    for f in &files {
        assert_eq!(std::fs::read_to_string(f).unwrap(), "Bar\n", "{f}");
    }
}

#[test]
fn replace_in_files_saves_only_the_files_it_changed() {
    let dir = temp_dir("rif-count");
    let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
    std::fs::write(&a, "Foo\n").unwrap();
    std::fs::write(&b, "Bar\n").unwrap();
    let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
    std::fs::File::options()
        .write(true)
        .open(&b)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let out = s.call_ok(
        1,
        "replace_in_files",
        json!({ "files": [a, b], "pattern": "Foo\\|Bar", "replacement": "Bar", "mode": "regex" }),
    );
    assert!(out.contains("saved 1 file(s)"), "{out}");
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "Bar\n");
    assert_eq!(std::fs::metadata(&b).unwrap().modified().unwrap(), old);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_buffer_without_a_file_skips_the_default_save() {
    let mut s = Server::spawn();
    s.call_ok(1, "open_text", json!({ "text": "x" }));
    // The default save has nowhere to write, so it quietly does nothing.
    s.call_ok(
        2,
        "replace_text",
        json!({ "pattern": "x", "replacement": "y" }),
    );
    // An explicit save:true still errors.
    let err = s.call_err(
        3,
        "replace_text",
        json!({ "pattern": "y", "replacement": "z", "save": true }),
    );
    assert!(err.contains("no visited file"), "got: {err}");
}

#[test]
fn undo_last_writes_the_rewound_text() {
    let dir = temp_dir("undo-writes");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let p = file.to_string_lossy().into_owned();

    s.call_ok(
        1,
        "replace_text",
        json!({ "path": p, "pattern": "alpha", "replacement": "beta" }),
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "beta\n");
    s.call_ok(2, "undo_last", json!({ "path": p }));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\n");

    // save:false rewinds the buffer only.
    s.call_ok(
        3,
        "replace_text",
        json!({ "path": p, "pattern": "alpha", "replacement": "beta" }),
    );
    let out = s.call_ok(4, "undo_last", json!({ "path": p, "save": false }));
    assert!(out.contains("unsaved"), "got: {out}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "beta\n");
}

#[test]
fn undo_last_across_a_re_read_from_disk_does_not_overwrite_the_file() {
    let dir = temp_dir("undo-reread");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let p = file.to_string_lossy().into_owned();

    s.call_ok(
        1,
        "replace_text",
        json!({ "path": p, "pattern": "alpha", "replacement": "beta" }),
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "beta\n");
    // An external writer replaces the file; the next read re-reads it.
    let external = "written by someone else, a different length\n";
    std::fs::write(&file, external).unwrap();
    s.call_ok(2, "view", json!({ "path": p }));

    // Rewinding past the re-read must not write the pre-edit text over the
    // external content.
    let err = s.call_err(3, "undo_last", json!({ "path": p }));
    assert!(err.contains("refusing to save"), "got: {err}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), external);
}

#[test]
fn restore_checkpoint_across_a_re_read_from_disk_does_not_overwrite_the_file() {
    let dir = temp_dir("restore-reread");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let p = file.to_string_lossy().into_owned();

    s.call_ok(
        1,
        "run_program",
        json!({ "path": p, "program": "(checkpoint \"cp\")" }),
    );
    s.call_ok(
        2,
        "replace_text",
        json!({ "path": p, "pattern": "alpha", "replacement": "beta" }),
    );
    let external = "written by someone else, a different length\n";
    std::fs::write(&file, external).unwrap();
    s.call_ok(3, "view", json!({ "path": p }));

    let err = s.call_err(
        4,
        "run_program",
        json!({ "path": p, "program": "(restore-checkpoint \"cp\")" }),
    );
    assert!(err.contains("refusing to save"), "got: {err}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), external);
}

#[test]
fn restore_checkpoint_after_a_saved_edit_writes_the_checkpoint() {
    let dir = temp_dir("restore-writes");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let p = file.to_string_lossy().into_owned();

    s.call_ok(
        1,
        "run_program",
        json!({ "path": p, "program": "(checkpoint \"cp\")" }),
    );
    s.call_ok(
        2,
        "replace_text",
        json!({ "path": p, "pattern": "alpha", "replacement": "beta" }),
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "beta\n");
    s.call_ok(
        3,
        "run_program",
        json!({ "path": p, "program": "(restore-checkpoint \"cp\")" }),
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\n");
}

#[test]
fn removed_and_gated_tools_are_not_listed_on_stdio() {
    let mut s = Server::spawn();
    let list = s.request(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }));
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for gone in [
        "rehearse",
        "checkpoint",
        "restore_checkpoint",
        "open_workspace",
        "close_workspace",
        "git_exec_over",
    ] {
        assert!(!names.contains(&gone), "{gone} is still listed");
    }
    let err = s.call_err(2, "rehearse", json!({ "program": "(point)" }));
    assert!(err.contains("unknown tool"), "got: {err}");
    let err = s.call_err(3, "open_workspace", json!({}));
    assert!(err.contains("unknown tool"), "got: {err}");

    let mut s = Server::spawn_with_env(&[("MIME_EXEC", std::path::Path::new("1"))]);
    let list = s.request(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }));
    assert!(
        list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "git_exec_over"),
        "MIME_EXEC=1 lists git_exec_over"
    );
}
