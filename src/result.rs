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
        json!({
            "ok": true,
            "buffer": self.buffer_name,
            "dirty": self.dirty,
            "rehearsed": self.rehearsed,
            "point": self.point,
            "len_before": self.len_before,
            "len_after": self.len_after,
            "diff": self.diff,
            "reports": reports_to_json(&self.reports),
            "log": self.log,
        })
    }
}

/// The `reports` map as JSON. A repeated key (a per-item report stream —
/// `treesit-list-defuns` emits one "defun" line per function) aggregates into
/// an array; a key reported once stays a plain string.
pub fn reports_to_json(reports: &[(String, String)]) -> Value {
    let mut map = serde_json::Map::new();
    for (k, v) in reports {
        let v = Value::String(v.clone());
        match map.get_mut(k) {
            None => {
                map.insert(k.clone(), v);
            }
            Some(Value::Array(items)) => items.push(v),
            Some(first) => {
                let first = first.take();
                map.insert(k.clone(), Value::Array(vec![first, v]));
            }
        }
    }
    Value::Object(map)
}

/// The failure shape every front-end emits for a program that signaled an
/// error: `ok:false` + the error string, PLUS the `reports`/`log` the program
/// accumulated before it died — the diagnostics callers used to pack into the
/// error message itself. Additive: `ok` stays the discriminator, and there is
/// deliberately no `diff` (what a failed run left behind is the *next* run's
/// concern, not a result).
pub fn failure_json(error: &str, reports: &[(String, String)], log: &[String]) -> Value {
    json!({
        "ok": false,
        "error": error,
        "reports": reports_to_json(reports),
        "log": log,
    })
}

/// A unified (line-based) diff of the buffer before/after the program.
pub fn unified_diff(before: &str, after: &str) -> String {
    format!(
        "{}",
        similar::TextDiff::from_lines(before, after).unified_diff()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_report_keys_aggregate_into_an_array() {
        let r = RunReport {
            buffer_name: "t".into(),
            diff: String::new(),
            dirty: false,
            point: 1,
            len_before: 0,
            len_after: 0,
            reports: vec![
                ("once".into(), "a".into()),
                ("many".into(), "1".into()),
                ("many".into(), "2".into()),
                ("many".into(), "3".into()),
            ],
            log: vec![],
            rehearsed: false,
            final_text: String::new(),
        };
        let j = r.to_json();
        assert_eq!(j["reports"]["once"], "a");
        assert_eq!(j["reports"]["many"], serde_json::json!(["1", "2", "3"]));
    }
}
