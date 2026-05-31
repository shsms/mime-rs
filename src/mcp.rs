//! mime-mcp — an MCP (Model Context Protocol) server over stdio.
//!
//! This is the agent-facing surface for mime-rs. It speaks JSON-RPC 2.0 over
//! stdio (one JSON object per line in on stdin, one per line out on stdout;
//! `eprintln!` for logs so stdout stays a clean protocol channel) and exposes
//! the editing engine as MCP tools.
//!
//! Like the daemon, it embeds the engine directly — it does NOT talk to a running
//! daemon. It owns a `HashMap<String, Workspace>` of *warm* sessions: each
//! `Workspace` holds a long-lived `TulispContext` + `Session`, so a buffer,
//! checkpoints, kill-ring, and agent-defined `defun`s persist across
//! `run_program` calls. A `session` argument names one; it defaults to
//! `"default"` when omitted, so a single-session agent never has to think about
//! it.
//!
//! Threading: a `Workspace` embeds an `Rc`-based `TulispContext` and is `!Send`,
//! so this server is single-threaded — it reads, dispatches, and replies on one
//! thread, one request to completion before the next. MCP over stdio is a single
//! client, so this is the natural model (mirrors the daemon's single-writer loop).
//!
//! Most tools are implemented by running a tiny tulisp program through
//! `Workspace::run` (so there is one editing code path) and reading values back
//! out of the resulting `RunReport.reports`. The core `run_program` tool returns
//! the full `RunReport` JSON (diff + reports + point + len).
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::Path;

use crate::{Buffer, Quire, TextStore, Workspace};
use serde_json::{Value, json};

const DEFAULT_SESSION: &str = "default";
const PROTOCOL_VERSION: &str = "2024-11-05";

pub fn run() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    // The single source of truth: session id -> warm workspace. Not behind a
    // lock — the server is single-threaded (a `Workspace` is `!Send`).
    let mut sessions: HashMap<String, Workspace> = HashMap::new();

    eprintln!("mime-mcp: ready on stdio (protocol {PROTOCOL_VERSION})");

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("mime-mcp: read error: {e}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        // `handle_line` returns `None` for notifications (no `id`) — those get
        // no response per JSON-RPC.
        if let Some(response) = handle_line(&line, &mut sessions) {
            if writeln!(out, "{response}").is_err() {
                break;
            }
            let _ = out.flush();
        }
    }
}

/// Parse one JSON-RPC request line and dispatch it. Returns `Some(response)` for
/// requests (those with an `id`) and `None` for notifications.
fn handle_line(line: &str, sessions: &mut HashMap<String, Workspace>) -> Option<Value> {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            // Can't recover an id from unparseable input; report against null.
            return Some(rpc_error(Value::Null, -32700, &format!("parse error: {e}")));
        }
    };

    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    // No `id` => a notification: act on it but never reply.
    let is_notification = id.is_none();

    match method {
        "initialize" => reply(id, is_notification, initialize_result(&params)),
        "notifications/initialized" | "initialized" => {
            // Pure notification — nothing to do, no response.
            None
        }
        "ping" => reply(id, is_notification, json!({})),
        "tools/list" => reply(id, is_notification, tools_list_result()),
        "tools/call" => reply(id, is_notification, tools_call_result(&params, sessions)),
        other => {
            if is_notification {
                eprintln!("mime-mcp: ignoring unknown notification {other}");
                None
            } else {
                Some(rpc_error(
                    id.unwrap_or(Value::Null),
                    -32601,
                    "method not found",
                ))
            }
        }
    }
}

/// Wrap a successful result for a request, or drop it for a notification.
fn reply(id: Option<Value>, is_notification: bool, result: Value) -> Option<Value> {
    if is_notification {
        return None;
    }
    Some(json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(Value::Null),
        "result": result,
    }))
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

fn initialize_result(params: &Value) -> Value {
    // Echo the client's protocolVersion when it sent one; otherwise advertise
    // ours. Both are valid MCP; echoing avoids a needless mismatch.
    let version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(PROTOCOL_VERSION);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "mime-rs", "version": "0.1.0" },
    })
}

// ---- tool dispatch ---------------------------------------------------------

/// Dispatch `tools/call`. Always returns a tool *result* envelope
/// (`{content, isError}`) — even on failure — because at the JSON-RPC layer the
/// call itself succeeded; the tool-level error rides in `isError` + the text.
fn tools_call_result(params: &Value, sessions: &mut HashMap<String, Workspace>) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let outcome: Result<String, String> = match name {
        "open_file" => tool_open_file(&args, sessions),
        "open_text" => tool_open_text(&args, sessions),
        "run_program" => tool_run_program(&args, sessions, false),
        "rehearse" => tool_run_program(&args, sessions, true),
        "read_region" => tool_read_region(&args, sessions),
        "view" => tool_view(&args, sessions),
        "insert_text" => tool_insert_text(&args, sessions),
        "search" => tool_search(&args, sessions),
        "checkpoint" => tool_checkpoint(&args, sessions),
        "restore_checkpoint" => tool_restore_checkpoint(&args, sessions),
        "list_checkpoints" => tool_list_checkpoints(&args, sessions),
        "save_buffer" => tool_save_buffer(&args, sessions),
        "session_status" => tool_session_status(sessions),
        other => Err(format!("unknown tool: {other}")),
    };

    match outcome {
        Ok(text) => tool_text(text, false),
        Err(message) => tool_text(message, true),
    }
}

/// Build the `{content, isError}` envelope MCP expects for a tool result.
fn tool_text(text: String, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

fn session_arg(args: &Value) -> String {
    args.get("session")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_SESSION)
        .to_string()
}

fn str_arg(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing or non-string argument \"{key}\""))
}

fn int_arg(args: &Value, key: &str) -> Result<i64, String> {
    args.get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("missing or non-integer argument \"{key}\""))
}

/// An optional boolean argument, defaulting to `false` when absent or non-bool.
fn bool_arg(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Build a warm [`Workspace`], read-only when requested.
fn make_workspace(store: Box<dyn TextStore>, read_only: bool) -> Workspace {
    // Sandboxed (agent-facing) tier: the MCP server registers the core editing
    // vocabulary only, never the orchestration group — read-only vs writable is
    // the only per-session distinction.
    if read_only {
        Workspace::new_read_only(store)
    } else {
        Workspace::new(store)
    }
}

/// Run a program against an existing session and return the resulting
/// `RunReport`; an engine error becomes `Err(message)`. The internal tools that
/// build on this (read_region, view, search, …) only read, so they always
/// `run`; see [`run_or_rehearse`] for the user-facing rehearse path.
fn run_in_session(
    sessions: &mut HashMap<String, Workspace>,
    session: &str,
    program: &str,
) -> Result<crate::RunReport, String> {
    run_or_rehearse(sessions, session, program, false)
}

/// Like [`run_in_session`], but `rehearse` selects a dry-run that rolls the
/// session back afterwards (the `rehearse` tool) instead of a persisting `run`.
fn run_or_rehearse(
    sessions: &mut HashMap<String, Workspace>,
    session: &str,
    program: &str,
    rehearse: bool,
) -> Result<crate::RunReport, String> {
    let ws = sessions
        .get_mut(session)
        .ok_or_else(|| format!("no such session: {session} (open_file/open_text first)"))?;
    if rehearse {
        ws.rehearse(program)
    } else {
        ws.run(program)
    }
}

/// Pull the value a `(report KEY ...)` recorded, by key.
fn report_value(report: &crate::RunReport, key: &str) -> Option<String> {
    report
        .reports
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
}

// ---- tools -----------------------------------------------------------------

// open_file/save_buffer are the only FS-touching tools; both run their path
// through `safety::check_path`, which confines them to the configured workspace
// roots ($MIME_ROOTS, default cwd) and rejects `..`/symlink/absolute escapes.
// No shell, process spawn, or network is ever exposed — see plan.org §"Safety,
// sandboxing, permissioning".

/// `open_file {path, session?}` — open a file via `Quire` into `session`,
/// replacing any existing session of that name. The path must resolve inside an
/// allowed root.
fn tool_open_file(
    args: &Value,
    sessions: &mut HashMap<String, Workspace>,
) -> Result<String, String> {
    let session = session_arg(args);
    let path = str_arg(args, "path")?;
    let read_only = bool_arg(args, "read_only");
    let checked = crate::safety::check_path(Path::new(&path))?;
    let quire = Quire::open(&checked).map_err(|e| format!("cannot open file {path}: {e}"))?;
    let name = quire.name().to_string();
    let len = quire.char_len();
    sessions.insert(session.clone(), make_workspace(Box::new(quire), read_only));
    Ok(format!(
        "opened file {path} as buffer \"{name}\" ({len} chars{mode}) in session \"{session}\"",
        mode = if read_only { ", read-only" } else { "" }
    ))
}

/// `open_text {text, session?, name?}` — open an in-memory `Buffer`.
fn tool_open_text(
    args: &Value,
    sessions: &mut HashMap<String, Workspace>,
) -> Result<String, String> {
    let session = session_arg(args);
    let text = str_arg(args, "text")?;
    let read_only = bool_arg(args, "read_only");
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(session.as_str())
        .to_string();
    let buffer = Buffer::from_string(name.clone(), text.clone());
    let len = text.chars().count();
    sessions.insert(session.clone(), make_workspace(Box::new(buffer), read_only));
    Ok(format!(
        "opened in-memory buffer \"{name}\" ({len} chars{mode}) in session \"{session}\"",
        mode = if read_only { ", read-only" } else { "" }
    ))
}

/// `run_program {program, session?}` — the core tool. Evaluate a tulisp edit
/// program against the warm session and return the full `RunReport` JSON.
///
/// With `rehearse = true` (the `rehearse` tool) the program is dry-run: the same
/// `RunReport` comes back (diff/reports/len of the hypothetical edit, with
/// `rehearsed: true`), but the buffer — and the kill-ring/checkpoints — are
/// rolled back, so nothing persists. The two share one body since they differ
/// only in whether the effects stick.
fn tool_run_program(
    args: &Value,
    sessions: &mut HashMap<String, Workspace>,
    rehearse: bool,
) -> Result<String, String> {
    let session = session_arg(args);
    let program = str_arg(args, "program")?;
    // TODO: resource limits (needs tulisp eval interruption) — a per-program
    // wall-clock/CPU bound can't be enforced until tulisp eval is cancellable.
    let report = run_or_rehearse(sessions, &session, &program, rehearse)?;
    // A rehearsal persists nothing, so it audits as a non-mutating event.
    crate::safety::audit(
        "mime-mcp",
        &session,
        &program,
        report.dirty && !rehearse,
        report.len_before,
        report.len_after,
    );
    Ok(pretty(&report.to_json()))
}

/// `read_region {session?, start, end}` — the substring `[start, end)`, fetched
/// on demand via `(buffer-substring START END)` so the agent never has to dump
/// the whole buffer. Reading does not change the buffer text.
fn tool_read_region(
    args: &Value,
    sessions: &mut HashMap<String, Workspace>,
) -> Result<String, String> {
    let session = session_arg(args);
    let start = int_arg(args, "start")?;
    let end = int_arg(args, "end")?;
    // `message` takes the substring *as a string* and stores it verbatim in the
    // log, so we get the raw text back; going through `report` would re-quote it
    // with tulisp's string printer (\"hello\" rather than hello).
    let program = format!("(message (buffer-substring {start} {end}))");
    let report = run_in_session(sessions, &session, &program)?;
    report
        .log
        .into_iter()
        .next()
        .ok_or_else(|| "read_region: no text returned".to_string())
}

/// `view {session?, lines?, pos?}` — a rendered viewport: `lines` rows of context
/// on each side of the cursor (or of `pos`), with a gutter, the current line
/// marked, and a header (buffer name, line/col, point/size). The agent's "look at
/// the screen". Backed by the `window` builtin; like `read_region`, it only reads.
fn tool_view(args: &Value, sessions: &mut HashMap<String, Workspace>) -> Result<String, String> {
    let session = session_arg(args);
    // Both args are optional and map straight onto `(window LINES POS)`; we build
    // the call positionally, dropping trailing args so `window`'s own defaults
    // (4 lines, current point) apply when they're omitted.
    let lines = args.get("lines").and_then(Value::as_i64);
    let pos = args.get("pos").and_then(Value::as_i64);
    let call = match (lines, pos) {
        (Some(n), Some(p)) => format!("(window {n} {p})"),
        (Some(n), None) => format!("(window {n})"),
        (None, Some(p)) => format!("(window 4 {p})"),
        (None, None) => "(window)".to_string(),
    };
    // `message` stashes the rendered viewport verbatim in the log (no re-quoting),
    // same trick as `read_region`.
    let report = run_in_session(sessions, &session, &format!("(message {call})"))?;
    report
        .log
        .into_iter()
        .next()
        .ok_or_else(|| "view: no text returned".to_string())
}

/// `insert_text {session?, text, pos?}` — insert literal `text` at point (or at
/// `pos`). The text arrives as a raw JSON string and is escaped for tulisp *on the
/// server*, so the agent never hand-escapes a program — the fix for the "big
/// literal blocks" friction (no JSON-over-Lisp double escaping). Newlines and tabs
/// become `\n`/`\t` so the generated `(insert "…")` stays a single, unambiguous
/// line. Edits the warm buffer; call `save_buffer` to persist.
fn tool_insert_text(
    args: &Value,
    sessions: &mut HashMap<String, Workspace>,
) -> Result<String, String> {
    let session = session_arg(args);
    let text = str_arg(args, "text")?;
    // lisp_escape handles \ and "; we additionally fold real newlines/tabs into
    // their escape sequences (valid in a tulisp string literal) to keep the
    // program on one line.
    let escaped = lisp_escape(&text).replace('\n', "\\n").replace('\t', "\\t");
    let program = match args.get("pos").and_then(Value::as_i64) {
        Some(pos) => {
            format!("(progn (goto-char {pos}) (insert \"{escaped}\") (report \"point\" (point)))")
        }
        None => format!("(progn (insert \"{escaped}\") (report \"point\" (point)))"),
    };
    let report = run_in_session(sessions, &session, &program)?;
    let chars = text.chars().count();
    let point = report_value(&report, "point").unwrap_or_default();
    Ok(format!("inserted {chars} chars; point is now {point}"))
}

/// `search {session?, pattern, mode?}` — search forward from point and report
/// the 1-based position just after the match (Emacs `*-search-forward`
/// semantics), or report that nothing matched. `mode` ∈ exact|regex
/// (default exact). Point moves to the match, as in Emacs.
fn tool_search(args: &Value, sessions: &mut HashMap<String, Workspace>) -> Result<String, String> {
    let session = session_arg(args);
    let pattern = str_arg(args, "pattern")?;
    let mode = args.get("mode").and_then(Value::as_str).unwrap_or("exact");
    // `search-forward` is literal, `re-search-forward` is regex. Each takes
    // `(NEEDLE BOUND NOERROR)`; noerror=t makes a miss return nil instead of
    // erroring. (Whitespace/case-insensitive "fuzzy" matching isn't a built-in:
    // pass a regex — that's all mime's find_fuzzy compiles to.)
    let (lisp_fn, needle) = match mode {
        "exact" => ("search-forward", lisp_escape(&pattern)),
        "regex" => ("re-search-forward", lisp_escape(&pattern)),
        other => return Err(format!("unknown search mode: {other} (exact|regex)")),
    };
    let program = format!(
        "(let ((p ({lisp_fn} \"{needle}\" nil t)))\
           (if p (progn (report \"found\" 1) (report \"pos\" p))\
                 (report \"found\" 0)))"
    );
    let report = run_in_session(sessions, &session, &program)?;
    if report_value(&report, "found").as_deref() == Some("1") {
        let pos = report_value(&report, "pos").unwrap_or_default();
        Ok(format!(
            "match ({mode}): point is now {pos} (just after the match)"
        ))
    } else {
        Ok(format!("no {mode} match for pattern"))
    }
}

/// `checkpoint {session?, label?}` — capture a restore point. Returns the label
/// the engine assigned (auto-generated when omitted).
fn tool_checkpoint(
    args: &Value,
    sessions: &mut HashMap<String, Workspace>,
) -> Result<String, String> {
    let session = session_arg(args);
    let program = match args.get("label").and_then(Value::as_str) {
        Some(label) => format!("(report \"label\" (checkpoint \"{}\"))", lisp_escape(label)),
        None => "(report \"label\" (checkpoint))".to_string(),
    };
    let report = run_in_session(sessions, &session, &program)?;
    // The engine reports the label via tulisp's printer, which quotes strings;
    // unquote it for a clean message.
    let label = report_value(&report, "label")
        .map(|s| s.trim_matches('"').to_string())
        .unwrap_or_default();
    Ok(format!("checkpoint \"{label}\" captured"))
}

/// `restore_checkpoint {session?, label}` — rewind the buffer to a checkpoint.
fn tool_restore_checkpoint(
    args: &Value,
    sessions: &mut HashMap<String, Workspace>,
) -> Result<String, String> {
    let session = session_arg(args);
    let label = str_arg(args, "label")?;
    let program = format!("(restore-checkpoint \"{}\")", lisp_escape(&label));
    run_in_session(sessions, &session, &program)?;
    Ok(format!("restored to checkpoint \"{label}\""))
}

/// `list_checkpoints {session?}` — the labels currently captured.
fn tool_list_checkpoints(
    args: &Value,
    sessions: &mut HashMap<String, Workspace>,
) -> Result<String, String> {
    let session = session_arg(args);
    let program = "(report \"checkpoints\" (list-checkpoints))".to_string();
    let report = run_in_session(sessions, &session, &program)?;
    let labels = report_value(&report, "checkpoints").unwrap_or_else(|| "nil".to_string());
    Ok(format!("checkpoints: {labels}"))
}

/// `save_buffer {session?, path}` — write the session buffer's text to disk.
fn tool_save_buffer(
    args: &Value,
    sessions: &mut HashMap<String, Workspace>,
) -> Result<String, String> {
    let session = session_arg(args);
    let path = str_arg(args, "path")?;
    let checked = crate::safety::check_path(Path::new(&path))?;
    // save_to writes atomically (temp + rename) and re-bases the buffer onto the
    // new file, so an in-place save reclaims the pre-save mmap backing.
    let bytes = sessions
        .get_mut(&session)
        .ok_or_else(|| format!("no such session: {session}"))?
        .save_to(&checked)
        .map_err(|e| format!("cannot write {path}: {e}"))?;
    Ok(format!("wrote {bytes} bytes to {path}"))
}

/// `session_status {}` — the live session ids plus the sandbox the engine
/// enforces: the allowed filesystem roots (as display strings) and whether the
/// audit journal is on. Advertising the roots lets the agent target a writable
/// path up front instead of discovering the bounds via a rejected save.
fn tool_session_status(sessions: &HashMap<String, Workspace>) -> Result<String, String> {
    let mut ids: Vec<&String> = sessions.keys().collect();
    ids.sort();
    let roots: Vec<String> = crate::safety::roots()
        .iter()
        .map(|r| r.display().to_string())
        .collect();
    Ok(json!({
        "sessions": ids,
        "roots": roots,
        "audit": crate::safety::audit_enabled(),
    })
    .to_string())
}

// ---- helpers ---------------------------------------------------------------

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

/// Escape a string for embedding inside a tulisp double-quoted literal. Only
/// backslash and double-quote are special inside an Elisp string; everything
/// else (including the regex metacharacters search/regex modes rely on) passes
/// through verbatim.
fn lisp_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ---- the tool catalogue ----------------------------------------------------

/// `tools/list` result — every tool with a JSON Schema `inputSchema`.
fn tools_list_result() -> Value {
    json!({ "tools": tool_schemas() })
}

fn tool_schemas() -> Vec<Value> {
    // A reusable optional `session` property.
    let session = json!({
        "type": "string",
        "description": "Warm session id; defaults to \"default\" when omitted."
    });
    vec![
        json!({
            "name": "open_file",
            "description": "Open a file from disk into a warm session (replacing any existing session of that name). The buffer stays resident so later tools need no file re-reads. The path must resolve inside an allowed root (MIME_ROOTS, default cwd).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Filesystem path to open." },
                    "read_only": { "type": "boolean", "description": "Attach the buffer unwritable; mutating programs are rejected. Default false." },
                    "session": session,
                },
                "required": ["path"],
            },
        }),
        json!({
            "name": "open_text",
            "description": "Open an in-memory text buffer into a warm session (replacing any existing session of that name).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "Initial buffer contents." },
                    "name": { "type": "string", "description": "Optional buffer name." },
                    "read_only": { "type": "boolean", "description": "Attach the buffer unwritable; mutating programs are rejected. Default false." },
                    "session": session,
                },
                "required": ["text"],
            },
        }),
        json!({
            "name": "run_program",
            "description": "Evaluate an Emacs-Lisp (tulisp) edit program against the session buffer and return a structured RunReport (unified diff, point, length before/after, and any (report ...)/(message ...) output). This is the core, general-purpose editing tool; the buffer and any defined functions persist for the next call.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "program": { "type": "string", "description": "Emacs-Lisp program, e.g. (while (re-search-forward \"foo\" nil t) (replace-match \"bar\"))." },
                    "session": session,
                },
                "required": ["program"],
            },
        }),
        json!({
            "name": "rehearse",
            "description": "Dry-run an Emacs-Lisp (tulisp) edit program and return the same RunReport run_program would (unified diff, length before/after, reports), showing what WOULD happen — then roll the session back so nothing persists: the buffer, point/mark/narrowing, kill-ring, and checkpoints are all left exactly as before (the report carries rehearsed=true). The 'try before you commit' preview; follow up with run_program to actually apply it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "program": { "type": "string", "description": "Emacs-Lisp program to rehearse (run then roll back)." },
                    "session": session,
                },
                "required": ["program"],
            },
        }),
        json!({
            "name": "read_region",
            "description": "Return the buffer text between two 1-based char positions [start, end). Use this to pull context on demand instead of dumping the whole buffer.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "start": { "type": "integer", "description": "1-based start position (inclusive)." },
                    "end": { "type": "integer", "description": "1-based end position (exclusive)." },
                    "session": session,
                },
                "required": ["start", "end"],
            },
        }),
        json!({
            "name": "view",
            "description": "Render a viewport around the cursor (or a given position): a few lines of context on each side, with a gutter, the current line marked, and a header. Your 'look at the screen'. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "lines": { "type": "integer", "description": "Context lines on each side of the cursor line (default 4)." },
                    "pos": { "type": "integer", "description": "1-based position to center on (default: current point)." },
                    "session": session,
                },
                "required": [],
            },
        }),
        json!({
            "name": "insert_text",
            "description": "Insert literal text at point (or at `pos`). Pass the text as a plain string — no Lisp escaping needed, the server handles it. Prefer this over run_program with (insert …) for multi-line or quote-heavy content. Edits the warm buffer; call save_buffer to persist.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "The literal text to insert." },
                    "pos": { "type": "integer", "description": "1-based position to insert at (default: current point)." },
                    "session": session,
                },
                "required": ["text"],
            },
        }),
        json!({
            "name": "search",
            "description": "Search forward from point for a pattern and report the resulting position. Moves point to the match (Emacs semantics).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "What to search for." },
                    "mode": {
                        "type": "string",
                        "enum": ["exact", "regex"],
                        "description": "exact (literal) or regex (RE2). Defaults to exact. (For whitespace/case-insensitive matching, pass a regex.)",
                    },
                    "session": session,
                },
                "required": ["pattern"],
            },
        }),
        json!({
            "name": "checkpoint",
            "description": "Capture a restore point of the current buffer. Cheap (structural sharing for files).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "label": { "type": "string", "description": "Optional label; auto-generated (auto-N) when omitted." },
                    "session": session,
                },
                "required": [],
            },
        }),
        json!({
            "name": "restore_checkpoint",
            "description": "Rewind the buffer to a previously captured checkpoint by label.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "label": { "type": "string", "description": "Label of the checkpoint to restore." },
                    "session": session,
                },
                "required": ["label"],
            },
        }),
        json!({
            "name": "list_checkpoints",
            "description": "List the labels of checkpoints captured in this session.",
            "inputSchema": {
                "type": "object",
                "properties": { "session": session },
                "required": [],
            },
        }),
        json!({
            "name": "save_buffer",
            "description": "Write the session buffer's current text to a file on disk.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Destination path." },
                    "session": session,
                },
                "required": ["path"],
            },
        }),
        json!({
            "name": "session_status",
            "description": "Report engine status: the ids of the currently live sessions, the allowed filesystem roots that open_file/save_buffer are confined to (MIME_ROOTS, default cwd), and whether the audit journal is on. Check the roots before opening or saving to learn the writable sandbox up front.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": [],
            },
        }),
    ]
}
