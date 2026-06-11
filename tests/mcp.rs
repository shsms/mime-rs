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
        "search",
        "replace_text",
        "occur",
        "conflicts",
        "checkpoint",
        "restore_checkpoint",
        "undo_last",
        "list_checkpoints",
        "save_buffer",
        "session_status",
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

    // list_checkpoints shows our label.
    let cps = s.call_ok(8, "list_checkpoints", json!({}));
    assert!(cps.contains("before"), "list_checkpoints said: {cps}");

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

    // A program that edits and THEN dies: the failure says the partial edit
    // persists (a run does not roll back).
    let err = s.call_err(
        3,
        "run_program",
        json!({ "program": r#"(insert "partial ") (error "late boom")"# }),
    );
    let failure: Value = serde_json::from_str(&err).expect("failure content is JSON");
    assert_eq!(failure["dirty"], true);
    let text = s.call_ok(4, "read_region", json!({ "start": 1, "end": 14 }));
    assert_eq!(text, "partial hello", "the pre-error edit persisted");
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

    // Point did not move: an exact search still finds "alpha" ahead of point.
    let found = s.call_ok(3, "search", json!({ "pattern": "alpha" }));
    assert!(found.contains("point is now 6"), "got: {found}");

    // Regex mode and the limit tail both pass through to the builtin.
    let out = s.call_ok(4, "occur", json!({ "pattern": "g.mma", "mode": "regex" }));
    assert!(out.contains("1 match on 1 line"), "got: {out}");
    let out = s.call_ok(5, "occur", json!({ "pattern": "beta", "limit": 1 }));
    assert!(out.contains("… and 2 more matching lines"), "got: {out}");
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
fn rehearse_does_not_auto_revert_a_clean_drifted_buffer() {
    // rehearse is a dry-run that must persist NOTHING — so it must not silently
    // auto-revert (a buffer swap that would land before the rollback snapshot).
    let dir = temp_dir("rehearse-revert");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "original\n").unwrap();
    let mut s = Server::spawn_with_env(&[("MIME_ROOTS", dir.as_path())]);
    let p = file.to_string_lossy().into_owned();
    s.call_ok(1, "open_file", json!({ "path": p }));

    // Drift the file (different length) while the buffer is clean.
    std::fs::write(&file, "changed on disk and longer\n").unwrap();
    s.call_ok(2, "rehearse", json!({ "path": p, "program": "(point)" }));

    // session_status reads is_stale directly (no auto-revert): the buffer must
    // still be stale, proving the rehearse did not silently revert it.
    let status = s.call_ok(3, "session_status", json!({}));
    let status: Value = serde_json::from_str(&status).unwrap();
    assert!(
        status["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|sess| sess["stale"] == true),
        "rehearse must leave the buffer stale (no silent revert): {status}"
    );

    // A real read, by contrast, DOES auto-revert the clean drifted buffer.
    let txt = s.call_ok(4, "read_region", json!({ "path": p, "start": 1, "end": 8 }));
    assert_eq!(txt, "changed", "a read auto-reverts where rehearse did not");
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
    let cps = s.call_ok(3, "list_checkpoints", json!({ "path": p }));
    assert!(cps.contains("cp"), "list said: {cps}");
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
fn search_reports_direction_line_echo_and_case_folding() {
    let mut s = Server::spawn();
    s.call_ok(
        1,
        "open_text",
        json!({ "text": "Alpha one\nbeta two\nALPHA three\n" }),
    );

    // Forward + case-insensitive exact: finds "Alpha" and echoes the line.
    let hit = s.call_ok(
        2,
        "search",
        json!({ "pattern": "alpha", "case_insensitive": true }),
    );
    assert!(hit.contains("line 1: Alpha one"), "got: {hit}");

    // Backward from the end: the latest case-folded match is on line 3.
    s.call_ok(3, "run_program", json!({ "program": "(end-of-buffer)" }));
    let hit = s.call_ok(
        4,
        "search",
        json!({ "pattern": "alpha", "case_insensitive": true, "direction": "backward" }),
    );
    assert!(hit.contains("line 3: ALPHA three"), "got: {hit}");
    assert!(hit.contains("at the match start"), "got: {hit}");

    // occur with case folding sees both spellings.
    let oc = s.call_ok(
        5,
        "occur",
        json!({ "pattern": "alpha", "case_insensitive": true }),
    );
    assert!(
        oc.contains("Alpha one") && oc.contains("ALPHA three"),
        "got: {oc}"
    );
}
