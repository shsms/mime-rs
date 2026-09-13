//! Streamable HTTP transport for the MCP server — a second front end beside
//! stdio (`--mcp`), for hosted / multi-client harnesses. It is dual-era: a
//! legacy client initializes and carries an `Mcp-Session-Id` on every later
//! request, while a 2026-07-28 client is stateless — no session header at all,
//! each request self-describing through its standard headers, with the
//! `workspace` handle it wants to reuse passed as a tool argument in the
//! body. Both eras share the
//! transport-agnostic [`crate::rpc::handle_line`] dispatch and answer with
//! plain JSON: mime never sends a server-initiated message, so there is no SSE
//! stream to open (a spec-valid choice — a server MAY return `application/json`
//! for any request).
//!
//! Security, per the MCP guidance: bind localhost by default and reject a
//! non-local browser `Origin` (anti-DNS-rebinding). Legacy `initialize` mints an
//! unguessable, random `Mcp-Session-Id` and isolates that client's warm
//! sessions under it; the id is the client's bearer token, so every later
//! request MUST carry it (an absent/unknown one is a 404 — re-initialize). A
//! modern request instead names its workspace handle, which is equally
//! unguessable. The workspace store is capped and FIFO-evicted so a flood can't
//! exhaust memory or file descriptors. Requests are served one at a time (like
//! the daemon): simple and race-free, at the cost of head-of-line blocking if a
//! single op runs long.

use crate::rpc::{CallContext, Transport, WorkspaceStore};
use serde_json::{Value, json};
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

/// What the router needs from the body without dispatching it.
struct Peek {
    id: Value,
    method: String,
    /// `params.name` (tools/call).
    name: Option<String>,
    /// Whether `_meta` carries the protocol-version KEY — the era test, the
    /// same one [`crate::rpc::era_of`] applies. A non-string value is still
    /// modern, so a malformed request gets the protocol layer's `-32602`
    /// rather than a legacy "unknown session" 404.
    meta_present: bool,
    /// That key's value when it is a string, for the header comparison.
    meta_version: Option<String>,
}

fn peek(body: &str) -> Option<Peek> {
    let v: Value = serde_json::from_str(body).ok()?;
    Some(Peek {
        id: v.get("id").cloned().unwrap_or(Value::Null),
        method: v
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        name: v["params"]["name"].as_str().map(str::to_string),
        meta_present: v["params"]["_meta"]
            .get(crate::rpc::META_PROTOCOL_VERSION)
            .is_some(),
        meta_version: v["params"]["_meta"][crate::rpc::META_PROTOCOL_VERSION]
            .as_str()
            .map(str::to_string),
    })
}

fn route(req: &HttpRequest, store: &mut WorkspaceStore) -> HttpReply {
    // Anti-DNS-rebinding: a browser-set Origin must be localhost. A missing
    // Origin (curl, an SDK, a CLI harness) is allowed — the attack is browser-only.
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
        // Ending a legacy session needs its id — the id is the bearer token,
        // so this can only drop a session the caller already holds.
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

    let Some(pk) = peek(&req.body) else {
        // Not JSON: let the JSON-RPC layer produce the -32700.
        let mut scratch = WorkspaceStore::new();
        let ctx = CallContext {
            transport: Transport::Http,
            implicit_workspace: None,
        };
        return match crate::rpc::handle_line(&req.body, &mut scratch, &ctx) {
            Some(v) => HttpReply::json(400, &v),
            None => HttpReply::empty(202),
        };
    };

    if pk.meta_present {
        route_modern(req, &pk, store)
    } else {
        route_legacy(req, &pk, store)
    }
}

/// Legacy era: `initialize` mints a session; every other request must carry
/// a known `Mcp-Session-Id`. A missing `MCP-Protocol-Version` header is
/// tolerated (the spec allows it for servers supporting pre-2025-06-18 clients).
fn route_legacy(req: &HttpRequest, pk: &Peek, store: &mut WorkspaceStore) -> HttpReply {
    if pk.method == "initialize" {
        let id = store.mint();
        let ctx = CallContext {
            transport: Transport::Http,
            implicit_workspace: Some(&id),
        };
        let reply = crate::rpc::handle_line(&req.body, store, &ctx);
        let mut out = match reply {
            Some(v) => HttpReply::json(200, &v),
            None => HttpReply::empty(202),
        };
        out.session_id = Some(id);
        return out;
    }
    let Some(id) = req.header("mcp-session-id").filter(|id| store.contains(id)) else {
        return HttpReply::text(
            404,
            "unknown or missing Mcp-Session-Id — send initialize first",
        );
    };
    let id = id.to_string();
    let ctx = CallContext {
        transport: Transport::Http,
        implicit_workspace: Some(&id),
    };
    match crate::rpc::handle_line(&req.body, store, &ctx) {
        Some(v) => HttpReply::json(200, &v),
        None => HttpReply::empty(202),
    }
}

/// Modern era (2026-07-28): stateless. The standard request headers must
/// agree with the body; `Mcp-Session-Id` / `Last-Event-ID` are ignored.
fn route_modern(req: &HttpRequest, pk: &Peek, store: &mut WorkspaceStore) -> HttpReply {
    let mut checks: Vec<(&str, Option<String>, String)> = Vec::new();
    // Nothing to compare the header against when the body's version is not a
    // string: skip this one check and let `handle_line` report the malformed
    // `_meta` as -32602 (mapped to 400 below), which is the accurate error.
    if let Some(version) = pk.meta_version.as_deref() {
        checks.push((
            "MCP-Protocol-Version",
            req.header("mcp-protocol-version").map(str::to_string),
            version.to_string(),
        ));
    }
    checks.push((
        "Mcp-Method",
        req.header("mcp-method").map(str::to_string),
        pk.method.clone(),
    ));
    if pk.method == "tools/call" {
        checks.push((
            "Mcp-Name",
            req.header("mcp-name").and_then(decode_mcp_name),
            pk.name.clone().unwrap_or_default(),
        ));
    }
    for (header, got, expected) in checks {
        if got.as_deref() != Some(expected.as_str()) {
            let err = crate::rpc::rpc_error_data(
                pk.id.clone(),
                -32020,
                &format!("header {header} missing or does not match the request body"),
                json!({ "header": header, "expected": expected }),
            );
            return HttpReply::json(400, &err);
        }
    }
    let ctx = CallContext {
        transport: Transport::Http,
        implicit_workspace: None,
    };
    match crate::rpc::handle_line(&req.body, store, &ctx) {
        Some(v) => {
            let status = match v["error"]["code"].as_i64() {
                Some(-32601) => 404,
                Some(-32602 | -32022 | -32020 | -32700) => 400,
                _ => 200,
            };
            HttpReply::json(status, &v)
        }
        None => HttpReply::empty(202),
    }
}

/// `Mcp-Name` is the tool name verbatim, or `=?base64?<b64>?=` when the name
/// is not header-safe. Undecodable input is `None` (a mismatch).
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

    const META: &str = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}"#;

    fn modern_body(id: u32, method: &str, params: &str) -> String {
        let sep = if params.is_empty() { "" } else { "," };
        format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{{{params}{sep}{META}}}}}"#
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
        let h = v["result"]["structuredContent"]["workspace"]
            .as_str()
            .unwrap()
            .to_string();
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
