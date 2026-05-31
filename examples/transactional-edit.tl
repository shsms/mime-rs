;; transactional-edit.tl — several edits that all land, or none do.
;;
;; Run:  mimectl run --local examples/transactional-edit.tl --file SOMEFILE
;;
;; `with-transaction` snapshots the buffer, runs its body, and — if the body
;; signals an error — restores the snapshot, so a multi-step edit is atomic:
;; either every step applies or the buffer is left exactly as it was. Here we
;; wrap a three-step rewrite (a header line, a normalisation, and a footer) in
;; one transaction. The `(unless ...)(error ...)` is a guard: if the document is
;; missing the marker we expect, we abort and the whole transaction unwinds,
;; leaving the file untouched — no half-applied edit to clean up.
;;
;; `condition-case` catches that abort so the *program* still finishes cleanly
;; and reports whether the edit committed, instead of failing the whole run.
(condition-case e
    (with-transaction
      ;; Step 1: require an anchor, or roll the whole thing back.
      (goto-char (point-min))
      (unless (search-forward "BEGIN" nil t)
        (error "no BEGIN anchor; aborting the transaction"))
      ;; Step 2: stamp a header right after the anchor.
      (insert "\n;; processed by mime-rs\n")
      ;; Step 3: normalise tabs-to-spaces across the buffer.
      (goto-char (point-min))
      (replace-regexp "\t" "    ")
      (report "committed" 1))
  (error
   ;; The transaction already rolled the buffer back; just say so. The caught
   ;; condition `e` is a (SYMBOL . MESSAGE) cons, so (cdr e) is the message.
   (report "committed" 0)
   (report "reason" (cdr e))))
