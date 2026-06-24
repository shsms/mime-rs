//! Streamable HTTP transport for the MCP server — a second front end beside
//! stdio (`--mcp`), for hosted / multi-client harnesses. It reuses the
//! transport-agnostic [`crate::mcp::handle_line`] dispatch and answers with
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

use crate::Workspace;
use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::sync::Mutex;
use tiny_http::{Header, Method, Request, Response, Server};

/// Cap on a request body — programs and buffers can be large, but not unbounded.
const MAX_BODY: u64 = 64 * 1024 * 1024;

/// Cap on concurrent client sessions (oldest FIFO-evicted past this). Bounds
/// memory and open file descriptors against an `initialize` flood.
const SESSION_CAP: usize = 256;

/// One client's warm-session map (the same type the stdio server holds).
type Sessions = HashMap<String, Workspace>;

/// Bounded set of per-client session maps, keyed by an unguessable
/// `Mcp-Session-Id`. `order` tracks creation order for FIFO eviction.
struct Store {
    map: HashMap<String, Sessions>,
    order: VecDeque<String>,
}

impl Store {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }
    /// Create a fresh empty session, evicting the oldest while at capacity.
    fn insert(&mut self, id: String) {
        while self.map.len() >= SESSION_CAP {
            match self.order.pop_front() {
                Some(old) => {
                    self.map.remove(&old);
                }
                None => break,
            }
        }
        self.order.push_back(id.clone());
        self.map.entry(id).or_default();
    }
    /// Drop a session by id; returns whether it existed.
    fn remove(&mut self, id: &str) -> bool {
        self.order.retain(|x| x != id);
        self.map.remove(id).is_some()
    }
}

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

    let store = Mutex::new(Store::new());
    for request in server.incoming_requests() {
        serve(request, &store);
    }
}

fn serve(mut request: Request, store: &Mutex<Store>) {
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
            let id = new_session_id();
            store.insert(id.clone());
            let sessions = store.map.get_mut(&id).expect("just inserted");
            (crate::mcp::handle_line(&body, sessions), Some(id))
        } else {
            match id_header
                .as_deref()
                .filter(|id| store.map.contains_key(*id))
            {
                Some(id) => {
                    let sessions = store.map.get_mut(id).expect("checked present");
                    (crate::mcp::handle_line(&body, sessions), None)
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

/// A 128-bit random session id (hex). The id is the client's bearer token, so
/// it must not be guessable — read it from the OS CSPRNG, failing closed if
/// that is somehow unavailable rather than minting a predictable id.
fn new_session_id() -> String {
    let mut buf = [0u8; 16];
    match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)) {
        Ok(()) => buf.iter().map(|b| format!("{b:02x}")).collect(),
        Err(e) => {
            eprintln!("mime-http: cannot read /dev/urandom for a session id: {e}");
            std::process::exit(1);
        }
    }
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

    #[test]
    fn session_ids_are_random_hex_and_distinct() {
        let a = new_session_id();
        let b = new_session_id();
        assert_eq!(a.len(), 32, "128 bits as hex");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two ids must differ");
    }

    #[test]
    fn store_caps_at_session_cap_and_evicts_oldest() {
        let mut s = Store::new();
        for i in 0..SESSION_CAP + 5 {
            s.insert(format!("id{i}"));
        }
        assert_eq!(s.map.len(), SESSION_CAP);
        assert!(!s.map.contains_key("id0"), "oldest evicted");
        assert!(
            s.map.contains_key(&format!("id{}", SESSION_CAP + 4)),
            "newest kept"
        );
        assert!(s.remove(&format!("id{}", SESSION_CAP + 4)));
        assert!(!s.remove("nope"));
    }
}
