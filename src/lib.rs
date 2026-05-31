//! mime-rs — a scriptable, transactional editing engine for AI agents.
//!
//! M0 vertical slice: an in-memory [`Buffer`] (the `TextStore` oracle that
//! `Quire` — the piece-tree-over-mmap store — will later replace behind the same
//! surface) driven by Emacs-Lisp editor primitives over tulisp. The language is
//! Emacs Lisp: an implicit current buffer with point/mark, `re-search-forward` +
//! `replace-match`, etc. See `plan.org` for the full design.

pub mod buffer;
pub mod builtins;
pub mod engine;
pub mod result;
pub mod strings;

pub use buffer::Buffer;
pub use engine::run_program;
pub use result::RunReport;
