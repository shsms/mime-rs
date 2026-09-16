//! Streamable HTTP transport for the MCP server — a second front end beside
//! stdio (`--mcp`), for hosted / multi-client harnesses. It is dual-era: a
//! legacy client initializes and carries an `Mcp-Session-Id` on every later
//! request, while a 2026-07-28 client is stateless — no session header at all,
//! each request self-describing through its standard headers, with the
//! `workspace` handle it wants to reuse passed as a tool argument in the body.
//! Both eras share the transport-agnostic [`crate::rpc::handle_request`]
//! dispatch (stdio enters through [`crate::rpc::handle_line`], which parses a
//! line and calls it) and answer with plain JSON: mime never sends a
//! server-initiated message, so there is no SSE stream to open (a spec-valid
//! choice — a server MAY return `application/json` for any request).
//!
//! Security, per the MCP guidance: bind localhost by default and reject a
//! non-local browser `Origin` (anti-DNS-rebinding). Legacy `initialize` mints
//! an unguessable, random `Mcp-Session-Id` and isolates that client's warm
//! sessions under it; the id is the client's bearer token, so every later
//! request MUST carry it (an absent/unknown one is a 404 — re-initialize). A
//! modern request instead names its workspace handle, which is equally
//! unguessable. The workspace store is bounded: past its cap the oldest
//! workspace without unsaved edits is evicted, so a flood of handshakes cannot
//! exhaust memory or file descriptors, while unsaved work is never dropped.
//! Requests are served one at a time (like the daemon): simple and race-free,
//! at the cost of head-of-line blocking if a single op runs long.

use crate::rpc::{CallContext, Transport, WorkspaceStore};
use serde_json::{Value, json};
use std::io::Read;
use std::sync::Mutex;
use tiny_http::{Header, Method, Request, Response, Server};

/// Cap on a request body — programs and buffers can be large, but not
/// unbounded.
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HttpMethod {
    Get,
    Post,
    Delete,
    Other,
}

/// A request with the transport details already read, so routing is a pure
/// function that tests can drive without a socket.
struct HttpRequest {
    method: HttpMethod,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl HttpRequest {
    /// First header matching `name`, case-insensitively.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

struct HttpReply {
    status: u16,
    /// Body is JSON (sets Content-Type). Plain text otherwise.
    json: bool,
    /// `Mcp-Session-Id` to send back (legacy `initialize` only).
    session_id: Option<String>,
    body: String,
}

impl HttpReply {
    fn text(status: u16, body: &str) -> Self {
        Self {
            status,
            json: false,
            session_id: None,
            body: body.to_string(),
        }
    }
    fn empty(status: u16) -> Self {
        Self::text(status, "")
    }
    fn json(status: u16, value: &Value) -> Self {
        Self {
            status,
            json: true,
            session_id: None,
            body: value.to_string(),
        }
    }
}

/// The 404 an `initialize`-less legacy request gets: the session id is the
/// client's bearer token, so a missing or stale one can only be re-earned.
const NO_SESSION: &str = "unknown or missing Mcp-Session-Id — send initialize first";

/// Build this request's [`CallContext`], dispatch it, and map the protocol
/// layer's answer onto an HTTP reply: a notification (no reply) is a 202, and
/// `status` picks the code for a real one (legacy always 200; the modern era
/// maps JSON-RPC error codes onto HTTP ones).
fn dispatch(
    req: Value,
    store: &mut WorkspaceStore,
    implicit: Option<&str>,
    status: impl Fn(&Value) -> u16,
) -> HttpReply {
    let ctx = CallContext {
        transport: Transport::Http,
        implicit_workspace: implicit,
    };
    match crate::rpc::handle_request(req, store, &ctx) {
        Some(v) => HttpReply::json(status(&v), &v),
        None => HttpReply::empty(202),
    }
}

fn route(req: &HttpRequest, store: &mut WorkspaceStore) -> HttpReply {
    // Anti-DNS-rebinding: a browser-set Origin must be localhost. A missing
    // Origin (curl, an SDK, a CLI harness) is allowed — the attack is
    // browser-only.
    if let Some(origin) = req.header("origin")
        && !is_local_origin(origin)
    {
        return HttpReply::text(403, "origin not allowed");
    }
    if req.path.split('?').next() != Some("/mcp") {
        return HttpReply::empty(404);
    }
    match req.method {
        HttpMethod::Post => {}
        // Ending a legacy session needs its id — the id is the bearer token, so
        // this can only drop a session the caller already holds.
        HttpMethod::Delete => {
            let dropped = req
                .header("mcp-session-id")
                .is_some_and(|id| store.remove(id));
            return HttpReply::empty(if dropped { 204 } else { 404 });
        }
        // No server-initiated messages, so no SSE stream to open on GET.
        HttpMethod::Get => return HttpReply::text(405, "no event stream; POST JSON-RPC to /mcp"),
        HttpMethod::Other => return HttpReply::empty(405),
    }

    // Parsed once, here: the era comes off the parsed value, and the same value
    // is handed to the protocol layer instead of being re-parsed.
    let body: Value = match serde_json::from_str(&req.body) {
        Ok(v) => v,
        // An unparseable body names no era — the era lives inside it — so the
        // client's own headers have to say which answer it can understand. An
        // `MCP-Protocol-Version` header naming the modern version can only come
        // from a modern client (legacy clients since 2025-06-18 send the header
        // too, with their own version), so that client gets the modern shape
        // for a bad request: HTTP 400 carrying the `-32700` JSON-RPC body. A
        // legacy version, or no header, is answered the way the legacy path
        // answers everything else, which is also how this server answered
        // before it was dual-era: a client holding a known session gets the
        // `-32700` body at HTTP 200 (the error is at the protocol layer, not
        // the transport), and one without gets the same 404 as any other
        // sessionless legacy request. Garbage must not reveal more about the
        // server than well-formed JSON does.
        Err(e) => {
            if req.header("mcp-protocol-version") == Some(crate::rpc::PROTOCOL_VERSION) {
                return HttpReply::json(400, &crate::rpc::parse_error(&e));
            }
            return match req.header("mcp-session-id").filter(|id| store.contains(id)) {
                Some(_) => HttpReply::json(200, &crate::rpc::parse_error(&e)),
                None => HttpReply::text(404, NO_SESSION),
            };
        }
    };
    // A malformed `_meta` is still modern, so it gets the protocol layer's
    // `-32602` rather than a legacy "unknown session" 404.
    match crate::rpc::era_of(&body["params"]) {
        crate::rpc::Era::Modern => route_modern(req, body, store),
        crate::rpc::Era::Legacy => route_legacy(req, body, store),
    }
}

/// Legacy era: `initialize` mints a session; every other request must carry a
/// known `Mcp-Session-Id`. A missing `MCP-Protocol-Version` header is tolerated
/// (the spec allows it for servers supporting pre-2025-06-18 clients).
fn route_legacy(req: &HttpRequest, body: Value, store: &mut WorkspaceStore) -> HttpReply {
    if body["method"] == "initialize" {
        let id = store.mint();
        let mut out = dispatch(body, store, Some(&id), |_| 200);
        out.session_id = Some(id);
        return out;
    }
    let Some(id) = req.header("mcp-session-id").filter(|id| store.contains(id)) else {
        return HttpReply::text(404, NO_SESSION);
    };
    let id = id.to_string();
    dispatch(body, store, Some(&id), |_| 200)
}

/// Modern era (2026-07-28): stateless. The standard request headers must agree
/// with the body; `Mcp-Session-Id` / `Last-Event-ID` are ignored.
fn route_modern(req: &HttpRequest, body: Value, store: &mut WorkspaceStore) -> HttpReply {
    let method = body["method"].as_str().unwrap_or("").to_string();
    let mut checks: Vec<(&str, Option<String>, String)> = Vec::new();
    // Nothing to compare the header against when the body's version is not a
    // string: skip this one check and let the protocol layer report the
    // malformed `_meta` as -32602 (mapped to 400 below), the accurate error.
    if let Some(version) = body["params"]["_meta"][crate::rpc::META_PROTOCOL_VERSION].as_str() {
        checks.push((
            "MCP-Protocol-Version",
            req.header("mcp-protocol-version").map(str::to_string),
            version.to_string(),
        ));
    }
    checks.push((
        "Mcp-Method",
        req.header("mcp-method").map(str::to_string),
        method.clone(),
    ));
    if method == "tools/call" {
        checks.push((
            "Mcp-Name",
            req.header("mcp-name").and_then(decode_mcp_name),
            body["params"]["name"].as_str().unwrap_or("").to_string(),
        ));
    }
    for (header, got, expected) in checks {
        if got.as_deref() != Some(expected.as_str()) {
            // A notification (no `id`) has nowhere to carry a JSON-RPC error:
            // an error object answering no request would be a protocol
            // violation, so the mismatch is reported by the status code alone.
            let Some(id) = body.get("id") else {
                return HttpReply::empty(400);
            };
            let err = crate::rpc::rpc_error_data(
                id.clone(),
                -32020,
                &format!("header {header} missing or does not match the request body"),
                json!({ "header": header, "expected": expected }),
            );
            return HttpReply::json(400, &err);
        }
    }
    dispatch(body, store, None, |v| match v["error"]["code"].as_i64() {
        Some(-32601) => 404,
        Some(-32602 | -32022 | -32020 | -32700) => 400,
        _ => 200,
    })
}

/// `Mcp-Name` is the tool name verbatim, or `=?base64?<b64>?=` when the name is
/// not header-safe. Undecodable input is `None` (a mismatch).
fn decode_mcp_name(raw: &str) -> Option<String> {
    match raw
        .strip_prefix("=?base64?")
        .and_then(|r| r.strip_suffix("?="))
    {
        Some(b64) => String::from_utf8(base64_decode(b64)?).ok(),
        None => Some(raw.to_string()),
    }
}

/// Standard-alphabet base64 with `=` padding. Small enough not to earn a crate.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a') as u32 + 26,
            b'0'..=b'9' => (c - b'0') as u32 + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    }
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let pad = chunk.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || chunk[..4 - pad].contains(&b'=') {
            return None;
        }
        let mut acc: u32 = 0;
        for &c in &chunk[..4 - pad] {
            acc = (acc << 6) | val(c)?;
        }
        acc <<= 6 * pad as u32;
        let b = acc.to_be_bytes();
        out.extend_from_slice(&b[1..4 - pad]);
    }
    Some(out)
}

/// Read the socket request into an [`HttpRequest`], route it, write the reply
/// back. The body-size guard lives here because it needs the stream itself.
fn serve(mut request: Request, store: &Mutex<WorkspaceStore>) {
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
    let req = HttpRequest {
        method: match request.method() {
            Method::Get => HttpMethod::Get,
            Method::Post => HttpMethod::Post,
            Method::Delete => HttpMethod::Delete,
            _ => HttpMethod::Other,
        },
        path: request.url().to_string(),
        headers: request
            .headers()
            .iter()
            .map(|h| {
                (
                    h.field.as_str().as_str().to_string(),
                    h.value.as_str().to_string(),
                )
            })
            .collect(),
        body,
    };
    // Resolve under the lock, respond outside it. A poisoned lock means an
    // earlier request panicked mid-edit; there is no safe state to serve from.
    let reply = route(&req, &mut store.lock().expect("workspace store lock"));
    let mut resp = Response::from_string(reply.body).with_status_code(reply.status);
    if reply.json {
        resp = resp.with_header(json_header());
    }
    if let Some(id) = reply.session_id {
        resp = resp.with_header(session_header(&id));
    }
    let _ = request.respond(resp);
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

/// Header names are static ASCII, so these cannot fail.
fn json_header() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
        .expect("static Content-Type header")
}

fn session_header(id: &str) -> Header {
    Header::from_bytes(&b"Mcp-Session-Id"[..], id.as_bytes()).expect("hex session id header")
}

fn text(code: u16, msg: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(msg.to_string()).with_status_code(code)
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

    fn req(method: HttpMethod, headers: &[(&str, &str)], body: &str) -> HttpRequest {
        HttpRequest {
            method,
            path: "/mcp".into(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: body.to_string(),
        }
    }

    fn modern_body(id: u32, method: &str, params: &str) -> String {
        let sep = if params.is_empty() { "" } else { "," };
        let meta = crate::rpc::modern_meta_json();
        format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{{{params}{sep}{meta}}}}}"#
        )
    }

    fn modern_headers<'a>(method: &'a str, name: Option<&'a str>) -> Vec<(&'a str, &'a str)> {
        let mut h = vec![
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", method),
        ];
        if let Some(n) = name {
            h.push(("Mcp-Name", n));
        }
        h
    }

    fn json(reply: &HttpReply) -> Value {
        serde_json::from_str(&reply.body).unwrap_or_else(|e| panic!("{e}: {}", reply.body))
    }

    #[test]
    fn legacy_flow_is_unchanged() {
        let mut store = WorkspaceStore::new();
        let init = route(
            &req(
                HttpMethod::Post,
                &[],
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
            ),
            &mut store,
        );
        assert_eq!(init.status, 200);
        let sid = init
            .session_id
            .clone()
            .expect("initialize mints a session id");
        assert_eq!(json(&init)["result"]["protocolVersion"], "2025-06-18");

        let missing = route(
            &req(
                HttpMethod::Post,
                &[],
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            ),
            &mut store,
        );
        assert_eq!(missing.status, 404);

        let ok = route(
            &req(
                HttpMethod::Post,
                &[("Mcp-Session-Id", &sid)],
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#,
            ),
            &mut store,
        );
        assert_eq!(ok.status, 200);
        assert!(ok.session_id.is_none(), "the id is only sent on initialize");
        assert!(json(&ok)["result"].get("resultType").is_none());

        let notif = route(
            &req(
                HttpMethod::Post,
                &[("Mcp-Session-Id", &sid)],
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            ),
            &mut store,
        );
        assert_eq!(notif.status, 202);

        assert_eq!(
            route(&req(HttpMethod::Get, &[], ""), &mut store).status,
            405
        );
        assert_eq!(
            route(
                &req(HttpMethod::Delete, &[("Mcp-Session-Id", "nope")], ""),
                &mut store
            )
            .status,
            404
        );
        assert_eq!(
            route(
                &req(HttpMethod::Delete, &[("Mcp-Session-Id", &sid)], ""),
                &mut store
            )
            .status,
            204
        );
        assert!(!store.contains(&sid));
    }

    #[test]
    fn remote_origin_is_forbidden_before_anything_else() {
        let mut store = WorkspaceStore::new();
        let r = route(
            &req(HttpMethod::Post, &[("Origin", "http://evil.com")], "{}"),
            &mut store,
        );
        assert_eq!(r.status, 403);
    }

    #[test]
    fn modern_request_needs_no_session_and_reports_a_workspace() {
        let mut store = WorkspaceStore::new();
        let body = modern_body(
            1,
            "tools/call",
            r#""name":"open_text","arguments":{"text":"x","session":"c"}"#,
        );
        let r = route(
            &req(
                HttpMethod::Post,
                &modern_headers("tools/call", Some("open_text")),
                &body,
            ),
            &mut store,
        );
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(
            r.session_id.is_none(),
            "no Mcp-Session-Id is minted or echoed"
        );
        let v = json(&r);
        assert_eq!(v["result"]["resultType"], "complete");
        let h = crate::rpc::handle_of(&v);
        assert!(store.contains(&h));

        // A stray session header is ignored, not required.
        let body = modern_body(2, "tools/list", "");
        let mut hs = modern_headers("tools/list", None);
        hs.push(("Mcp-Session-Id", "stale"));
        hs.push(("Last-Event-ID", "7"));
        let r = route(&req(HttpMethod::Post, &hs, &body), &mut store);
        assert_eq!(r.status, 200);
    }

    #[test]
    fn each_required_header_is_checked_against_the_body() {
        let mut store = WorkspaceStore::new();
        let body = modern_body(1, "tools/call", r#""name":"help","arguments":{}"#);
        let cases: Vec<(Vec<(&str, &str)>, &str)> = vec![
            (
                vec![("Mcp-Method", "tools/call"), ("Mcp-Name", "help")],
                "MCP-Protocol-Version",
            ),
            (
                vec![
                    ("MCP-Protocol-Version", "2025-11-25"),
                    ("Mcp-Method", "tools/call"),
                    ("Mcp-Name", "help"),
                ],
                "MCP-Protocol-Version",
            ),
            (
                vec![("MCP-Protocol-Version", "2026-07-28"), ("Mcp-Name", "help")],
                "Mcp-Method",
            ),
            (
                vec![
                    ("MCP-Protocol-Version", "2026-07-28"),
                    ("Mcp-Method", "tools/list"),
                    ("Mcp-Name", "help"),
                ],
                "Mcp-Method",
            ),
            (
                vec![
                    ("MCP-Protocol-Version", "2026-07-28"),
                    ("Mcp-Method", "tools/call"),
                ],
                "Mcp-Name",
            ),
            (
                vec![
                    ("MCP-Protocol-Version", "2026-07-28"),
                    ("Mcp-Method", "tools/call"),
                    ("Mcp-Name", "view"),
                ],
                "Mcp-Name",
            ),
        ];
        for (headers, offending) in cases {
            let r = route(&req(HttpMethod::Post, &headers, &body), &mut store);
            assert_eq!(r.status, 400, "{headers:?}");
            let v = json(&r);
            assert_eq!(v["error"]["code"], -32020, "{headers:?}");
            assert_eq!(v["error"]["data"]["header"], offending, "{headers:?}");
            assert_eq!(v["id"], 1);
        }
        // Mcp-Name is only required on tools/call.
        let body = modern_body(2, "tools/list", "");
        let r = route(
            &req(HttpMethod::Post, &modern_headers("tools/list", None), &body),
            &mut store,
        );
        assert_eq!(r.status, 200);
        assert_eq!(store.len(), 0, "list and help never mint a workspace");
    }

    #[test]
    fn mcp_name_accepts_the_base64_sentinel() {
        assert_eq!(decode_mcp_name("help").as_deref(), Some("help"));
        assert_eq!(
            decode_mcp_name("=?base64?aGVscA==?=").as_deref(),
            Some("help")
        );
        assert_eq!(decode_mcp_name("=?base64?aGVsc?="), None, "bad padding");
        assert_eq!(decode_mcp_name("=?base64?@@@@?="), None, "not base64");
        assert_eq!(
            base64_decode("aGVsbG8gd29ybGQ="),
            Some(b"hello world".to_vec())
        );
        assert_eq!(base64_decode(""), Some(Vec::new()));
    }

    #[test]
    fn error_codes_map_to_http_statuses() {
        let mut store = WorkspaceStore::new();
        let unknown = modern_body(1, "no/such", "");
        let r = route(
            &req(HttpMethod::Post, &modern_headers("no/such", None), &unknown),
            &mut store,
        );
        assert_eq!(
            (r.status, json(&r)["error"]["code"].as_i64()),
            (404, Some(-32601))
        );

        let bad_version = r#"{"jsonrpc":"2.0","id":2,"method":"ping","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2031-01-01","io.modelcontextprotocol/clientCapabilities":{}}}}"#;
        let r = route(
            &req(
                HttpMethod::Post,
                &[
                    ("MCP-Protocol-Version", "2031-01-01"),
                    ("Mcp-Method", "ping"),
                ],
                bad_version,
            ),
            &mut store,
        );
        assert_eq!(
            (r.status, json(&r)["error"]["code"].as_i64()),
            (400, Some(-32022))
        );

        let no_caps = r#"{"jsonrpc":"2.0","id":3,"method":"ping","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#;
        let r = route(
            &req(
                HttpMethod::Post,
                &[
                    ("MCP-Protocol-Version", "2026-07-28"),
                    ("Mcp-Method", "ping"),
                ],
                no_caps,
            ),
            &mut store,
        );
        assert_eq!(
            (r.status, json(&r)["error"]["code"].as_i64()),
            (400, Some(-32602))
        );

        // A tool-level failure is still HTTP 200 with isError.
        let body = modern_body(
            4,
            "tools/call",
            r#""name":"view","arguments":{"session":"none","workspace":"0000000000000000ffffffffffffffff"}"#,
        );
        let r = route(
            &req(
                HttpMethod::Post,
                &modern_headers("tools/call", Some("view")),
                &body,
            ),
            &mut store,
        );
        assert_eq!(r.status, 200);
        assert_eq!(json(&r)["result"]["isError"], true);
    }

    #[test]
    fn legacy_parse_error_is_200_with_a_session() {
        // A legacy client that holds its session id gets the JSON-RPC parse
        // error for a malformed body — HTTP 200, because the failure is at the
        // protocol layer, not the transport. (This is what the server answered
        // before it was dual-era, and it stays.)
        let mut store = WorkspaceStore::new();
        let init = route(
            &req(
                HttpMethod::Post,
                &[],
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
            ),
            &mut store,
        );
        let sid = init.session_id.clone().expect("initialize mints a session");
        let r = route(
            &req(HttpMethod::Post, &[("Mcp-Session-Id", &sid)], "{not json"),
            &mut store,
        );
        assert_eq!(r.status, 200, "{}", r.body);
        assert_eq!(json(&r)["error"]["code"], -32700);
        assert!(
            json(&r)["id"].is_null(),
            "no id is recoverable from garbage"
        );
        // A legacy client since 2025-06-18 also sends MCP-Protocol-Version with
        // its own version; that must not be mistaken for the modern era.
        let r = route(
            &req(
                HttpMethod::Post,
                &[
                    ("Mcp-Session-Id", &sid),
                    ("MCP-Protocol-Version", "2025-06-18"),
                ],
                "{not json",
            ),
            &mut store,
        );
        assert_eq!(r.status, 200, "{}", r.body);
        assert_eq!(json(&r)["error"]["code"], -32700);
    }

    #[test]
    fn a_modern_client_with_a_malformed_body_gets_400_not_the_legacy_404() {
        // A malformed body carries no era, but an `MCP-Protocol-Version` header
        // naming the modern version does (legacy clients since 2025-06-18 send
        // the header too, with their own version). So the client gets the
        // modern answer to a bad request — HTTP 400 with the `-32700` body —
        // instead of being mistaken for a legacy client and handed the
        // sessionless 404.
        let mut store = WorkspaceStore::new();
        let r = route(
            &req(
                HttpMethod::Post,
                &[
                    ("MCP-Protocol-Version", "2026-07-28"),
                    ("Mcp-Method", "ping"),
                ],
                "{not json",
            ),
            &mut store,
        );
        assert_eq!(r.status, 400, "{}", r.body);
        assert_eq!(json(&r)["error"]["code"], -32700);
    }

    #[test]
    fn legacy_bad_body_without_a_session_is_404() {
        // Without a known session there is nothing to answer at the protocol
        // layer: garbage gets the same 404 as any other sessionless legacy
        // request, so an unparseable body reveals no more than a valid one.
        let mut store = WorkspaceStore::new();
        for headers in [vec![], vec![("Mcp-Session-Id", "deadbeef")]] {
            let r = route(&req(HttpMethod::Post, &headers, "{not json"), &mut store);
            assert_eq!(r.status, 404, "{headers:?}");
            assert!(r.body.contains("Mcp-Session-Id"), "{}", r.body);
        }
    }

    #[test]
    fn a_modern_notification_with_bad_headers_is_a_bodyless_400() {
        // A notification has no `id`, so a JSON-RPC error object would be
        // answering no request at all: the header mismatch is reported by the
        // status code alone.
        let mut store = WorkspaceStore::new();
        let meta = crate::rpc::modern_meta_json();
        let body = format!(
            r#"{{"jsonrpc":"2.0","method":"notifications/initialized","params":{{{meta}}}}}"#
        );
        let r = route(
            &req(
                HttpMethod::Post,
                &[("MCP-Protocol-Version", "2026-07-28")],
                &body,
            ),
            &mut store,
        );
        assert_eq!(r.status, 400);
        assert!(r.body.is_empty(), "{}", r.body);
        assert!(!r.json, "no JSON-RPC error rides on a notification");
        // With the headers it needs, the same notification is simply accepted.
        let ok = route(
            &req(
                HttpMethod::Post,
                &modern_headers("notifications/initialized", None),
                &body,
            ),
            &mut store,
        );
        assert_eq!(ok.status, 202);
    }

    #[test]
    fn a_legacy_session_status_does_not_echo_the_session_id() {
        // The legacy handle IS the `Mcp-Session-Id` bearer token: the client
        // already holds it, and it must not travel back inside tool output.
        let mut store = WorkspaceStore::new();
        let init = route(
            &req(
                HttpMethod::Post,
                &[],
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
            ),
            &mut store,
        );
        let sid = init.session_id.clone().expect("initialize mints a session");
        let r = route(
            &req(
                HttpMethod::Post,
                &[("Mcp-Session-Id", &sid)],
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"session_status","arguments":{}}}"#,
            ),
            &mut store,
        );
        assert_eq!(r.status, 200, "{}", r.body);
        let v = json(&r);
        assert!(
            v["result"]["structuredContent"]["workspace"].is_null(),
            "{}",
            v["result"]["structuredContent"]
        );
        assert!(!r.body.contains(&sid), "the session id must not leak");
    }

    /// Send one raw HTTP/1.1 request and read the whole response. The write
    /// half is closed after the head: tiny_http drains an unread body when it
    /// drops the request, and the over-cap case below announces a body it never
    /// sends.
    fn raw(addr: std::net::SocketAddr, head: &str) -> String {
        use std::io::Write as _;
        let mut sock = std::net::TcpStream::connect(addr).expect("connect to the test server");
        sock.set_read_timeout(Some(std::time::Duration::from_secs(20)))
            .expect("a read timeout, so a regression fails instead of hanging");
        sock.write_all(head.as_bytes()).expect("write the request");
        sock.shutdown(std::net::Shutdown::Write)
            .expect("half-close the request");
        let mut out = Vec::new();
        sock.read_to_end(&mut out).expect("read the response");
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn serve_over_a_real_socket() {
        // `route` is pure and covered above; this exercises the adapter around
        // it — the tiny_http method/path mapping and the body-size guard that
        // must answer BEFORE reading the body it is refusing.
        let server = Server::http("127.0.0.1:0").expect("bind an ephemeral port");
        let addr = server
            .server_addr()
            .to_ip()
            .expect("an ip address, not a unix socket");
        // The worker reports over a channel rather than being joined: a test
        // that waits on a server thread must never be able to hang the suite,
        // so completion is awaited with a timeout (below) instead.
        let (done, finished) = std::sync::mpsc::channel();
        let _worker = std::thread::spawn(move || {
            let store = Mutex::new(WorkspaceStore::new());
            let mut served = 0usize;
            for r in server.incoming_requests().take(4) {
                serve(r, &store);
                served += 1;
            }
            let _ = done.send(served);
        });

        let head = |req: &str| -> String {
            let resp = raw(addr, req);
            resp.split("\r\n\r\n").next().unwrap_or("").to_string()
        };
        let status = |resp: &str| resp.lines().next().unwrap_or("").to_string();

        let patch = head(
            "PATCH /mcp HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        );
        assert!(status(&patch).starts_with("HTTP/1.1 405"), "{patch}");

        let other = head(
            "POST /other HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        );
        assert!(status(&other).starts_with("HTTP/1.1 404"), "{other}");

        // MAX_BODY + 1, announced and never sent: the guard reads the length,
        // not the stream, so the answer comes back without the body.
        let huge = head(
            "POST /mcp HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 67108865\r\n\r\n",
        );
        assert!(status(&huge).starts_with("HTTP/1.1 413"), "{huge}");

        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#;
        let ok = head(&format!(
            "POST /mcp HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{init}",
            init.len()
        ));
        assert!(status(&ok).starts_with("HTTP/1.1 200"), "{ok}");
        let lower = ok.to_ascii_lowercase();
        assert!(lower.contains("mcp-session-id:"), "{ok}");
        assert!(lower.contains("content-type: application/json"), "{ok}");

        // The `take(4)` loop can under-run (a connection the server never
        // yields), so the count comes back with the signal and names itself.
        let served = finished.recv_timeout(std::time::Duration::from_secs(30));
        assert!(
            served == Ok(4),
            "the server thread should finish having served all 4 requests, got {served:?}"
        );
    }

    #[test]
    fn a_non_string_meta_version_is_a_modern_400_not_a_legacy_404() {
        // The era is decided by the KEY, so a malformed version is a modern
        // request with bad `_meta` — not a legacy one missing its session id.
        let mut store = WorkspaceStore::new();
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":7,"io.modelcontextprotocol/clientCapabilities":{}}}}"#;
        let r = route(
            &req(HttpMethod::Post, &[("Mcp-Method", "ping")], body),
            &mut store,
        );
        assert_eq!(r.status, 400, "{}", r.body);
        assert_eq!(json(&r)["error"]["code"], -32602);
    }
}
