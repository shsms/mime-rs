//! Core library (the Emacs `subr.el` corner) — list and function helpers the
//! tulisp base lacks: push, identity, delete-dups, nreverse, butlast,
//! number-sequence, fboundp, seq-remove, seq-uniq, format-message, user-error.
//! Pure Lisp-value functions with no buffer state, registered in every tier
//! alongside the string library. Semantics are checked against GNU Emacs 30 in
//! the tests.
use tulisp::{Error, TulispContext, TulispObject, destruct_bind, list};

pub fn register(ctx: &mut TulispContext) {
    // (push NEWELT PLACE) — macro expanding to (setq PLACE (cons NEWELT
    // PLACE)).  PLACE must be a symbol; Emacs's generalized places (setf-able
    // forms) are not supported.
    ctx.defmacro("push", |ctx, args| {
        destruct_bind!((newelt place) = args);
        list!(
            ,ctx.intern("setq")
            ,place.clone()
            ,list!(,ctx.intern("cons") ,newelt ,place)?
        )
    });

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
    ctx.defun(
        "nreverse",
        |list: TulispObject| -> Result<TulispObject, Error> {
            Ok(list_items(&list)?.into_iter().rev().collect())
        },
    );

    // (butlast LIST &optional N) — LIST without its last N elements (default
    // 1), as a fresh list; N <= 0 returns LIST itself, N >= its length nil.
    ctx.defun(
        "butlast",
        |list: TulispObject, n: Option<i64>| -> Result<TulispObject, Error> {
            let n = n.unwrap_or(1);
            if n <= 0 {
                return Ok(list);
            }
            let mut items = list_items(&list)?;
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
    // tulisp keeps one value cell per symbol, so this is "bound, and bound to
    // something callable": unlike Emacs, a variable holding a lambda counts
    // too. Function values print as their kind (`Defun`, `Func`, …); a symbol
    // that merely prints that way is excluded by `symbolp`.
    ctx.defun("fboundp", |sym: TulispObject| -> Result<bool, Error> {
        // nil is itself a symbol in Emacs (unlike tulisp's own `Nil` variant,
        // which `symbolp` excludes) — answer nil for it rather than falling
        // into the non-symbol error below.
        if sym.null() {
            return Ok(false);
        }
        if !sym.symbolp() {
            return Err(Error::type_mismatch(format!(
                "Wrong type argument: symbolp, {sym}"
            )));
        }
        if !sym.boundp() {
            return Ok(false);
        }
        let value = sym.get()?;
        Ok(!value.symbolp()
            && matches!(
                value.to_string().as_str(),
                "Func" | "Defun" | "Macro" | "Defmacro" | "Lambda" | "CompiledDefun"
            ))
    });

    // Helpers that call back into Lisp live as Lisp: a Rust defun that funcalls
    // a compiled predicate deadlocks (see tulisp's prelude.lisp).
    // format-message curves the format string's quotes as Emacs's default
    // text-quoting-style does, and user-error signals a plain `error` with the
    // formatted message (a condition-case arm for `user-error` itself will not
    // catch it).
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
    (error (apply #'format-message fmt args))))"#,
    )
    .expect("the subr Lisp helpers define");
}

/// The elements of a proper list, in order.  Errors `wrong-type-argument listp`
/// on an improper (dotted) list, as Emacs's `length` does — `butlast` walks a
/// copy the same way internally.
fn list_items(list: &TulispObject) -> Result<Vec<TulispObject>, Error> {
    let mut items = Vec::new();
    let mut cur = list.clone();
    while cur.consp() {
        items.push(cur.car()?);
        cur = cur.cdr()?;
    }
    if !cur.null() {
        return Err(Error::type_mismatch(format!(
            "Wrong type argument: listp, {list}"
        )));
    }
    Ok(items)
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
        // (wrong-type-argument listp …) — butlast walks the same way internally
        // (via `length`).
        let mut ctx = TulispContext::new();
        super::register(&mut ctx);
        let e = ctx.eval_string("(nreverse (cons 1 2))").unwrap_err();
        assert!(format!("{e:?}").contains("listp"), "{e:?}");

        let mut ctx = TulispContext::new();
        super::register(&mut ctx);
        let e = ctx.eval_string("(butlast (cons 1 2))").unwrap_err();
        assert!(format!("{e:?}").contains("listp"), "{e:?}");

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

        // Emacs: (fboundp 5) and (fboundp "identity") signal
        // (wrong-type-argument symbolp …); nil is itself a symbol, so it still
        // answers nil rather than erroring.
        let mut ctx = TulispContext::new();
        super::register(&mut ctx);
        let e = ctx.eval_string("(fboundp 5)").unwrap_err();
        assert!(format!("{e:?}").contains("symbolp"), "{e:?}");

        let mut ctx = TulispContext::new();
        super::register(&mut ctx);
        let e = ctx.eval_string(r#"(fboundp "identity")"#).unwrap_err();
        assert!(format!("{e:?}").contains("symbolp"), "{e:?}");
    }
}
