//! mime-rs — a scriptable, transactional editing engine for AI agents.
// The MCP tool schemas are one large `json!` literal each; git_rebase's
// outgrew serde_json's default macro recursion depth.
#![recursion_limit = "192"]
//!
//! M0 vertical slice: an in-memory [`Buffer`] (the `TextStore` oracle that
//! `Quire` — the piece-tree-over-mmap store — will later replace behind the same
//! surface) driven by Emacs-Lisp editor primitives over tulisp. The language is
//! Emacs Lisp: an implicit current buffer with point/mark, `re-search-forward` +
//! `replace-match`, etc. Pending work is tracked in `todo.org`.

pub mod buffer;
pub mod builtins;
pub mod cli;
pub mod coding;
pub mod conflict;
pub mod daemon;
pub mod engine;
pub mod help;
pub mod http;
pub mod mcp;
pub mod quire;
pub mod regex_dialect;
pub mod result;
pub mod safety;
pub mod sequencer;
pub mod store;
pub mod strings;
pub mod syntax;
#[cfg(feature = "tui")]
pub mod tui;
pub mod tui_step;

pub use buffer::Buffer;
pub use engine::{Workspace, run_program};
pub use quire::Quire;
pub use result::RunReport;
pub use store::TextStore;
