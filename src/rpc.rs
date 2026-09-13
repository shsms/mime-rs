//! The MCP protocol layer: JSON-RPC 2.0 framing, the `initialize`
//! handshake and version negotiation. Transport-agnostic — the stdio loop
//! (`mcp::run`) and the HTTP front end (`http`) both feed lines to
//! [`handle_line`]. This file never touches a buffer: every buffer-touching
//! tool lives in `mcp`. The two exceptions dispatched here are the workspace
//! tools `open_workspace` and `close_workspace`, because they act on the
//! [`WorkspaceStore`] itself rather than on any one session map.
use std::collections::{HashMap, VecDeque};
use std::io::Read;

use crate::Workspace;
use crate::mcp::{
    ToolOutput, tool_result, tool_text, tool_uses_workspace, tools_call_result, validate_args,
};
use serde_json::{Value, json};

/// Every protocol version mime implements, newest first. `2026-07-28` is the
/// stateless "modern" era (per-request `_meta`, no handshake); the rest are
/// the `initialize`-based legacy era. A dual-era server in the spec's sense.
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
/// static for the life of the process, so a long TTL lets clients keep the
/// list in their prompt cache.
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
/// `serverInfo` and in a modern reply's `_meta`.
pub fn server_info() -> Value {
    json!({
        "name": "mime-rs",
        "version": env!("CARGO_PKG_VERSION"),
        "description": env!("CARGO_PKG_DESCRIPTION"),
    })
}

/// `server/discover`: the handshake-free way to learn what this server speaks.
fn discover_result() -> Value {
    json!({
        "supportedVersions": SUPPORTED_PROTOCOL_VERSIONS,
        "capabilities": { "tools": {} },
        "serverInfo": server_info(),
        "instructions": crate::mcp::instructions(),
        "ttlMs": LIST_TTL_MS,
        "cacheScope": "public",
    })
}

/// One client's warm-session map: session id -> that session's engine
/// state (a `Workspace` buffer, not the handle-scoped workspace above).
pub type Sessions = HashMap<String, Workspace>;

/// Cap on concurrent workspaces; the oldest is FIFO-evicted past it, so an
/// open_workspace / initialize flood cannot exhaust memory or file descriptors.
pub const WORKSPACE_CAP: usize = 256;

/// Bounded set of workspaces keyed by an unguessable handle. A legacy HTTP
/// `Mcp-Session-Id` and a modern `workspace` tool argument are the same key.
pub struct WorkspaceStore {
    map: HashMap<String, Sessions>,
    order: VecDeque<String>,
    /// A handle eviction must never take: stdio's implicit workspace, which
    /// is minted first and would otherwise be the first thing an
    /// `open_workspace` flood dropped — taking the agent's warm buffers with it.
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
    /// Mint a fresh empty workspace, evicting the oldest UNPINNED one while at
    /// capacity (skipping the pinned handle rather than dropping it).
    pub fn mint(&mut self) -> String {
        while self.map.len() >= WORKSPACE_CAP {
            let victim = self
                .order
                .iter()
                .position(|id| Some(id) != self.pinned.as_ref());
            match victim {
                Some(i) => {
                    if let Some(old) = self.order.remove(i) {
                        self.map.remove(&old);
                    }
                }
                // Nothing left but the pinned workspace — grow rather than drop it.
                None => break,
            }
        }
        let id = new_handle();
        self.order.push_back(id.clone());
        self.map.insert(id.clone(), Sessions::new());
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

/// A 128-bit random handle (hex). The handle is the client's bearer token
/// for its warm state, so it must not be guessable — read it from the OS
/// CSPRNG, failing closed if that is somehow unavailable.
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

/// Parse one JSON-RPC request line and dispatch it. Returns `Some(response)` for
/// requests (those with an `id`) and `None` for notifications.
pub fn handle_line(line: &str, store: &mut WorkspaceStore, ctx: &CallContext) -> Option<Value> {
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
    let err_id = || id.clone().unwrap_or(Value::Null);

    // A modern request must carry a well-formed, supported `_meta` before it
    // is dispatched; a malformed notification is still silently dropped.
    let era = era_of(&params);
    if era == Era::Modern
        && let Err((code, message, data)) = validate_modern_meta(&params)
    {
        return (!is_notification).then(|| rpc_error_data(err_id(), code, &message, data));
    }

    let (result, workspace) = match method {
        "initialize" => (initialize_result(&params), None),
        // Pure notification — nothing to do, no response.
        "notifications/initialized" | "initialized" => return None,
        "ping" => (json!({}), None),
        "server/discover" => (discover_result(), None),
        "tools/list" => (crate::mcp::tools_list_result(), None),
        "tools/call" => tools_call(&params, store, ctx, era),
        other => {
            if is_notification {
                eprintln!("mime-mcp: ignoring unknown notification {other}");
                return None;
            }
            return Some(rpc_error(err_id(), -32601, "method not found"));
        }
    };
    reply(
        id,
        is_notification,
        shape_result(era, result, workspace.as_deref()),
    )
}

/// Dispatch `tools/call`: the two workspace tools act on the store itself;
/// everything else runs against one resolved workspace's session map (or a
/// throwaway map for tools that never touch warm state: git_*, help).
///
/// A workspace minted for this call and left empty by it is reaped again
/// before returning: a handle-free read-only call (`session_status` on a
/// fresh modern-HTTP request, say) would otherwise leave a permanently
/// unreachable workspace behind on every poll, filling the store until the
/// cap evicted live ones. Nothing is reported for a reaped workspace, since
/// there is no warm state to come back to.
fn tools_call(
    params: &Value,
    store: &mut WorkspaceStore,
    ctx: &CallContext,
    era: Era,
) -> (Value, Option<String>) {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    match name {
        "open_workspace" => {
            if let Err(m) = validate_args(name, &args) {
                return (tool_text(m, true), None);
            }
            let h = store.mint();
            // open_workspace's reply IS the handle — nothing to report on top.
            return (
                tool_result(ToolOutput::with(
                    format!("workspace: {h}"),
                    json!({ "workspace": h }),
                )),
                None,
            );
        }
        "close_workspace" => {
            if let Err(m) = validate_args(name, &args) {
                return (tool_text(m, true), None);
            }
            let Some(h) = args.get("workspace").and_then(Value::as_str) else {
                return (
                    tool_text("close_workspace needs `workspace`".into(), true),
                    None,
                );
            };
            if ctx.transport == Transport::Stdio && ctx.implicit_workspace == Some(h) {
                return (
                    tool_text(
                        "cannot close the stdio default workspace — close_session drops one session"
                            .into(),
                        true,
                    ),
                    None,
                );
            }
            return if store.remove(h) {
                (tool_text(format!("workspace {h} closed"), false), None)
            } else {
                (tool_text(unknown_workspace(h), true), None)
            };
        }
        _ => {}
    }
    if !tool_uses_workspace(name) {
        let mut scratch = Sessions::new();
        return (tools_call_result(params, &mut scratch, ""), None);
    }
    let (handle, minted) =
        match resolve_workspace(args.get("workspace").and_then(Value::as_str), store, ctx) {
            Ok(h) => h,
            Err(m) => return (tool_text(m, true), None),
        };
    // Only the modern HTTP client is handed its workspace back: it has no
    // session header and a bare call mints a fresh one, so without the handle
    // it could never return to these warm buffers. Stdio and the legacy
    // session header already know where they are.
    let report = (era == Era::Modern && ctx.transport == Transport::Http).then(|| handle.clone());
    // Resolution already proved the handle is present; treat a miss as a tool
    // error anyway, so no reachable panic can take the server down with it.
    let Some(sessions) = store.get_mut(&handle) else {
        return (tool_text(unknown_workspace(&handle), true), None);
    };
    let result = tools_call_result(params, sessions, &handle);
    // Freshly minted and still empty: the call opened no session, so the
    // handle leads nowhere. Drop it rather than accumulate dead workspaces.
    if minted && store.get_mut(&handle).is_some_and(|s| s.is_empty()) {
        store.remove(&handle);
        return (result, None);
    }
    (result, report)
}

fn unknown_workspace(h: &str) -> String {
    format!(
        "unknown workspace {h} — it was closed or evicted; open_workspace (or, on the \
         stateless HTTP protocol, any stateful call without a handle) starts a new one; \
         on stdio omit the argument"
    )
}

/// The spec's resolution table: explicit handle must exist; otherwise the
/// transport's implicit workspace (which must also still exist — an unpinned
/// one can have been evicted); otherwise (modern HTTP) mint one.
///
/// Returns `(handle, minted)`. `minted` is true only for that last case, and
/// tells the caller the workspace is this call's own: if the call leaves it
/// empty, it can be reaped instead of lingering unreachable (see
/// [`tools_call`]).
fn resolve_workspace(
    explicit: Option<&str>,
    store: &mut WorkspaceStore,
    ctx: &CallContext,
) -> Result<(String, bool), String> {
    match (explicit, ctx.implicit_workspace) {
        (Some(h), _) if store.contains(h) => Ok((h.to_string(), false)),
        (Some(h), _) => Err(unknown_workspace(h)),
        (None, Some(h)) if store.contains(h) => Ok((h.to_string(), false)),
        (None, Some(h)) => Err(unknown_workspace(h)),
        (None, None) => Ok((store.mint(), true)),
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

/// The last step for every successful result. Modern requests get the
/// `resultType` and `serverInfo` envelope; legacy results are returned
/// untouched (byte-identical to before the dual-era work). `report_workspace`
/// is the handle a modern HTTP stateful call ran in, surfaced in text and
/// `structuredContent` so the agent can pass it back.
fn shape_result(era: Era, mut result: Value, report_workspace: Option<&str>) -> Value {
    if era == Era::Modern {
        result["resultType"] = json!("complete");
        // Index-assignment on a non-object would silently drop the serverInfo,
        // so make sure `_meta` is one first (as `structuredContent` is below).
        if !result["_meta"].is_object() {
            result["_meta"] = json!({});
        }
        result["_meta"][META_SERVER_INFO] = server_info();
    }
    if let Some(h) = report_workspace {
        if let Some(text) = result["content"][0]["text"].as_str().map(str::to_string) {
            result["content"][0]["text"] = Value::String(format!("{text}\nworkspace: {h}"));
        }
        if !result["structuredContent"].is_object() {
            result["structuredContent"] = json!({});
        }
        result["structuredContent"]["workspace"] = json!(h);
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
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": server_info(),
        "instructions": crate::mcp::instructions(),
    })
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

        // initialize: a supported version is echoed; capabilities + instructions present.
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
        assert_eq!(json["workspace"], h);
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
        let mut store = WorkspaceStore::new();
        let h = store.mint();
        let ctx = stdio_ctx(&h);
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
    fn close_workspace_refuses_the_implicit_one_on_stdio_but_drops_a_minted_one() {
        let mut store = WorkspaceStore::new();
        let h = store.mint();
        let ctx = stdio_ctx(&h);
        let req = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"close_workspace","arguments":{{"workspace":"{h}"}}}}}}"#
        );
        let r = call(&req, &mut store, &ctx);
        assert_eq!(r["result"]["isError"], true);
        assert!(text_of(&r).contains("cannot close"), "{}", text_of(&r));

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
        // The modern-HTTP shape (Task 5 wires the transport): no implicit
        // workspace, no argument -> a fresh workspace each call.
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
        // A modern-HTTP poll with no handle mints a workspace, opens nothing
        // in it, and so must not leave it behind — nor report a handle that
        // leads to no warm state.
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
        assert_eq!(store.len(), 0, "the minted-but-empty workspace is reaped");
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
        assert_eq!(
            kept["result"]["structuredContent"]["workspace"]
                .as_str()
                .map(str::len),
            Some(32)
        );
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
    const META: &str = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}"#;

    fn modern(id: u32, method: &str, params_body: &str) -> String {
        let sep = if params_body.is_empty() { "" } else { "," };
        format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{{{params_body}{sep}{META}}}}}"#
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
        // A legacy version inside modern _meta is served (the client chose the shape).
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
        let h = r["result"]["structuredContent"]["workspace"]
            .as_str()
            .expect("handle reported")
            .to_string();
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
        assert!(s["result"].get("structuredContent").is_none());
        assert!(!text_of(&s).contains("workspace:"));
    }

    #[test]
    fn tools_list_order_is_stable_and_matches_the_catalogue() {
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
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, catalogue);
    }
}
