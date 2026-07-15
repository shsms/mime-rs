//! The stepping core behind `mime tui` (Phase 0): split a tulisp script into
//! its top-level forms and evaluate them one at a time against a warm
//! [`Workspace`], with a per-step report (diff, reports/log, error). Kept out
//! of the feature-gated `tui` module so the logic builds — and its tests run —
//! in every configuration; the ratatui shell is just a renderer over this.

use crate::Workspace;

/// One top-level form of the script: its source text and 1-based start line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Form {
    pub text: String,
    pub line: usize,
}

/// Split tulisp source into top-level forms without evaluating anything:
/// parens tracked outside strings (`"…"` with `\` escapes), `;` comments, and
/// `?c` / `?\c` character literals (recognized only where a char literal can
/// start — after whitespace or an opening paren — so `foo?` symbols survive).
/// A bare top-level atom counts as a form too.
pub fn split_forms(src: &str) -> Vec<Form> {
    let bytes = src.as_bytes();
    let mut forms = Vec::new();
    let mut i = 0;
    let mut line = 1usize;
    let mut depth = 0usize;
    let mut start: Option<(usize, usize)> = None; // (byte, line)
    let mut in_string = false;
    let mut prev_opens_char = true; // start-of-input can start a char literal

    let push = |from: usize, to: usize, at_line: usize, forms: &mut Vec<Form>| {
        let text = src[from..to].trim_end().to_string();
        if !text.is_empty() {
            forms.push(Form {
                text,
                line: at_line,
            });
        }
    };

    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\n' {
            line += 1;
        }
        if in_string {
            match c {
                b'\\' => i += 1, // skip the escaped byte
                b'"' => in_string = false,
                _ => {}
            }
            i += 1;
            prev_opens_char = false;
            continue;
        }
        match c {
            b';' => {
                // Comment to end of line. At depth 0 a comment ends a bare atom.
                if depth == 0
                    && let Some((s, l)) = start.take()
                {
                    push(s, i, l, &mut forms);
                }
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                prev_opens_char = true;
                continue;
            }
            b'"' => {
                in_string = true;
                if start.is_none() {
                    start = Some((i, line));
                }
            }
            b'?' if prev_opens_char => {
                // Character literal: consume `?x` or `?\x` blindly, so a
                // paren/quote/semicolon CHARACTER cannot derail the scan.
                if start.is_none() {
                    start = Some((i, line));
                }
                i += 1;
                if i < bytes.len() && bytes[i] == b'\\' {
                    i += 1;
                }
                if i < bytes.len() {
                    if bytes[i] == b'\n' {
                        line += 1;
                    }
                    i += 1;
                }
                prev_opens_char = false;
                continue;
            }
            b'(' | b'[' => {
                if start.is_none() {
                    start = Some((i, line));
                }
                depth += 1;
            }
            b')' | b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0
                    && let Some((s, l)) = start.take()
                {
                    push(s, i + 1, l, &mut forms);
                }
            }
            b' ' | b'\t' | b'\r' | b'\n' => {
                // Whitespace at depth 0 ends a bare atom.
                if depth == 0
                    && let Some((s, l)) = start.take()
                {
                    push(s, i, l, &mut forms);
                }
            }
            _ => {
                if start.is_none() {
                    start = Some((i, line));
                }
            }
        }
        prev_opens_char = matches!(c, b'(' | b'[' | b' ' | b'\t' | b'\r' | b'\n');
        i += 1;
    }
    if let Some((s, l)) = start {
        push(s, bytes.len(), l, &mut forms);
    }
    forms
}

/// What one step produced — everything the report pane renders.
#[derive(Clone, Debug, Default)]
pub struct StepOutcome {
    pub index: usize,
    pub diff: String,
    pub dirty: bool,
    pub reports: Vec<(String, String)>,
    pub log: Vec<String>,
    pub error: Option<String>,
}

/// Evaluate a script form by form against one warm workspace. The buffer and
/// any `defun`s persist between steps, exactly as if the whole program ran in
/// one `mime run` — only the pacing differs.
pub struct Stepper {
    ws: Workspace,
    pub forms: Vec<Form>,
    pub next: usize,
    /// Point after the last step (1 before any step ran).
    pub point: usize,
    pub last: Option<StepOutcome>,
}

impl Stepper {
    /// A trusted workspace over `store` (the CLI tier — scripts get the
    /// orchestration builtins), stepping `source`'s top-level forms.
    pub fn new(
        store: Box<dyn crate::TextStore>,
        source: &str,
        prog_args: Vec<(String, String)>,
    ) -> Result<Stepper, String> {
        let forms = split_forms(source);
        if forms.is_empty() {
            return Err("the script has no top-level forms".to_string());
        }
        let ws = Workspace::new_trusted(store);
        ws.set_program_args(prog_args);
        Ok(Stepper {
            ws,
            forms,
            next: 0,
            point: 1,
            last: None,
        })
    }

    pub fn finished(&self) -> bool {
        self.next >= self.forms.len()
    }

    /// The buffer's current text (for the viewport pane).
    pub fn text(&self) -> String {
        self.ws.text()
    }

    /// Evaluate the next form; `None` when the script is finished. A failed
    /// form reports its error (its edits were rolled back by the engine's
    /// transactional default) and stepping may continue.
    pub fn step(&mut self) -> Option<&StepOutcome> {
        if self.finished() {
            return None;
        }
        let index = self.next;
        let form = self.forms[index].text.clone();
        self.next += 1;
        let outcome = match self.ws.run(&form) {
            Ok(report) => {
                self.point = report.point;
                StepOutcome {
                    index,
                    diff: report.diff,
                    dirty: report.dirty,
                    reports: report.reports,
                    log: report.log,
                    error: None,
                }
            }
            Err(e) => {
                let (reports, log, _dirty, _rolled_back) = self.ws.failure_context();
                StepOutcome {
                    index,
                    reports,
                    log,
                    error: Some(e),
                    ..StepOutcome::default()
                }
            }
        };
        self.last = Some(outcome);
        self.last.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Buffer;

    #[test]
    fn splitter_finds_top_level_forms_only() {
        let src = r#"
;; a comment (with parens) and a "string"
(goto-char (point-min)) ; trailing comment
(insert "two (not a form) \" ; not a comment")
(search-forward "x" nil t)
"#;
        let forms = split_forms(src);
        let texts: Vec<&str> = forms.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "(goto-char (point-min))",
                "(insert \"two (not a form) \\\" ; not a comment\")",
                "(search-forward \"x\" nil t)",
            ]
        );
        assert_eq!(forms[0].line, 3);
        assert_eq!(forms[2].line, 5);
    }

    #[test]
    fn splitter_handles_char_literals_and_bare_atoms() {
        // `?(` is the open-paren CHARACTER, not a paren; `?\)` likewise.
        let forms = split_forms("(insert ?\\() (insert ?a)\nt\n(point)");
        let texts: Vec<&str> = forms.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(texts, vec!["(insert ?\\()", "(insert ?a)", "t", "(point)"]);
        // A symbol ending in `?` must not swallow the next form.
        let forms = split_forms("(foo? (bar))\n(baz)");
        assert_eq!(forms.len(), 2, "{forms:?}");
    }

    #[test]
    fn stepper_runs_forms_one_at_a_time_with_per_step_diffs() {
        let store = Box::new(Buffer::from_string("t", "alpha\n"));
        let src = "(goto-char (point-max))\n(insert \"beta\\n\")\n(report \"len\" (point-max))";
        let mut s = Stepper::new(store, src, Vec::new()).unwrap();
        assert_eq!(s.forms.len(), 3);

        // Step 1: motion only — clean.
        let out = s.step().unwrap();
        assert!(!out.dirty && out.diff.is_empty());
        assert_eq!(s.point, 7);

        // Step 2: the edit shows its own diff.
        let out = s.step().unwrap();
        assert!(out.dirty, "insert is dirty");
        assert!(out.diff.contains("+beta"), "{}", out.diff);
        assert_eq!(s.text(), "alpha\nbeta\n");

        // Step 3: report only; then the script is finished.
        let out = s.step().unwrap();
        assert_eq!(out.reports, vec![("len".to_string(), "12".to_string())]);
        assert!(s.finished());
        assert!(s.step().is_none());
    }

    #[test]
    fn stepper_reports_a_failed_form_and_keeps_going() {
        let store = Box::new(Buffer::from_string("t", "x"));
        let src = "(error \"boom\")\n(insert \"ok\")";
        let mut s = Stepper::new(store, src, Vec::new()).unwrap();
        let out = s.step().unwrap();
        assert!(out.error.as_deref().unwrap_or("").contains("boom"));
        let out = s.step().unwrap();
        assert!(out.error.is_none() && out.dirty);
        assert_eq!(s.text(), "okx");
    }
}
