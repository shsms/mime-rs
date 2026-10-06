//! Core library (the Emacs `subr.el` corner) — list and function helpers the
//! tulisp base lacks: identity, delete-dups, nreverse, butlast,
//! number-sequence, fboundp, seq-remove, seq-uniq, format-message, and a
//! user-error that curves its quotes as Emacs's does. Pure Lisp-value functions
//! with no buffer state, registered in every tier alongside the string library.
//! Semantics are checked against GNU Emacs 30 in the tests.
use tulisp::{Error, TulispContext, TulispObject};

pub fn register(ctx: &mut TulispContext) {
    // (identity ARG) — return ARG unchanged.
    ctx.defun("identity", |arg: TulispObject| -> TulispObject { arg });

    // (delete-dups LIST) — destructively drop `equal` duplicates, keeping each
    // element's FIRST occurrence; returns LIST. Later cells are spliced out
    // with `setcdr`, so the list a variable holds changes too, as in Emacs.
    ctx.defun(
        "delete-dups",
        |list: TulispObject| -> Result<TulispObject, Error> {
            let mut tail = list.clone();
            while tail.consp() {
                let head = tail.car()?;
                let mut prev = tail.clone();
                let mut cur = tail.cdr()?;
                while cur.consp() {
                    let next = cur.cdr()?;
                    if cur.car()?.equal(&head) {
                        prev.set_cdr(next.clone())?;
                    } else {
                        prev = cur;
                    }
                    cur = next;
                }
                tail = tail.cdr()?;
            }
            Ok(list)
        },
    );

    // (nreverse LIST) — LIST reversed. Emacs may reuse LIST's cells; this
    // builds a fresh list, which is what callers must use in Emacs too (the
    // argument's value afterwards is unspecified there).
    ctx.defun("nreverse", |list: Vec<TulispObject>| -> TulispObject {
        list.into_iter().rev().collect()
    });

    // (butlast LIST &optional N) — LIST without its last N elements (default
    // 1), as a fresh list; N <= 0 returns LIST itself, N >= its length nil.
    ctx.defun(
        "butlast",
        |ctx: &mut TulispContext,
         list: TulispObject,
         n: Option<i64>|
         -> Result<TulispObject, Error> {
            let n = n.unwrap_or(1);
            if n <= 0 {
                return Ok(list);
            }
            let mut items: Vec<TulispObject> = list.convert(ctx)?;
            items.truncate(items.len().saturating_sub(n as usize));
            Ok(TulispObject::from(items))
        },
    );

    // (number-sequence FROM &optional TO SEP) — FROM, FROM+SEP, … up to TO
    // (down to it for a negative SEP; default SEP 1). Without TO, or with TO =
    // FROM, it is (FROM); a range SEP never reaches is nil. Integers only
    // (Emacs also takes floats); a zero SEP is Emacs's error.
    ctx.defun(
        "number-sequence",
        |from: i64, to: Option<i64>, sep: Option<i64>| -> Result<TulispObject, Error> {
            let Some(to) = to.filter(|&to| to != from) else {
                return Ok(TulispObject::from(vec![from]));
            };
            let sep = sep.unwrap_or(1);
            if sep == 0 {
                return Err(Error::lisp_error(
                    "The increment can not be zero".to_string(),
                ));
            }
            let mut items = Vec::new();
            let mut n = from;
            while (sep > 0 && n <= to) || (sep < 0 && n >= to) {
                items.push(n);
                match n.checked_add(sep) {
                    Some(next) => n = next,
                    None => break,
                }
            }
            Ok(TulispObject::from(items))
        },
    );

    // (fboundp SYMBOL) — t when SYMBOL names a function, macro or special form.
    // tulisp keeps one value cell per symbol, so unlike Emacs, a variable
    // holding a lambda counts too.
    ctx.defun(
        "fboundp",
        |ctx: &mut TulispContext, sym: TulispObject| -> Result<bool, Error> {
            // A symbol that make-symbol made is not the one its name interns,
            // so it is asked about its own value. tulisp can only ask that
            // value whether it is a function, so a macro or special form held
            // there answers nil.
            let name = sym.symbol_name()?;
            if ctx.intern(&name).eq(&sym) {
                Ok(ctx.fboundp(&name))
            } else {
                Ok(sym.functionp(ctx))
            }
        },
    );

    // Helpers that call back into Lisp live as Lisp: a Rust defun that funcalls
    // a compiled predicate deadlocks (see tulisp's prelude.lisp).
    // format-message curves the format string's quotes as Emacs's default
    // text-quoting-style does, and user-error signals tulisp's `user-error`
    // with the message format-message makes, as Emacs's does.
    ctx.eval_string(
        r#"(progn
  (defun seq-remove (pred seq)
    (let ((out nil))
      (dolist (item seq)
        (unless (funcall pred item)
          (setq out (cons item out))))
      (reverse out)))
  (defun seq-uniq (seq &optional testfn)
    (let ((out nil))
      (dolist (item seq)
        (let ((dup nil))
          (dolist (kept out)
            (when (if testfn (funcall testfn kept item) (equal kept item))
              (setq dup t)))
          (unless dup
            (setq out (cons item out)))))
      (reverse out)))
  (defun format-message (fmt &rest args)
    (apply #'format (string-replace "'" "’" (string-replace "`" "‘" fmt)) args))
  (defun user-error (fmt &rest args)
    (signal 'user-error (list (apply #'format-message fmt args)))))"#,
    )
    .expect("the subr Lisp helpers define");
}

#[cfg(test)]
mod tests {
    use tulisp::TulispContext;

    /// Eval a program in a context with the core library and print the result
    /// the way tulisp does.
    fn p(prog: &str) -> String {
        let mut ctx = TulispContext::new();
        super::register(&mut ctx);
        ctx.eval_string(prog).unwrap().to_string()
    }

    #[test]
    fn push_conses_onto_a_variable() {
        assert_eq!(p("(let ((l nil)) (push 1 l) (push 2 l) l)"), "(2 1)");
        // The value is the new list, as with setq.
        assert_eq!(p("(let ((l nil)) (push 1 l))"), "(1)");
        assert_eq!(
            p("(macroexpand '(push 1 my-list))"),
            "(setq my-list (cons 1 my-list))"
        );
        // Inside a defun body (the compiled path) too.
        assert_eq!(
            p(r#"(defun f () (let ((l nil)) (push "a" l) (push "b" l) l)) (f)"#),
            r#"("b" "a")"#
        );
    }

    #[test]
    fn identity_returns_its_argument() {
        assert_eq!(p("(identity 5)"), "5");
        assert_eq!(p(r#"(identity "s")"#), r#""s""#);
        assert_eq!(p("(funcall 'identity '(1 2))"), "(1 2)");
        assert_eq!(p("(identity nil)"), "nil");
    }

    #[test]
    fn delete_dups_keeps_first_occurrences_in_place() {
        assert_eq!(p("(delete-dups (list 1 2 1 3 2))"), "(1 2 3)");
        // Destructive: the variable's list is the deduplicated one.
        assert_eq!(p("(let ((l (list 1 2 1))) (delete-dups l) l)"), "(1 2)");
        // `equal`, not `eq`: strings and lists compare by content.
        assert_eq!(
            p(r#"(delete-dups (list "a" "a" (list 1) (list 1)))"#),
            r#"("a" (1))"#
        );
        assert_eq!(p("(delete-dups nil)"), "nil");
        assert_eq!(p("(delete-dups (list 1))"), "(1)");
    }

    #[test]
    fn nreverse_and_butlast_match_emacs() {
        assert_eq!(p("(nreverse (list 1 2 3))"), "(3 2 1)");
        assert_eq!(p("(nreverse nil)"), "nil");
        assert_eq!(p("(butlast (list 1 2 3))"), "(1 2)");
        assert_eq!(p("(butlast (list 1 2 3) 2)"), "(1)");
        assert_eq!(p("(butlast (list 1 2) 0)"), "(1 2)");
        assert_eq!(p("(butlast (list 1 2) 5)"), "nil");
        assert_eq!(p("(butlast nil)"), "nil");
    }

    #[test]
    fn nreverse_and_butlast_reject_improper_lists() {
        // Emacs: (nreverse (cons 1 2)) and (butlast (cons 1 2)) both signal
        // (wrong-type-argument listp 2), naming the tail that is not a list.
        for call in ["(nreverse (cons 1 2))", "(butlast (cons 1 2))"] {
            assert_eq!(
                p(&format!("(condition-case e {call} (error e))")),
                "(wrong-type-argument listp 2)",
                "{call}"
            );
        }

        // n <= 0 short-circuits before the list is walked, as in Emacs.
        assert_eq!(p("(butlast (cons 1 2) 0)"), "(1 . 2)");
    }

    #[test]
    fn number_sequence_matches_emacs() {
        assert_eq!(p("(number-sequence 1 5)"), "(1 2 3 4 5)");
        assert_eq!(p("(number-sequence 5)"), "(5)");
        assert_eq!(p("(number-sequence 1 10 3)"), "(1 4 7 10)");
        assert_eq!(p("(number-sequence 5 1 -2)"), "(5 3 1)");
        assert_eq!(p("(number-sequence 5 1)"), "nil");
        assert_eq!(p("(number-sequence 1 1)"), "(1)");
        assert_eq!(p("(number-sequence 1 1 0)"), "(1)");
        assert_eq!(
            p("(number-sequence 9223372036854775806 9223372036854775807)"),
            "(9223372036854775806 9223372036854775807)"
        );
        let mut ctx = TulispContext::new();
        super::register(&mut ctx);
        let err = ctx.eval_string("(number-sequence 1 5 0)").unwrap_err();
        assert!(format!("{err:?}").contains("can not be zero"), "{err:?}");
    }

    #[test]
    fn seq_remove_and_seq_uniq_match_emacs() {
        assert_eq!(
            p("(seq-remove (lambda (x) (> x 2)) (list 1 2 3 4))"),
            "(1 2)"
        );
        assert_eq!(p("(seq-uniq (list 1 2 1 3 2))"), "(1 2 3)");
        assert_eq!(
            p("(seq-uniq (list 1 2 3 4) (lambda (a b) (= (% a 2) (% b 2))))"),
            "(1 2)"
        );
        // TESTFN is called (KEPT NEW), as Emacs does: 2 is dropped because (< 1
        // 2) holds, 1 is kept because (< 3 1) does not.
        assert_eq!(p("(seq-uniq (list 3 1 2) (lambda (a b) (< a b)))"), "(3 1)");
    }

    #[test]
    fn fboundp_is_true_for_callables_only() {
        assert_eq!(p("(fboundp 'identity)"), "t");
        assert_eq!(p("(fboundp 'if)"), "t");
        assert_eq!(p("(fboundp 'when)"), "t");
        assert_eq!(p("(fboundp 'no-such-fn-xyz)"), "nil");
        assert_eq!(p("(fboundp nil)"), "nil");
        // A symbol holding data, even data that prints like a function type.
        assert_eq!(p("(progn (setq fb-data 5) (fboundp 'fb-data))"), "nil");
        assert_eq!(p("(progn (setq fb-sym 'Func) (fboundp 'fb-sym))"), "nil");
        // A symbol make-symbol made answers for itself, not for the interned
        // symbol of the same name. Its value counts, as for any symbol in
        // tulisp; Emacs answers nil for the lambda.
        assert_eq!(p(r#"(fboundp (make-symbol "car"))"#), "nil");
        assert_eq!(
            p(r#"(let ((s (make-symbol "f"))) (set s (lambda () 1)) (fboundp s))"#),
            "t"
        );

        // Emacs: (fboundp 5) and (fboundp "identity") signal
        // (wrong-type-argument symbolp …); nil is itself a symbol, so it still
        // answers nil rather than erroring.
        assert_eq!(
            p("(condition-case e (fboundp 5) (error e))"),
            "(wrong-type-argument symbolp 5)"
        );
        assert_eq!(
            p(r#"(condition-case e (fboundp "identity") (error e))"#),
            r#"(wrong-type-argument symbolp "identity")"#
        );
    }
}
