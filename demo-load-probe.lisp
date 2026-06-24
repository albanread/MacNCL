;;;; Load every demo file in turn and report which (run-...) entrypoint
;;;; was defined. Catches compile-time / load-time errors per demo.

(defparameter *demos*
  ;; (filename-stem . expected-entrypoint-symbol)
  '(("hello-igui"    . run-hello-igui)
    ("shapes"        . run-shapes)
    ("text-styles"   . run-text-styles)
    ("buttons"       . run-buttons)
    ("gui-repl"      . run-gui-repl)
    ("bouncing"      . run-bouncing)
    ("draw-square"   . run-draw-square)
    ("paint-and-log" . run-paint-and-log)
    ("click-counter" . run-click-counter)
    ("heap-monitor"  . run-heap-monitor)
    ;; clos-tour has no run-* entrypoint; runs at load time.
    ("clos-tour"     . nil)))

(defparameter *demo-dir* "E:/CL/NewCormanLisp/Lisp/demos/")

(defparameter *results* nil)

(defun probe-demo (entry)
  (let* ((name    (car entry))
         (run-sym (cdr entry))
         (path    (concatenate 'string *demo-dir* name ".lisp")))
    (handler-case
        (let ((out (make-string-output-stream)))
          ;; Swallow any output the demo writes at load time so we can
          ;; report cleanly after the loop.
          (let ((*standard-output* out))
            (load path))
          (push (list name :ok
                      (cond ((null run-sym) :none)
                            ((fboundp run-sym) :defined)
                            (t :missing)))
                *results*))
      (error (c)
        (push (list name :error (format nil "~A" c)) *results*)))))

(dolist (d *demos*) (probe-demo d))

(format t "~%~%── demo load summary ──~%")
(dolist (r (reverse *results*))
  (cond
    ((eq (cadr r) :ok)
     (format t "  ~22A load: OK   run-entry: ~A~%"
             (car r)
             (case (caddr r)
               (:defined "DEFINED")
               (:missing "MISSING")
               (:none    "—"))))
    (t
     (format t "  ~22A load: FAIL ~A~%" (car r) (caddr r)))))
nil
