;;;; Lisp/Library/scratch.lisp — playground for hot-reload smoke testing.
;;;;
;;;; Used by the hot-reload demo:
;;;;
;;;;   ncl> (require 'scratch)
;;;;   ncl> (start-hot-reload)
;;;;   ncl> (scratch-message)
;;;;   "v1"
;;;;   ;;; edit this file, save it ...
;;;;   ncl> (scratch-message)
;;;;   "v2"   ; reloaded automatically between prompts
;;;;
;;;; Try saving a syntactically-broken version (unbalanced paren,
;;;; unterminated string) — the SKIP message fires and the
;;;; previous definition stays in place. No partial-state from
;;;; a half-saved file.

(defun scratch-message () "v1")
(provide 'scratch)
