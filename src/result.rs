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
    /// `Some` only when the run actually changed the text (`dirty`): a clean
    /// run never materializes the document (the version-stamp fast path), so
    /// there is nothing to copy here. Callers that need the text of a clean
    /// buffer ask the workspace ([`Workspace::text`](crate::Workspace::text)).
    pub final_text: Option<String>,
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
/// error message itself — and `dirty`, whether the dying program's edits
/// persist (true only for a warm writable run; read-only and rehearse roll
/// back). Additive: `ok` stays the discriminator, and there is deliberately
/// no `diff` (what a failed run left behind is the *next* run's concern, not
/// a result).
pub fn failure_json(
    error: &str,
    reports: &[(String, String)],
    log: &[String],
    dirty: bool,
) -> Value {
    json!({
        "ok": false,
        "error": error,
        "dirty": dirty,
        "reports": reports_to_json(reports),
        "log": log,
    })
}

/// Clamp a unified diff for transport: a bulk edit (replace-regexp over a
/// big file) produces a diff proportional to the whole change — megabytes
/// straight into an agent's context. Beyond `max_lines`, keep the head and
/// tail halves around an elision line that says how much was suppressed.
pub fn clamp_diff(diff: &str, max_lines: usize) -> String {
    let total = diff.lines().count();
    if total <= max_lines.max(2) {
        return diff.to_string();
    }
    let head_n = max_lines / 2;
    let tail_n = max_lines - head_n;
    let head: Vec<&str> = diff.lines().take(head_n).collect();
    let tail: Vec<&str> = diff.lines().skip(total - tail_n).collect();
    format!(
        "{}\n… diff clamped: {} of {} lines elided (pass full_diff:true for everything) …\n{}",
        head.join("\n"),
        total - max_lines,
        total,
        tail.join("\n")
    )
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
    fn clamp_diff_keeps_head_and_tail_around_an_elision_line() {
        let diff: String = (1..=100).map(|i| format!("line {i}\n")).collect();
        let clamped = clamp_diff(&diff, 10);
        assert!(clamped.contains("line 1\n"));
        assert!(clamped.contains("line 100"));
        assert!(clamped.contains("90 of 100 lines elided"), "{clamped}");
        assert!(!clamped.contains("line 50"));
        // Under the cap: untouched.
        assert_eq!(clamp_diff("a\nb\nc", 10), "a\nb\nc");
    }

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
            final_text: None,
        };
        let j = r.to_json();
        assert_eq!(j["reports"]["once"], "a");
        assert_eq!(j["reports"]["many"], serde_json::json!(["1", "2", "3"]));
    }
}
