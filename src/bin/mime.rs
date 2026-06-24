//! `mime` — the single mime-rs binary. One executable, three front-ends, the
//! mode picked by a flag:
//!
//! * `mime …` — the CLI (the embedded one-shot `run`, the `repl`, and the
//!   daemon-client verbs). The default when no mode flag is present.
//! * `mime --daemon` — the long-lived warm-session daemon (JSON-lines over
//!   `$MIME_SOCKET`).
//! * `mime --mcp` — the MCP server (JSON-RPC 2.0 over stdio).
//! * `mime --http [ADDR]` — the MCP server over Streamable HTTP (default
//!   `127.0.0.1:7711`, or `$MIME_HTTP_ADDR`).
//!
//! This binary only peeks for the mode flag and dispatches into the matching
//! library front-end (`mime_rs::{cli,daemon,mcp,http}`), which re-reads `argv`
//! itself. The mode flags are consumed by the peek; each front-end ignores them.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--mcp") {
        mime_rs::mcp::run();
    } else if args.iter().any(|a| a == "--http") {
        // Optional addr is the token right after --http (anything not a flag).
        let addr = args
            .iter()
            .position(|a| a == "--http")
            .and_then(|i| args.get(i + 1))
            .filter(|a| !a.starts_with('-'))
            .cloned()
            .or_else(|| std::env::var("MIME_HTTP_ADDR").ok())
            .unwrap_or_else(|| "127.0.0.1:7711".to_string());
        mime_rs::http::run(&addr);
    } else if args.iter().any(|a| a == "--daemon") {
        mime_rs::daemon::run();
    } else {
        mime_rs::cli::run();
    }
}
