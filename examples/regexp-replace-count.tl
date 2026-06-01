;; regexp-replace-count.tl — bulk regexp replace, reporting how many hits.
;;
;; Run:  mime run --local examples/regexp-replace-count.tl --file SOMEFILE
;;
;; Replaces every run of digits with "#" and reports the count. `replace-regexp`
;; (the buffer-level bulk command) does the whole pass from point to end of
;; buffer in one call and *returns the number of replacements* — so we just
;; `report` that. This is the map-shaped edit; for the sequential, hand-rolled
;; form see the (while (re-search-forward ...) (replace-match ...)) loop in
;; uppercase.tl.
(goto-char (point-min))
(report "replaced" (replace-regexp "[0-9]+" "#"))
