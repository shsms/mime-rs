//! Integration test for the `mime-mcp` MCP server.
//!
//! Spawns the built `mime-mcp` binary as a subprocess with piped stdin/stdout
//! and drives it with real JSON-RPC 2.0 lines, asserting on the responses. This
//! exercises the full stdio protocol path — handshake, `tools/list`,
//! `tools/call` for a real edit program, and the checkpoint → mutate → restore
//! round-trip — exactly as an MCP client would.
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

/// A live `mime-mcp` subprocess with line-buffered stdin/stdout handles.
struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Server {
    fn spawn() -> Server {
        let mut child = Command::new(env!("CARGO_BIN_EXE_mime-mcp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn mime-mcp");
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
        "read_region",
        "search",
        "checkpoint",
        "restore_checkpoint",
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

    // session_status lists both.
    let status = s.call_ok(6, "session_status", json!({}));
    let status: Value = serde_json::from_str(&status).unwrap();
    let ids = status["sessions"].as_array().unwrap();
    assert!(ids.iter().any(|v| v == "one"));
    assert!(ids.iter().any(|v| v == "two"));

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
