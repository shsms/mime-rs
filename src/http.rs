//! Streamable HTTP transport for the MCP server — a second front end beside
//! stdio (`--mcp`), for hosted / multi-client harnesses. It reuses the
//! transport-agnostic [`crate::rpc::handle_line`] dispatch and answers with
//! plain JSON: mime never sends a server-initiated message, so there is no SSE
//! stream to open (a spec-valid choice — a server MAY return `application/json`
//! for any request).
//!
//! Security, per the MCP guidance: bind localhost by default and reject a
//! non-local browser `Origin` (anti-DNS-rebinding). `initialize` mints an
//! unguessable, random `Mcp-Session-Id` and isolates that client's warm
//! sessions under it; the id is the client's bearer token, so every later
//! request MUST carry it (an absent/unknown one is a 404 — re-initialize). The
//! session store is capped and FIFO-evicted so an initialize flood can't
//! exhaust memory or file descriptors. Requests are served one at a time (like
//! the daemon): simple and race-free, at the cost of head-of-line blocking if a
//! single op runs long.

use crate::rpc::WorkspaceStore;
use std::io::Read;
use std::sync::Mutex;
use tiny_http::{Header, Method, Request, Response, Server};

/// Cap on a request body — programs and buffers can be large, but not unbounded.
const MAX_BODY: u64 = 64 * 1024 * 1024;

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
        crate::rpc::PROTOCOL_VERSION
    );

    let store = Mutex::new(WorkspaceStore::new());
    for request in server.incoming_requests() {
        serve(request, &store);
    }
}

fn serve(mut request: Request, store: &Mutex<WorkspaceStore>) {
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

    let id_header = header(&request, "mcp-session-id");

    match request.method() {
        Method::Post => {}
        // Ending a session needs its id — the id is the bearer token, so this
        // can only drop a session the caller already holds.
        Method::Delete => {
            let dropped = id_header
                .as_deref()
                .is_some_and(|id| store.lock().unwrap().remove(id));
            let _ = request.respond(empty(if dropped { 204 } else { 404 }));
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

    // Refuse an over-cap body rather than silently truncating it (which would
    // surface later as a misleading JSON parse error).
    if request.body_length().is_some_and(|n| n as u64 > MAX_BODY) {
        let _ = request.respond(text(413, "request body too large"));
        return;
    }
    let mut body = String::new();
    let read = request
        .as_reader()
        .take(MAX_BODY + 1)
        .read_to_string(&mut body);
    if read.is_err() || body.len() as u64 > MAX_BODY {
        let _ = request.respond(text(413, "request body too large or unreadable"));
        return;
    }

    let is_init = is_initialize(&body);

    // Resolve the client's session map under the lock, then respond outside it.
    // initialize starts a new session (fresh random id); every other request
    // must name an existing one.
    let (reply, new_id) = {
        let mut store = store.lock().unwrap();
        if is_init {
            let id = store.mint();
            let ctx = crate::rpc::CallContext {
                transport: crate::rpc::Transport::Http,
                implicit_workspace: Some(&id),
            };
            let reply = crate::rpc::handle_line(&body, &mut store, &ctx);
            (reply, Some(id))
        } else {
            match id_header.as_deref().filter(|id| store.contains(id)) {
                Some(id) => {
                    let id = id.to_string();
                    let ctx = crate::rpc::CallContext {
                        transport: crate::rpc::Transport::Http,
                        implicit_workspace: Some(&id),
                    };
                    (crate::rpc::handle_line(&body, &mut store, &ctx), None)
                }
                None => {
                    drop(store);
                    let _ = request.respond(text(
                        404,
                        "unknown or missing Mcp-Session-Id — send initialize first",
                    ));
                    return;
                }
            }
        }
    };

    match reply {
        Some(value) => {
            let json = serde_json::to_string(&value).unwrap_or_else(|_| value.to_string());
            let mut resp = Response::from_string(json).with_header(json_header());
            if let Some(id) = new_id {
                resp = resp.with_header(session_header(&id));
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
