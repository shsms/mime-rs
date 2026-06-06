//! mimed — the long-lived editing daemon.
//!
//! It owns a map of `session-id -> Workspace` (each a *warm* workspace: buffers,
//! checkpoints, kill-ring, and agent-defined tulisp `defun`s that persist across
//! programs) and serves a JSON-lines control API over a unix socket. One JSON
//! request per line in, one JSON response per line out.
//!
//! Concurrency / threading model: a [`Workspace`] embeds a `TulispContext`,
//! which is `Rc`-based and therefore `!Send` — it cannot cross threads. So mimed
//! runs a single-threaded blocking accept loop: connections are served one at a
//! time, each request to completion before the next. This *is* the design intent
//! — a session is a single writer and programs are serialized — and it sidesteps
//! `Send` entirely. (A future multi-session-parallel daemon would shard
//! workspaces onto per-session threads, each owning its own `Rc` graph; the
//! map-of-workspaces shape here is the seam for that.) The session map is still
//! held behind a `Mutex` so the model is explicit and the lock is the obvious
//! place to add cross-session coordination later.
//!
//! Socket path: `$MIME_SOCKET` or `/tmp/mimed.sock`. A stale socket file at that
//! path is removed on startup.
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Mutex;

use crate::{Buffer, Quire, TextStore, Workspace};
use serde_json::{Value, json};

const DEFAULT_SOCKET: &str = "/tmp/mimed.sock";

fn socket_path() -> String {
    std::env::var("MIME_SOCKET").unwrap_or_else(|_| DEFAULT_SOCKET.to_string())
}

pub fn run() {
    let path = socket_path();
    // Remove a stale socket from a previous run; bind would otherwise fail with
    // "Address already in use".
    if Path::new(&path).exists() {
        let _ = std::fs::remove_file(&path);
    }
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("mimed: cannot bind {path}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("mimed: listening on {path}");

    // The single source of truth. `Mutex` guards it so the single-writer model is
    // explicit; with the single-threaded accept loop it is never contended.
    let sessions: Mutex<HashMap<String, Workspace>> = Mutex::new(HashMap::new());

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => serve(stream, &sessions),
            Err(e) => eprintln!("mimed: accept error: {e}"),
        }
    }
}

/// Serve one connection: read JSON-lines requests, write one JSON response per
/// line, until the peer closes.
fn serve(stream: UnixStream, sessions: &Mutex<HashMap<String, Workspace>>) {
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("mimed: clone stream: {e}");
            return;
        }
    };
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("mimed: read error: {e}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let response = handle_line(&line, sessions);
        // One response per line. If the client has gone away, stop.
        if writeln!(writer, "{response}").is_err() {
            break;
        }
        let _ = writer.flush();
    }
}

/// Parse one request line and dispatch it, always producing a JSON value.
fn handle_line(line: &str, sessions: &Mutex<HashMap<String, Workspace>>) -> Value {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return err(&format!("invalid JSON: {e}")),
    };
    let op = match req.get("op").and_then(Value::as_str) {
        Some(op) => op,
        None => return err("missing \"op\""),
    };
    match op {
        "open" => op_open(&req, sessions),
        "run" => op_run(&req, sessions, false),
        "rehearse" => op_run(&req, sessions, true),
        "status" => op_status(sessions),
        "save" => op_save(&req, sessions),
        "close" => op_close(&req, sessions),
        "ping" => json!({ "ok": true, "pong": true }),
        other => err(&format!("unknown op: {other}")),
    }
}

/// `{"op":"open","session":"S","file":"PATH"}` or `{"op":"open","session":"S","text":"..."}`
/// — create or replace session `S` with a fresh warm workspace. An optional
/// `"read_only": true` attaches the buffer unwritable (mutating programs are
/// rejected).
fn op_open(req: &Value, sessions: &Mutex<HashMap<String, Workspace>>) -> Value {
    let session = match str_field(req, "session") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let store: Box<dyn TextStore> = match (req.get("file"), req.get("text")) {
        (Some(file), _) => {
            let Some(path) = file.as_str() else {
                return err("\"file\" must be a string");
            };
            let path = match crate::safety::check_path(Path::new(path)) {
                Ok(p) => p,
                Err(e) => return err(&e),
            };
            match Quire::open(&path) {
                Ok(q) => Box::new(q),
                Err(e) => return err(&format!("cannot open file {}: {e}", path.display())),
            }
        }
        (None, Some(text)) => {
            let Some(text) = text.as_str() else {
                return err("\"text\" must be a string");
            };
            Box::new(Buffer::from_string(session.as_str(), text))
        }
        (None, None) => return err("open requires \"file\" or \"text\""),
    };
    let name = store.name().to_string();
    let read_only = req
        .get("read_only")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // Sandboxed (agent-facing) tier: core editing vocabulary only — the daemon
    // never registers the orchestration group. (A local daemon could opt up with
    // a future --trusted flag.)
    let workspace = if read_only {
        Workspace::new_read_only(store)
    } else {
        Workspace::new(store)
    };
    sessions.lock().unwrap().insert(session.clone(), workspace);
    json!({ "ok": true, "session": session, "buffer": name, "read_only": read_only })
}

/// `{"op":"run","session":"S","program":"..."}` — eval against the warm
/// workspace and return the per-program `RunReport`. With `rehearse = true`
/// (the `rehearse` op) the program is dry-run and rolled back, so the report
/// shows what *would* happen but nothing persists.
fn op_run(req: &Value, sessions: &Mutex<HashMap<String, Workspace>>, rehearse: bool) -> Value {
    let session = match str_field(req, "session") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let program = match str_field(req, "program") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let mut map = sessions.lock().unwrap();
    let Some(ws) = map.get_mut(&session) else {
        return err(&format!("no such session: {session}"));
    };
    // TODO: resource limits (needs tulisp eval interruption) — a per-program
    // wall-clock/CPU bound can't be enforced until tulisp eval is cancellable.
    let result = if rehearse {
        ws.rehearse(&program)
    } else {
        ws.run(&program)
    };
    match result {
        Ok(report) => {
            // A rehearsal persists nothing, so it is reported as a non-mutating
            // event (dirty=false) regardless of the hypothetical edit.
            crate::safety::audit(
                "mimed",
                &session,
                &program,
                report.dirty && !rehearse,
                report.len_before,
                report.len_after,
            );
            report.to_json()
        }
        Err(e) => {
            // Failure shape: the reports/log the program accumulated before it
            // died ride along, so diagnostics survive the error.
            let (reports, log, dirty) = ws.failure_context();
            crate::result::failure_json(&e, &reports, &log, dirty)
        }
    }
}

/// `{"op":"status"}` — the live session ids plus the sandbox the engine
/// enforces: the allowed filesystem roots (display strings) and whether auditing
/// is on, so a client learns the writable bounds without a rejected save.
fn op_status(sessions: &Mutex<HashMap<String, Workspace>>) -> Value {
    let map = sessions.lock().unwrap();
    let mut ids: Vec<&String> = map.keys().collect();
    ids.sort();
    // Per-session visibility, the same shape the MCP session_status reports:
    // buffer, visited file, narrowing, staleness.
    let entries: Vec<Value> = ids
        .into_iter()
        .map(|id| {
            let ws = &map[id];
            json!({
                "id": id,
                "buffer": ws.buffer_name(),
                "file": ws.visited_path().map(|p| p.display().to_string()),
                "narrowed": ws.is_narrowed(),
                "stale": ws.is_stale(),
            })
        })
        .collect();
    let roots: Vec<String> = crate::safety::roots()
        .iter()
        .map(|r| r.display().to_string())
        .collect();
    json!({
        "ok": true,
        "sessions": entries,
        "roots": roots,
        "audit": crate::safety::audit_enabled(),
    })
}

/// `{"op":"save","session":"S","path":"PATH"}` — write the session buffer's
/// current text to PATH.
fn op_save(req: &Value, sessions: &Mutex<HashMap<String, Workspace>>) -> Value {
    let session = match str_field(req, "session") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let path = match str_field(req, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let checked = match crate::safety::check_path(Path::new(&path)) {
        Ok(p) => p,
        Err(e) => return err(&e),
    };
    let mut map = sessions.lock().unwrap();
    let Some(ws) = map.get_mut(&session) else {
        return err(&format!("no such session: {session}"));
    };
    // save_to writes atomically and re-bases the buffer onto the new file.
    match ws.save_to(&checked) {
        Ok(bytes) => json!({ "ok": true, "session": session, "path": path, "bytes": bytes }),
        Err(e) => err(&format!("cannot write {path}: {e}")),
    }
}

/// `{"op":"close","session":"S"}` — drop a session and free its workspace.
fn op_close(req: &Value, sessions: &Mutex<HashMap<String, Workspace>>) -> Value {
    let session = match str_field(req, "session") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let existed = sessions.lock().unwrap().remove(&session).is_some();
    json!({ "ok": true, "session": session, "closed": existed })
}

/// Pull a required string field, or an error response naming it.
fn str_field(req: &Value, field: &str) -> Result<String, Value> {
    match req.get(field).and_then(Value::as_str) {
        Some(s) => Ok(s.to_string()),
        None => Err(err(&format!("missing or non-string \"{field}\""))),
    }
}

fn err(msg: &str) -> Value {
    json!({ "ok": false, "error": msg })
}
