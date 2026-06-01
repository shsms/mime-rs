;; delete-trailing-whitespace.tl — strip trailing spaces/tabs from every line.
;;
;; Run:  mime run --local examples/delete-trailing-whitespace.tl --file SOMEFILE
;;
;; `delete-trailing-whitespace` is a builtin that scans the whole buffer and
;; removes any [ \t]+ before each line end. We also count what we cleaned first
;; so the run reports how many lines were touched — handy when an agent wants a
;; number, not just a diff. The `(?m)` flag makes `$` match at every line end
;; (not just end of buffer); without it the count would only see the final line.
;; `count-matches` saves and restores point, so the cleanup pass starts fresh.
(goto-char (point-min))
(report "lines-cleaned" (count-matches "(?m)[ \t]+$"))
(delete-trailing-whitespace)
