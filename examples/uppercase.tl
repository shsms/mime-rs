;; uppercase.tl — replace every "world" with "WORLD" and report the count.
;; Run:  mime run --local examples/uppercase.tl --file <some-file>
;; A map-shaped edit expressed as the Emacs sequential loop (parallel
;; `replace-all` arrives in M6).
(goto-char (point-min))
(let ((n 0))
  (while (re-search-forward "world" nil t)
    (replace-match "WORLD")
    (setq n (1+ n)))
  (report "replaced" n))
