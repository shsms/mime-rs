//! Integration tests for buffer-level commands, driven through the public API.
use mime_rs::{Buffer, run_program};

fn run(text: &str, program: &str) -> String {
    run_program(Box::new(Buffer::from_string("t", text)), program)
        .expect("program should run")
        .final_text
        .expect("these programs all change the text")
}

#[test]
fn kill_line_kills_to_end_of_line() {
    assert_eq!(run("foo\nbar", "(goto-char 1) (kill-line)"), "\nbar");
}

#[test]
fn kill_line_at_eol_kills_the_newline() {
    // point after "foo" (position 4) is end-of-line; kill-line removes the newline.
    assert_eq!(run("foo\nbar", "(goto-char 4) (kill-line)"), "foobar");
}

#[test]
fn delete_trailing_whitespace_cleans_lines() {
    assert_eq!(
        run("foo   \nbar\t\nbaz", "(delete-trailing-whitespace)"),
        "foo\nbar\nbaz"
    );
}

// ---- filling ----

/// Run `program` over `text` in a buffer named `name` (its extension picks
/// the language) and return the final text (the input when nothing changed).
fn run_as(name: &str, text: &str, program: &str) -> String {
    run_program(Box::new(Buffer::from_string(name, text)), program)
        .expect("program should run")
        .final_text
        .unwrap_or_else(|| text.to_string())
}

/// The value `program` reports under `key`, and the final text.
fn reported(name: &str, text: &str, program: &str, key: &str) -> (String, Option<String>) {
    let report = run_program(Box::new(Buffer::from_string(name, text)), program)
        .expect("program should run");
    let value = report
        .reports
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .expect("the program reports the key");
    (value, report.final_text)
}

/// The error a failing `program` reports.
fn fails_as(name: &str, text: &str, program: &str) -> String {
    run_program(Box::new(Buffer::from_string(name, text)), program)
        .err()
        .expect("program should fail")
}

#[test]
fn fill_paragraph_reflows_the_doc_comment_at_point() {
    assert_eq!(
        run_as(
            "t.rs",
            "/// aaa bbb\n/// ccc\nfn f() {}\n",
            "(goto-char 3) (fill-paragraph)"
        ),
        "/// aaa bbb ccc\nfn f() {}\n"
    );
}

#[test]
fn fill_column_defaults_to_80_and_setq_changes_it() {
    let text = "/// aaa bbb ccc\n";
    assert_eq!(run_as("t.rs", text, "(fill-paragraph)"), text);
    assert_eq!(
        run_as("t.rs", text, "(setq fill-column 9) (fill-paragraph)"),
        "/// aaa\n/// bbb\n/// ccc\n"
    );
}

#[test]
fn fill_column_can_be_let_bound() {
    assert_eq!(
        run_as(
            "t.rs",
            "/// aaa bbb ccc\n",
            "(let ((fill-column 9)) (fill-paragraph))"
        ),
        "/// aaa\n/// bbb\n/// ccc\n"
    );
}

#[test]
fn fill_paragraph_refuses_code_and_names_it() {
    let err = fails_as("t.rs", "fn f() {}\n", "(goto-char 4) (fill-paragraph)");
    assert!(err.contains("function_item"), "{err}");
    assert!(err.contains("fill-prefix"), "{err}");
}

#[test]
fn fill_paragraph_fills_a_python_docstring() {
    let text = "def f():\n    \"\"\"Summary that\n    wraps.\"\"\"\n";
    assert_eq!(
        run_as("t.py", text, "(goto-char 20) (fill-paragraph)"),
        "def f():\n    \"\"\"Summary that wraps.\"\"\"\n"
    );
}

#[test]
fn fill_paragraph_fills_a_markdown_list_item() {
    assert_eq!(
        run_as(
            "t.md",
            "- aaa\n  bbb\n\ncc\n",
            "(goto-char 4) (fill-paragraph)"
        ),
        "- aaa bbb\n\ncc\n"
    );
}

#[test]
fn fill_prefix_overrides_detection_and_bounds_the_paragraph() {
    let text = ";; aaa\n;; bbb\nplain\n";
    assert_eq!(
        run_as(
            "t.md",
            text,
            "(setq fill-prefix \";; \") (goto-char 1) (fill-paragraph)"
        ),
        ";; aaa bbb\nplain\n"
    );
}

#[test]
fn sentence_end_double_space_is_on_by_default() {
    assert_eq!(
        run_as("t.md", "One.\nTwo.\n", "(fill-paragraph)"),
        "One.  Two.\n"
    );
    assert_eq!(
        run_as(
            "t.md",
            "One.\nTwo.\n",
            "(setq sentence-end-double-space nil) (fill-paragraph)"
        ),
        "One. Two.\n"
    );
}

#[test]
fn fill_paragraph_returns_the_unit_it_filled() {
    let (value, _) = reported(
        "t.rs",
        "/// aaa\n/// bbb\nfn f() {}\n",
        "(report \"u\" (fill-paragraph))",
        "u",
    );
    // The span after the fill: the two lines became one.
    assert_eq!(value, "(\"comment\" 1 13)");
}

#[test]
fn a_unit_ending_at_point_max_still_fills() {
    // The narrowing ends on the run's last newline.
    assert_eq!(
        run_as(
            "t.rs",
            "x\n// aaa\n// bbb\n",
            "(narrow-to-region 3 16) (goto-char 4) (fill-paragraph)"
        ),
        "x\n// aaa bbb\n"
    );
}

#[test]
fn a_file_type_without_a_grammar_is_refused() {
    let err = fails_as(
        "Foo.java",
        "public class Foo {\n    int x;\n}\n",
        "(fill-paragraph)",
    );
    assert!(err.contains("no grammar for .java"), "{err}");
    assert_eq!(
        run_as("notes.txt", "aaa\nbbb\n", "(fill-paragraph)"),
        "aaa bbb\n"
    );
    assert_eq!(
        run_as("scratch", "aaa\nbbb\n", "(fill-paragraph)"),
        "aaa bbb\n"
    );
    // A treesit-set-language override wins, and a uniquified name keeps
    // its extension.
    assert_eq!(
        run_as(
            "Foo.java",
            "// aaa\n// bbb\nclass F {}\n",
            "(treesit-set-language \"rust\") (fill-paragraph)"
        ),
        "// aaa bbb\nclass F {}\n"
    );
    assert_eq!(
        run_as("lib.rs<2>", "// aaa\n// bbb\n", "(fill-paragraph)"),
        "// aaa bbb\n"
    );
    assert_eq!(
        run_as("notes.txt<2>", "aaa\nbbb\n", "(fill-paragraph)"),
        "aaa bbb\n"
    );
}

#[test]
fn point_at_the_end_of_the_buffer_fills_the_last_unit() {
    assert_eq!(
        run_as(
            "t.rs",
            "// aa\n// bb\n",
            "(goto-char (point-max)) (fill-paragraph)"
        ),
        "// aa bb\n"
    );
    assert_eq!(
        run_as(
            "t.md",
            "aa\nbb\n",
            "(goto-char (point-max)) (fill-paragraph)"
        ),
        "aa bb\n"
    );
}
