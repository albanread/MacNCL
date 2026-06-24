;;;; Lisp/Library/advice.lisp — function advice (before/after/around).
;;;;
;;;; Ported from Corman Lisp's Modules/advice.lisp by Vassili Bykov.
;;;; Adapted for NCL: macrolet replaced by flet; destructuring-bind in
;;;; generated code replaced by (apply (lambda ...)); in-package / export
;;;; stripped (NCL has a flat namespace).
;;;;
;;;; Provides:
;;;;   advise    SYMBOL (LAMBDA-LIST) BODY...
;;;;             — wrap SYMBOL's function with new advice code.
;;;;               LAMBDA-LIST mirrors the function's arg list.
;;;;               Inside BODY, call (call-advised-function) to invoke
;;;;               the original.
;;;;
;;;;   unadvise  SYMBOL...
;;;;             — remove advice from each SYMBOL (or all if none given).
;;;;
;;;;   symbol-function-advised-p SYMBOL
;;;;             — T if SYMBOL is currently being advised.
;;;;
;;;;   call-advised-function
;;;;             — available as a local function inside an advise body;
;;;;               calls the original function with the same args.
;;;;
;;;; Requires: symbols.lisp  (get / remprop / setf get)
;;;;           symbol-function, setf symbol-function (built-in shims)

;; ── Internal state ─────────────────────────────────────────────────────────

(defvar *advised-symbols* nil
  "List of symbols that are currently being advised.")

;; ── Core registration / removal ────────────────────────────────────────────

(defun %register-advice (symbol advice-fn)
  "Install ADVICE-FN as the wrapper for SYMBOL.
   The original function is stashed on SYMBOL's property list and the
   symbol's function cell is replaced with a closure that calls ADVICE-FN."
  (unless (symbolp symbol)
    (error "advise: not a symbol: ~S" symbol))
  (let ((original (or (get symbol 'advice-original)
                      (symbol-function symbol))))
    (unless original
      (error "advise: ~A has no function binding" symbol))
    (pushnew symbol *advised-symbols* :test #'eq)
    (setf (get symbol 'advice-original) original)
    (setf (symbol-function symbol)
          (lambda (&rest args)
            (funcall advice-fn original args))))
  *advised-symbols*)

(defun %unregister-advice (symbol)
  "Remove advice from SYMBOL, restoring the original function."
  (let ((original (get symbol 'advice-original)))
    (when original
      (setq *advised-symbols* (delete symbol *advised-symbols* :test #'eq))
      (remprop symbol 'advice-original)
      (setf (symbol-function symbol) original)))
  *advised-symbols*)

(defun %unadvise-list (symbols)
  "Remove advice from each symbol in SYMBOLS."
  (dolist (s symbols)
    (unless (symbolp s)
      (error "unadvise: not a symbol: ~S" s))
    (%unregister-advice s)))

;; ── Public API ─────────────────────────────────────────────────────────────

(defun symbol-function-advised-p (symbol)
  "Return non-NIL if SYMBOL's function is currently advised."
  (get symbol 'advice-original))

(defmacro advise (&rest whole)
  "Wrap SYMBOL's function with advice code.

   Usage: (advise SYMBOL (LAMBDA-LIST) BODY...)

   LAMBDA-LIST mirrors the target function's argument list.  Inside BODY:
     * ADVISED-FUNCTION  — the original function object
     * (call-advised-function) — apply the original to the same args

   Installing advice on an already-advised symbol replaces the old advice.

   With no arguments, (advise) returns *ADVISED-SYMBOLS*."
  (if (null whole)
      '*advised-symbols*
      (let ((sym         (car whole))
            (lambda-list (cadr whole))
            (body        (cddr whole)))
        `(%register-advice
          ',sym
          (lambda (advised-function %all-arguments)
            (flet ((call-advised-function ()
                     (apply advised-function %all-arguments)))
              (apply (lambda ,lambda-list ,@body)
                     %all-arguments)))))))

(defmacro unadvise (&rest symbols)
  "Remove advice from each named SYMBOL.
   With no arguments, removes advice from all advised symbols."
  (if symbols
      `(%unadvise-list ',symbols)
      '(%unadvise-list *advised-symbols*)))

(provide 'advice)
nil
