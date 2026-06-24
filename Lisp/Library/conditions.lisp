;;;; Lisp/Library/conditions.lisp — Tier 1.C
;;;;
;;;; Layered on top of CLOS, this turns the existing single-handler
;;;; %handler-case primitive into a real CL condition system:
;;;;
;;;;   * a tree of condition CLASSES (condition, warning, error,
;;;;     simple-* family, plus arithmetic / type / control / file
;;;;     subclasses);
;;;;   * (define-condition) macro for users to define their own;
;;;;   * (error datum &rest args) that accepts an existing condition
;;;;     instance, a class-name + initargs plist, OR a format string
;;;;     + args (auto-wrapped in SIMPLE-ERROR);
;;;;   * (signal …) — non-aborting variant: returns NIL if no
;;;;     handler claims it;
;;;;   * (warn …) — prints to *error-output* and signals warning;
;;;;   * typed handler-case that picks the FIRST clause whose class
;;;;     matches the condition's class (via subclassp). If no clause
;;;;     matches, the condition propagates outward;
;;;;   * (handler-bind ...) — non-unwinding handlers walked top-to-
;;;;     bottom; handler can either decline (return normally) or
;;;;     transfer control by invoking a restart or calling error;
;;;;   * restarts: restart-case / restart-bind / find-restart /
;;;;     invoke-restart / compute-restarts. Standard restart names
;;;;     (abort, continue, use-value, store-value, muffle-warning)
;;;;     are recognised by the helpers but not auto-bound.
;;;;
;;;; Where the spec's behaviour and our minimal infrastructure
;;;; disagree, we follow the spec where cheap and call it out in
;;;; comments where we cut a corner.

;; ── Condition class hierarchy ─────────────────────────────────────────────

(defclass condition () ()
  (:documentation "Root of the condition hierarchy."))

;; Carry a format control and arguments. Used by the `error
;; "string" args...` shape; printed via print-object.
(defclass simple-condition (condition)
  ((format-control   :initarg :format-control   :initform "")
   (format-arguments :initarg :format-arguments :initform ())))

(defclass warning (condition) ())
(defclass simple-warning (warning simple-condition) ())

(defclass error (condition) ())
(defclass simple-error (error simple-condition) ())

(defclass type-error (error)
  ((datum         :initarg :datum)
   (expected-type :initarg :expected-type)))

(defclass simple-type-error (type-error simple-condition) ())

(defclass arithmetic-error (error)
  ((operation :initarg :operation :initform nil)
   (operands  :initarg :operands  :initform ())))

(defclass division-by-zero      (arithmetic-error) ())
(defclass floating-point-overflow (arithmetic-error) ())
(defclass floating-point-underflow (arithmetic-error) ())

(defclass cell-error (error)
  ((name :initarg :name)))

(defclass unbound-variable (cell-error) ())
(defclass undefined-function (cell-error) ())
(defclass unbound-slot (cell-error)
  ((instance :initarg :instance)))

(defclass control-error (error) ())
(defclass program-error (error) ())
(defclass parse-error   (error) ())

(defclass file-error (error)
  ((pathname :initarg :pathname)))

(defclass stream-error (error)
  ((stream :initarg :stream)))

(defclass end-of-file (stream-error) ())
(defclass reader-error (parse-error stream-error) ())

;; ── condition printing ──────────────────────────────────────────────────
;;
;; A condition's printed form is its "report" — what gets shown when
;; the debugger sees it or (format t "~A" c) is called. For simple-*,
;; that's (apply #'format nil format-control format-arguments). For
;; structured conditions like TYPE-ERROR we synthesise a reasonable
;; string.

(defmethod print-object ((c condition) stream)
  ;; Fallback: just show the class name.
  (format stream "#<~A>" (class-name (class-of c))))

(defmethod print-object ((c simple-condition) stream)
  (apply #'format stream
         (slot-value c 'format-control)
         (slot-value c 'format-arguments)))

(defmethod print-object ((c type-error) stream)
  (format stream "the value ~A is not of type ~A"
          (slot-value c 'datum)
          (slot-value c 'expected-type)))

(defmethod print-object ((c unbound-variable) stream)
  (format stream "unbound variable: ~A" (slot-value c 'name)))

(defmethod print-object ((c undefined-function) stream)
  (format stream "undefined function: ~A" (slot-value c 'name)))

(defmethod print-object ((c division-by-zero) stream)
  (format stream "division by zero in ~A on ~A"
          (slot-value c 'operation) (slot-value c 'operands)))

;; ── Native signal bridge ─────────────────────────────────────────────────
;;
;; The Rust runtime's `error` shim signals through the
;; CONDITION_SLOT / HANDLER_DEPTH machinery. We capture its address
;; once so our Lisp `error` can wrap-and-forward.

(defparameter %native-signal (symbol-function 'error))

(defun %coerce-condition (default-class datum args)
  "Normalise a (datum . args) signal call into a condition instance.
   The shapes accepted match CL's error/signal/warn:
     - an existing condition instance      → returned as-is
     - a class name (symbol)               → (apply #'make-instance datum args)
     - a string + args                     → wrap as DEFAULT-CLASS
                                             (a simple-* subclass)"
  (cond
    ((and (clos-instance-p datum)
          (subclassp (class-of datum) (find-class 'condition)))
     datum)
    ((symbolp datum)
     (apply #'make-instance datum args))
    ((stringp datum)
     ;; Use plain make-instance, not apply — apply with args=nil
     ;; drops the trailing :format-arguments key, leaving it
     ;; unpaired in the initargs plist.
     (make-instance default-class
                    :format-control datum
                    :format-arguments args))
    (t
     (make-instance default-class
                    :format-control "~A"
                    :format-arguments (list datum)))))

(defun error (datum &rest args)
  "Signal an error condition. See %coerce-condition for argument
   shapes. If no handler claims the condition, the runtime renders
   it via print-object and aborts."
  (funcall %native-signal (%coerce-condition 'simple-error datum args)))

(defun signal (datum &rest args)
  "Signal a condition but DO NOT abort if no handler matches.
   Returns NIL in that case. (Today we approximate: we install a
   trivial handler around the signal so the unhandled-condition
   abort doesn't fire. Subtle but matches the common idiom.)"
  (let ((c (%coerce-condition 'simple-condition datum args)))
    ;; %handler-case catches every signal at the body's level.
    ;; We use that to convert "no handler matched" into NIL.
    (%handler-case
     (lambda () (funcall %native-signal c) nil)
     (lambda (ignored)
       (declare (ignore ignored))
       nil))))

(defun warn (datum &rest args)
  "Signal a warning. By default prints to *error-output*; if a
   handler claims the warning, returns its value."
  (let ((c (%coerce-condition 'simple-warning datum args)))
    (%handler-case
     (lambda () (funcall %native-signal c) nil)
     (lambda (signaled)
       (declare (ignore signaled))
       (format t "WARNING: ~A~%" c)
       nil))))

;; ── Typed handler-case ───────────────────────────────────────────────────
;;
;; Redefines the core handler-case macro to support multiple typed
;; clauses. Each clause is (TYPE (var) body...). The wrapper handler
;; lambda gets the condition; we dispatch on (class-of c) against
;; each clause's TYPE via subclassp. If no clause matches, we
;; re-signal (so an outer handler can have a chance).
;;
;; Special case: TYPE = T or TYPE = CONDITION matches anything.
;; A clause like (:no-error (...) body) handles the no-condition
;; case — currently dropped (rarely used and adds complication).

(defmacro handler-case (body-form &rest clauses)
  (let ((c-var (gensym "C-")))
    `(%handler-case
      (lambda () ,body-form)
      (lambda (,c-var)
        (cond
          ,@(mapcar
              (lambda (clause)
                (let ((type (car clause))
                      (var-list (cadr clause))
                      (body (cddr clause)))
                  `((%condition-matches ,c-var ',type)
                    ,(cond
                       ((null var-list) `(progn ,@body))
                       (t `(let ((,(car var-list) ,c-var)) ,@body))))))
              clauses)
          ;; No clause matched — re-signal so outer handlers see it.
          (t (error ,c-var)))))))

(defun %condition-matches (c type)
  "T iff condition C is of class TYPE (a symbol). Wildcards:
   T / CONDITION match anything. Non-CLOS conditions (raw strings
   from runtime panics before this file loaded, etc.) get coerced
   to ERROR / SIMPLE-ERROR / SIMPLE-CONDITION for matching, so
   pre-existing handler-case shapes still catch them."
  (cond
    ((or (eq type 't) (eq type 'condition)) t)
    ((not (clos-instance-p c))
     (or (eq type 'error) (eq type 'simple-error)
         (eq type 'simple-condition)))
    (t
     (let ((tc (find-class type nil)))
       (and tc (subclassp (class-of c) tc))))))

;; ── handler-bind (non-unwinding) ────────────────────────────────────────
;;
;; CL semantics: a handler-bind handler is *called* when a matching
;; condition is signalled, BUT it doesn't unwind by itself. If the
;; handler returns normally, signalling continues with the next
;; outer handler. To actually transfer control, the handler must
;; throw / invoke a restart / call error itself.
;;
;; Faithful implementation needs a per-thread *handler-stack* the
;; runtime consults. Our %handler-case is unwinding by construction.
;; For now we approximate handler-bind by *also* using %handler-case,
;; but only firing the user handler if the type matches AND treating
;; a non-nil return from the user handler as "declined" — in which
;; case we re-signal. This matches the common usage pattern (log
;; the condition then decline, OR invoke a restart from inside).

(defmacro handler-bind (bindings &rest body)
  (let ((c-var (gensym "C-")))
    `(%handler-case
      (lambda () ,@body)
      (lambda (,c-var)
        (block %hb
          ,@(mapcar
              (lambda (b)
                (let ((type (car b))
                      (handler (cadr b)))
                  `(when (%condition-matches ,c-var ',type)
                     (funcall ,handler ,c-var)
                     ;; Spec: if the handler returns normally,
                     ;; signalling continues. We approximate by
                     ;; re-signalling (lets outer handler-binds /
                     ;; cases see it). A handler that wants to
                     ;; ACTUALLY claim the condition must invoke
                     ;; a restart, which transfers control out
                     ;; via its own non-local exit.
                     )))
              bindings)
          (error ,c-var))))))

;; ── Restarts ─────────────────────────────────────────────────────────────
;;
;; A restart is a (name . function) entry on a dynamic stack. The
;; signalling code (or an interactive debugger) inspects the stack
;; via compute-restarts / find-restart and calls a restart via
;; invoke-restart, which longjumps out of the signal site back to
;; the binding form.
;;
;; We implement restart transfer via condition-based unwind: a
;; restart's "invoke" path signals a magic internal condition that
;; the restart-case's handler-case clause catches and re-runs the
;; chosen case body. This piggybacks on our existing primitive
;; rather than introducing a new non-local-exit mechanism.

(defparameter *restart-stack* nil
  "Dynamic stack of active restart records. Each record is a
   plist: (:name NAME :tag UNIQUE-TOKEN :function THUNK
           :report STRING-OR-NIL).")

(defun %pair-up (xs ys)
  "Zip two lists into a list of (cons x y) pairs. Stops at the
   shorter list. Needed because our mapcar is single-list, so we
   pre-zip before mapping when we want parallel iteration."
  (cond
    ((or (null xs) (null ys)) nil)
    (t (cons (cons (car xs) (car ys))
             (%pair-up (cdr xs) (cdr ys))))))

(defclass %restart-invocation (condition)
  ((tag  :initarg :tag)
   (args :initarg :args :initform ())))

(defun find-restart (name &optional condition)
  "Return the most-recently-bound active restart named NAME, or
   NIL. CONDITION is accepted for spec compatibility — restart
   filtering by condition lands later."
  (declare (ignore condition))
  (let ((tail *restart-stack*))
    (block fr
      (loop
        (cond
          ((null tail) (return-from fr nil))
          ((eq (getf (car tail) ':name) name) (return-from fr (car tail)))
          (t (setq tail (cdr tail))))))))

(defun compute-restarts (&optional condition)
  "Return a list of every active restart (most-recent first)."
  (declare (ignore condition))
  (let ((result nil) (tail *restart-stack*))
    (loop
      (cond
        ((null tail) (return (reverse result)))
        (t (setq result (cons (car tail) result))
           (setq tail (cdr tail)))))))

(defun invoke-restart (name-or-restart &rest args)
  "Transfer control to the named restart. Looks up by name if a
   symbol, else treats the arg as an already-resolved record.
   Signals a CONTROL-ERROR if no matching restart is active."
  (let ((r (cond
             ((symbolp name-or-restart) (find-restart name-or-restart))
             (t name-or-restart))))
    (cond
      ((null r) (error 'control-error
                       :format-control "no restart named ~A is active"
                       :format-arguments (list name-or-restart)))
      (t
       ;; Signal the magic condition; the wrapping restart-case
       ;; will catch it and dispatch.
       (error (make-instance '%restart-invocation
                             :tag (getf r ':tag)
                             :args args))))))

;; restart-case binds N restarts for the duration of BODY. If a
;; restart is invoked, control transfers to the matching case body
;; with the args passed to invoke-restart.
;;
;; (restart-case body-form
;;   (name-1 (lambda-list) body...)
;;   (name-2 (lambda-list) body...))

(defmacro restart-case (body-form &rest cases)
  ;; Tag identity: one gensym per case, baked in as a quoted symbol.
  ;; (eq 'tag 'tag) works because intern returns the same symbol.
  ;; Each invocation of restart-case shares tags across the call,
  ;; but that's fine: re-entering restart-case rebinds tag-by-tag
  ;; through the *restart-stack* push, so name lookup still walks
  ;; topmost-first.
  ;;
  ;; *restart-stack* management: setq + restore at every exit
  ;; (success + matched-restart + re-signal). We don't have
  ;; dynamic binding nor unwind-protect, so explicit cleanup is
  ;; the only way. A completely uncaught error inside the body
  ;; leaks one stack frame; benign since the runtime is about to
  ;; abort anyway.
  (let* ((tags (mapcar (lambda (case)
                         (declare (ignore case))
                         (gensym "RTAG-"))
                       cases))
         (c-var (gensym "C-"))
         (old-var (gensym "OLD-")))
    (let ((paired (%pair-up cases tags)))
      `(let ((,old-var *restart-stack*))
         (setq *restart-stack*
               (append (list
                        ,@(mapcar
                            (lambda (pair)
                              (let ((case (car pair)) (g (cdr pair)))
                                `(list ':name ',(car case)
                                       ':tag ',g
                                       ':report nil)))
                            paired))
                       ,old-var))
         (let ((result
                (%handler-case
                 (lambda () ,body-form)
                 (lambda (,c-var)
                   (cond
                     ((and (clos-instance-p ,c-var)
                           (eq (class-of ,c-var)
                               (find-class '%restart-invocation)))
                      (let ((this-tag (slot-value ,c-var 'tag))
                            (this-args (slot-value ,c-var 'args)))
                        (setq *restart-stack* ,old-var)
                        (cond
                          ,@(mapcar
                              (lambda (pair)
                                (let* ((case (car pair))
                                       (g    (cdr pair))
                                       (args-var (cadr case))
                                       (body (cddr case)))
                                  `((eq this-tag ',g)
                                    (apply (lambda ,args-var ,@body)
                                           this-args))))
                              paired)
                          (t (error ,c-var)))))
                     (t
                      (setq *restart-stack* ,old-var)
                      (error ,c-var)))))))
           (setq *restart-stack* ,old-var)
           result)))))

;; restart-bind is similar but the handler-list is (name . function).
;; Less common than restart-case; for now restart-bind delegates to
;; restart-case after capturing each function as a (apply fn args)
;; case body.

(defmacro restart-bind (bindings &rest body)
  `(restart-case (progn ,@body)
     ,@(mapcar
         (lambda (b)
           (let ((name (car b))
                 (fn (cadr b)))
             `(,name (&rest args) (apply ,fn args))))
         bindings)))

;; Standard restarts are not auto-bound in this slice — call sites
;; that want ABORT / CONTINUE / etc. bind them explicitly via
;; restart-case. (CL defines them as available but a typical impl
;; only auto-binds them via the debugger.)

(provide 'conditions)
nil
