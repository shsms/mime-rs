//! Streamable HTTP transport for the MCP server — a second front end beside
//! stdio (`--mcp`), for hosted / multi-client harnesses. It reuses the
//! transport-agnostic [`crate::mcp::handle_line`] dispatch and answers with
//! plain JSON: mime never sends a server-initiated message, so there is no SSE
//! stream to open (a spec-valid choice — a server MAY return `application/json`
//! for any request). Security per the MCP guidance: bind localhost by default
//! and reject a non-local browser `Origin` (anti-DNS-rebinding). Each client's
//! warm sessions are isolated by the `Mcp-Session-Id` it is issued on
//! `initialize`, so concurrent clients don't share buffers.

use crate::Workspace;
use std::collections::HashMap;
use std::io::Read;
use std::sync::Mutex;
use tiny_http::{Header, Method, Request, Response, Server};

/// Cap on a request body — programs and buffers can be large, but not unbounded.
const MAX_BODY: u64 = 64 * 1024 * 1024;

/// One client's warm-session map (the same type the stdio server holds).
type Sessions = HashMap<String, Workspace>;

/// Serve the MCP protocol over Streamable HTTP at `addr` until killed.
pub fn run(addr: &str) {
    let server = match Server::http(addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("mime-http: cannot bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "mime-http: ready on http://{addr}/mcp (MCP protocol {})",
        crate::mcp::PROTOCOL_VERSION
    );

    // Per-client session maps keyed by Mcp-Session-Id, plus a counter that mints
    // ids on initialize. Requests are served one at a time (like the daemon), so
    // a single lock around the store is all the synchronization needed.
    let store: Mutex<HashMap<String, Sessions>> = Mutex::new(HashMap::new());
    let mut next_id: u64 = 0;
    let seed = id_seed();
    for request in server.incoming_requests() {
        serve(request, &store, &mut next_id, seed);
    }
}

fn serve(
    mut request: Request,
    store: &Mutex<HashMap<String, Sessions>>,
    next_id: &mut u64,
    seed: u64,
) {
    // Anti-DNS-rebinding: a browser-set Origin must be localhost. A missing
    // Origin (curl, an SDK, a CLI harness) is allowed — the attack is browser-only.
    if let Some(origin) = header(&request, "origin")
        && !is_local_origin(&origin)
    {
        let _ = request.respond(text(403, "origin not allowed"));
        return;
    }

    if request.url().split('?').next() != Some("/mcp") {
        let _ = request.respond(empty(404));
        return;
    }

    match request.method() {
        Method::Post => {}
        Method::Delete => {
            if let Some(id) = header(&request, "mcp-session-id") {
                store.lock().unwrap().remove(&id);
            }
            let _ = request.respond(empty(204));
            return;
        }
        // No server-initiated messages, so no SSE stream to open on GET.
        Method::Get => {
            let _ = request.respond(text(405, "no event stream; POST JSON-RPC to /mcp"));
            return;
        }
        _ => {
            let _ = request.respond(empty(405));
            return;
        }
    }

    let mut body = String::new();
    if request
        .as_reader()
        .take(MAX_BODY)
        .read_to_string(&mut body)
        .is_err()
    {
        let _ = request.respond(text(400, "could not read request body"));
        return;
    }

    // Pick the client's session map; mint an id on initialize when the client
    // didn't supply one. A non-initialize request without an id shares "default"
    // (lenient — a simple client that ignores Mcp-Session-Id still works).
    let header_id = header(&request, "mcp-session-id");
    let is_init = is_initialize(&body);
    let session_id = match (&header_id, is_init) {
        (Some(id), _) => id.clone(),
        (None, true) => {
            *next_id += 1;
            format!("{seed:x}-{}", *next_id)
        }
        (None, false) => "default".to_string(),
    };

    let reply = {
        let mut store = store.lock().unwrap();
        let sessions = store.entry(session_id.clone()).or_default();
        crate::mcp::handle_line(&body, sessions)
    };

    match reply {
        Some(value) => {
            let json = serde_json::to_string(&value).unwrap_or_else(|_| value.to_string());
            let mut resp = Response::from_string(json).with_header(json_header());
            if is_init {
                resp = resp.with_header(session_header(&session_id));
            }
            let _ = request.respond(resp);
        }
        // A notification (no `id`) — accepted, with nothing to return.
        None => {
            let _ = request.respond(empty(202));
        }
    }
}

/// First request header matching `name` (case-insensitive), as an owned String.
fn header(request: &Request, name: &str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str().to_string())
}

/// Whether an `Origin` value names a localhost host (any scheme/port).
fn is_local_origin(origin: &str) -> bool {
    let after = origin.split("://").nth(1).unwrap_or(origin);
    let host = after.split('/').next().unwrap_or(after);
    for h in ["localhost", "127.0.0.1", "[::1]"] {
        if host == h
            || host
                .strip_prefix(h)
                .is_some_and(|rest| rest.starts_with(':'))
        {
            return true;
        }
    }
    false
}

/// Whether a JSON-RPC body is an `initialize` request.
fn is_initialize(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("method").and_then(|m| m.as_str().map(str::to_string)))
        .is_some_and(|m| m == "initialize")
}

fn id_seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn json_header() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap()
}

fn session_header(id: &str) -> Header {
    Header::from_bytes(&b"Mcp-Session-Id"[..], id.as_bytes()).unwrap()
}

fn text(code: u16, msg: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(msg.to_string()).with_status_code(code)
}

fn empty(code: u16) -> Response<std::io::Empty> {
    Response::empty(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_origins_pass_remote_ones_dont() {
        for ok in [
            "http://localhost",
            "http://localhost:7711",
            "http://127.0.0.1:7711",
            "https://[::1]:9000",
        ] {
            assert!(is_local_origin(ok), "{ok} should be local");
        }
        for bad in [
            "http://evil.com",
            "https://localhost.attacker.com",
            "http://10.0.0.5:7711",
        ] {
            assert!(!is_local_origin(bad), "{bad} should be rejected");
        }
    }

    #[test]
    fn initialize_is_detected() {
        assert!(is_initialize(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#
        ));
        assert!(!is_initialize(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#
        ));
        assert!(!is_initialize("not json"));
    }
}
