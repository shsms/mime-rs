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
            .stderr(Stdio::null());
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
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
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
        "rehearse",
        "read_region",
        "replace_text",
        "occur",
        "conflicts",
        "checkpoint",
        "restore_checkpoint",
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

    // --- read_region pulls text on demand without mutating ---
    let region = s.call_ok(5, "read_region", json!({ "start": 1, "end": 6 }));
    assert_eq!(region, "hello");

    // --- checkpoint, mutate, restore_checkpoint, confirm the revert ---
    let cp = s.call_ok(6, "checkpoint", json!({ "label": "before" }));
    assert!(cp.contains("before"), "checkpoint said: {cp}");

    // Mutate: blow the buffer away.
    let mutated = s.call_ok(
        7,
        "run_program",
        json!({ "program": r#"(erase-buffer) (insert "DESTROYED")"# }),
    );
    let mutated: Value = serde_json::from_str(&mutated).unwrap();
    assert_eq!(mutated["len_after"], 9);

    // session_status shows our checkpoint label.
    let cps = s.call_ok(8, "session_status", json!({}));
    assert!(cps.contains("before"), "session_status said: {cps}");

    // Restore.
    let restored = s.call_ok(9, "restore_checkpoint", json!({ "label": "before" }));
    assert!(restored.contains("before"), "restore said: {restored}");

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
        "rehearse",
        json!({ "program": r#"(while (re-search-forward "world" nil t) (replace-match "mime")) (report "done" 1)"# }),
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

    // But the live buffer is untouched: a follow-up read still sees "hello world".
    let region = s.call_ok(4, "read_region", json!({ "start": 1, "end": 12 }));
    assert_eq!(region, "hello world");

    // And a real run_program afterwards persists normally, proving rehearse left
    // the session fully usable.
    let applied = s.call_ok(
        5,
        "run_program",
        json!({ "program": r#"(while (re-search-forward "world" nil t) (replace-match "mime"))"# }),
    );
    let applied: Value = serde_json::from_str(&applied).unwrap();
    assert_eq!(applied["rehearsed"], false);
    let confirm = s.call_ok(6, "read_region", json!({ "start": 1, "end": 11 }));
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

    // A tool-level failure (running against a session that was never opened)
    // is a *successful* JSON-RPC call with isError=true.
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

    // A defun defined in session "one" persists (warmth) and only affects "one".
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
    let text = s.call_ok(5, "read_region", json!({ "start": 1, "end": 19 }));
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
    let text = s.call_ok(4, "read_region", json!({ "start": 1, "end": 22 }));
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
    let text = s.call_ok(
        7,
        "read_region",
        json!({ "start": 1, "end": 31, "session": "q" }),
    );
    assert_eq!(text, "write!(w, \"\\u{2026} occur \\1\")");

    // No match is a proper error that names the pattern — and a true no-op:
    // the buffer and point are exactly as before.
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
    let out = s.call_ok(3, "read_region", json!({ "lines": [1, 4] }));
    assert_eq!(out, "alpha\nbeta\ngamma\nomega");

    // Anchor on a literal LINE: insert after the line matching "beta".
    s.call_ok(
        4,
        "insert_text",
        json!({ "text": "\nbeta-note", "anchor": { "pattern": "beta" } }),
    );
    let out = s.call_ok(5, "read_region", json!({ "lines": [1, 5] }));
    assert_eq!(out, "alpha\nbeta\nbeta-note\ngamma\nomega");
    // ...and before it.
    s.call_ok(
        6,
        "insert_text",
        json!({ "text": "pre-gamma\n", "anchor": { "pattern": "gamma", "where": "before" } }),
    );
    let out = s.call_ok(7, "read_region", json!({ "lines": [1, 6] }));
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
fn read_region_takes_line_ranges_and_view_rejects_them_loudly() {
    let mut s = Server::spawn();
    s.call_ok(1, "open_text", json!({ "text": "l1\nl2\nl3\nl4\nl5\n" }));

    // The line form: 1-based inclusive.
    let out = s.call_ok(2, "read_region", json!({ "lines": [2, 4] }));
    assert_eq!(out, "l2\nl3\nl4");

    // Wrong shapes fail loudly with the right form in the error.
    let err = s.call_err(3, "read_region", json!({ "lines": "2-4" }));
    assert!(err.contains("[start, end]"), "{err}");
    let err = s.call_err(4, "view", json!({ "lines": [2, 4] }));
    assert!(
        err.contains("COUNT") && err.contains("read_region"),
        "a range passed to view names the tool that takes ranges: {err}"
    );
    let err = s.call_err(
        5,
        "read_region",
        json!({ "lines": [2, 4], "start": 1, "end": 3 }),
    );
    assert!(err.contains("not both"), "{err}");
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
    let text = s.call_ok(3, "read_region", json!({ "start": 1, "end": 31 }));
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
    let text = s.call_ok(
        6,
        "read_region",
        json!({ "start": 1, "end": 10, "session": "q" }),
    );
    assert_eq!(text, "yx1 x2 x3");

    // expect_unique keeps its semantics per pattern: an ambiguous regex is
    // an error listing the match lines, and nothing is replaced.
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
    let text = s.call_ok(
        8,
        "read_region",
        json!({ "start": 1, "end": 10, "session": "q" }),
    );
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
    let text = s.call_ok(
        11,
        "read_region",
        json!({ "start": 1, "end": 9, "session": "q" }),
    );
    assert_eq!(text, "x1 x2 do");

    // An unknown mode is a loud error.
    let err = s.call_err(
        12,
        "replace_text",
        json!({ "pattern": "a", "replacement": "b", "mode": "fuzzy", "session": "q" }),
    );
    assert!(err.contains("mode"), "got: {err}");
}

/// Every literal-taking tool escapes user strings into generated tulisp on
/// the server (lisp_literal; occur adds regexp-quote / regex_dialect::quote
/// underneath); one missed path is a silent wrong edit or a false miss.
/// Round-trip a gauntlet of hostile strings through insert_text →
/// read_region, occur, replace_text, and the edits batch, requiring
/// byte-exact results everywhere.
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
        let txt = s.call_ok(
            id,
            "read_region",
            json!({ "session": sess, "start": 1, "end": n + 3 }),
        );
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
        let txt = s.call_ok(
            id,
            "read_region",
            json!({ "session": sess, "start": 1, "end": m + 3 }),
        );
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
    let txt = s.call_ok(
        id,
        "read_region",
        json!({ "session": "batch", "start": 1, "end": 11 }),
    );
    assert_eq!(txt, "x\t\n\"Q\\S\" y", "batch round trip");
}

/// grep compiles the exact-mode pattern with RE2 escaping (a separate path
/// from occur's in-engine regexp-quote) — hostile literals must match there
/// too, and the file's rendered line must survive clamping intact.
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
    let text = s.call_ok(4, "read_region", json!({ "start": 1, "end": 6 }));
    assert_eq!(text, "hello", "the pre-error edit was rolled back");

    // keep_partial:true opts out: the edit persists and the error says how
    // to revert it.
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
    let text = s.call_ok(6, "read_region", json!({ "start": 1, "end": 14 }));
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
        json!({ "path": p, "pattern": "alpha", "replacement": "beta" }),
    );
    let status = s.call_ok(3, "session_status", json!({}));
    let status: Value = serde_json::from_str(&status).unwrap();
    assert_eq!(
        status["sessions"].as_array().unwrap().len(),
        1,
        "one warm copy: {status}"
    );
    let txt = s.call_ok(
        4,
        "read_region",
        json!({ "session": "custom", "start": 1, "end": 5 }),
    );
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
    let txt = s.call_ok(3, "read_region", json!({ "path": p, "start": 1, "end": 6 }));
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

    // Now MODIFY the buffer, then drift the file again. A modified buffer is the
    // genuine conflict — it is NOT auto-reverted, so reads carry the warning.
    s.call_ok(
        6,
        "run_program",
        json!({ "path": p, "program": "(goto-char (point-max)) (insert \"mine\\n\")" }),
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
    let txt = s.call_ok(
        10,
        "read_region",
        json!({ "path": p, "start": 1, "end": 6 }),
    );
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
        json!({ "path": p, "pattern": "alpha", "replacement": "ALPHA" }),
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
        json!({ "path": p, "pattern": "beta", "replacement": "BETA" }),
    );
    assert!(
        ok.contains("changed on disk"),
        "the edit result must carry the stale note: {ok}"
    );
    let ok = s.call_ok(4, "insert_text", json!({ "path": p, "text": "tail\n" }));
    assert!(ok.contains("changed on disk"), "insert_text too: {ok}");
}

#[test]
fn rehearse_auto_reverts_a_clean_drifted_buffer_like_a_run_would() {
    // A clean drifted buffer auto-reverts BEFORE the rehearse snapshot: the
    // revert discards nothing, and without it the preview would run against
    // bytes the committing run — which does revert — won't use, so preview
    // and commit could legitimately disagree.
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
        "rehearse",
        json!({ "path": p, "program": r#"(search-forward "changed" nil t) (replace-match "previewed")"# }),
    );
    assert!(
        preview.contains("previewed"),
        "the preview sees the CURRENT file: {preview}"
    );

    // The rehearsal's rollback lands on the reverted (fresh) state: not
    // stale, text matching the disk.
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
    let txt = s.call_ok(4, "read_region", json!({ "path": p, "start": 1, "end": 8 }));
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
        json!({ "path": p, "program": "(goto-char (point-max)) (insert \"beta\\n\")" }),
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
        json!({ "path": p, "pattern": "alpha", "replacement": "ALPHA" }),
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
fn unsaved_flag_covers_insert_rehearse_and_restore_checkpoint() {
    let dir = temp_dir("unsaved2");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let p = file.to_string_lossy().into_owned();
    // Explicit session id so the checkpoint tools (keyed by session, not path)
    // address the same buffer.
    s.call_ok(1, "open_file", json!({ "session": "s", "path": p }));

    // insert_text without save → its text message carries the reminder.
    let msg = s.call_ok(2, "insert_text", json!({ "session": "s", "text": "X" }));
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
        "rehearse",
        json!({ "session": "s", "program": "(goto-char (point-max)) (insert \"Z\")" }),
    ))
    .unwrap();
    assert!(
        out.get("unsaved").is_none(),
        "rehearse rolls back → not unsaved: {out}"
    );

    // Checkpoint at the saved state, save a DIFFERENT content to disk, then
    // restore the checkpoint — the buffer now differs from disk → unsaved.
    s.call_ok(5, "checkpoint", json!({ "session": "s", "label": "cp0" }));
    s.call_ok(
        6,
        "run_program",
        json!({ "session": "s", "program": "(erase-buffer) (insert \"BETA\\n\")", "save": true }),
    );
    let restored = s.call_ok(
        7,
        "restore_checkpoint",
        json!({ "session": "s", "label": "cp0" }),
    );
    assert!(
        restored.contains("unsaved"),
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
    let region = s.call_ok(4, "read_region", json!({ "start": 1, "end": 10 }));
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

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn uniform_path_addressing_on_checkpoint_and_save_tools() {
    let dir = temp_dir("uniform-addr");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let p = file.to_string_lossy().into_owned();

    // checkpoint / list / restore address the file by path, like every
    // other tool — no need to know the canonical-path session id.
    let cp = s.call_ok(1, "checkpoint", json!({ "path": p, "label": "cp" }));
    assert!(cp.contains("cp"), "checkpoint said: {cp}");
    s.call_ok(
        2,
        "replace_text",
        json!({ "path": p, "pattern": "alpha", "replacement": "beta" }),
    );
    let cps = s.call_ok(3, "session_status", json!({}));
    assert!(cps.contains("cp"), "session_status said: {cps}");
    s.call_ok(4, "restore_checkpoint", json!({ "path": p, "label": "cp" }));
    let txt = s.call_ok(5, "read_region", json!({ "path": p, "start": 1, "end": 6 }));
    assert!(txt.starts_with("alpha"), "restored: {txt}");

    // save_buffer without `to` writes back to the visited file.
    s.call_ok(
        6,
        "replace_text",
        json!({ "path": p, "pattern": "alpha", "replacement": "gamma" }),
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

    // Two separate mutating calls, then a read (which must not consume
    // an undo step).
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
    let txt = s.call_ok(4, "read_region", json!({ "start": 1, "end": 3 }));
    assert_eq!(txt, "v2");

    // First undo: back to v1. Second: back to v0. Then the ring is dry.
    let u1 = s.call_ok(5, "undo_last", json!({}));
    assert!(u1.contains("rewound"), "undo said: {u1}");
    let txt = s.call_ok(6, "read_region", json!({ "start": 1, "end": 3 }));
    assert_eq!(txt, "v1");
    s.call_ok(7, "undo_last", json!({}));
    let txt = s.call_ok(8, "read_region", json!({ "start": 1, "end": 3 }));
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

    // Two occurrences: the unique replace refuses and lists the lines,
    // and nothing changes.
    let err = s.call_err(
        2,
        "replace_text",
        json!({ "pattern": "use a;", "replacement": "use z;", "expect_unique": true }),
    );
    assert!(err.contains("matches at lines 1, 3"), "got: {err}");
    let txt = s.call_ok(3, "read_region", json!({ "start": 1, "end": 8 }));
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
    let txt = s.call_ok(7, "read_region", json!({ "start": 8, "end": 15 }));
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
        json!({ "path": p, "pattern": "alpha", "replacement": "beta" }),
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
    let txt = s.call_ok(4, "read_region", json!({ "start": 1, "end": end }));
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

    // A miss in the SECOND file rolls the first back: nothing changes
    // anywhere (warm buffers included).
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
    let txt = s.call_ok(
        3,
        "read_region",
        json!({ "path": pa, "start": 1, "end": 9 }),
    );
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
/// (unquoted, unescaped), other types tulisp-printed — so a read-only
/// inspector like `(conflict-diff N)` is readable without wrapping it in
/// `(message …)`; a `nil` value is omitted, like `stale`/`unsaved`.
#[test]
fn run_program_surfaces_the_final_form_value() {
    let mut s = Server::spawn();
    s.request(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }));
    s.call_ok(2, "open_text", json!({ "text": "hello" }));

    // A non-nil string value comes back raw (no quotes, escapes undone),
    // riding alongside an empty read-only `diff`.
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

    // full_diff + view are run_program args; rehearse must accept them, not
    // reject them as unknown.
    let out: Value = serde_json::from_str(&s.call_ok(
        3,
        "rehearse",
        json!({
            "program": r#"(goto-char (point-min)) (while (re-search-forward "a" nil t) (replace-match "A"))"#,
            "full_diff": true,
            "view": 4,
        }),
    ))
    .unwrap();
    assert_eq!(out["rehearsed"], json!(true), "rehearse rolls back: {out}");
    assert!(
        out["diff"].as_str().is_some_and(|d| !d.is_empty()),
        "full_diff returns the preview: {out}"
    );
}
