//! Structured result of a program run — the diff + reports the agent sees.
//! (Token-frugal by design: a unified diff and machine-readable reports, not
//! the whole buffer.)
use serde_json::{Value, json};

pub struct RunReport {
    pub buffer_name: String,
    pub diff: String,
    pub dirty: bool,
    pub point: usize,
    pub len_before: usize,
    pub len_after: usize,
    pub reports: Vec<(String, String)>,
    pub log: Vec<String>,
    /// True when this report came from a *rehearsal* — the program ran and the
    /// diff/reports below describe what *would* have happened, but the live
    /// buffer (and kill-ring/checkpoints) were rolled back, so nothing persisted.
    /// `false` for a normal `run`.
    pub rehearsed: bool,
    /// Final buffer text — not serialized into the JSON; used by `--write`.
    pub final_text: String,
}

impl RunReport {
    pub fn to_json(&self) -> Value {
        let reports: serde_json::Map<String, Value> = self
            .reports
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        json!({
            "ok": true,
            "buffer": self.buffer_name,
            "dirty": self.dirty,
            "rehearsed": self.rehearsed,
            "point": self.point,
            "len_before": self.len_before,
            "len_after": self.len_after,
            "diff": self.diff,
            "reports": reports,
            "log": self.log,
        })
    }
}

/// A unified (line-based) diff of the buffer before/after the program.
pub fn unified_diff(before: &str, after: &str) -> String {
    format!(
        "{}",
        similar::TextDiff::from_lines(before, after).unified_diff()
    )
}
