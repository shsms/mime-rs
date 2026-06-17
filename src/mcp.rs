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
use std::sync::LazyLock;

use crate::{Buffer, Quire, TextStore, Workspace};
use serde_json::{Value, json};

const DEFAULT_SESSION: &str = "default";
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Cap on warm sessions. Each file-backed session pins an open fd and a warm
/// buffer; agents rarely work more than a handful of files, so past the cap
/// the least-recently-used CLEAN session is evicted to make room. Sessions
/// with un-persisted content (file-backed unsaved edits, or any modified
/// scratch buffer) are never evicted — boundedness must not cost edits.
const SESSION_CAP: usize = 16;

/// The next recency stamp for [`Workspace::touch`].
fn next_stamp() -> u64 {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Make room before installing a new warm session: evict clean LRU sessions
/// until under [`SESSION_CAP`]. Best-effort — if everything holds unsaved
/// work, the map grows past the cap rather than dropping edits.
fn evict_for_room(sessions: &mut HashMap<String, Workspace>) {
    while sessions.len() >= SESSION_CAP {
        let victim = sessions
            .iter()
            .filter(|(_, ws)| !ws.is_modified())
            .min_by_key(|(_, ws)| ws.last_used())
            .map(|(k, _)| k.clone());
        match victim {
            Some(k) => {
                eprintln!("mime-mcp: evicting idle clean session {k} (cap {SESSION_CAP})");
                sessions.remove(&k);
            }
            None => break,
        }
    }
}

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

    // Reject arguments the tool doesn't declare, instead of silently dropping
    // them (a `view {offset: N}` typo for `pos` would otherwise render the
    // wrong place with no signal). Driven by the same schemas tools/list
    // advertises, so it can never drift from what's accepted.
    if let Err(message) = validate_args(name, &args) {
        return tool_text(message, true);
    }

    let outcome: Result<String, String> = match name {
        "open_file" => tool_open_file(&args, sessions),
        "open_text" => tool_open_text(&args, sessions),
        "run_program" => tool_run_program(&args, sessions, false),
        "rehearse" => tool_run_program(&args, sessions, true),
        "read_region" => tool_read_region(&args, sessions),
        "view" => tool_view(&args, sessions),
        "insert_text" => tool_insert_text(&args, sessions),
        "replace_text" => tool_replace_text(&args, sessions),
        "search" => tool_search(&args, sessions),
        "occur" => tool_occur(&args, sessions),
        "outline" => tool_outline(&args, sessions),
        "conflicts" => tool_conflicts(&args, sessions),
        "checkpoint" => tool_checkpoint(&args, sessions),
        "restore_checkpoint" => tool_restore_checkpoint(&args, sessions),
        "undo_last" => tool_undo_last(&args, sessions),
        "close_session" => tool_close_session(&args, sessions),
        "list_checkpoints" => tool_list_checkpoints(&args, sessions),
        "save_buffer" => tool_save_buffer(&args, sessions),
        "session_status" => tool_session_status(sessions),
        "help" => tool_help(&args),
        other => Err(format!("unknown tool: {other}")),
    };

    match outcome {
        Ok(text) => tool_text(text, false),
        Err(message) => tool_text(message, true),
    }
}

/// Reject any argument key the named tool does not declare in its
/// `inputSchema.properties`. An unknown key is far more likely a typo or a
/// borrowed spelling (e.g. `offset` for `view`'s `pos`) than something the tool
/// should silently ignore — so name the offender and list what is accepted.
/// Tools absent from the schema list, or declaring no properties, are not
/// constrained (nothing to validate against).
fn validate_args(name: &str, args: &Value) -> Result<(), String> {
    let Some(obj) = args.as_object() else {
        return Ok(());
    };
    let schemas = tool_schemas();
    let Some(schema) = schemas.iter().find(|t| t["name"] == name) else {
        return Ok(());
    };
    let Some(props) = schema["inputSchema"]["properties"].as_object() else {
        return Ok(());
    };
    if let Some(key) = obj.keys().find(|k| !props.contains_key(*k)) {
        let mut valid: Vec<&str> = props.keys().map(String::as_str).collect();
        valid.sort_unstable();
        return Err(format!(
            "unknown argument \"{key}\" for tool \"{name}\"; valid arguments: {}",
            valid.join(", ")
        ));
    }
    Ok(())
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

/// Resolve which session a tool call addresses. With `path`, the file is
/// auto-opened (`check_path`-confined) into a session KEYED BY ITS CANONICAL
/// PATH unless already warm — the one-call alternative to a separate
/// `open_file`, and immune to basename collisions since the path is the key.
/// Passing both `path` and `session` is ambiguous and rejected.
fn resolve_session(
    args: &Value,
    sessions: &mut HashMap<String, Workspace>,
) -> Result<String, String> {
    let path = args.get("path").and_then(Value::as_str);
    let session = args.get("session").and_then(Value::as_str);
    match (path, session) {
        (Some(_), Some(_)) => Err("pass either \"path\" or \"session\", not both".to_string()),
        (None, s) => Ok(s.unwrap_or(DEFAULT_SESSION).to_string()),
        (Some(p), None) => {
            let checked = crate::safety::check_path(Path::new(p))?;
            let id = checked.to_string_lossy().into_owned();
            if sessions.contains_key(&id) {
                return Ok(id);
            }
            // One warm copy per file: a session already VISITING this file
            // (e.g. opened via open_file under a custom name) is reused, not
            // shadowed by a divergent second buffer of the same document.
            // Several sessions visiting it is refused rather than guessed at —
            // HashMap order would make {path} address a random one.
            let mut visiting: Vec<String> = sessions
                .iter()
                .filter(|(_, ws)| ws.visited_path().as_deref() == Some(checked.as_path()))
                .map(|(k, _)| k.clone())
                .collect();
            match visiting.len() {
                1 => return Ok(visiting.remove(0)),
                0 => {}
                _ => {
                    visiting.sort();
                    return Err(format!(
                        "ambiguous: sessions {} all visit {} — pass \"session\" explicitly",
                        visiting.join(", "),
                        checked.display()
                    ));
                }
            }
            let quire = Quire::open(&checked).map_err(|e| format!("cannot open file {p}: {e}"))?;
            evict_for_room(sessions);
            sessions.insert(id.clone(), make_workspace(Box::new(quire), false));
            Ok(id)
        }
    }
}

/// Save the session's buffer back to its VISITED file — the `save: true`
/// half of one-call editing. `Workspace::save_to` supplies the atomic write,
/// the stale-read guard, and the rebase. Errors for a buffer with no visited
/// file (e.g. `open_text`): those need `save_buffer` with an explicit path.
fn save_visited(
    sessions: &mut HashMap<String, Workspace>,
    session: &str,
) -> Result<String, String> {
    if !sessions.contains_key(session) {
        return Err(no_such_session(sessions, session));
    }
    let ws = sessions.get_mut(session).expect("checked above");
    let Some(path) = ws.visited_path() else {
        return Err(
            "save: the buffer has no visited file — use save_buffer with a path".to_string(),
        );
    };
    // A refused save (e.g. the stale-read guard) must not strand the edit:
    // the error names the warm session that still holds it.
    let bytes = ws.save_to(&path).map_err(|e| {
        format!(
            "save failed: {e} — the edit is preserved in warm session \
             \"{session}\"; save_buffer it elsewhere, or run_program \
             (revert-buffer) to discard it and re-read the disk state"
        )
    })?;
    Ok(format!(
        "; saved {bytes} bytes to {}{}",
        path.display(),
        parse_warning(sessions, session)
    ))
}

/// A save-time syntax check for buffers tree-sitter parses as CODE (Markdown
/// almost never errors, and huge prose buffers should not pay a parse): a
/// non-empty warning when the buffer no longer parses. Warns, never blocks.
fn parse_warning(sessions: &mut HashMap<String, Workspace>, session: &str) -> &'static str {
    // Markdown and HTML are forgiving grammars (almost nothing errors);
    // everything else is strict enough that a parse error after a save
    // very likely means the edit broke the file.
    let program = "(if (member (treesit-language)\
                               '(\"rust\" \"python\" \"javascript\" \"css\"\
                                 \"toml\" \"yaml\" \"elisp\"))\
                       (report \"err\" (if (treesit-has-error) 1 0)))";
    match run_in_session(sessions, session, program) {
        Ok(report) if report_value(&report, "err").as_deref() == Some("1") => {
            "; WARNING: the saved buffer no longer parses (treesit-has-error)"
        }
        _ => "",
    }
}

/// Build a warm [`Workspace`], read-only when requested.
fn make_workspace(store: Box<dyn TextStore>, read_only: bool) -> Workspace {
    // Sandboxed (agent-facing) tier: the MCP server registers the core editing
    // vocabulary only, never the orchestration group — read-only vs writable is
    // the only per-session distinction.
    let ws = if read_only {
        Workspace::new_read_only(store)
    } else {
        Workspace::new(store)
    };
    // Creation counts as a use, so eviction order is well-defined from birth.
    ws.touch(next_stamp());
    ws
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
    run_or_rehearse(sessions, session, program, false).map(|(report, _value)| report)
}

/// Like [`run_in_session`], but `rehearse` selects a dry-run that rolls the
/// session back afterwards (the `rehearse` tool) instead of a persisting `run`.
fn run_or_rehearse(
    sessions: &mut HashMap<String, Workspace>,
    session: &str,
    program: &str,
    rehearse: bool,
) -> Result<(crate::RunReport, String), String> {
    if !sessions.contains_key(session) {
        return Err(no_such_session(sessions, session));
    }
    let ws = sessions.get_mut(session).expect("checked above");
    ws.touch(next_stamp());
    // Emacs auto-revert-mode: before serving a read or running a program, if the
    // visited file drifted and the warm buffer has NO unsaved edits, silently
    // re-read it so the operation sees the current file rather than stale-or-
    // corrupt bytes. A modified buffer is left alone (the stale-WARN conflict).
    // This runs BEFORE a rehearse takes its rollback snapshot too: the revert
    // discards nothing (the buffer is clean), and without it the preview would
    // run against bytes the committing run — which does revert — won't use.
    ws.auto_revert_if_clean();
    if !rehearse {
        // Auto-capture the pre-program state (version-deduped, bounded) so
        // undo_last can rewind a misfired edit without prior checkpoint
        // discipline. After the auto-revert, so undo never resurrects bytes
        // an external writer already replaced.
        ws.push_undo();
    }
    if rehearse {
        ws.rehearse_value(program)
    } else {
        ws.run_value(program)
    }
}

/// Resolve a tool's target to an EXISTING warm session — the lookup half of
/// [`resolve_session`] without its auto-open (closing a file that isn't warm
/// must not first open it). `path` matches the canonical-path key or a
/// session visiting the file; a bare `session` id is taken as-is.
fn resolve_existing_session(
    args: &Value,
    sessions: &HashMap<String, Workspace>,
) -> Result<String, String> {
    let path = args.get("path").and_then(Value::as_str);
    let session = args.get("session").and_then(Value::as_str);
    match (path, session) {
        (Some(_), Some(_)) => Err("pass either \"path\" or \"session\", not both".to_string()),
        (None, s) => Ok(s.unwrap_or(DEFAULT_SESSION).to_string()),
        (Some(p), None) => {
            let checked = crate::safety::check_path(Path::new(p))?;
            let id = checked.to_string_lossy().into_owned();
            if sessions.contains_key(&id) {
                return Ok(id);
            }
            let mut visiting: Vec<String> = sessions
                .iter()
                .filter(|(_, ws)| ws.visited_path().as_deref() == Some(checked.as_path()))
                .map(|(k, _)| k.clone())
                .collect();
            match visiting.len() {
                1 => Ok(visiting.remove(0)),
                0 => Err(format!("no warm session visits {}", checked.display())),
                _ => {
                    visiting.sort();
                    Err(format!(
                        "ambiguous: sessions {} all visit {} — pass \"session\" explicitly",
                        visiting.join(", "),
                        checked.display()
                    ))
                }
            }
        }
    }
}

/// `close_session {session?|path?, force?}` — drop a warm session, releasing
/// its buffer (and the open fd a file-backed one holds). Refuses while the
/// buffer has unsaved edits unless `force: true` discards them.
fn tool_close_session(
    args: &Value,
    sessions: &mut HashMap<String, Workspace>,
) -> Result<String, String> {
    let session = resolve_existing_session(args, sessions)?;
    if !sessions.contains_key(&session) {
        return Err(no_such_session(sessions, &session));
    }
    let unsaved = is_unsaved(sessions, &session);
    if unsaved && !bool_arg(args, "force") {
        return Err(format!(
            "session \"{session}\" has unsaved edits — save_buffer (or save: true) \
             first, or pass force: true to discard them"
        ));
    }
    sessions.remove(&session);
    Ok(if unsaved {
        format!("closed session \"{session}\" (unsaved edits discarded)")
    } else {
        format!("closed session \"{session}\"")
    })
}

/// A session-miss error that names the warm sessions, so a mistyped (or
/// restart-orphaned) id is a one-glance fix instead of a guessing game.
fn no_such_session(sessions: &HashMap<String, Workspace>, session: &str) -> String {
    let mut ids: Vec<&String> = sessions.keys().collect();
    ids.sort();
    if ids.is_empty() {
        format!(
            "no such session: {session} (none are warm — pass path, or open_file/open_text first)"
        )
    } else {
        format!(
            "no such session: {session} — warm sessions: {}",
            ids.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
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
// No shell, process spawn, or network is ever exposed.

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
    evict_for_room(sessions);
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
    evict_for_room(sessions);
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
    let session = resolve_session(args, sessions)?;
    let program = str_arg(args, "program")?;
    // TODO: resource limits (needs tulisp eval interruption) — a per-program
    // wall-clock/CPU bound can't be enforced until tulisp eval is cancellable.
    let (report, value) = match run_or_rehearse(sessions, &session, &program, rehearse) {
        Ok(rv) => rv,
        // A failed program still said things before it died: the error
        // content is the failure JSON carrying its reports/log, not just the
        // bare error string.
        Err(e) => {
            let (reports, log, dirty) = sessions
                .get(&session)
                .map(|ws| ws.failure_context())
                .unwrap_or_default();
            return Err(pretty(&crate::result::failure_json(
                &e, &reports, &log, dirty,
            )));
        }
    };
    // A rehearsal persists nothing, so it audits as a non-mutating event.
    crate::safety::audit(
        "mime-mcp",
        &session,
        &program,
        report.dirty && !rehearse,
        report.len_before,
        report.len_after,
    );
    let mut json = report.to_json();
    // Surface the program's final value (rendered the way tulisp prints it, as
    // the repl verb does) — present only when it is not `nil`, like `stale` /
    // `unsaved`. This is what makes a bare read-only inspector readable: e.g.
    // `(conflict-diff 1)` returns its unified diff here without the caller
    // having to wrap it in `(message …)`; the buffer `diff` field stays empty
    // for a read-only call.
    if value != "nil" {
        json["value"] = Value::String(value);
    }
    // A bulk edit's diff can run to megabytes; clamp it for transport unless
    // the caller asked for everything. 200 lines ≈ a large hand-made edit.
    if !bool_arg(args, "full_diff") {
        json["diff"] = Value::String(crate::result::clamp_diff(&report.diff, 200));
    }
    // Same drift signal the read tools carry (see stale_note), structured:
    // present only when true, so the common case costs no tokens.
    if sessions.get(&session).is_some_and(|ws| ws.is_stale()) {
        json["stale"] = Value::Bool(true);
    }
    // `save` is rejected on rehearse upstream by `validate_args` (rehearse's
    // schema declares no `save`), so only a real run reaches here.
    if bool_arg(args, "save") {
        let note = save_visited(sessions, &session)?;
        json["saved"] = Value::String(note.trim_start_matches("; ").to_string());
    }
    // Structured, present only when true (like `stale`): the edit is in the warm
    // buffer, not on disk — saving was not requested and the buffer is dirty.
    if is_unsaved(sessions, &session) {
        json["unsaved"] = Value::Bool(true);
    }
    let view = view_echo(args, sessions, &session);
    if !view.is_empty() {
        json["view"] = Value::String(view.trim_start_matches('\n').to_string());
    }
    Ok(pretty(&json))
}

/// One warning line appended to read-tool output when the visited file has
/// drifted under the warm buffer. The stale guard protects saves; READS were
/// silent — and a warm mmap-backed buffer can serve outright corrupted bytes
/// after an external IN-PLACE overwrite (rename-based writers are safe), so
/// a stale read must not pass as clean.
fn stale_note(sessions: &HashMap<String, Workspace>, session: &str) -> &'static str {
    if sessions.get(session).is_some_and(|ws| ws.is_stale()) {
        "\nWARNING: the visited file changed on disk after it was opened — \
         this read may be stale or corrupted; run_program (revert-buffer) \
         re-reads the file (discarding the warm buffer's edits)"
    } else {
        ""
    }
}

/// Whether the session's buffer is file-backed and has edits NOT yet on its
/// visited file — the signal that a `save` was forgotten. False for a clean
/// buffer or one with no file (an `open_text` scratch buffer, which has nothing
/// to forget to save).
fn is_unsaved(sessions: &HashMap<String, Workspace>, session: &str) -> bool {
    sessions
        .get(session)
        .is_some_and(|ws| ws.visited_path().is_some() && ws.is_modified())
}

/// A reminder appended to an edit tool's message when the edit lives only in the
/// warm buffer, not on disk — so a forgotten `save` reads as a visible note
/// instead of a silent loss. Empty when there's nothing to save.
fn unsaved_note(sessions: &HashMap<String, Workspace>, session: &str) -> &'static str {
    if is_unsaved(sessions, session) {
        "\n(unsaved: edits are in the warm buffer, not on disk — pass save:true or save_buffer)"
    } else {
        ""
    }
}

/// Evaluate `(message EXPR)` in the session and hand back the logged string
/// verbatim. `message` stores its argument *raw* in the log, so rendered text
/// comes back without tulisp's string re-quoting (`report` would print
/// \"hello\" rather than hello). The read-only convenience tools —
/// read_region, view, occur, conflicts — all ride this channel, so the stale
/// warning lands on each of them here.
fn run_message(
    sessions: &mut HashMap<String, Workspace>,
    session: &str,
    expr: &str,
    what: &str,
) -> Result<String, String> {
    let report = run_in_session(sessions, session, &format!("(message {expr})"))?;
    let text = report
        .log
        .into_iter()
        .next()
        .ok_or_else(|| format!("{what}: no text returned"))?;
    Ok(format!("{text}{}", stale_note(sessions, session)))
}

/// `read_region {session?, start, end}` — the substring `[start, end)`, fetched
/// on demand via `(buffer-substring START END)` so the agent never has to dump
/// the whole buffer. Reading does not change the buffer text.
fn tool_read_region(
    args: &Value,
    sessions: &mut HashMap<String, Workspace>,
) -> Result<String, String> {
    let session = resolve_session(args, sessions)?;
    let start = int_arg(args, "start")?;
    let end = int_arg(args, "end")?;
    run_message(
        sessions,
        &session,
        &format!("(buffer-substring {start} {end})"),
        "read_region",
    )
}

/// `view {session?, lines?, pos?}` — a rendered viewport: `lines` rows of context
/// on each side of the cursor (or of `pos`), with a gutter, the current line
/// marked, and a header (buffer name, line/col, point/size). The agent's "look at
/// the screen". Backed by the `window` builtin; like `read_region`, it only reads.
fn tool_view(args: &Value, sessions: &mut HashMap<String, Workspace>) -> Result<String, String> {
    let session = resolve_session(args, sessions)?;
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
    run_message(sessions, &session, &call, "view")
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
    let session = resolve_session(args, sessions)?;
    let text = str_arg(args, "text")?;
    let escaped = lisp_literal(&text);
    let anchor = anchor_prelude(args)?;
    if anchor.is_some() && args.get("pos").is_some() {
        return Err("pass either \"pos\" or \"anchor\", not both".to_string());
    }
    let program = match (&anchor, args.get("pos").and_then(Value::as_i64)) {
        (Some((_, prelude)), _) => {
            format!("(progn {prelude} (insert \"{escaped}\") (report \"point\" (point)))")
        }
        (None, Some(pos)) => {
            format!("(progn (goto-char {pos}) (insert \"{escaped}\") (report \"point\" (point)))")
        }
        (None, None) => format!("(progn (insert \"{escaped}\") (report \"point\" (point)))"),
    };
    let report = match run_in_session(sessions, &session, &program) {
        Ok(r) => r,
        Err(e) if e.contains("__no_defun__") => {
            let name = anchor.map(|(n, _)| n).unwrap_or_default();
            return Err(format!(
                "anchor: {}",
                no_defun_error(sessions, &session, &name)
            ));
        }
        Err(e) => return Err(e),
    };
    audit_tool(&session, &program, &report);
    let chars = text.chars().count();
    let point = report_value(&report, "point").unwrap_or_default();
    let saved = if bool_arg(args, "save") {
        save_visited(sessions, &session)?
    } else {
        String::new()
    };
    let unsaved = unsaved_note(sessions, &session);
    let view = view_echo(args, sessions, &session);
    Ok(format!(
        "inserted {chars} chars; point is now {point}{saved}{unsaved}{view}"
    ))
}

/// `replace_text {session?, pattern, replacement, all?}` — replace the first
/// occurrence of literal `pattern` (searching from the top of the accessible
/// region) with literal `replacement`; `all: true` replaces every occurrence.
/// `insert_text`'s counterpart: both strings arrive as raw JSON and are
/// escaped on the server, and the replacement is spliced via
/// delete-region + insert — never `replace-match` — so backslashes and `\1`
/// in the replacement stay literal. Errors when nothing matches (the agent's
/// signal that its anchor text is wrong). Edits the warm buffer; `save_buffer`
/// persists.
fn tool_replace_text(
    args: &Value,
    sessions: &mut HashMap<String, Workspace>,
) -> Result<String, String> {
    if args.get("files").is_some() {
        return tool_replace_files(args, sessions);
    }
    let session = resolve_session(args, sessions)?;
    if let Some(edits) = args.get("edits") {
        if args.get("pattern").is_some() || args.get("replacement").is_some() {
            return Err("pass either \"edits\" or pattern/replacement, not both".to_string());
        }
        return replace_text_batch(args, sessions, &session, edits);
    }
    let pattern = str_arg(args, "pattern")?;
    let replacement = str_arg(args, "replacement")?;
    if pattern.is_empty() {
        return Err("replace_text: pattern must not be empty".to_string());
    }
    let all = bool_arg(args, "all");
    let unique = bool_arg(args, "expect_unique");
    if unique && all {
        return Err("expect_unique contradicts all:true (every occurrence is wanted)".to_string());
    }
    let (pat, rep) = (lisp_literal(&pattern), lisp_literal(&replacement));
    let all_flag = if all { "t" } else { "nil" };
    // search → delete → insert per hit, tracking the line of the last
    // replacement so the result can say WHERE it landed; `more` counts the
    // matches left after the last replacement so a single replace can say
    // "N more remain". The continue-guard runs BEFORE the search so a
    // finished single replace does not move point past (and so under-count)
    // the next match. A miss restores point — a failed replace is a no-op,
    // not a stealth (goto-char (point-min)).
    //
    // expect_unique wraps the same loop in a transaction and errors (rolling
    // the replacement back) if the pattern still matches afterwards — a
    // repeated anchor means the FIRST hit may not be the intended one, so
    // ambiguity is an error, not a silent edit.
    let program = if unique {
        format!(
            "(with-transaction (let ((n 0) (line 0))\
               (goto-char (point-min))\
               (while (and (= n 0) (search-forward \"{pat}\" nil t))\
                 (delete-region (match-beginning 0) (point))\
                 (insert \"{rep}\")\
                 (setq line (line-number-at-pos (point)))\
                 (setq n (+ n 1)))\
               (if (= n 0) (error \"__miss__\"))\
               (if (search-forward \"{pat}\" nil t) (error \"__ambiguous__\"))\
               (report \"n\" n)\
               (report \"line\" line)\
               (report \"point\" (point))))"
        )
    } else {
        format!(
            "(progn (let ((p0 (point)) (n 0) (line 0))\
               (goto-char (point-min))\
               (while (and (or {all_flag} (= n 0)) (search-forward \"{pat}\" nil t))\
                 (delete-region (match-beginning 0) (point))\
                 (insert \"{rep}\")\
                 (setq line (line-number-at-pos (point)))\
                 (setq n (+ n 1)))\
               (if (= n 0) (goto-char p0))\
               (report \"n\" n)\
               (report \"line\" line)\
               (report \"more\" (count-matches (regexp-quote \"{pat}\")))\
               (report \"point\" (point))))"
        )
    };
    let scope = scope_prelude(args)?;
    let program = match &scope {
        Some((_, prelude)) => format!("(save-restriction {prelude} {program})"),
        None => program,
    };
    let report = match run_in_session(sessions, &session, &program) {
        Ok(r) => r,
        Err(e) if e.contains("__no_defun__") => {
            let name = scope.map(|(n, _)| n).unwrap_or_default();
            return Err(format!(
                "scope: {}",
                no_defun_error(sessions, &session, &name)
            ));
        }
        Err(e) if unique && e.contains("__ambiguous__") => {
            let lines = match_lines(sessions, &session, &pat);
            return Err(format!(
                "replace_text: pattern {:?} matches at lines {lines} — expect_unique \
                 requires exactly one; nothing was replaced. Refine the anchor \
                 (occur shows every match in context).",
                truncate_for_error(&pattern)
            ));
        }
        Err(e) if unique && e.contains("__miss__") => {
            return Err(format!(
                "replace_text: no match for the pattern {:?}",
                truncate_for_error(&pattern)
            ));
        }
        Err(e) => return Err(e),
    };
    audit_tool(&session, &program, &report);
    let n: usize = report_value(&report, "n")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if n == 0 {
        return Err(format!(
            "replace_text: no match for the pattern {:?}",
            truncate_for_error(&pattern)
        ));
    }
    let more: usize = report_value(&report, "more")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let point = report_value(&report, "point").unwrap_or_default();
    let line = report_value(&report, "line").unwrap_or_default();
    let saved = if bool_arg(args, "save") {
        save_visited(sessions, &session)?
    } else {
        String::new()
    };
    let unsaved = unsaved_note(sessions, &session);
    let view = view_echo(args, sessions, &session);
    Ok(match (all, more) {
        (true, _) => format!(
            "replaced {n} occurrence(s), last at line {line}; point is now {point}{saved}{unsaved}{view}"
        ),
        (false, 0) => format!(
            "replaced 1 occurrence at line {line}; point is now {point}{saved}{unsaved}{view}"
        ),
        (false, more) => format!(
            "replaced 1 occurrence at line {line}; point is now {point}; {more} more \
             match(es) remain (pass all:true to replace every occurrence, or \
             expect_unique:true to make ambiguity an error){saved}{unsaved}{view}"
        ),
    })
}

/// Parse `anchor: {defun: NAME, where?: "after"|"before"}` into the motion
/// prelude for insert_text — "add this block right after function X" without
/// hand-writing a program. `after` (the default) lands at the defun's end;
/// `before` at its start — which includes any Rust `#[attributes]` / Python
/// decorators, so the insert lands above the whole decorated item.
fn anchor_prelude(args: &Value) -> Result<Option<(String, String)>, String> {
    let Some(anchor) = args.get("anchor") else {
        return Ok(None);
    };
    let defun = anchor.get("defun").and_then(Value::as_str).ok_or(
        "anchor: only {\"defun\": \"name\", \"where\": \"after\"|\"before\"} is supported",
    )?;
    let name = lisp_escape(defun);
    let then = match anchor
        .get("where")
        .and_then(Value::as_str)
        .unwrap_or("after")
    {
        "after" => "(goto-char (treesit-node-end (treesit-defun-at)))",
        "before" => "nil",
        other => {
            return Err(format!(
                "anchor.where must be \"after\" or \"before\", got {other:?}"
            ));
        }
    };
    Ok(Some((
        defun.to_string(),
        format!("(if (treesit-goto-defun \"{name}\") {then} (error \"__no_defun__\"))"),
    )))
}

/// Parse `scope: {defun: NAME}` into a prelude that narrows to that defun
/// (the caller wraps it in `save-restriction` so the narrowing is scoped to
/// the one call). `None` when no scope was given; only the defun form exists
/// today.
fn scope_prelude(args: &Value) -> Result<Option<(String, String)>, String> {
    let Some(scope) = args.get("scope") else {
        return Ok(None);
    };
    let defun = scope
        .get("defun")
        .and_then(Value::as_str)
        .ok_or("scope: only {\"defun\": \"name\"} is supported")?;
    let name = lisp_escape(defun);
    Ok(Some((
        defun.to_string(),
        format!(
            "(if (treesit-goto-defun \"{name}\") (treesit-narrow-to-defun) (error \"__no_defun__\"))"
        ),
    )))
}

/// The error an `__no_defun__` abort becomes: names the missing defun and
/// lists what the outline actually has, so the next call needs no detour
/// through a separate orientation step.
fn no_defun_error(sessions: &mut HashMap<String, Workspace>, session: &str, name: &str) -> String {
    let names: Vec<String> = run_in_session(sessions, session, "(treesit-list-defuns)")
        .map(|r| {
            r.reports
                .iter()
                .filter(|(k, _)| k == "defun")
                .filter_map(|(_, v)| v.splitn(4, ' ').nth(3).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if names.is_empty() {
        format!(
            "no defun named {name:?} — the buffer outlines no defuns at all \
             (wrong language? treesit-set-language overrides extension detection)"
        )
    } else if names.len() > 12 {
        format!(
            "no defun named {name:?} — defuns here: {} … ({} total; the outline tool lists all)",
            names[..12].join(", "),
            names.len()
        )
    } else {
        format!(
            "no defun named {name:?} — defuns here: {}",
            names.join(", ")
        )
    }
}

/// Line numbers of every occurrence of the (already lisp-escaped) literal
/// `pat`, as "12, 40, 73" clamped to the first eight — the detail an
/// expect_unique ambiguity error needs to be actionable. Point is preserved.
fn match_lines(sessions: &mut HashMap<String, Workspace>, session: &str, pat: &str) -> String {
    let program = format!(
        "(save-excursion (goto-char (point-min))\
           (while (search-forward \"{pat}\" nil t)\
             (report \"line\" (line-number-at-pos (match-beginning 0)))))"
    );
    let lines: Vec<String> = run_in_session(sessions, session, &program)
        .map(|r| {
            r.reports
                .iter()
                .filter(|(k, _)| k == "line")
                .map(|(_, v)| v.clone())
                .collect()
        })
        .unwrap_or_default();
    if lines.len() > 8 {
        format!("{} … ({} total)", lines[..8].join(", "), lines.len())
    } else {
        lines.join(", ")
    }
}

/// The `edits: [{pattern, replacement, all?}, …]` form of `replace_text`:
/// sequential literal edits applied in ONE `with-transaction`, so they are
/// all-or-nothing — a miss rolls every earlier edit back and the error names
/// the edit that failed. Each edit searches from the top of the accessible
/// region against the buffer as the previous edits left it.
fn replace_text_batch(
    args: &Value,
    sessions: &mut HashMap<String, Workspace>,
    session: &str,
    edits: &Value,
) -> Result<String, String> {
    let items = edits
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| "\"edits\" must be a non-empty array".to_string())?;
    let scope = scope_prelude(args)?;
    let total = run_batch_edits(sessions, session, items, &scope)?;
    let saved = if bool_arg(args, "save") {
        save_visited(sessions, session)?
    } else {
        String::new()
    };
    let unsaved = unsaved_note(sessions, session);
    let view = view_echo(args, sessions, session);
    Ok(format!(
        "applied {} edit(s), {total} replacement(s){saved}{unsaved}{view}",
        items.len()
    ))
}

/// Build + run the transactional edit batch against ONE session and return
/// the total replacement count — the core shared by the single-session
/// `edits` form and the multi-file form. All-or-nothing per session: a miss
/// or failed uniqueness check rolls the whole transaction back.
fn run_batch_edits(
    sessions: &mut HashMap<String, Workspace>,
    session: &str,
    items: &[Value],
    scope: &Option<(String, String)>,
) -> Result<usize, String> {
    let mut body = String::new();
    for (i, item) in items.iter().enumerate() {
        let pattern = str_arg(item, "pattern")?;
        let replacement = str_arg(item, "replacement")?;
        if pattern.is_empty() {
            return Err(format!("edit {}: pattern must not be empty", i + 1));
        }
        let all = bool_arg(item, "all");
        let unique = bool_arg(item, "expect_unique");
        if unique && all {
            return Err(format!(
                "edit {}: expect_unique contradicts all:true",
                i + 1
            ));
        }
        let (pat, rep) = (lisp_literal(&pattern), lisp_literal(&replacement));
        // The error messages that abort (and roll back) the transaction name
        // the edit; the pattern is echoed clamped.
        let miss = lisp_escape(&format!(
            "edit {}: no match for the pattern {:?}",
            i + 1,
            truncate_for_error(&pattern)
        ));
        let ambiguous = lisp_escape(&format!(
            "edit {}: pattern {:?} matches more than once (expect_unique) — \
             nothing was applied; refine the anchor",
            i + 1,
            truncate_for_error(&pattern)
        ));
        let all_flag = if all { "t" } else { "nil" };
        // The uniqueness post-check searches on from point (just past the
        // replacement), so a later genuine occurrence aborts the whole
        // transaction — evaluated against the buffer as the previous edits
        // left it, like everything else in the batch.
        let unique_check = if unique {
            format!("(if (search-forward \"{pat}\" nil t) (error \"{ambiguous}\"))")
        } else {
            String::new()
        };
        body.push_str(&format!(
            "(goto-char (point-min))\
             (let ((n 0))\
               (while (and (or {all_flag} (= n 0)) (search-forward \"{pat}\" nil t))\
                 (delete-region (match-beginning 0) (point))\
                 (insert \"{rep}\")\
                 (setq n (+ n 1)))\
               (if (= n 0) (error \"{miss}\"))\
               {unique_check}\
               (report \"n\" n))"
        ));
    }
    let program = match scope {
        Some((_, prelude)) => format!("(save-restriction {prelude} (with-transaction {body}))"),
        None => format!("(with-transaction {body})"),
    };
    let report = match run_in_session(sessions, session, &program) {
        Ok(r) => r,
        Err(e) if e.contains("__no_defun__") => {
            let name = scope.as_ref().map(|(n, _)| n.clone()).unwrap_or_default();
            return Err(format!(
                "scope: {}",
                no_defun_error(sessions, session, &name)
            ));
        }
        // The transaction aborts via (error "edit N: …"); hand back just that
        // line, not the generated program's lisp backtrace.
        Err(e) => {
            let line = e
                .lines()
                .find(|l| l.contains("no match for the pattern") || l.contains("expect_unique"))
                .map(|l| l.trim_start_matches("ERR LispError: ").trim().to_string());
            return Err(line.unwrap_or(e));
        }
    };
    audit_tool(session, &program, &report);
    let total: usize = report
        .reports
        .iter()
        .filter(|(k, _)| k == "n")
        .filter_map(|(_, v)| v.parse::<usize>().ok())
        .sum();
    Ok(total)
}

/// The `files: [path…]` form of `replace_text`: the same edit spec applied
/// to EVERY listed file, atomically ACROSS the set — a failure in any file
/// rolls the already-edited ones back via their undo rings, so a cross-file
/// rename is one call that either lands everywhere or nowhere. With
/// `save: true` the files are saved only after every edit succeeded.
fn tool_replace_files(
    args: &Value,
    sessions: &mut HashMap<String, Workspace>,
) -> Result<String, String> {
    if args.get("path").is_some() || args.get("session").is_some() {
        return Err("pass \"files\" OR path/session, not both".to_string());
    }
    let files: Vec<String> = args
        .get("files")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .filter(|v: &Vec<String>| !v.is_empty())
        .ok_or_else(|| "\"files\" must be a non-empty array of paths".to_string())?;
    // Normalize the edit spec: an explicit `edits` array, or the single
    // pattern/replacement promoted to a one-item batch.
    let items: Vec<Value> = match args.get("edits") {
        Some(edits) => edits
            .as_array()
            .filter(|a| !a.is_empty())
            .ok_or_else(|| "\"edits\" must be a non-empty array".to_string())?
            .clone(),
        None => {
            let pattern = str_arg(args, "pattern")?;
            if pattern.is_empty() {
                return Err("replace_text: pattern must not be empty".to_string());
            }
            vec![json!({
                "pattern": pattern,
                "replacement": str_arg(args, "replacement")?,
                "all": bool_arg(args, "all"),
                "expect_unique": bool_arg(args, "expect_unique"),
            })]
        }
    };
    let scope = scope_prelude(args)?;

    // Apply per file; on any failure undo the files already edited so the
    // whole call is all-or-nothing in the warm buffers.
    let mut done: Vec<(String, String, usize)> = Vec::new(); // (path, session, n)
    let rollback = |sessions: &mut HashMap<String, Workspace>, done: &[(String, String, usize)]| {
        for (_, session, _) in done.iter().rev() {
            if let Some(ws) = sessions.get_mut(session) {
                let _ = ws.undo_last();
            }
        }
    };
    for f in &files {
        let session = match resolve_session(&json!({ "path": f }), sessions) {
            Ok(s) => s,
            Err(e) => {
                rollback(sessions, &done);
                return Err(format!(
                    "{f}: {e}\nnothing changed — the {} file(s) edited before it were rolled back",
                    done.len()
                ));
            }
        };
        match run_batch_edits(sessions, &session, &items, &scope) {
            Ok(n) => done.push((f.clone(), session, n)),
            Err(e) => {
                rollback(sessions, &done);
                return Err(format!(
                    "{f}: {e}\nnothing changed — the {} file(s) edited before it were rolled back",
                    done.len()
                ));
            }
        }
    }

    // Saves happen only after every file succeeded. A failed save (the
    // stale guard) reports precisely which files reached disk and which
    // stay warm — nothing is silently lost.
    let mut save_note = String::new();
    if bool_arg(args, "save") {
        let mut saved = 0usize;
        for (f, session, _) in &done {
            if let Err(e) = save_visited(sessions, session) {
                return Err(format!(
                    "{f}: {e}\n({saved} file(s) before it were saved; the rest hold their \
                     edits in warm sessions)"
                ));
            }
            saved += 1;
        }
        save_note = format!("; saved {saved} file(s)");
    }
    let total: usize = done.iter().map(|(_, _, n)| n).sum();
    let lines: Vec<String> = done
        .iter()
        .map(|(f, _, n)| format!("  {f} — {n} replacement(s)"))
        .collect();
    let unsaved = if bool_arg(args, "save") {
        ""
    } else {
        "\n(unsaved: edits are in the warm buffers, not on disk — pass save:true or save_buffer each)"
    };
    Ok(format!(
        "applied {} edit(s) in {} file(s), {total} replacement(s) total:\n{}{save_note}{unsaved}",
        items.len(),
        done.len(),
        lines.join("\n")
    ))
}

/// `search {session?, pattern, mode?}` — search forward from point and report
/// the 1-based position just after the match (Emacs `*-search-forward`
/// semantics), or report that nothing matched. `mode` ∈ exact|regex
/// (default exact). Point moves to the match, as in Emacs.
fn tool_search(args: &Value, sessions: &mut HashMap<String, Workspace>) -> Result<String, String> {
    let session = resolve_session(args, sessions)?;
    let pattern = str_arg(args, "pattern")?;
    let mode = args.get("mode").and_then(Value::as_str).unwrap_or("exact");
    let backward = match args.get("direction").and_then(Value::as_str) {
        None | Some("forward") => false,
        Some("backward") => true,
        Some(other) => return Err(format!("unknown direction: {other} (forward|backward)")),
    };
    let ci = bool_arg(args, "case_insensitive");
    // `search-*` is literal, `re-search-*` is regex; each takes
    // `(NEEDLE BOUND NOERROR)`, noerror=t making a miss return nil. A
    // case-insensitive exact search compiles to a regex over the escaped
    // literal with the (?i) flag.
    let (lisp_fn, needle) = match (mode, ci) {
        ("exact", false) if !backward => ("search-forward", lisp_escape(&pattern)),
        ("exact", false) => ("search-backward", lisp_escape(&pattern)),
        ("exact", true) | ("regex", _) => {
            let re = match (mode, ci) {
                ("exact", _) => format!("(?i){}", regex::escape(&pattern)),
                (_, true) => format!("(?i){pattern}"),
                _ => pattern.clone(),
            };
            (
                if backward {
                    "re-search-backward"
                } else {
                    "re-search-forward"
                },
                lisp_escape(&re),
            )
        }
        (other, _) => return Err(format!("unknown search mode: {other} (exact|regex)")),
    };
    // On a hit, also report the line and echo its text (via the raw message
    // channel) — a search was almost always followed by a view just to see
    // what matched.
    let program = format!(
        "(let ((p ({lisp_fn} \"{needle}\" nil t)))\
           (if p (progn (report \"found\" 1) (report \"pos\" p)\
                        (report \"line\" (line-number-at-pos p))\
                        (message (buffer-substring (line-beginning-position) (line-end-position))))\
                 (report \"found\" 0)))"
    );
    let report = run_in_session(sessions, &session, &program)?;
    let note = stale_note(sessions, &session);
    if report_value(&report, "found").as_deref() == Some("1") {
        let pos = report_value(&report, "pos").unwrap_or_default();
        let line = report_value(&report, "line").unwrap_or_default();
        let text = report.log.first().cloned().unwrap_or_default();
        let where_ = if backward {
            "at the match start"
        } else {
            "just after the match"
        };
        Ok(format!(
            "match ({mode}): point is now {pos} ({where_}) — line {line}: {text}{note}"
        ))
    } else {
        Ok(format!("no {mode} match for pattern{note}"))
    }
}

/// `occur {session?, pattern, mode?, nlines?, limit?}` — every line in the
/// accessible region matching the pattern, rendered with line numbers + char
/// positions (and `nlines` of context), via the `occur` builtin. Read-only
/// orientation: point does not move. `exact` mode (the default) matches the
/// pattern literally.
fn tool_occur(args: &Value, sessions: &mut HashMap<String, Workspace>) -> Result<String, String> {
    let session = resolve_session(args, sessions)?;
    let pattern = str_arg(args, "pattern")?;
    let mode = args.get("mode").and_then(Value::as_str).unwrap_or("exact");
    let ci = bool_arg(args, "case_insensitive");
    let pat_expr = match (mode, ci) {
        ("exact", false) => format!("(regexp-quote \"{}\")", lisp_escape(&pattern)),
        ("exact", true) => format!(
            "\"{}\"",
            lisp_escape(&format!("(?i){}", regex::escape(&pattern)))
        ),
        ("regex", ci) => {
            let re = if ci {
                format!("(?i){pattern}")
            } else {
                pattern.clone()
            };
            format!("\"{}\"", lisp_escape(&re))
        }
        (other, _) => return Err(format!("unknown occur mode: {other} (exact|regex)")),
    };
    let nlines = args.get("nlines").and_then(Value::as_i64).unwrap_or(0);
    let limit = args.get("limit").and_then(Value::as_i64).unwrap_or(100);
    let scope = scope_prelude(args)?;
    let expr = match &scope {
        // Inside the scope, occur's line numbers are defun-relative; the
        // @positions stay absolute and goto-char-able.
        Some((_, prelude)) => {
            format!("(save-restriction {prelude} (occur {pat_expr} {nlines} {limit}))")
        }
        None => format!("(occur {pat_expr} {nlines} {limit})"),
    };
    match run_message(sessions, &session, &expr, "occur") {
        Err(e) if e.contains("__no_defun__") => {
            let name = scope.map(|(n, _)| n).unwrap_or_default();
            Err(format!(
                "scope: {}",
                no_defun_error(sessions, &session, &name)
            ))
        }
        other => other,
    }
}

/// `outline {session?|path?}` — the buffer's structural outline: one
/// `KIND START END NAME` line per defun (Rust/Python functions, types,
/// impls; Markdown sections), via `treesit-list-defuns`. The natural first
/// move on a code file — survey without reading it whole.
fn tool_outline(args: &Value, sessions: &mut HashMap<String, Workspace>) -> Result<String, String> {
    let session = resolve_session(args, sessions)?;
    let report = run_in_session(
        sessions,
        &session,
        "(progn (report \"lang\" (treesit-language)) (treesit-list-defuns))",
    )?;
    let lang = report_value(&report, "lang")
        .map(|s| s.trim_matches('"').to_string())
        .unwrap_or_default();
    let lines: Vec<String> = report
        .reports
        .iter()
        .filter(|(k, _)| k == "defun")
        .map(|(_, v)| v.clone())
        .collect();
    let note = stale_note(sessions, &session);
    if lines.is_empty() {
        return Ok(format!(
            "no defuns found (language: {lang}) — for an extension-less buffer, \
             treesit-set-language (rust|python|javascript|html|css|toml|yaml|elisp|markdown) \
             overrides detection{note}"
        ));
    }
    Ok(format!(
        "— outline ({lang}, {} defuns): KIND START END NAME —\n{}{note}",
        lines.len(),
        lines.join("\n")
    ))
}

/// `conflicts {session?}` — the rendered merge-conflict overview (hunk
/// numbers, positions, labels, side sizes) via the `conflict-hunks` builtin.
/// Read-only; resolution runs through `run_program` (`conflict-keep` /
/// `conflict-replace` / `conflict-resolve-trivial`).
fn tool_conflicts(
    args: &Value,
    sessions: &mut HashMap<String, Workspace>,
) -> Result<String, String> {
    let session = resolve_session(args, sessions)?;
    run_message(sessions, &session, "(conflict-hunks)", "conflicts")
}

/// `checkpoint {session?, label?}` — capture a restore point. Returns the label
/// the engine assigned (auto-generated when omitted).
fn tool_checkpoint(
    args: &Value,
    sessions: &mut HashMap<String, Workspace>,
) -> Result<String, String> {
    let session = resolve_session(args, sessions)?;
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
    let session = resolve_session(args, sessions)?;
    let label = str_arg(args, "label")?;
    let program = format!("(restore-checkpoint \"{}\")", lisp_escape(&label));
    run_in_session(sessions, &session, &program)?;
    // Rewinding the buffer can leave it modified vs. disk — flag it like the
    // edit tools so the restore isn't a silent unsaved change.
    let unsaved = unsaved_note(sessions, &session);
    Ok(format!("restored to checkpoint \"{label}\"{unsaved}"))
}

/// `undo_last {session?|path?}` — rewind to the state before the most recent
/// mutating call (each call steps one further back; no redo). The automatic
/// safety net: unlike restore_checkpoint it needs no label captured up front.
fn tool_undo_last(
    args: &Value,
    sessions: &mut HashMap<String, Workspace>,
) -> Result<String, String> {
    let session = resolve_session(args, sessions)?;
    if !sessions.contains_key(&session) {
        return Err(no_such_session(sessions, &session));
    }
    let ws = sessions.get_mut(&session).expect("checked above");
    ws.undo_last()?;
    let len = ws.char_len();
    let unsaved = unsaved_note(sessions, &session);
    Ok(format!(
        "rewound to the state before the last mutating call ({len} chars){unsaved}"
    ))
}

/// `list_checkpoints {session?}` — the labels currently captured.
fn tool_list_checkpoints(
    args: &Value,
    sessions: &mut HashMap<String, Workspace>,
) -> Result<String, String> {
    let session = resolve_session(args, sessions)?;
    let program = "(report \"checkpoints\" (list-checkpoints))".to_string();
    let report = run_in_session(sessions, &session, &program)?;
    let labels = report_value(&report, "checkpoints").unwrap_or_else(|| "nil".to_string());
    Ok(format!("checkpoints: {labels}"))
}

/// `save_buffer {session?|path?, to?}` — write the session buffer's text to
/// disk. `path` addresses WHICH session, like on every other tool; the
/// destination is `to` (save-as), defaulting to the session's visited file —
/// so a plain `save_buffer {path}` is "save this file" with the stale guard
/// and parse warning applied.
fn tool_save_buffer(
    args: &Value,
    sessions: &mut HashMap<String, Workspace>,
) -> Result<String, String> {
    let session = resolve_session(args, sessions)?;
    match args.get("to").and_then(Value::as_str) {
        None => save_visited(sessions, &session).map(|n| n.trim_start_matches("; ").to_string()),
        Some(to) => {
            let checked = crate::safety::check_path(Path::new(to))?;
            if !sessions.contains_key(&session) {
                return Err(no_such_session(sessions, &session));
            }
            // save_to writes atomically (temp + rename) and re-bases the
            // buffer onto the new file, so an in-place save reclaims the
            // pre-save mmap backing.
            let bytes = sessions
                .get_mut(&session)
                .expect("checked above")
                .save_to(&checked)
                .map_err(|e| format!("cannot write {to}: {e}"))?;
            Ok(format!("wrote {bytes} bytes to {to}"))
        }
    }
}

/// `session_status {}` — the live session ids plus the sandbox the engine
/// enforces: the allowed filesystem roots (as display strings) and whether the
/// audit journal is on. Advertising the roots lets the agent target a writable
/// path up front instead of discovering the bounds via a rejected save.
fn tool_session_status(sessions: &HashMap<String, Workspace>) -> Result<String, String> {
    let mut ids: Vec<&String> = sessions.keys().collect();
    ids.sort();
    // Per session: the current buffer, its visited file, and the states a
    // resuming agent must know without probe programs — an active narrowing,
    // whether the visited file drifted on disk (the stale-save guard's view),
    // and whether the buffer has edits NOT yet written to its file (so a
    // forgotten `save` is visible, not a silent loss).
    let sessions_json: Vec<Value> = ids
        .into_iter()
        .map(|id| {
            let ws = &sessions[id];
            json!({
                "id": id,
                "buffer": ws.buffer_name(),
                "file": ws.visited_path().map(|p| p.display().to_string()),
                "narrowed": ws.is_narrowed(),
                "stale": ws.is_stale(),
                "unsaved": ws.visited_path().is_some() && ws.is_modified(),
            })
        })
        .collect();
    let roots: Vec<String> = crate::safety::roots()
        .iter()
        .map(|r| r.display().to_string())
        .collect();
    Ok(json!({
        "sessions": sessions_json,
        "roots": roots,
        "audit": crate::safety::audit_enabled(),
    })
    .to_string())
}

/// `help {topic?}` — the canonical reference briefs (regex dialect, treesit
/// vocabulary, conflict workflow, session/saving semantics, recipes), served
/// on demand so the always-loaded schemas can stay terse. No topic (or an
/// unknown one) lists what exists.
fn tool_help(args: &Value) -> Result<String, String> {
    let index = || {
        crate::help::TOPICS
            .iter()
            .map(|(name, blurb)| format!("  {name} — {blurb}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    match args.get("topic").and_then(Value::as_str) {
        None => Ok(format!("help topics:\n{}", index())),
        Some(t) => crate::help::topic(t)
            .map(str::to_string)
            .ok_or_else(|| format!("unknown help topic {t:?} — topics:\n{}", index())),
    }
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

/// Escape raw text into a tulisp string-literal body: [`lisp_escape`] plus
/// folding real newlines/tabs into their escape sequences, so the generated
/// program stays a single unambiguous line. The shared escaping of the
/// literal-text tools (`insert_text`, `replace_text`).
fn lisp_literal(s: &str) -> String {
    lisp_escape(s).replace('\n', "\\n").replace('\t', "\\t")
}

/// Journal a convenience-tool edit exactly like `run_program` audits its
/// programs, so the audit trail covers every mutating tool.
fn audit_tool(session: &str, program: &str, report: &crate::RunReport) {
    crate::safety::audit(
        "mime-mcp",
        session,
        program,
        report.dirty,
        report.len_before,
        report.len_after,
    );
}

/// Render the viewport around point when the caller asked for it (`view: N`
/// lines of context, or `true` for the default 4) — visual confirmation that
/// an edit landed where intended, without a follow-up call. Empty when not
/// requested.
fn view_echo(args: &Value, sessions: &mut HashMap<String, Workspace>, session: &str) -> String {
    let lines = match args.get("view") {
        Some(Value::Bool(true)) => 4,
        Some(Value::Number(n)) => n.as_i64().unwrap_or(4).max(0),
        _ => return String::new(),
    };
    run_in_session(sessions, session, &format!("(message (window {lines}))"))
        .ok()
        .and_then(|r| r.log.into_iter().next())
        .map(|t| format!("\n— view —\n{t}"))
        .unwrap_or_default()
}

/// A pattern echoed into an error message, clamped so a pathological pattern
/// cannot flood the result.
fn truncate_for_error(s: &str) -> String {
    const MAX: usize = 80;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        let head: String = s.chars().take(MAX).collect();
        format!("{head}…")
    }
}

// ---- the tool catalogue ----------------------------------------------------

/// `tools/list` result — every tool with a JSON Schema `inputSchema`.
fn tools_list_result() -> Value {
    json!({ "tools": tool_schemas() })
}

/// The MCP tool catalogue, built once and shared. `validate_args` reads it on
/// every dispatch, so the ~20-entry Vec is held behind a `LazyLock` rather than
/// rebuilt per call; `tools/list`, `describe-mcp`, and argument validation all
/// borrow this single source.
pub(crate) fn tool_schemas() -> &'static [Value] {
    static SCHEMAS: LazyLock<Vec<Value>> = LazyLock::new(build_tool_schemas);
    &SCHEMAS
}

fn build_tool_schemas() -> Vec<Value> {
    // A reusable optional `session` property.
    let session = json!({
        "type": "string",
        "description": "Warm session id; defaults to \"default\" when omitted."
    });
    let path = json!({
        "type": "string",
        "description": "One-call alternative to open_file: auto-open this file into a session keyed by its canonical path (reused while warm). Relative paths resolve against the server's cwd. Pass path OR session, not both."
    });
    let save = json!({
        "type": "boolean",
        "description": "After a successful edit, atomically save back to the visited file (stale-guard + audit apply); code buffers warn if they no longer parse. Default false."
    });
    let scope = json!({
        "type": "object",
        "description": "Restrict this call to one part of the buffer without writing a program. {\"defun\": \"name\"} narrows to that function/class/section (see the outline tool for names) for just this call; an unknown name errors and lists the defuns that exist.",
        "properties": { "defun": { "type": "string" } },
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
            "description": "Evaluate an Emacs-Lisp (tulisp) edit program against the session buffer and return a structured RunReport (unified diff, point, length before/after, any (report ...)/(message ...) output, and `value`: the final form's result rendered the way tulisp prints it — present only when non-nil, so a read-only inspector like (conflict-diff N) is readable without wrapping it in (message ...)). This is the core, general-purpose editing tool; the buffer and any defined functions persist for the next call. On failure the error content is a JSON object {ok:false, error, dirty, reports, log} carrying the diagnostics the program emitted before dying; dirty=true means its pre-error edits persist (a run does not roll back on error — use rehearse, or with-transaction inside the program, for atomicity).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "program": { "type": "string", "description": "Emacs-Lisp program, e.g. (while (re-search-forward \"foo\" nil t) (replace-match \"bar\"))." },
                    "full_diff": { "type": "boolean", "description": "Return the whole unified diff. Default false: diffs beyond 200 lines come back clamped to head + tail around an elision line carrying the suppressed count." },
                    "view": { "type": ["boolean", "integer"], "description": "Add a rendered viewport around point to the report (true = 4 context lines, or a line count)." },
                    "session": session,
                    "path": path,
                    "save": save,
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
                    "full_diff": { "type": "boolean", "description": "Return the whole unified diff. Default false: diffs beyond 200 lines come back clamped to head + tail around an elision line carrying the suppressed count." },
                    "view": { "type": ["boolean", "integer"], "description": "Add a rendered viewport around point to the report (true = 4 context lines, or a line count)." },
                    "session": session,
                    "path": path,
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
                    "path": path,
                },
                "required": ["start", "end"],
            },
        }),
        json!({
            "name": "view",
            "description": "Render a viewport around the cursor (or a given position): a few lines of context on each side, with a gutter, the current line marked, and a header (flagged 'Narrow' when a restriction is active). Read-only. Coordinate convention everywhere: char positions (@N, point) are ABSOLUTE — feed goto-char; line numbers count from the accessible region's start — feed goto-line.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "lines": { "type": "integer", "description": "Context lines on each side of the cursor line (default 4)." },
                    "pos": { "type": "integer", "description": "1-based position to center on (default: current point)." },
                    "session": session,
                    "path": path,
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
                    "anchor": { "type": "object", "description": "Insert relative to a named defun instead of a position: {\"defun\": \"name\", \"where\": \"after\"|\"before\"} (default after — lands at the defun's end; include separating newlines in the text). \"before\" lands above the whole decorated item (Rust #[attributes] / Python decorators included). Not combinable with pos.", "properties": { "defun": { "type": "string" }, "where": { "type": "string", "enum": ["after", "before"] } } },
                    "view": { "type": ["boolean", "integer"], "description": "Append a rendered viewport around point after the edit (true = 4 context lines, or a line count) — confirm the insert landed right without a follow-up view call." },
                    "session": session,
                    "path": path,
                    "save": save,
                },
                "required": ["text"],
            },
        }),
        json!({
            "name": "replace_text",
            "description": "Replace the FIRST occurrence of a literal pattern with literal replacement text (searching from the top of the accessible region); pass all:true to replace every occurrence. Both strings are plain — no Lisp escaping, no regex, no backref expansion (insert_text's counterpart; the fix for quote-heavy edits). Errors when nothing matches (and leaves point untouched); a single replace reports how many more matches remain. Pattern occurrences INSIDE just-inserted replacement text are not re-matched or counted. Edits the warm buffer; call save_buffer to persist. For regex or position-scoped replacement, use run_program.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "The exact text to find (literal, not regex)." },
                    "replacement": { "type": "string", "description": "The literal replacement text." },
                    "all": { "type": "boolean", "description": "Replace every occurrence (default false: first only)." },
                    "expect_unique": { "type": "boolean", "description": "Require the pattern to match exactly once: more than one match is an error (listing the match lines) and nothing is replaced. RECOMMENDED whenever the anchor text could plausibly repeat — first-match semantics would silently edit the wrong site. Default false." },
                    "scope": scope,
                    "view": { "type": ["boolean", "integer"], "description": "Append a rendered viewport around point after the edit (true = 4 context lines, or a line count)." },
                    "edits": { "type": "array", "description": "Instead of pattern/replacement: [{pattern, replacement, all?, expect_unique?}, …] applied in order inside ONE transaction — all-or-nothing; a miss (or a failed uniqueness check) rolls everything back and names the failed edit.", "items": { "type": "object" } },
                    "files": { "type": "array", "description": "Apply the SAME edit spec (pattern/replacement or edits) to every listed file in one call — the cross-file rename. Atomic across the set: a failure in any file rolls the already-edited ones back; with save:true the files are saved only after every edit succeeded. Each path must contain the pattern (a miss is an error — list exactly the files you grepped). Not combinable with path/session.", "items": { "type": "string" } },
                    "session": session,
                    "path": path,
                    "save": save,
                },
                "required": [],
            },
        }),
        json!({
            "name": "search",
            "description": "Search from point for a pattern; report the resulting position plus the matched line's number and text. Moves point to the match — just after it (forward) or to its start (backward), Emacs semantics.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "What to search for." },
                    "mode": {
                        "type": "string",
                        "enum": ["exact", "regex"],
                        "description": "exact (literal) or regex (RE2). Defaults to exact.",
                    },
                    "direction": {
                        "type": "string",
                        "enum": ["forward", "backward"],
                        "description": "Search direction from point. Default forward. Backward finds the latest match wholly before point.",
                    },
                    "case_insensitive": { "type": "boolean", "description": "Match case-insensitively (both modes). Default false." },
                    "session": session,
                    "path": path,
                },
                "required": ["pattern"],
            },
        }),
        json!({
            "name": "occur",
            "description": "Overview of every line matching a pattern in the whole accessible region (composes with narrowing): line number (narrowing-relative, goto-line-able) + char position (absolute, goto-char-able) per hit, optional context lines, long lines clamped. Read-only; point does not move. Your 'grep the buffer' for orientation before editing.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "What to list matches for." },
                    "mode": {
                        "type": "string",
                        "enum": ["exact", "regex"],
                        "description": "exact (literal) or regex (RE2). Defaults to exact.",
                    },
                    "case_insensitive": { "type": "boolean", "description": "Match case-insensitively (both modes). Default false." },
                    "nlines": { "type": "integer", "description": "Context lines around each hit (default 0)." },
                    "limit": { "type": "integer", "description": "Max matching lines rendered (default 100); the rest are summarized in a tail line." },
                    "scope": scope,
                    "session": session,
                    "path": path,
                },
                "required": ["pattern"],
            },
        }),
        json!({
            "name": "outline",
            "description": "The buffer's structural outline: one 'KIND START END NAME' line per defun (Rust/Python functions, types, impls, mods; Markdown sections), in document order with nested ones included. The natural first move on a code file — survey it without reading it whole; the names feed scope/anchor parameters and treesit-goto-defun.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session,
                    "path": path,
                },
                "required": [],
            },
        }),
        json!({
            "name": "conflicts",
            "description": "Overview of the merge-conflict hunks in the buffer: number, position + line, branch labels, side sizes; warns about marker lines it could not parse (malformed/nested). Read-only. Resolve via run_program: (conflict-keep SIDE &optional N) with ours|theirs|both, or base|all on diff3 hunks only; (conflict-replace TEXT &optional N) for a hand-crafted merge; (conflict-resolve-trivial) to sweep the safe ones; (conflict-diff &optional N) to see what differs; (conflict-text SIDE &optional N) to read one side. Mutating calls return the remaining count — wrap them in (report \"left\" …) to see it in run_program's JSON. N is 1-based and refreshes after each edit; nil N = the hunk at point. @positions are absolute, L labels narrowing-relative; a narrowing that cuts through a hunk hides it entirely — widen before resolving.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session,
                    "path": path,
                },
                "required": [],
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
                    "path": path,
                },
                "required": [],
            },
        }),
        json!({
            "name": "undo_last",
            "description": "Rewind the buffer to its state before the most recent mutating call — the automatic safety net for a misfired edit (every mutating tool call captures a restore point first; bounded ring of 8, no redo). Each call steps one mutating call further back. Unlike restore_checkpoint, nothing needs to have been captured up front.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session,
                    "path": path,
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
                    "path": path,
                },
                "required": ["label"],
            },
        }),
        json!({
            "name": "list_checkpoints",
            "description": "List the labels of checkpoints captured in this session.",
            "inputSchema": {
                "type": "object",
                "properties": { "session": session, "path": path },
                "required": [],
            },
        }),
        json!({
            "name": "save_buffer",
            "description": "Write the session buffer's text to disk. Without `to`, save back to the session's visited file (atomic write, stale-read guard, parse warning — the same save the edit tools' save:true performs); with `to`, save-as to that path. NOTE: `path` addresses WHICH session, exactly like on every other tool — the destination parameter is `to`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": { "type": "string", "description": "Optional save-as destination. Omitted: write back to the visited file." },
                    "session": session,
                    "path": path,
                },
                "required": [],
            },
        }),
        json!({
            "name": "close_session",
            "description": "Drop a warm session: releases its buffer and the open file handle a file-backed session holds. Refuses while the session has unsaved edits unless force:true discards them. Use it when done with a file, or to force a clean re-open from disk.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "force": { "type": "boolean", "description": "Discard unsaved edits. Default false: closing an unsaved session is an error." },
                    "session": session,
                    "path": path,
                },
                "required": [],
            },
        }),
        json!({
            "name": "help",
            "description": "Reference briefs served on demand: the regex dialect (RE2 patterns, Emacs anchors/replacements), the treesit structural-editing vocabulary, the merge-conflict workflow, session/saving/undo semantics, and ready-to-adapt edit recipes. Call it with no topic to list the topics; reach for it BEFORE guessing at syntax or vocabulary.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "topic": { "type": "string", "enum": ["regex", "treesit", "conflicts", "sessions", "recipes"], "description": "Which brief to fetch; omit to list them." },
                },
                "required": [],
            },
        }),
        json!({
            "name": "session_status",
            "description": "Report engine status: per live session the current buffer, its visited file, and whether it is narrowed, stale (its file drifted on disk), or unsaved (has edits not yet written to that file — so a forgotten save is visible); plus the allowed filesystem roots that open_file/save_buffer are confined to (MIME_ROOTS, default cwd), and whether the audit journal is on. Check the roots before opening or saving to learn the writable sandbox up front.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": [],
            },
        }),
    ]
}
