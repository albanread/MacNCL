;;;; native-repl.lisp — native graphical REPL using the Rust-implemented
;;;; repl_child MDI window.
;;;;
;;;; The window handles all editing, history, syntax colouring, paren
;;;; balance checking, and rendering in Rust.  This file is the thin
;;;; Lisp worker: open the window, wait for :REPL-SUBMIT events, evaluate
;;;; the form, and push the result back via (repl-output) / (repl-error).

(defun run-native-repl ()
  (let ((id (open-repl-window "NewCormanLisp")))
    (unless id
      (format t "** open-repl-window failed — is the iGui frame up?~%")
      (return-from run-native-repl :failed))

    (event-loop-for id
      (:frame-close (return :done))
      (:close       (return :done))

      ;; A complete Lisp form has been entered and submitted.
      (:repl-submit
       (let ((src (repl-pop-input id)))
         (when src
           (handler-case
               (let ((result (eval-string src)))
                 (repl-output id (or result "")))
             (error (c)
               (repl-error id (format nil "~A" c))))))))))
