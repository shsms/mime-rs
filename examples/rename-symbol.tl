;; rename-symbol.tl — rename one identifier everywhere, whole-word only.
;;
;; Run:  mime run examples/rename-symbol.tl --file SOMEFILE
;;
;; Renames the symbol `oldName` to `newName`. The `\b` word boundaries are what
;; make this a *symbol* rename rather than a blind substring replace: `oldName`
;; matches, but `oldNameSuffix`, `prefixOldName`, and `oldName_2` do NOT (RE2
;; treats `_` and alphanumerics as word constituents, so `\b` sits only at true
;; identifier edges). `replace-regexp` returns the count, which we report.
;;
;; NOTE on escaping: inside a tulisp string each backslash is written doubled,
;; so the regexp `\boldName\b` is the literal string "\\boldName\\b" below.
(goto-char (point-min))
(report "renamed" (replace-regexp "\\boldName\\b" "newName"))
