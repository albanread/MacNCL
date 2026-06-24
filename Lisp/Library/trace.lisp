;;;; Lisp/Library/trace.lisp
;;;;
;;;; Function-call tracing, ported from Corman's Sys/trace.lisp.
;;;; The mechanism is pure Lisp on top of the symbol-function and
;;;; method-function indirection layers — no compiler or runtime
;;;; hooks. Tracing `foo`:
;;;;
;;;;   1. Save the current `(symbol-function 'foo)` as ORIG.
;;;;   2. Install a wrapping closure that prints args, calls ORIG,
;;;;      prints the result, and returns the result.
;;;;   3. Record (foo . ORIG) on `*traced-functions*` so UNTRACE
;;;;      can put ORIG back.
;;;;
;;;; CLOS generic functions get the same treatment one method at a
;;;; time: each method's `method-function` slot is replaced with a
;;;; wrapper. The per-GF dispatch cache (`classes-to-emf-table`)
;;;; is cleared so the next call hits the wrappers.
;;;;
;;;; Macros and special operators are NOT traceable in v1 —
;;;; macro-function isn't exposed yet and special-operator-p
;;;; doesn't exist. Trying to trace one signals an error.
;;;;
;;;; Usage:
;;;;
;;;;   (require 'trace)
;;;;   (defun fact (n) (if (<= n 1) 1 (* n (fact (- n 1)))))
;;;;   (trace fact)
;;;;   (fact 4)
;;;;   ;; =>(FACT 4)
;;;;   ;;   =>(FACT 3)
;;;;   ;;     =>(FACT 2)
;;;;   ;;       =>(FACT 1)
;;;;       (FACT 1)=> 1
;;;;     (FACT 2)=> 2
;;;;   (FACT 3)=> 6
;;;; (FACT 4)=> 24
;;;;   (untrace fact)
;;;;
;;;; Limits inherited from v1:
;;;;   * Without per-thread dynamic-variable bindings, *trace-level*
;;;;     and *trace-enabled* are process-global. Tracing while
;;;;     multiple Lisp threads call traced functions concurrently
;;;;     produces interleaved output and slightly confused
;;;;     indentation. Single-thread REPL use is fine.
;;;;   * No unwind-protect yet, so a non-local exit out of a
;;;;     traced function leaves the trace-level counter elevated.
;;;;     `(reset-trace)` re-zeroes it.

;; ── State ───────────────────────────────────────────────────────────────

(defparameter *trace-enabled* t
  "When NIL, traced functions call through silently. Re-bound by
   the wrapper itself to NIL while it's printing, so the print
   path doesn't recursively trace its own helpers.")

(defparameter *trace-level* 0
  "Depth counter for indentation. Manually bumped on enter,
   decremented on exit. Not perfectly correct under non-local
   exits — `(reset-trace)` resets.")

(defparameter *max-trace-level* 15
  "Tracing stops indenting (and pretty-printing args) once
   recursion exceeds this depth.")

(defparameter *traced-functions* nil
  "Alist of (NAME . ORIGINAL). For generic functions ORIGINAL is
   a list of (METHOD ORIG-FN METHOD ORIG-FN ...) pairs so untrace
   can put every method back.")

(defparameter *trace-output* t
  "Stream for trace output. T means *standard-output*. Override
   to capture trace to a file or string-output-stream.")

(defparameter *untraceable*
  '(apply funcall eval lambda)
  "Functions that, if wrapped, recurse infinitely. Tracing any
   of them is rejected with an error.")

;; ── Trace-disabled scope (best-effort without unwind-protect) ─────────

(defmacro with-trace-disabled (&rest body)
  "Run BODY with *trace-enabled* forced to NIL, restoring on
   normal exit. A non-local exit will leak the disabled state;
   callers that need stricter guarantees should pair this with
   their own reset."
  (let ((saved (gensym "SAVED-")))
    `(let ((,saved *trace-enabled*))
       (setq *trace-enabled* nil)
       (let ((result (progn ,@body)))
         (setq *trace-enabled* ,saved)
         result))))

;; ── Output helpers ──────────────────────────────────────────────────────

(defun %trace-indent ()
  (dotimes (i *trace-level*)
    (format *trace-output* "  ")))

(defun %trace-enter (name args)
  (with-trace-disabled
    (%trace-indent)
    (format *trace-output* "=>(~A~{ ~S~})~%" name args)))

(defun %trace-leave (name args result)
  (with-trace-disabled
    (%trace-indent)
    (format *trace-output* "(~A~{ ~S~})=> ~S~%" name args result)))

(defun reset-trace ()
  "Force *trace-level* and *trace-enabled* back to a clean state.
   Useful when a non-local exit out of a traced function has
   left the counter elevated."
  (setq *trace-level* 0)
  (setq *trace-enabled* t))

;; ── Function wrapping ───────────────────────────────────────────────────

(defun %make-trace-wrapper (name orig)
  "Build the closure that gets installed in NAME's function cell."
  (lambda (&rest args)
    (cond
      ((not *trace-enabled*)
       (apply orig args))
      ((>= *trace-level* *max-trace-level*)
       ;; Bottomed out — silently pass through to avoid drowning
       ;; the output in recursion-noise.
       (apply orig args))
      (t
       (%trace-enter name args)
       (let ((saved-level *trace-level*))
         (setq *trace-level* (+ saved-level 1))
         (let ((result (apply orig args)))
           (setq *trace-level* saved-level)
           (%trace-leave name args result)
           result))))))

(defun %register-traced-function (name)
  (let ((orig (symbol-function name)))
    (setf (symbol-function name) (%make-trace-wrapper name orig))
    (setq *traced-functions*
          (cons (cons name orig) *traced-functions*))
    name))

(defun %unregister-traced-function (name)
  (let ((cell (assoc name *traced-functions*)))
    (when cell
      (setf (symbol-function name) (cdr cell))
      (setq *traced-functions*
            (%remove-alist name *traced-functions*)))))

(defun %remove-alist (key alist)
  (cond
    ((null alist) nil)
    ((eq (car (car alist)) key) (cdr alist))
    (t (cons (car alist) (%remove-alist key (cdr alist))))))

;; ── Generic-function wrapping ──────────────────────────────────────────

(defun %register-traced-generic (name)
  "Replace every method's function with a wrapper that announces
   itself, then clear the GF's emf cache so the next call hits
   the wrappers. The replaced functions are recorded as a
   (method orig-fn method orig-fn …) list so untrace can
   reverse the swap exactly."
  (let ((gf (symbol-function name))
        (saved nil))
    (dolist (method (generic-function-methods gf))
      (let ((orig (method-function method)))
        (setq saved (cons orig (cons method saved)))
        (setf (slot-value method 'function)
              (%make-trace-wrapper name orig))))
    (clear-method-table (classes-to-emf-table gf))
    (setq *traced-functions*
          (cons (cons name (reverse saved)) *traced-functions*))
    name))

(defun %unregister-traced-generic (name)
  (let ((cell (assoc name *traced-functions*)))
    (when cell
      (let ((pairs (cdr cell))
            (gf (symbol-function name)))
        ;; Walk pairs two at a time: (METHOD ORIG METHOD ORIG …).
        (loop
          (when (null pairs) (return))
          (let ((method (car pairs))
                (orig (car (cdr pairs))))
            (setf (slot-value method 'function) orig))
          (setq pairs (cdr (cdr pairs))))
        (clear-method-table (classes-to-emf-table gf)))
      (setq *traced-functions*
            (%remove-alist name *traced-functions*)))))

;; ── Dispatch by kind ────────────────────────────────────────────────────

(defun %traced-already-p (name)
  (assoc name *traced-functions*))

(defun %register-trace-1 (name)
  (cond
    ((not (symbolp name))
     (error "Not a symbol: ~A" name))
    ((null name)
     (error "NIL cannot be traced"))
    ((member name *untraceable*)
     (error "~A cannot be traced" name))
    ((not (fboundp name))
     (error "~A has no function binding" name))
    ((%traced-already-p name)
     nil)
    ((standard-generic-function-p (symbol-function name))
     (%register-traced-generic name))
    (t
     (%register-traced-function name))))

(defun %unregister-trace-1 (name)
  (let ((cell (%traced-already-p name)))
    (when cell
      (cond
        ((standard-generic-function-p (symbol-function name))
         (%unregister-traced-generic name))
        (t
         (%unregister-traced-function name))))))

;; ── Public API ──────────────────────────────────────────────────────────

(defmacro trace (&rest names)
  "Install trace wrappers for each NAME. With no NAMES, returns
   the current list of traced names."
  (cond
    ((null names) `(mapcar #'car *traced-functions*))
    (t `(with-trace-disabled
          ,@(mapcar (lambda (n) `(%register-trace-1 ',n)) names)
          (mapcar #'car *traced-functions*)))))

(defmacro untrace (&rest names)
  "Remove trace wrappers from each NAME. With no NAMES, untrace
   every traced function."
  (cond
    ((null names)
     `(with-trace-disabled
        (dolist (n (mapcar #'car *traced-functions*))
          (%unregister-trace-1 n))
        nil))
    (t `(with-trace-disabled
          ,@(mapcar (lambda (n) `(%unregister-trace-1 ',n)) names)
          (mapcar #'car *traced-functions*)))))

(provide 'trace)
nil
