//! The MCP protocol layer: JSON-RPC 2.0 framing, the `initialize`
//! handshake and version negotiation. Transport-agnostic — the stdio loop
//! (`mcp::run`) and the HTTP front end (`http`) both feed lines to
//! [`handle_line`]. Tools live in `mcp`; this file never touches a buffer.
use std::collections::HashMap;

use crate::Workspace;
use serde_json::{Value, json};

/// MCP protocol versions mime implements (latest first — the tools surface is
/// stable across them). `initialize` echoes the client's if it's one of these,
/// else returns the latest, per the spec.
const SUPPORTED_PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
pub(crate) const PROTOCOL_VERSION: &str = SUPPORTED_PROTOCOL_VERSIONS[0];

/// Parse one JSON-RPC request line and dispatch it. Returns `Some(response)` for
/// requests (those with an `id`) and `None` for notifications.
pub(crate) fn handle_line(line: &str, sessions: &mut HashMap<String, Workspace>) -> Option<Value> {
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
        "tools/call" => reply(
            id,
            is_notification,
            crate::mcp::tools_call_result(&params, sessions),
        ),
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
    use crate::Workspace;
    use std::collections::HashMap;

    #[test]
    fn mcp_handshake_is_spec_conformant() {
        let mut s: HashMap<String, Workspace> = HashMap::new();
        let call = |line: &str, s: &mut HashMap<String, Workspace>| handle_line(line, s);

        // initialize: a supported version is echoed; capabilities + instructions present.
        let init = call(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
            &mut s,
        )
        .unwrap();
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
            &mut s,
        )
        .unwrap();
        assert_eq!(old["result"]["protocolVersion"], PROTOCOL_VERSION);

        // notifications/initialized is a pure notification — no reply.
        assert!(
            call(
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                &mut s
            )
            .is_none()
        );

        // ping → empty result object.
        assert!(
            call(r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#, &mut s).unwrap()["result"]
                .is_object()
        );

        // tools/list → every tool carries an inputSchema and annotations.
        let list = call(r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#, &mut s).unwrap();
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
            &mut s,
        )
        .unwrap();
        let view = call(
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"view","arguments":{"session":"c"}}}"#,
            &mut s,
        )
        .unwrap();
        assert_eq!(view["result"]["isError"], false);

        // unknown method → a JSON-RPC error (-32601), not a result.
        let err = call(r#"{"jsonrpc":"2.0","id":6,"method":"no/such"}"#, &mut s).unwrap();
        assert_eq!(err["error"]["code"], -32601);

        // unparseable input → parse error (-32700) against a null id.
        let parse = call("{not json", &mut s).unwrap();
        assert_eq!(parse["error"]["code"], -32700);
    }
}
