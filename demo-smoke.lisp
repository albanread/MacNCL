;;;; Headless smoke for ONE GUI demo. Edit *demo-name* below.
;;;;
;;;; Strategy: load the demo, spawn a 3-second watchdog that calls
;;;; (igui-quit), then call the demo's run- entrypoint on the main
;;;; thread. Each demo's event loop should return :done when iGui
;;;; shuts down. Prints "SMOKE-RESULT <name> => <result>" so the
;;;; harness can grep it.

(require 'threads)

(defparameter *demos-to-smoke*
  '(("hello-igui"    . run-hello-igui)
    ("shapes"        . run-shapes)
    ("text-styles"   . run-text-styles)
    ("buttons"       . run-buttons)
    ("paint-and-log" . run-paint-and-log)
    ("draw-square"   . run-draw-square)
    ("click-counter" . run-click-counter)
    ("gui-repl"      . run-gui-repl)
    ("bouncing"      . run-bouncing)))

;; *demo-name* MUST be defparameter'd by a prior --load before this file.

(let* ((name *demo-name*)
       (entry (cdr (assoc name *demos-to-smoke* :test #'equal)))
       (path  (format nil "E:/CL/NewCormanLisp/Lisp/demos/~A.lisp" name)))
  (cond
    ((null entry)
     (format t "SMOKE-RESULT ~A => UNKNOWN-DEMO~%" name))
    (t
     (load path)
     (create-thread
       (lambda ()
         (sleep 2)
         (igui-quit))
       :report-when-finished nil)
     (handler-case
         (let ((result (funcall (symbol-function entry))))
           (format t "SMOKE-RESULT ~A => ~A~%" name result))
       (error (c)
         (format t "SMOKE-RESULT ~A => ERROR ~A~%" name c))))))
nil
