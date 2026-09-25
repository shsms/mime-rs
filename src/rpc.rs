//! The MCP protocol layer: JSON-RPC 2.0 framing, the `initialize` handshake and
//! version negotiation. Transport-agnostic — the stdio loop (`mcp::run`) feeds
//! lines to [`handle_line`]; the HTTP front end parses its own body and calls
//! [`handle_request`]. This file never touches a buffer: every buffer-touching
//! tool lives in `mcp`. The two exceptions dispatched here are the workspace
//! tools `open_workspace` and `close_workspace`, because they act on the
//! [`WorkspaceStore`] itself rather than on any one session map.
use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::sync::LazyLock;

use crate::Workspace;
use crate::mcp::{
    Render, ToolOutput, tool_result, tool_text, tool_uses_workspace, tools_call_result,
    validate_args,
};
use serde_json::{Value, json};

/// Every protocol version mime implements, newest first. `2026-07-28` is the
/// stateless "modern" era (per-request `_meta`, no handshake); the rest are the
/// `initialize`-based legacy era. A dual-era server in the spec's sense.
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 5] = [
    "2026-07-28",
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];
pub const PROTOCOL_VERSION: &str = SUPPORTED_PROTOCOL_VERSIONS[0];
/// The stateless era: selected per request by `_meta`, never by `initialize`.
const MODERN_VERSION: &str = "2026-07-28";
/// What `initialize` answers when it cannot echo the client's version.
const LATEST_LEGACY_VERSION: &str = "2025-11-25";

/// The reserved `_meta` keys of the modern era.
pub const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
pub const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
pub const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

/// Freshness hint on `tools/list` and `server/discover`: the catalogue is
/// static for the life of the process, so a long TTL lets clients keep the list
/// in their prompt cache.
pub const LIST_TTL_MS: u64 = 86_400_000;

/// Which of the two protocol eras a request speaks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Era {
    Legacy,
    Modern,
}

/// A request is modern iff its `_meta` carries the protocol-version key.
/// Malformed values still select modern — they then fail validation with a
/// precise error instead of being silently served as legacy.
pub fn era_of(params: &Value) -> Era {
    match params
        .get("_meta")
        .and_then(|m| m.get(META_PROTOCOL_VERSION))
    {
        Some(_) => Era::Modern,
        None => Era::Legacy,
    }
}

/// Validate a modern request's `_meta`: `(code, message, data)` on failure.
fn validate_modern_meta(params: &Value) -> Result<(), (i64, String, Value)> {
    let meta = &params["_meta"];
    let Some(version) = meta.get(META_PROTOCOL_VERSION).and_then(Value::as_str) else {
        return Err((
            -32602,
            format!("_meta.{META_PROTOCOL_VERSION} must be a string"),
            Value::Null,
        ));
    };
    if !meta
        .get(META_CLIENT_CAPABILITIES)
        .is_some_and(Value::is_object)
    {
        return Err((
            -32602,
            format!("_meta.{META_CLIENT_CAPABILITIES} must be an object"),
            Value::Null,
        ));
    }
    if !SUPPORTED_PROTOCOL_VERSIONS.contains(&version) {
        return Err((
            -32022,
            "unsupported protocol version".to_string(),
            json!({ "supported": SUPPORTED_PROTOCOL_VERSIONS, "requested": version }),
        ));
    }
    Ok(())
}

/// Who the client is talking to — the same object in `initialize`'s
/// `serverInfo` and in a modern reply's `_meta`. Built once: it rides on every
/// modern reply, and its fields are compile-time constants.
pub fn server_info() -> Value {
    static INFO: LazyLock<Value> = LazyLock::new(|| {
        json!({
            "name": "mime-rs",
            "version": env!("CARGO_PKG_VERSION"),
            "description": env!("CARGO_PKG_DESCRIPTION"),
        })
    });
    INFO.clone()
}

/// What both handshake-free `server/discover` and the legacy `initialize`
/// answer say about this server, independently of version negotiation.
fn server_identity() -> Value {
    json!({
        "capabilities": { "tools": {} },
        "serverInfo": server_info(),
        "instructions": crate::mcp::instructions(),
    })
}

/// Extend an object result with more keys. Output order is unaffected —
/// serde_json sorts map keys — so the two callers can build theirs in any
/// order.
fn extend(mut base: Value, more: Value) -> Value {
    if let (Some(b), Some(m)) = (base.as_object_mut(), more.as_object()) {
        for (k, v) in m {
            b.insert(k.clone(), v.clone());
        }
    }
    base
}

/// `server/discover`: the handshake-free way to learn what this server speaks.
fn discover_result() -> Value {
    extend(
        server_identity(),
        json!({
            "supportedVersions": SUPPORTED_PROTOCOL_VERSIONS,
            "ttlMs": LIST_TTL_MS,
            "cacheScope": "public",
        }),
    )
}

/// One client's warm-session map: session id -> that session's engine state (a
/// `Workspace` buffer, not the handle-scoped workspace above).
pub type Sessions = HashMap<String, Workspace>;

/// Soft cap on concurrent workspaces: making room for a new one evicts the
/// OLDEST workspace that is neither pinned nor holding unsaved edits, so an
/// open_workspace / initialize flood cannot exhaust memory or file descriptors.
/// It is a bound, not a ceiling — when every workspace holds unsaved work there
/// is nothing evictable left, and the store grows past `WORKSPACE_CAP` (saying
/// so on stderr) rather than dropping an agent's edits to stay under it.
pub const WORKSPACE_CAP: usize = 256;

/// Bounded set of workspaces keyed by an unguessable handle. A legacy HTTP
/// `Mcp-Session-Id` and a modern `workspace` tool argument are the same key.
pub struct WorkspaceStore {
    map: HashMap<String, Sessions>,
    order: VecDeque<String>,
    /// A handle eviction must never take: stdio's implicit workspace, which is
    /// minted first and would otherwise be the first thing an `open_workspace`
    /// flood dropped — taking the agent's warm buffers with it.
    pinned: Option<String>,
}

impl Default for WorkspaceStore {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspaceStore {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            pinned: None,
        }
    }
    /// Protect one handle from eviction (the caller's own implicit workspace).
    /// An unknown handle is ignored — pinning is a safety net, not a create.
    pub fn pin(&mut self, id: &str) {
        if self.map.contains_key(id) {
            self.pinned = Some(id.to_string());
        }
    }
    /// Make room for one more workspace: drop the OLDEST one that is neither
    /// pinned nor holding unsaved work. Best effort, the same policy session
    /// eviction follows (`mcp::evict_for_room`) — when every candidate is
    /// pinned or dirty the store grows past [`WORKSPACE_CAP`] rather than
    /// throwing edits away. Boundedness must not cost an agent its work.
    fn evict_for_room(&mut self) {
        while self.map.len() >= WORKSPACE_CAP {
            let victim = self.order.iter().position(|id| {
                Some(id) != self.pinned.as_ref()
                    && self
                        .map
                        .get(id)
                        .is_some_and(|s| s.values().all(|ws| !ws.is_modified()))
            });
            match victim {
                Some(i) => {
                    if let Some(old) = self.order.remove(i) {
                        self.map.remove(&old);
                    }
                }
                // Only the pinned workspace and workspaces with unsaved edits
                // are left — grow rather than drop any of them, and say so on
                // stderr (once per insert that leaves the store over the cap)
                // so an operator can see the bound give way.
                None => {
                    let len = self.map.len() + 1;
                    eprintln!(
                        "mime: workspace store holds {len} workspaces, over the cap of \
                         {WORKSPACE_CAP} — none is evictable (unsaved edits or pinned)"
                    );
                    break;
                }
            }
        }
    }
    /// Mint a fresh EMPTY workspace (what `open_workspace` and a legacy
    /// `initialize` hand out), making room first.
    pub fn mint(&mut self) -> String {
        self.insert(Sessions::new())
    }
    /// Take an already-populated session map into the store under a freshly
    /// minted handle. This is how a handle-free modern-HTTP call keeps the warm
    /// state it created: the map is built outside the store and only lands in
    /// it — earning a handle — once the call has left something in it.
    pub fn insert(&mut self, sessions: Sessions) -> String {
        self.evict_for_room();
        let id = new_handle();
        self.order.push_back(id.clone());
        self.map.insert(id.clone(), sessions);
        id
    }
    pub fn contains(&self, id: &str) -> bool {
        self.map.contains_key(id)
    }
    pub fn get_mut(&mut self, id: &str) -> Option<&mut Sessions> {
        self.map.get_mut(id)
    }
    /// Drop a workspace and every session in it (unsaved edits included).
    pub fn remove(&mut self, id: &str) -> bool {
        self.order.retain(|x| x != id);
        if self.pinned.as_deref() == Some(id) {
            self.pinned = None;
        }
        self.map.remove(id).is_some()
    }
    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// A 128-bit random handle (hex). The handle is the client's bearer token for
/// its warm state, so it must not be guessable — read it from the OS CSPRNG,
/// failing closed if that is somehow unavailable.
pub fn new_handle() -> String {
    let mut buf = [0u8; 16];
    match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)) {
        Ok(()) => buf.iter().map(|b| format!("{b:02x}")).collect(),
        Err(e) => {
            eprintln!("mime: cannot read /dev/urandom for a workspace handle: {e}");
            std::process::exit(1);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transport {
    Stdio,
    Http,
}

/// What the transport knows about a request that the JSON-RPC body does not.
/// `implicit_workspace` is where a call without a `workspace` argument lands:
/// stdio's startup workspace, or the legacy HTTP session named by
/// `Mcp-Session-Id`. `None` (modern HTTP) means "mint a fresh one".
pub struct CallContext<'a> {
    pub transport: Transport,
    pub implicit_workspace: Option<&'a str>,
}

/// Parse one JSON-RPC request line and dispatch it. Returns `Some(response)`
/// for requests (those with an `id`) and `None` for notifications. A transport
/// that has already parsed the body (the HTTP front end reads its era from it)
/// calls [`handle_request`] directly instead of re-serialising it.
pub fn handle_line(line: &str, store: &mut WorkspaceStore, ctx: &CallContext) -> Option<Value> {
    match serde_json::from_str(line) {
        Ok(req) => handle_request(req, store, ctx),
        Err(e) => Some(parse_error(&e)),
    }
}

/// The `-32700` reply for a body that is not JSON. An id cannot be recovered
/// from unparseable input, so it is reported against null.
pub fn parse_error(e: &serde_json::Error) -> Value {
    rpc_error(Value::Null, -32700, &format!("parse error: {e}"))
}

/// Dispatch one already-parsed JSON-RPC request. See [`handle_line`].
pub fn handle_request(req: Value, store: &mut WorkspaceStore, ctx: &CallContext) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    // No `id` => a notification: act on it but never reply.
    let is_notification = id.is_none();
    let err_id = || id.clone().unwrap_or(Value::Null);

    // A modern request must carry a well-formed, supported `_meta` before it is
    // dispatched; a malformed notification is still silently dropped.
    let era = era_of(&params);
    if era == Era::Modern
        && let Err((code, message, data)) = validate_modern_meta(&params)
    {
        return (!is_notification).then(|| rpc_error_data(err_id(), code, &message, data));
    }

    let result = match method {
        "initialize" => initialize_result(&params),
        // Pure notification — nothing to do, no response.
        "notifications/initialized" | "initialized" => return None,
        "ping" => json!({}),
        "server/discover" => discover_result(),
        "tools/list" => crate::mcp::tools_list_result(ctx.transport),
        "tools/call" => tools_call(&params, store, ctx, era),
        other => {
            if is_notification {
                eprintln!("mime-mcp: ignoring unknown notification {other}");
                return None;
            }
            return Some(rpc_error(err_id(), -32601, "method not found"));
        }
    };
    reply(id, is_notification, shape_result(era, result))
}

/// Dispatch `tools/call`: the two workspace tools act on the store itself;
/// everything else runs against one resolved workspace's session map (or a
/// throwaway map for tools that never touch warm state: git_*, help).
///
/// A handle-free call (modern HTTP, no `workspace` argument) runs against a
/// fresh session map the store does NOT hold, and that map is inserted — which
/// is what mints this call's handle — only if the tool left warm state in it. A
/// read-only poll (`session_status` on a bare modern request, say) therefore
/// creates nothing: no handle to report, and no unreachable workspace piling up
/// on every poll until the cap evicted the live ones.
///
/// Whenever the call HAS a workspace the handle is reported (to the modern HTTP
/// client only), on a failure too: a call that failed after auto-opening a file
/// left warm state, and its own error text tells the client to come back to it.
/// (Only a tool with a structured value of its own — run_program, grep,
/// session_status … — carries the handle in `structuredContent`; a failing one
/// that supplies its own failure JSON keeps it and gets `workspace` alongside
/// the keys it already wrote. The text-only tools carry no structured value at
/// all: Claude Code renders one in preference to the text, so even `{}` would
/// hide their prose.)
fn tools_call(params: &Value, store: &mut WorkspaceStore, ctx: &CallContext, era: Era) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    // Borrowed, not cloned: the arguments can be a whole buffer's worth of
    // text, and `mcp::tools_call_result` makes the one copy that is needed (it
    // rewrites alias spellings in place).
    let no_args = json!({});
    let args = params.get("arguments").unwrap_or(&no_args);
    // A tool this transport does not list is not callable on it either.
    if !crate::mcp::tool_listed(name, ctx.transport) {
        return tool_text(format!("unknown tool: {name}"), true);
    }
    // The two workspace tools act on the store, not on a session map, so they
    // never pass through `mcp::tools_call_result`.
    if matches!(name, "open_workspace" | "close_workspace") {
        let out = match validate_args(name, args) {
            Err(m) => ToolOutput::error(m),
            Ok(()) => {
                if name == "open_workspace" {
                    open_workspace(store)
                } else {
                    close_workspace(args, store, ctx)
                }
            }
        };
        return tool_result(out);
    }
    if !tool_uses_workspace(name) {
        let mut scratch = Sessions::new();
        return tool_result(tools_call_result(params, &mut scratch, None));
    }
    let resolved =
        match resolve_workspace(args.get("workspace").and_then(Value::as_str), store, ctx) {
            Ok(h) => h,
            Err(m) => return tool_text(m, true),
        };
    // Only the modern HTTP client is handed its workspace back: it has no
    // session header and a bare call runs in a fresh workspace, so without the
    // handle it could never return to those warm buffers. Stdio and the legacy
    // session header already know where they are — and the legacy id is a
    // bearer token, which must not surface in tool output at all.
    let reports = era == Era::Modern && ctx.transport == Transport::Http;
    let (mut out, handle) = match resolved {
        Some(h) => {
            // Resolution already proved the handle is present; treat a miss as
            // a tool error anyway, so no reachable panic can take the server
            // down with it.
            let Some(sessions) = store.get_mut(&h) else {
                return tool_text(unknown_workspace(&h), true);
            };
            let out = tools_call_result(params, sessions, reports.then_some(h.as_str()));
            (out, Some(h))
        }
        // Lazy insertion: run against a map that is not in the store, and keep
        // it — minting this call's handle — only if the tool left something
        // warm in it. A `session_status` poll opens nothing, so nothing is
        // kept.
        None => {
            let mut fresh = Sessions::new();
            let out = tools_call_result(params, &mut fresh, None);
            let handle = (!fresh.is_empty()).then(|| store.insert(fresh));
            (out, handle)
        }
    };
    let report = if reports { handle.as_deref() } else { None };
    if let Some(h) = report
        && let Some(map) = out.structured.as_mut().and_then(Value::as_object_mut)
    {
        // The handle is machine-readable too, so a client need not parse it
        // back out of the text. Nothing is padded on to carry it: the handle
        // joins whatever structured value the tool itself supplied — its
        // result, or its own failure JSON. A text-only tool has none and
        // reports the handle on the trailing line below only; a prose-rendered
        // tool with data (grep, outline) gets both.
        map.insert("workspace".to_string(), json!(h));
    }
    // Text and structured value must keep saying the same thing: a tool whose
    // text IS its JSON re-renders it; prose gets the handle as a trailing line.
    out.rerender();
    if let (Some(h), Render::Prose) = (report, out.render) {
        out.text = format!("{}\nworkspace: {h}", out.text);
    }
    tool_result(out)
}

/// The `open_workspace` dispatch: its whole reply IS the handle it minted, so
/// there is never anything to report on top of it.
fn open_workspace(store: &mut WorkspaceStore) -> ToolOutput {
    let h = store.mint();
    ToolOutput::with(
        format!("workspace: {h}"),
        json!({ "workspace": h }),
        Render::Prose,
    )
}

/// The `close_workspace` dispatch: it acts on the store, not on a session map.
fn close_workspace(args: &Value, store: &mut WorkspaceStore, ctx: &CallContext) -> ToolOutput {
    let Some(h) = args.get("workspace").and_then(Value::as_str) else {
        return ToolOutput::error("close_workspace needs `workspace`".into());
    };
    // Guards stdio, should close_workspace ever be listed there.
    if ctx.transport == Transport::Stdio && ctx.implicit_workspace == Some(h) {
        return ToolOutput::error(
            "cannot close the stdio default workspace — close_session drops one session".into(),
        );
    }
    if store.remove(h) {
        ToolOutput::from(format!("workspace {h} closed"))
    } else {
        ToolOutput::error(unknown_workspace(h))
    }
}

fn unknown_workspace(h: &str) -> String {
    format!(
        "unknown workspace {h} — it was closed or evicted; open_workspace (or, on the \
         stateless HTTP protocol, any stateful call without a handle) starts a new one; \
         on stdio omit the argument"
    )
}

/// The spec's resolution table: an explicit handle must exist; otherwise the
/// transport's implicit workspace (which must also still exist — an unpinned
/// one can have been evicted); otherwise (modern HTTP) none at all.
///
/// `Ok(None)` means "run against a fresh map": the call has no workspace yet,
/// and gets one only if it leaves warm state behind (see [`tools_call`]).
fn resolve_workspace(
    explicit: Option<&str>,
    store: &WorkspaceStore,
    ctx: &CallContext,
) -> Result<Option<String>, String> {
    match explicit.or(ctx.implicit_workspace) {
        Some(h) if store.contains(h) => Ok(Some(h.to_string())),
        Some(h) => Err(unknown_workspace(h)),
        None => Ok(None),
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

/// A JSON-RPC error with an optional `data` payload. A `Null` payload leaves
/// the `data` key off entirely, so legacy error bytes are unchanged.
pub fn rpc_error_data(id: Value, code: i64, message: &str, data: Value) -> Value {
    let mut e = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    });
    if !data.is_null() {
        e["error"]["data"] = data;
    }
    e
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    rpc_error_data(id, code, message, Value::Null)
}

/// The `_meta` object of a well-formed modern request, as the JSON-body
/// fragment the transport tests splice into a request line. Built from the real
/// key constants so a renamed key breaks compilation, not assertions.
#[cfg(test)]
pub(crate) fn modern_meta_json() -> String {
    format!(
        r#""_meta":{{"{META_PROTOCOL_VERSION}":"{MODERN_VERSION}","{META_CLIENT_CAPABILITIES}":{{}}}}"#
    )
}

/// The last step for every successful result. Modern requests get the
/// `resultType` and `serverInfo` envelope; legacy results are returned
/// untouched (byte-identical to before the dual-era work).
fn shape_result(era: Era, mut result: Value) -> Value {
    if era == Era::Modern {
        result["resultType"] = json!("complete");
        // Index-assignment on a non-object would silently drop the serverInfo,
        // so make sure `_meta` is one first.
        if !result["_meta"].is_object() {
            result["_meta"] = json!({});
        }
        result["_meta"][META_SERVER_INFO] = server_info();
    }
    result
}

fn initialize_result(params: &Value) -> Value {
    // Echo the client's version when it is a legacy one we implement; an
    // unknown or absent one — or the modern version, which has no handshake —
    // gets our newest legacy version.
    let version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .filter(|v| SUPPORTED_PROTOCOL_VERSIONS.contains(v) && *v != MODERN_VERSION)
        .unwrap_or(LATEST_LEGACY_VERSION);
    extend(server_identity(), json!({ "protocolVersion": version }))
}

/// The workspace handle a prose tool reports on its trailing line — for the
/// tests of both transports.
#[cfg(test)]
pub(crate) fn handle_of(reply: &Value) -> String {
    let text = reply["result"]["content"][0]["text"].as_str().unwrap_or("");
    text.lines()
        .last()
        .and_then(|l| l.strip_prefix("workspace: "))
        .unwrap_or_else(|| panic!("handle reported: {text}"))
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdio_ctx(handle: &str) -> CallContext<'_> {
        CallContext {
            transport: Transport::Stdio,
            implicit_workspace: Some(handle),
        }
    }

    fn call(line: &str, store: &mut WorkspaceStore, ctx: &CallContext) -> Value {
        handle_line(line, store, ctx).expect("a request gets a reply")
    }

    fn text_of(reply: &Value) -> String {
        reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    #[test]
    fn mcp_handshake_is_spec_conformant() {
        let mut store = WorkspaceStore::new();
        let h = store.mint();
        let ctx = stdio_ctx(&h);

        // initialize: a supported version is echoed; capabilities +
        // instructions present.
        let init = call(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
            &mut store,
            &ctx,
        );
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert!(init["result"]["capabilities"]["tools"].is_object());
        assert!(
            init["result"]["instructions"]
                .as_str()
                .unwrap()
                .contains("mime-rs")
        );

        // an unsupported version clamps to our latest instead of being echoed.
        let old = call(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#,
            &mut store,
            &ctx,
        );
        assert_eq!(old["result"]["protocolVersion"], "2025-11-25");

        // notifications/initialized is a pure notification — no reply.
        assert!(
            handle_line(
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                &mut store,
                &ctx
            )
            .is_none()
        );

        // ping → empty result object.
        assert!(
            call(
                r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#,
                &mut store,
                &ctx
            )["result"]
                .is_object()
        );

        // tools/list → every tool carries an inputSchema and annotations.
        let list = call(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#,
            &mut store,
            &ctx,
        );
        let tools = list["result"]["tools"].as_array().unwrap();
        assert!(tools.iter().any(|t| t["name"] == "view"));
        assert!(
            tools
                .iter()
                .all(|t| t["inputSchema"].is_object() && t["annotations"].is_object())
        );

        // tools/call → a result envelope (isError:false on success).
        call(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"open_text","arguments":{"text":"hi\n","session":"c"}}}"#,
            &mut store,
            &ctx,
        );
        let view = call(
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"view","arguments":{"session":"c"}}}"#,
            &mut store,
            &ctx,
        );
        assert_eq!(view["result"]["isError"], false);

        // unknown method → a JSON-RPC error (-32601), not a result.
        let err = call(
            r#"{"jsonrpc":"2.0","id":6,"method":"no/such"}"#,
            &mut store,
            &ctx,
        );
        assert_eq!(err["error"]["code"], -32601);

        // unparseable input → parse error (-32700) against a null id.
        let parse = call("{not json", &mut store, &ctx);
        assert_eq!(parse["error"]["code"], -32700);
    }

    #[test]
    fn handles_are_random_hex_and_distinct() {
        let a = new_handle();
        let b = new_handle();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn store_caps_at_workspace_cap_and_evicts_oldest() {
        let mut s = WorkspaceStore::new();
        let first = s.mint();
        for _ in 0..WORKSPACE_CAP + 4 {
            s.mint();
        }
        assert_eq!(s.len(), WORKSPACE_CAP);
        assert!(!s.contains(&first), "oldest evicted");
        assert!(!s.remove(&first));
    }

    /// One session map holding a single buffer with an unsaved edit.
    fn dirty_sessions() -> Sessions {
        let mut ws = Workspace::new(Box::new(crate::Buffer::from_string(
            "d".to_string(),
            "x\n".to_string(),
        )));
        ws.run("(insert \"y\")").expect("the edit runs");
        assert!(ws.is_modified(), "the buffer must read as modified");
        let mut sessions = Sessions::new();
        sessions.insert("c".to_string(), ws);
        sessions
    }

    #[test]
    fn eviction_skips_workspaces_with_unsaved_edits() {
        // Boundedness must not cost an agent its work: the oldest workspace is
        // the eviction candidate, but one holding an unsaved edit is passed
        // over for the oldest CLEAN one.
        let mut store = WorkspaceStore::new();
        let dirty = store.insert(dirty_sessions());
        let clean = store.mint();
        for _ in 0..WORKSPACE_CAP {
            store.mint();
        }
        assert_eq!(store.len(), WORKSPACE_CAP);
        assert!(
            store.contains(&dirty),
            "a workspace with unsaved edits is never evicted"
        );
        assert!(
            !store.contains(&clean),
            "the oldest CLEAN workspace goes in its place"
        );

        // And when nothing qualifies, the store grows past the cap rather than
        // dropping work — best effort, the policy session eviction follows too.
        let mut all_dirty = WorkspaceStore::new();
        for _ in 0..WORKSPACE_CAP + 4 {
            all_dirty.insert(dirty_sessions());
        }
        assert!(
            all_dirty.len() > WORKSPACE_CAP,
            "unsaved work outranks the cap: {}",
            all_dirty.len()
        );
    }

    #[test]
    fn the_pinned_workspace_survives_eviction() {
        let mut s = WorkspaceStore::new();
        let default = s.mint();
        s.pin(&default);
        for _ in 0..WORKSPACE_CAP + 5 {
            s.mint();
        }
        assert!(s.contains(&default), "the pinned workspace must survive");
        assert_eq!(s.len(), WORKSPACE_CAP);
    }

    #[test]
    fn open_workspace_flood_never_evicts_the_stdio_default() {
        let mut store = WorkspaceStore::new();
        let h = store.mint();
        store.pin(&h);
        let ctx = stdio_ctx(&h);
        call(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"open_text","arguments":{"text":"warm\n","session":"c"}}}"#,
            &mut store,
            &ctx,
        );
        for _ in 0..WORKSPACE_CAP + 5 {
            call(
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"open_workspace","arguments":{}}}"#,
                &mut store,
                &ctx,
            );
        }
        let v = call(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"view","arguments":{"session":"c"}}}"#,
            &mut store,
            &ctx,
        );
        assert_eq!(v["result"]["isError"], false, "{}", text_of(&v));
        assert!(text_of(&v).contains("warm"), "{}", text_of(&v));
    }

    #[test]
    fn an_evicted_implicit_workspace_is_a_tool_error_not_a_panic() {
        let mut store = WorkspaceStore::new();
        // Unpinned (the legacy-HTTP shape): a flood can evict it.
        let h = store.mint();
        let ctx = stdio_ctx(&h);
        for _ in 0..WORKSPACE_CAP + 5 {
            store.mint();
        }
        assert!(!store.contains(&h), "the unpinned workspace was evicted");
        let r = call(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"view","arguments":{"session":"c"}}}"#,
            &mut store,
            &ctx,
        );
        assert_eq!(r["result"]["isError"], true);
        assert!(text_of(&r).contains("unknown workspace"), "{}", text_of(&r));
    }

    #[test]
    fn stdio_calls_land_in_the_implicit_workspace() {
        let mut store = WorkspaceStore::new();
        let h = store.mint();
        let ctx = stdio_ctx(&h);
        call(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"open_text","arguments":{"text":"hi\n","session":"c"}}}"#,
            &mut store,
            &ctx,
        );
        let status = call(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"session_status","arguments":{}}}"#,
            &mut store,
            &ctx,
        );
        let json: Value = serde_json::from_str(&text_of(&status)).unwrap();
        // The session landed in the implicit workspace; the handle itself is
        // NOT echoed on stdio (see
        // `legacy_and_stdio_session_status_do_not_echo_the_handle`).
        assert!(json["workspace"].is_null(), "{json}");
        assert_eq!(json["sessions"][0]["id"], "c");
    }

    #[test]
    fn an_unknown_explicit_workspace_is_a_tool_error() {
        let mut store = WorkspaceStore::new();
        let h = store.mint();
        let ctx = stdio_ctx(&h);
        let r = call(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"open_text","arguments":{"text":"x","workspace":"deadbeef"}}}"#,
            &mut store,
            &ctx,
        );
        assert_eq!(r["result"]["isError"], true);
        assert!(
            text_of(&r).contains("unknown workspace deadbeef"),
            "{}",
            text_of(&r)
        );
    }

    #[test]
    fn open_workspace_isolates_session_names() {
        // open_workspace only means anything on HTTP, where several agents
        // share one process.
        let mut store = WorkspaceStore::new();
        let h = store.mint();
        let ctx = CallContext {
            transport: Transport::Http,
            implicit_workspace: Some(&h),
        };
        call(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"open_text","arguments":{"text":"first\n","session":"c"}}}"#,
            &mut store,
            &ctx,
        );
        let opened = call(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"open_workspace","arguments":{}}}"#,
            &mut store,
            &ctx,
        );
        let h2 = text_of(&opened)
            .trim_start_matches("workspace: ")
            .trim()
            .to_string();
        assert_eq!(h2.len(), 32, "{}", text_of(&opened));
        // The handle is also machine-readable, and the structured value says
        // exactly what the outputSchema declares: the one `workspace` key.
        let structured = &opened["result"]["structuredContent"];
        assert_eq!(structured["workspace"], h2);
        let keys: Vec<&String> = structured.as_object().expect("an object").keys().collect();
        assert_eq!(keys, ["workspace"]);
        assert_eq!(structured["workspace"].as_str().unwrap().len(), 32);
        let req = format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"open_text","arguments":{{"text":"second\n","session":"c","workspace":"{h2}"}}}}}}"#
        );
        call(&req, &mut store, &ctx);
        let v1 = call(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"view","arguments":{"session":"c"}}}"#,
            &mut store,
            &ctx,
        );
        assert!(text_of(&v1).contains("first"));
        let req = format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{{"name":"view","arguments":{{"session":"c","workspace":"{h2}"}}}}}}"#
        );
        let v2 = call(&req, &mut store, &ctx);
        assert!(text_of(&v2).contains("second"));
    }

    #[test]
    fn close_workspace_is_unknown_on_stdio_and_drops_a_minted_one_on_http() {
        // close_workspace is not listed on stdio at all.
        let mut store = WorkspaceStore::new();
        let h = store.mint();
        let ctx = stdio_ctx(&h);
        let req = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"close_workspace","arguments":{{"workspace":"{h}"}}}}}}"#
        );
        let r = call(&req, &mut store, &ctx);
        assert_eq!(r["result"]["isError"], true);
        assert!(text_of(&r).contains("unknown tool"), "{}", text_of(&r));

        // On HTTP, where it IS listed: drops a minted workspace.
        let ctx = CallContext {
            transport: Transport::Http,
            implicit_workspace: None,
        };
        let h2 = store.mint();
        let req = format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"close_workspace","arguments":{{"workspace":"{h2}"}}}}}}"#
        );
        let r = call(&req, &mut store, &ctx);
        assert_eq!(r["result"]["isError"], false);
        assert!(!store.contains(&h2));
        let req = format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"view","arguments":{{"session":"c","workspace":"{h2}"}}}}}}"#
        );
        let r = call(&req, &mut store, &ctx);
        assert!(text_of(&r).contains("unknown workspace"));
    }

    #[test]
    fn a_missing_implicit_workspace_mints_one_per_call() {
        // The modern-HTTP shape: no implicit workspace, no argument -> each
        // call runs in a fresh map, and each is kept (open_text left a session
        // in it), so two handle-free calls leave two workspaces.
        let mut store = WorkspaceStore::new();
        let ctx = CallContext {
            transport: Transport::Http,
            implicit_workspace: None,
        };
        call(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"open_text","arguments":{"text":"x","session":"c"}}}"#,
            &mut store,
            &ctx,
        );
        call(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"open_text","arguments":{"text":"y","session":"c"}}}"#,
            &mut store,
            &ctx,
        );
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn a_handle_free_read_only_call_leaves_no_workspace_behind() {
        // A modern-HTTP poll with no handle runs against a fresh map, opens
        // nothing in it, and so must not leave a workspace behind — nor report
        // a handle that leads to no warm state.
        let mut store = WorkspaceStore::new();
        let ctx = CallContext {
            transport: Transport::Http,
            implicit_workspace: None,
        };
        let r = call(
            &modern(1, "tools/call", r#""name":"session_status","arguments":{}"#),
            &mut store,
            &ctx,
        );
        assert_eq!(r["result"]["isError"], false, "{}", text_of(&r));
        assert_eq!(store.len(), 0, "an empty fresh map is never inserted");
        // Nothing is reported on top of the tool's own output: no trailing
        // `workspace:` line, and no handle merged into structuredContent.
        // (session_status's own JSON names the workspace it ran in — that is
        // the tool's payload, not the report, and the text is its JSON.)
        assert!(!text_of(&r).contains("\nworkspace:"), "{}", text_of(&r));
        let status: Value = serde_json::from_str(&text_of(&r)).expect("session_status is JSON");
        assert_eq!(
            r["result"]["structuredContent"], status,
            "structuredContent is the tool's value, unmerged"
        );
        assert!(status["sessions"].as_array().unwrap().is_empty());

        // A call that DOES open a session keeps its workspace and reports it.
        let kept = call(
            &modern(
                2,
                "tools/call",
                r#""name":"open_text","arguments":{"text":"x","session":"c"}"#,
            ),
            &mut store,
            &ctx,
        );
        assert_eq!(store.len(), 1);
        assert_eq!(handle_of(&kept).len(), 32);
    }

    #[test]
    fn prose_tools_carry_no_structured_content() {
        // A prose tool declares no outputSchema and answers with text alone, in
        // every era: clients render `structuredContent` in preference to the
        // text, so even an empty `{}` would hide what the tool wrote. help and
        // close_workspace are prose too.
        let mut store = WorkspaceStore::new();
        let h = store.mint();
        let ctx = stdio_ctx(&h);
        for body in [
            r#""name":"open_text","arguments":{"text":"hi\n","session":"c"}"#,
            r#""name":"view","arguments":{"session":"c"}"#,
            r#""name":"close_session","arguments":{"session":"c"}"#,
        ] {
            let req =
                format!(r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{{body}}}}}"#);
            let r = call(&req, &mut store, &ctx);
            assert!(
                r["result"].get("structuredContent").is_none(),
                "{body}: {}",
                r["result"]
            );
        }
        // close_workspace is prose as well — it only lists on HTTP.
        let http_ctx = CallContext {
            transport: Transport::Http,
            implicit_workspace: Some(&h),
        };
        let h2 = store.mint();
        let req = format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"close_workspace","arguments":{{"workspace":"{h2}"}}}}}}"#
        );
        let r = call(&req, &mut store, &http_ctx);
        assert_eq!(r["result"]["isError"], false, "{}", text_of(&r));
        assert!(r["result"].get("structuredContent").is_none());

        let help = call(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"help","arguments":{}}}"#,
            &mut store,
            &ctx,
        );
        assert!(help["result"].get("structuredContent").is_none());
    }

    #[test]
    fn error_results_carry_no_default_structured_content() {
        // "MUST provide structured results that conform" is about RESULTS: a
        // tool error has no structured value to give, and forcing `{}` on it
        // would break the tool's own `required` keys. So an error carries
        // structuredContent only when the tool itself supplied one — and no
        // handle is merged either, since there is nothing to come back to.
        let mut store = WorkspaceStore::new();
        let h = store.mint();
        let ctx = stdio_ctx(&h);
        let miss = call(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"view","arguments":{"session":"nope"}}}"#,
            &mut store,
            &ctx,
        );
        assert_eq!(miss["result"]["isError"], true, "{}", text_of(&miss));
        assert!(miss["result"].get("structuredContent").is_none());

        // run_program's failure JSON IS its structured value (ok:false) — the
        // tool supplied it, so it rides.
        call(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"open_text","arguments":{"text":"x\n","session":"c"}}}"#,
            &mut store,
            &ctx,
        );
        let boom = call(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"run_program","arguments":{"session":"c","program":"(error \"boom\")"}}}"#,
            &mut store,
            &ctx,
        );
        assert_eq!(boom["result"]["isError"], true, "{}", text_of(&boom));
        assert_eq!(boom["result"]["structuredContent"]["ok"], false);

        // Modern HTTP: a failed call on a held workspace still REPORTS it — the
        // client's warm state is right there — but the report is the only
        // structured key it gets; the `{}` default stays off an error.
        let http = CallContext {
            transport: Transport::Http,
            implicit_workspace: None,
        };
        let opened = call(
            &modern(
                4,
                "tools/call",
                r#""name":"open_text","arguments":{"text":"x","session":"c"}"#,
            ),
            &mut store,
            &http,
        );
        let h = handle_of(&opened);
        let body = format!(r#""name":"view","arguments":{{"session":"nope","workspace":"{h}"}}"#);
        let r = call(&modern(5, "tools/call", &body), &mut store, &http);
        assert_eq!(r["result"]["isError"], true, "{}", text_of(&r));
        assert!(
            r["result"].get("structuredContent").is_none(),
            "a prose tool carries no structured value: {}",
            r["result"]
        );
        assert!(
            text_of(&r).ends_with(&format!("workspace: {h}")),
            "{}",
            text_of(&r)
        );
        assert!(store.contains(&h), "the caller's workspace survives");
    }

    #[test]
    fn a_failed_handle_free_call_that_opened_a_file_keeps_it_and_reports_the_handle() {
        // `replace_text {path, …}` auto-opens the file and only then finds no
        // match: the call failed, but it left a warm buffer the client can
        // inspect, fix and save — and its own error text tells the client to
        // come back to it. So the workspace is kept and its handle IS reported;
        // dropping either would strand an open buffer nobody can reach.
        let pid = std::process::id();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("mime-lazy-{pid}"));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "hello\n").unwrap();

        let mut store = WorkspaceStore::new();
        let ctx = CallContext {
            transport: Transport::Http,
            implicit_workspace: None,
        };
        let body = format!(
            r#""name":"replace_text","arguments":{{"path":"{}","pattern":"zzz-no-such-text","replacement":"x"}}"#,
            file.display()
        );
        let r = call(&modern(1, "tools/call", &body), &mut store, &ctx);
        assert_eq!(r["result"]["isError"], true, "{}", text_of(&r));
        assert_eq!(store.len(), 1, "the file it opened stays warm");
        let h = handle_of(&r);
        assert_eq!(h.len(), 32);
        assert!(store.contains(&h), "and the reported handle reaches it");
        // A prose tool's error carries no structured value either.
        assert!(r["result"].get("structuredContent").is_none());
        assert!(
            text_of(&r).ends_with(&format!("workspace: {h}")),
            "{}",
            text_of(&r)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_handle_free_run_program_keeps_its_own_failure_json_beside_the_handle() {
        // The sibling case: run_program answers a failure with structured JSON
        // of its own (`ok: false` and the error). The handle joins that JSON
        // instead of replacing it — an error is never padded with `{}`, but it
        // is never stripped of what the tool actually said either.
        let pid = std::process::id();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("mime-lazy-failed-json-{pid}"));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "hello\n").unwrap();

        let mut store = WorkspaceStore::new();
        let ctx = CallContext {
            transport: Transport::Http,
            implicit_workspace: None,
        };
        let before = store.len();
        let body = format!(
            r#""name":"run_program","arguments":{{"path":"{}","program":"(error \"boom\")"}}"#,
            file.display()
        );
        let r = call(&modern(1, "tools/call", &body), &mut store, &ctx);
        assert_eq!(r["result"]["isError"], true, "{}", text_of(&r));
        let s = &r["result"]["structuredContent"];
        assert_eq!(s["ok"], false, "the tool's own failure JSON survives: {s}");
        let h = s["workspace"]
            .as_str()
            .unwrap_or_else(|| panic!("handle reported alongside it: {}", r["result"]))
            .to_string();
        assert_eq!(h.len(), 32);
        assert_eq!(store.len(), before + 1, "the file it opened stays warm");
        assert!(store.contains(&h), "and the reported handle reaches it");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_handle_free_call_that_created_nothing_reports_none() {
        // The other half: a handle-free call that fails before opening anything
        // has no workspace at all. Nothing is inserted, so there is no handle
        // to report — and no unreachable workspace counting against
        // WORKSPACE_CAP.
        let mut store = WorkspaceStore::new();
        let ctx = CallContext {
            transport: Transport::Http,
            implicit_workspace: None,
        };
        let r = call(
            &modern(
                1,
                "tools/call",
                r#""name":"view","arguments":{"session":"nope"}"#,
            ),
            &mut store,
            &ctx,
        );
        assert_eq!(r["result"]["isError"], true, "{}", text_of(&r));
        assert!(r["result"].get("structuredContent").is_none());
        assert!(!text_of(&r).contains("workspace:"), "{}", text_of(&r));
        assert_eq!(store.len(), 0, "nothing warm, nothing kept");
    }

    #[test]
    fn a_handle_free_session_status_reports_a_null_workspace() {
        // The call ran against a fresh map and opened nothing, so it never
        // earned a handle: the `workspace` session_status names in its own JSON
        // is null rather than a handle leading nowhere. Its text IS that JSON,
        // so both say it.
        let mut store = WorkspaceStore::new();
        let ctx = CallContext {
            transport: Transport::Http,
            implicit_workspace: None,
        };
        let r = call(
            &modern(1, "tools/call", r#""name":"session_status","arguments":{}"#),
            &mut store,
            &ctx,
        );
        assert_eq!(store.len(), 0, "an empty fresh map is never inserted");
        assert!(
            r["result"]["structuredContent"]["workspace"].is_null(),
            "{}",
            r["result"]["structuredContent"]
        );
        let status: Value = serde_json::from_str(&text_of(&r)).expect("session_status is JSON");
        assert!(status["workspace"].is_null(), "{status}");
        assert_eq!(
            r["result"]["structuredContent"], status,
            "text and structured value stay equal"
        );
    }

    #[test]
    fn legacy_and_stdio_session_status_do_not_echo_the_handle() {
        // A stdio client has one implicit workspace and a legacy HTTP client's
        // handle IS its `Mcp-Session-Id` — a bearer token for its warm state.
        // Neither needs the handle back, and a bearer token must not travel in
        // tool output, so session_status reports `workspace: null` off the
        // modern HTTP path. (The legacy-HTTP half is in `http`'s tests.)
        let mut store = WorkspaceStore::new();
        let h = store.mint();
        let ctx = stdio_ctx(&h);
        call(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"open_text","arguments":{"text":"x\n","session":"c"}}}"#,
            &mut store,
            &ctx,
        );
        let status = call(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"session_status","arguments":{}}}"#,
            &mut store,
            &ctx,
        );
        let json: Value = serde_json::from_str(&text_of(&status)).expect("session_status is JSON");
        assert!(json["workspace"].is_null(), "{json}");
        assert!(!text_of(&status).contains(&h), "the handle must not leak");
        assert_eq!(json["sessions"][0]["id"], "c", "but the state is there");
        assert!(status["result"]["structuredContent"]["workspace"].is_null());
    }

    #[test]
    fn git_and_help_take_no_workspace_and_mint_none() {
        let mut store = WorkspaceStore::new();
        let ctx = CallContext {
            transport: Transport::Http,
            implicit_workspace: None,
        };
        call(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"help","arguments":{}}}"#,
            &mut store,
            &ctx,
        );
        assert_eq!(store.len(), 0);
    }

    fn modern(id: u32, method: &str, params_body: &str) -> String {
        let sep = if params_body.is_empty() { "" } else { "," };
        let meta = modern_meta_json();
        format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{{{params_body}{sep}{meta}}}}}"#
        )
    }

    #[test]
    fn era_is_decided_by_the_meta_protocol_version_key() {
        assert_eq!(era_of(&json!({})), Era::Legacy);
        assert_eq!(era_of(&json!({"_meta": {}})), Era::Legacy);
        assert_eq!(
            era_of(&json!({"_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}})),
            Era::Modern
        );
        // Present but malformed still selects modern (then fails validation).
        assert_eq!(
            era_of(&json!({"_meta": {"io.modelcontextprotocol/protocolVersion": 7}})),
            Era::Modern
        );
    }

    #[test]
    fn initialize_negotiates_only_legacy_versions() {
        let mut store = WorkspaceStore::new();
        let h = store.mint();
        let ctx = stdio_ctx(&h);
        for (asked, want) in [
            ("2024-11-05", "2024-11-05"),
            ("2025-03-26", "2025-03-26"),
            ("2025-06-18", "2025-06-18"),
            ("2025-11-25", "2025-11-25"),
            ("2026-07-28", "2025-11-25"),
            ("1999-01-01", "2025-11-25"),
        ] {
            let req = format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"{asked}"}}}}"#
            );
            let r = call(&req, &mut store, &ctx);
            assert_eq!(r["result"]["protocolVersion"], want, "asked {asked}");
            assert!(
                r["result"].get("resultType").is_none(),
                "legacy replies are unshaped"
            );
        }
        let r = call(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            &mut store,
            &ctx,
        );
        assert_eq!(r["result"]["protocolVersion"], "2025-11-25");
        assert_eq!(
            r["result"]["serverInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert!(
            r["result"]["serverInfo"]["description"]
                .as_str()
                .unwrap()
                .contains("editing")
        );
    }

    #[test]
    fn server_discover_answers_in_both_eras() {
        let mut store = WorkspaceStore::new();
        let h = store.mint();
        let ctx = stdio_ctx(&h);
        let legacy = call(
            r#"{"jsonrpc":"2.0","id":1,"method":"server/discover"}"#,
            &mut store,
            &ctx,
        );
        assert_eq!(legacy["result"]["supportedVersions"][0], "2026-07-28");
        assert_eq!(
            legacy["result"]["supportedVersions"]
                .as_array()
                .unwrap()
                .len(),
            5
        );
        assert_eq!(legacy["result"]["ttlMs"], 86_400_000u64);
        assert_eq!(legacy["result"]["cacheScope"], "public");
        // Both eras learn who the server is, not only what it speaks.
        assert_eq!(legacy["result"]["serverInfo"]["name"], "mime-rs");
        assert_eq!(
            legacy["result"]["serverInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert!(legacy["result"].get("resultType").is_none());

        let modern = call(&modern(2, "server/discover", ""), &mut store, &ctx);
        assert_eq!(modern["result"]["resultType"], "complete");
        assert_eq!(
            modern["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
            "mime-rs"
        );
        assert!(
            modern["result"]["instructions"]
                .as_str()
                .unwrap()
                .contains("mime-rs")
        );
    }

    #[test]
    fn modern_results_carry_result_type_and_server_info_legacy_ones_do_not() {
        let mut store = WorkspaceStore::new();
        let h = store.mint();
        let ctx = stdio_ctx(&h);
        let m = call(&modern(1, "tools/list", ""), &mut store, &ctx);
        assert_eq!(m["result"]["resultType"], "complete");
        assert_eq!(
            m["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(m["result"]["ttlMs"], 86_400_000u64);
        let l = call(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            &mut store,
            &ctx,
        );
        assert!(l["result"].get("resultType").is_none());
        assert!(l["result"].get("_meta").is_none());
        assert_eq!(
            l["result"]["ttlMs"], 86_400_000u64,
            "cache hints ride in both eras"
        );
    }

    #[test]
    fn malformed_or_unsupported_meta_is_rejected() {
        let mut store = WorkspaceStore::new();
        let h = store.mint();
        let ctx = stdio_ctx(&h);
        let no_caps = call(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#,
            &mut store,
            &ctx,
        );
        assert_eq!(no_caps["error"]["code"], -32602);
        assert!(
            no_caps["error"]["message"]
                .as_str()
                .unwrap()
                .contains("clientCapabilities")
        );
        let bad_type = call(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":7,"io.modelcontextprotocol/clientCapabilities":{}}}}"#,
            &mut store,
            &ctx,
        );
        assert_eq!(bad_type["error"]["code"], -32602);
        let unsupported = call(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2030-01-01","io.modelcontextprotocol/clientCapabilities":{}}}}"#,
            &mut store,
            &ctx,
        );
        assert_eq!(unsupported["error"]["code"], -32022);
        assert_eq!(unsupported["error"]["data"]["requested"], "2030-01-01");
        assert_eq!(unsupported["error"]["data"]["supported"][0], "2026-07-28");
        // A legacy version inside modern _meta is served (the client chose the
        // shape).
        let legacy_in_meta = call(
            r#"{"jsonrpc":"2.0","id":4,"method":"ping","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2025-06-18","io.modelcontextprotocol/clientCapabilities":{}}}}"#,
            &mut store,
            &ctx,
        );
        assert_eq!(legacy_in_meta["result"]["resultType"], "complete");
        // A malformed modern NOTIFICATION gets no reply at all.
        assert!(
            handle_line(
                r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":7}}}"#,
                &mut store,
                &ctx
            )
            .is_none()
        );
    }

    #[test]
    fn modern_http_calls_report_their_workspace_stdio_ones_do_not() {
        let mut store = WorkspaceStore::new();
        let http = CallContext {
            transport: Transport::Http,
            implicit_workspace: None,
        };
        let r = call(
            &modern(
                1,
                "tools/call",
                r#""name":"open_text","arguments":{"text":"x","session":"c"}"#,
            ),
            &mut store,
            &http,
        );
        let h = handle_of(&r);
        assert_eq!(h.len(), 32);
        assert!(
            text_of(&r).ends_with(&format!("workspace: {h}")),
            "{}",
            text_of(&r)
        );
        // Passing it back lands in the same workspace.
        let body = format!(r#""name":"view","arguments":{{"session":"c","workspace":"{h}"}}"#);
        let v = call(&modern(2, "tools/call", &body), &mut store, &http);
        assert!(text_of(&v).contains('x'));
        assert_eq!(store.len(), 1);
        // git/help report nothing.
        let help = call(
            &modern(3, "tools/call", r#""name":"help","arguments":{}"#),
            &mut store,
            &http,
        );
        assert!(help["result"].get("structuredContent").is_none());

        let h2 = store.mint();
        let stdio = stdio_ctx(&h2);
        let s = call(
            &modern(
                4,
                "tools/call",
                r#""name":"open_text","arguments":{"text":"x"}"#,
            ),
            &mut store,
            &stdio,
        );
        // No structured value, and no handle line either.
        assert!(s["result"].get("structuredContent").is_none());
        assert!(!text_of(&s).contains("workspace:"));
    }

    #[test]
    fn tools_list_order_is_stable_and_matches_the_catalogue() {
        let _lock = crate::sequencer::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut store = WorkspaceStore::new();
        let h = store.mint();
        let ctx = stdio_ctx(&h);
        let a = call(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            &mut store,
            &ctx,
        );
        let b = call(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            &mut store,
            &ctx,
        );
        assert_eq!(a["result"]["tools"], b["result"]["tools"]);
        let names: Vec<&str> = a["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        let catalogue: Vec<&str> = crate::mcp::tool_schemas()
            .iter()
            .filter(|t| crate::mcp::tool_listed(t["name"].as_str().unwrap(), Transport::Stdio))
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, catalogue);
    }
}
