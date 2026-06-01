//! `mime` — the single mime-rs binary. One executable, three front-ends, the
//! mode picked by a flag:
//!
//! * `mime …` — the CLI (the embedded one-shot `run`, the `repl`, and the
//!   daemon-client verbs). The default when no mode flag is present.
//! * `mime --daemon` — the long-lived warm-session daemon (JSON-lines over
//!   `$MIME_SOCKET`).
//! * `mime --mcp` — the MCP server (JSON-RPC 2.0 over stdio).
//!
//! This binary only peeks for the mode flag and dispatches into the matching
//! library front-end (`mime_rs::{cli,daemon,mcp}`), which re-reads `argv` itself.
//! The `--mcp`/`--daemon` flags are consumed by the peek; each front-end ignores
//! them.
fn main() {
    let mode_mcp = std::env::args().any(|a| a == "--mcp");
    let mode_daemon = std::env::args().any(|a| a == "--daemon");
    if mode_mcp {
        mime_rs::mcp::run();
    } else if mode_daemon {
        mime_rs::daemon::run();
    } else {
        mime_rs::cli::run();
    }
}
