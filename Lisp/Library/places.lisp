;;;; Lisp/Library/places.lisp — extensible SETF places.
;;;;
;;;; Two things in one module:
;;;;
;;;;   1. The standard CL `(setf accessor)` declarations for the
;;;;      cons-cell accessor family — FIRST, SECOND, …, REST,
;;;;      CAAR, CADR, CDAR, CDDR, CAAAR through CDDDDR.  Each is a
;;;;      one-line `(defun (setf NAME) (val place) (setf <expansion>
;;;;      val))` that maps the named accessor onto the underlying
;;;;      `(setf (car …) …)` / `(setf (cdr …) …)` primitives.
;;;;
;;;;      Without these, code like `(setf (first xs) 99)` failed
;;;;      with "undefined function: %SETF-FIRST" — the generic-
;;;;      setf-fallback mangled FIRST correctly but no setter was
;;;;      registered.  These add the registration; the fallback
;;;;      now resolves.
;;;;
;;;;   2. A DEFSETF macro (the short-form variant) — sugar for the
;;;;      common case where the setter is just another function
;;;;      call:
;;;;
;;;;        (defsetf access-fn setter-fn)
;;;;
;;;;      installs an inverse of ACCESS-FN such that
;;;;
;;;;        (setf (access-fn arg1 … argN) val)
;;;;
;;;;      expands to
;;;;
;;;;        (setter-fn arg1 … argN val)
;;;;
;;;;      The long form `(defsetf access-fn lambda-list (store-var)
;;;;      body)` and the fully general
;;;;      `define-setf-expander` are out of scope for this slice —
;;;;      both can layer on top of `defsetf` later without changing
;;;;      the underlying machinery.

;; (No (in-package …) — NCL uses a flat symbol namespace at this
;; tier; everything interns into COMMON-LISP-USER by default.)

;; ── Named cons-cell accessors ───────────────────────────────────────────

(defun (setf first)  (val x) (setf (car x) val))
(defun (setf second) (val x) (setf (car (cdr x)) val))
(defun (setf third)  (val x) (setf (car (cdr (cdr x))) val))
(defun (setf fourth) (val x) (setf (car (cdr (cdr (cdr x)))) val))

(defun (setf rest)   (val x) (setf (cdr x) val))

(defun (setf caar) (val list) (setf (car (car list)) val))
(defun (setf cadr) (val list) (setf (car (cdr list)) val))
(defun (setf cdar) (val list) (setf (cdr (car list)) val))
(defun (setf cddr) (val list) (setf (cdr (cdr list)) val))

(defun (setf caaar) (val list) (setf (car (car (car list))) val))
(defun (setf caadr) (val list) (setf (car (car (cdr list))) val))
(defun (setf cadar) (val list) (setf (car (cdr (car list))) val))
(defun (setf caddr) (val list) (setf (car (cdr (cdr list))) val))
(defun (setf cdaar) (val list) (setf (cdr (car (car list))) val))
(defun (setf cdadr) (val list) (setf (cdr (car (cdr list))) val))
(defun (setf cddar) (val list) (setf (cdr (cdr (car list))) val))
(defun (setf cdddr) (val list) (setf (cdr (cdr (cdr list))) val))

(defun (setf caaaar) (val list) (setf (car (car (car (car list)))) val))
(defun (setf caaadr) (val list) (setf (car (car (car (cdr list)))) val))
(defun (setf caadar) (val list) (setf (car (car (cdr (car list)))) val))
(defun (setf caaddr) (val list) (setf (car (car (cdr (cdr list)))) val))
(defun (setf cadaar) (val list) (setf (car (cdr (car (car list)))) val))
(defun (setf cadadr) (val list) (setf (car (cdr (car (cdr list)))) val))
(defun (setf caddar) (val list) (setf (car (cdr (cdr (car list)))) val))
(defun (setf cadddr) (val list) (setf (car (cdr (cdr (cdr list)))) val))

(defun (setf cdaaar) (val list) (setf (cdr (car (car (car list)))) val))
(defun (setf cdaadr) (val list) (setf (cdr (car (car (cdr list)))) val))
(defun (setf cdadar) (val list) (setf (cdr (car (cdr (car list)))) val))
(defun (setf cdaddr) (val list) (setf (cdr (car (cdr (cdr list)))) val))
(defun (setf cddaar) (val list) (setf (cdr (cdr (car (car list)))) val))
(defun (setf cddadr) (val list) (setf (cdr (cdr (car (cdr list)))) val))
(defun (setf cdddar) (val list) (setf (cdr (cdr (cdr (car list)))) val))
(defun (setf cddddr) (val list) (setf (cdr (cdr (cdr (cdr list)))) val))

;; NTH / NTHCDR
;;
;; `(setf (nth n list) val)` and `(setf (nthcdr n list) val)` are
;; the obvious setf-able forms over the indexed-access functions.
;; CL spec: nth indexes from 0; same for nthcdr.

(defun (setf nth) (val n list)
  (setf (car (nthcdr n list)) val))

(defun (setf nthcdr) (val n list)
  ;; Replace the entire tail starting at position N.
  ;; CL doesn't strictly mandate this since nthcdr returns a tail
  ;; and setf-ing a tail is dodgy; we provide it via (setf (cdr
  ;; ...))-of-prefix for symmetry with nth.
  (cond
    ((zerop n)
     ;; Can't setf a binding through the car of the head — the
     ;; caller's list variable wouldn't update. Signal.
     (error "(setf nthcdr) with N=0 cannot mutate the variable; assign directly instead"))
    (t (setf (cdr (nthcdr (- n 1) list)) val))))

;; LAST
;;
;; `(setf (last list) val)` replaces the final cons cell. CL's
;; `last` returns the LAST cons cell of LIST; setting it via setf
;; means rplaca'ing the head of the last cell.

(defun (setf last) (val list)
  (setf (car (last list)) val))

;; ── DEFSETF (short form) ────────────────────────────────────────────────
;;
;; The convention: (defsetf foo set-foo) expands to
;;
;;   (defun (setf foo) (val arg1 … argN)
;;     (set-foo arg1 … argN val))
;;
;; So (setf (foo a b) v) → (%setf-foo v a b) [via lower.rs fallback]
;; → (set-foo a b v) [via the body we install].
;;
;; We don't know SET-FOO's exact arglist at macro-expansion time,
;; but we know it takes one more arg than FOO (the new value, last).
;; So we accept the place-fn's lambda-list implicitly via a single
;; &REST capture and apply through it.

(defmacro defsetf (access-fn setter-fn)
  "Short-form DEFSETF. (defsetf place-fn setter-fn) means: a
   (setf (place-fn arg…) val) form evaluates to (setter-fn arg… val).
   SETTER-FN must take one more argument than PLACE-FN, with the
   new value as its last positional argument.

   Long-form DEFSETF and DEFINE-SETF-EXPANDER are not yet
   implemented; use (defun (setf NAME) (val args…) …) directly
   for cases the short form doesn't cover."
  ;; Wrap in progn so the top-level recogniser walks the inner
  ;; defun — a bare (defun …) produced by a macro hits the
  ;; \"defun only at top level\" guard otherwise.
  (let ((val (gensym "VAL-"))
        (args (gensym "ARGS-")))
    `(progn
       (defun (setf ,access-fn) (,val &rest ,args)
         (apply (function ,setter-fn) (append ,args (list ,val)))))))

;; ── INCF / DECF ─────────────────────────────────────────────────────────
;;
;; CL's INCF and DECF are sugar for (setf place (+ place delta)) and
;; (setf place (- place delta)) — delta defaults to 1. Same caveat
;; about side-effecting subforms as ROTATEF below; CL's "full" form
;; uses get-setf-expansion to evaluate subforms once.

(defmacro incf (place &optional (delta 1))
  "Increment PLACE by DELTA (default 1). Returns the new value."
  `(setf ,place (+ ,place ,delta)))

(defmacro decf (place &optional (delta 1))
  "Decrement PLACE by DELTA (default 1). Returns the new value."
  `(setf ,place (- ,place ,delta)))

;; ── ROTATEF / SHIFTF ────────────────────────────────────────────────────
;;
;; ROTATEF rotates the values stored at N places one step to the LEFT:
;;
;;   (rotatef A B)         ; swap   — A gets B, B gets A
;;   (rotatef A B C)       ; rotate — A←B, B←C, C←A
;;
;; SHIFTF shifts values RIGHT through N places, returning the leftmost
;; value (the one that "falls off"):
;;
;;   (shiftf A B 99)       ; A's old value is returned; A←B, B←99
;;
;; CL's full versions use get-setf-expansion so subforms of each place
;; are evaluated only once. We don't have that yet, so the macros below
;; assume the places are either bare symbols or simple accessor forms
;; whose subforms are side-effect-free. Corman's `quicksort.lisp` uses
;; `(rotatef (elt vec i) (elt vec j))` which qualifies — `elt`'s
;; subforms are just variable reads.
;;
;; If a caller hands us a place with side-effecting subforms (rare
;; outside CL bootstrap code), the result will be wrong; that's the
;; same trade-off our PUSH and INCF make today.

(defmacro rotatef (&rest places)
  "Rotate values stored at PLACES one step to the left.
   (rotatef a b c) ≡ A←B, B←C, C←A. Returns NIL."
  (cond
    ((null places) nil)
    ((null (cdr places)) nil)  ; single place — no-op
    (t
     (let ((temps (mapcar (lambda (p) (declare (ignore p)) (gensym "ROT-"))
                          places))
           (vals  (mapcar (lambda (p) p) places)))
       ;; LET binds each temp to its place's current value, then we
       ;; SETF each place to its left-neighbour's saved value. The
       ;; first place gets the last temp.
       (let ((bindings (mapcar #'list temps vals))
             (writes  (let ((ws nil))
                        (do ((ps places (cdr ps))
                             (ts (cdr temps) (cdr ts)))
                            ((null (cdr ps))
                             (setq ws (cons `(setf ,(car ps) ,(car temps)) ws))
                             (nreverse ws))
                          (setq ws (cons `(setf ,(car ps) ,(car ts)) ws))))))
         `(let ,bindings
            ,@writes
            nil))))))

(defmacro shiftf (&rest args)
  "Shift values stored at PLACES one step to the right; returns the
   leftmost old value (the one that falls off).
   (shiftf a b c new) ≡ saves a, then A←B, B←C, C←new; returns saved.
   At least 2 args required: N-1 places + 1 incoming value."
  (cond
    ((or (null args) (null (cdr args)))
     (error "SHIFTF needs at least 2 args (one place + one value), got ~A"
            (length args)))
    (t
     (let* ((places (let ((all args) (out nil))
                      ;; All but last: places to shift through.
                      (do ()
                          ((null (cdr all)) (nreverse out))
                        (setq out (cons (car all) out))
                        (setq all (cdr all)))))
            (new-val (car (last args)))
            (temps   (mapcar (lambda (p) (declare (ignore p))
                               (gensym "SHIFT-"))
                             places))
            (bindings (mapcar #'list temps places))
            (writes   (let ((ws nil))
                        (do ((ps places (cdr ps))
                             (ts (cdr temps) (cdr ts)))
                            ((null (cdr ps))
                             (setq ws (cons `(setf ,(car ps) ,new-val) ws))
                             (nreverse ws))
                          (setq ws (cons `(setf ,(car ps) ,(car ts)) ws))))))
       `(let ,bindings
          ,@writes
          ,(car temps))))))

;; ── PSETQ / PSETF ────────────────────────────────────────────────────────────
;;
;; Parallel assignment: all right-hand sides are evaluated first (in
;; left-to-right order), then all assignments are performed.  Returns NIL.
;;
;; PSETQ assigns bare symbols; PSETF is the general version that handles
;; any setf-able place (same caveat as ROTATEF: compound places with
;; side-effecting subforms are evaluated in the binding phase, which is
;; good — each subform is evaluated once in order).

(defmacro psetq (&rest pairs)
  "Parallel SETQ: evaluate all right-hand sides, then set all variables.
   Returns NIL.  Unlike SETQ, assignment order does not matter — all
   old values are captured first."
  (when (oddp (length pairs))
    (error "PSETQ: odd number of arguments (~A)" (length pairs)))
  (let ((vs nil) (es nil) (ts nil))
    (do ((p pairs (cddr p)))
        ((null p))
      (push (car p)  vs)
      (push (cadr p) es)
      (push (gensym) ts))
    (let ((vs (nreverse vs))
          (es (nreverse es))
          (ts (nreverse ts)))
      `(let ,(mapcar #'list ts es)
         ,@(mapcar (lambda (v tmp) `(setq ,v ,tmp)) vs ts)
         nil))))

(defmacro psetf (&rest pairs)
  "Parallel SETF: evaluate all right-hand sides, then set all places.
   Returns NIL.  All old values are captured before any assignment."
  (when (oddp (length pairs))
    (error "PSETF: odd number of arguments (~A)" (length pairs)))
  (let ((places nil) (es nil) (ts nil))
    (do ((p pairs (cddr p)))
        ((null p))
      (push (car p)  places)
      (push (cadr p) es)
      (push (gensym) ts))
    (let ((places (nreverse places))
          (es     (nreverse es))
          (ts     (nreverse ts)))
      `(let ,(mapcar #'list ts es)
         ,@(mapcar (lambda (place tmp) `(setf ,place ,tmp)) places ts)
         nil))))

;; ── GET-SETF-EXPANSION ───────────────────────────────────────────────────────
;;
;; Returns five values describing how to read and write a generalised place:
;;
;;   vars       — list of gensyms, one per subform of the accessor call
;;   vals       — list of the actual subforms (parallel to vars)
;;   stores     — list of one gensym: the "new value" variable
;;   writer     — form: using the vars and stores, writes the new value
;;   reader     — form: using the vars, reads the current value
;;
;; Callers bind `(let* ,(mapcar #'list vars vals) ...)` to evaluate
;; each accessor subform exactly once, then bind `(let ((store ...)) ...)`
;; for the new value, evaluate `writer` to commit it, and use `reader`
;; to observe the old value.
;;
;; NCL's setf model: `(defun (setf f) (new-val arg1 ... argN) ...)` registers
;; as `%SETF-F`; `(setf (f arg1 ...argN) val)` calls `(%SETF-F val arg1...argN)`.
;; For standard built-in places (car, cdr, aref, gethash, …) the compiler
;; handles them natively without going through %SETF-*.
;;
;; The ENVIRONMENT argument is accepted for CL compatibility but ignored;
;; NCL does not yet have compile-time environment objects.

(defun get-setf-expansion (place &optional environment)
  "Return (values vars vals stores writer reader) for the generalised place PLACE.
   ENVIRONMENT is accepted for compatibility but ignored in NCL."
  (declare (ignore environment))
  (cond
    ;; Bare symbol — read/write directly.
    ((symbolp place)
     (let ((store (gensym "STORE-")))
       (values nil nil (list store)
               `(setq ,place ,store)
               place)))
    ;; (accessor arg1 ... argN) — general case.
    ((consp place)
     (let* ((fn    (car place))
            (args  (cdr place))
            (vars  (mapcar (lambda (a) (declare (ignore a)) (gensym "G-")) args))
            (store (gensym "STORE-")))
       (values vars
               args
               (list store)
               `(setf (,fn ,@vars) ,store)
               `(,fn ,@vars))))
    (t
     (error "get-setf-expansion: cannot expand place ~S" place))))

;; ── DEFINE-MODIFY-MACRO ──────────────────────────────────────────────────────
;;
;; Creates a read-modify-write macro analogous to INCF or PUSH.
;;
;;   (define-modify-macro name lambda-list function &optional doc)
;;
;; Generates a macro NAME that reads a generalised place, applies
;; FUNCTION to the old value and any extra arguments, then writes the
;; result back.  Example:
;;
;;   (define-modify-macro appendf (&rest more) append)
;;   (appendf my-list '(1 2 3))   ; ≡ (setf my-list (append my-list '(1 2 3)))
;;
;; The lambda-list may contain &optional and &rest (but not &key or &aux).
;; NOTE: the generated macro does NOT accept &environment in NCL because
;; NCL does not thread compile-time environments through defmacro.

(defmacro define-modify-macro (name lambda-list function &optional doc-string)
  "Define a read-modify-write macro named NAME.
   When NAME is called as (NAME place arg…), it is equivalent to
   (setf place (FUNCTION place arg…)).  The generalised place is
   evaluated only once."
  (let ((other-args nil)
        (rest-arg   nil)
        (reference  (gensym "PLACE-")))
    ;; Walk lambda-list to collect extra argument names.
    (do ((ll lambda-list (cdr ll)))
        ((null ll))
      (let ((arg (car ll)))
        (cond
          ((eq arg '&optional) nil)      ; skip keyword itself
          ((eq arg '&rest)
           (cond
             ((null (cdr ll))
              (error "DEFINE-MODIFY-MACRO ~S: &rest must be followed by a name" name))
             ((symbolp (cadr ll))
              (setq rest-arg (cadr ll))
              (return))
             (t
              (error "DEFINE-MODIFY-MACRO ~S: &rest arg must be a symbol" name))))
          ((member arg '(&key &allow-other-keys &aux))
           (error "DEFINE-MODIFY-MACRO ~S: ~S not allowed in lambda list" name arg))
          ((symbolp arg)
           (push arg other-args))
          ((and (consp arg) (symbolp (car arg)))
           ;; optional-with-default: (var default) — just want the var name
           (push (car arg) other-args))
          (t
           (error "DEFINE-MODIFY-MACRO ~S: bad lambda-list element ~S" name arg)))))
    (setq other-args (nreverse other-args))
    `(defmacro ,name (,reference ,@lambda-list)
       ,@(when doc-string (list doc-string))
       (multiple-value-bind (vars vals stores writer reader)
           (get-setf-expansion ,reference)
         (let ((let-list (mapcar #'list vars vals)))
           (push (list (car stores)
                       ,(if rest-arg
                            `(list* ',function reader ,@other-args ,rest-arg)
                            `(list ',function reader ,@other-args)))
                 let-list)
           ;; Build (let* (binding...) writer) without nested backquote —
           ;; NCL does not support nested backquotes yet.
           (cons 'let* (cons (nreverse let-list) (list writer))))))))

(provide 'places)
nil
