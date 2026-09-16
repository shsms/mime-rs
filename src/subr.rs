//! Core library (the Emacs `subr.el` corner) — list and function helpers the
//! tulisp base lacks: `push`, `identity`, `delete-dups`. Pure Lisp-value
//! functions with no buffer state, registered in every tier alongside the
//! string library. Semantics are checked against GNU Emacs 30 in the tests.
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
}
