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
