//! Run a tulisp program against a buffer, returning a structured result.
//!
//! M0 runs cold (a fresh `TulispContext` per program). Warm buffers + warm
//! definitions across programs (the daemon) arrive in M3.
use crate::buffer::Buffer;
use crate::result::{RunReport, unified_diff};
use std::cell::RefCell;
use std::rc::Rc;
use tulisp::TulispContext;

/// State shared between a running program and the editor builtins. The editor
/// primitives close over an `Rc<RefCell<Session>>` (tulisp is single-threaded,
/// so interior mutability via `RefCell` is sound for these leaf operations).
pub struct Session {
    pub buffer: Buffer,
    pub reports: Vec<(String, String)>,
    pub log: Vec<String>,
}

pub type SharedSession = Rc<RefCell<Session>>;

/// Evaluate `program` (Emacs Lisp / tulisp) against `buffer`; return the diff,
/// reports, and final state. The error string is the formatted tulisp error.
pub fn run_program(buffer: Buffer, program: &str) -> Result<RunReport, String> {
    let before = buffer.text().to_string();
    let len_before = before.chars().count();
    let name = buffer.name.clone();

    let session: SharedSession = Rc::new(RefCell::new(Session {
        buffer,
        reports: Vec::new(),
        log: Vec::new(),
    }));

    let mut ctx = TulispContext::new();
    crate::builtins::register(&mut ctx, &session);
    crate::strings::register(&mut ctx);

    ctx.eval_string(program).map_err(|e| e.format(&ctx))?;

    let s = session.borrow();
    let after = s.buffer.text().to_string();
    let len_after = after.chars().count();
    Ok(RunReport {
        buffer_name: name,
        dirty: after != before,
        diff: unified_diff(&before, &after),
        point: s.buffer.point(),
        len_before,
        len_after,
        reports: s.reports.clone(),
        log: s.log.clone(),
        final_text: after,
    })
}
