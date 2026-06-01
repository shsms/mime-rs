;; markers-viewport.tl — durable positions + multiple viewports.
;;
;; Run:  mime run --local examples/markers-viewport.tl --file SOMEFILE
;;
;; A *marker* is a position that rides edits: insert text before it and the
;; marker's position shifts to stay on the same character. This is the
;; multi-cursor primitive — set markers at several points of interest, edit
;; freely, and each marker still points where you meant. `window` renders a
;; small text viewport (a few lines, with a `‸` at the focus column) around any
;; position, so several `window` calls give you several simultaneous views — the
;; agent's eyes on more than one place at once.
;;
;; This program finds the first and last lines, drops a marker on each, inserts a
;; banner at the very top (which pushes both markers down), and then reports each
;; marker's new position and a viewport around it — proving the markers tracked
;; the edit. Works on any multi-line file.
(goto-char (point-min))
(end-of-line)
(let ((top (point-marker)))                 ; marker on line 1
  (goto-char (point-max))
  (beginning-of-line)
  (let ((bot (point-marker)))               ; marker on the last line
    (report "top-before" (marker-position top))
    (report "bot-before" (marker-position bot))
    ;; Insert a banner at the top; both markers slide forward by its length.
    (goto-char (point-min))
    (insert ";; --- annotated by mime-rs ---\n")
    (report "top-after" (marker-position top))
    (report "bot-after" (marker-position bot))
    ;; Two viewports, one per marker — the simultaneous multi-window view.
    (message (window 1 (marker-position top)))
    (message (window 1 (marker-position bot)))))
