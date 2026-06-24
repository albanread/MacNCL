;;;; Lisp/Library/hello.lisp
;;;;
;;;; Trivial library to prove `(require 'hello)` round-trips through
;;;; the loader. Resolves via *load-path* → Library/hello.lisp.

(defun hello (&optional (name "world"))
  "Print a greeting. Used as a smoke test for the loader."
  (format t "Hello, ~A!~%" name))

(provide 'hello)
