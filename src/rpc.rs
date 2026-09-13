//! The MCP protocol layer: JSON-RPC 2.0 framing, the `initialize`
//! handshake and version negotiation. Transport-agnostic — the stdio loop
//! (`mcp::run`) and the HTTP front end (`http`) both feed lines to
//! [`handle_line`]. Tools live in `mcp`; this file never touches a buffer.
use std::collections::{HashMap, VecDeque};
use std::io::Read;

use crate::Workspace;
use serde_json::{Value, json};

/// MCP protocol versions mime implements (latest first — the tools surface is
/// stable across them). `initialize` echoes the client's if it's one of these,
/// else returns the latest, per the spec.
const SUPPORTED_PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
pub(crate) const PROTOCOL_VERSION: &str = SUPPORTED_PROTOCOL_VERSIONS[0];

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

    match method {
        "initialize" => reply(id, is_notification, initialize_result(&params)),
        "notifications/initialized" | "initialized" => {
            // Pure notification — nothing to do, no response.
            None
        }
        "ping" => reply(id, is_notification, json!({})),
        "tools/list" => reply(id, is_notification, crate::mcp::tools_list_result()),
        "tools/call" => reply(id, is_notification, tools_call(&params, store, ctx)),
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

/// Dispatch `tools/call`: the two workspace tools act on the store itself;
/// everything else runs against one resolved workspace's session map (or a
/// throwaway map for tools that never touch warm state: git_*, help).
fn tools_call(params: &Value, store: &mut WorkspaceStore, ctx: &CallContext) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    match name {
        "open_workspace" => {
            if let Err(m) = crate::mcp::validate_args(name, &args) {
                return crate::mcp::tool_text(m, true);
            }
            let h = store.mint();
            return crate::mcp::tool_text(format!("workspace: {h}"), false);
        }
        "close_workspace" => {
            if let Err(m) = crate::mcp::validate_args(name, &args) {
                return crate::mcp::tool_text(m, true);
            }
            let Some(h) = args.get("workspace").and_then(Value::as_str) else {
                return crate::mcp::tool_text("close_workspace needs `workspace`".into(), true);
            };
            if ctx.transport == Transport::Stdio && ctx.implicit_workspace == Some(h) {
                return crate::mcp::tool_text(
                    "cannot close the stdio default workspace — close_session drops one session"
                        .into(),
                    true,
                );
            }
            return if store.remove(h) {
                crate::mcp::tool_text(format!("workspace {h} closed"), false)
            } else {
                crate::mcp::tool_text(unknown_workspace(h), true)
            };
        }
        _ => {}
    }
    if !crate::mcp::tool_uses_workspace(name) {
        let mut scratch = Sessions::new();
        return crate::mcp::tools_call_result(params, &mut scratch, "");
    }
    let handle = match resolve_workspace(args.get("workspace").and_then(Value::as_str), store, ctx)
    {
        Ok(h) => h,
        Err(m) => return crate::mcp::tool_text(m, true),
    };
    // Resolution already proved the handle is present; treat a miss as a tool
    // error anyway, so no reachable panic can take the server down with it.
    match store.get_mut(&handle) {
        Some(sessions) => crate::mcp::tools_call_result(params, sessions, &handle),
        None => crate::mcp::tool_text(unknown_workspace(&handle), true),
    }
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
fn resolve_workspace(
    explicit: Option<&str>,
    store: &mut WorkspaceStore,
    ctx: &CallContext,
) -> Result<String, String> {
    match (explicit, ctx.implicit_workspace) {
        (Some(h), _) if store.contains(h) => Ok(h.to_string()),
        (Some(h), _) => Err(unknown_workspace(h)),
        (None, Some(h)) if store.contains(h) => Ok(h.to_string()),
        (None, Some(h)) => Err(unknown_workspace(h)),
        (None, None) => Ok(store.mint()),
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
    // Echo the client's requested version when we actually implement it; for an
    // unknown/absent one, return our latest rather than falsely claiming theirs.
    let version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .filter(|v| SUPPORTED_PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSION);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "mime-rs", "version": "0.1.0" },
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
        assert_eq!(old["result"]["protocolVersion"], PROTOCOL_VERSION);

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
}
