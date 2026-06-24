;;;; Lisp/Library/numbers.lisp — polymorphic rounding family.
;;;;
;;;; Overrides FLOOR / CEILING / ROUND / TRUNCATE / MOD / REM with
;;;; CL-spec-shaped versions:
;;;;
;;;;   * Accept INTEGER, RATIO, and FLOAT operands (mixed allowed).
;;;;   * Two-value return for the four rounding ops: (values q r).
;;;;     Single-value return for REM and MOD (as the CL spec says).
;;;;   * Single-arg form (divisor defaults to 1) for the four
;;;;     rounding ops; REM and MOD always take two args.
;;;;
;;;; Implementation notes — multi-value propagation constraint
;;;; ──────────────────────────────────────────────────────────
;;;; The compiler's `instrument_tail_for_mv` pass wraps function
;;;; CALL expressions in tail position with `EnsureSingleMv`, which
;;;; collapses the MV slot to a single value. This means:
;;;;
;;;;   (defun foo () (bar))   ; if bar returns (values 1 2), foo
;;;;                          ; would return only 1 via the wrapper.
;;;;
;;;; To return multiple values a function must have `(values …)` as
;;;; its DIRECT tail expression — not tucked inside a helper call at
;;;; tail position. So the float/ratio paths are inlined into each
;;;; dispatcher rather than factored into a shared helper. The
;;;; integer-pair helpers (%i-floor etc.) are called inside `let`
;;;; bindings (non-tail position) — that's fine.

;; ── Capture the int-only natives BEFORE we redefine ──────────────────
;;
;; Same idiom as Library/sequences.lisp uses with `%list-remove` —
;; snapshot the current function value so the polymorphic wrappers
;; can reach it through the redefinition.

(defparameter %int-truncate (symbol-function 'truncate))
(defparameter %int-rem      (symbol-function 'rem))

;; ── Integer-pair quotient helpers ────────────────────────────────────
;;
;; Each takes two integers (p, q) and returns the integer quotient
;; under the given rounding rule. Called inside `let` bindings
;; (not at tail-call position), so MV propagation is not an issue.

(defun %i-truncate (p q) (funcall %int-truncate p q))
(defun %i-rem      (p q) (funcall %int-rem      p q))

(defun %i-floor (p q)
  "Floor of p/q, integer-pair form. Rounds toward -∞."
  (let* ((q0 (%i-truncate p q))
         (r0 (- p (* q0 q))))
    (cond
      ((zerop r0) q0)
      ((not (eq (minusp r0) (minusp q))) (- q0 1))
      (t q0))))

(defun %i-ceiling (p q)
  "Ceiling of p/q, integer-pair form. Rounds toward +∞."
  (let* ((q0 (%i-truncate p q))
         (r0 (- p (* q0 q))))
    (cond
      ((zerop r0) q0)
      ((eq (minusp r0) (minusp q)) (+ q0 1))
      (t q0))))

(defun %i-round (p q)
  "Round-half-to-even of p/q, integer-pair form."
  (let* ((q0 (%i-truncate p q))
         (r0 (- p (* q0 q)))
         (twice-abs-r (abs (* 2 r0)))
         (abs-q       (abs q)))
    (cond
      ((zerop r0) q0)
      ((< twice-abs-r abs-q) q0)
      ((> twice-abs-r abs-q)
       (if (eq (minusp r0) (minusp q)) (+ q0 1) (- q0 1)))
      ;; Exact tie — round to even.
      (t (if (zerop (%i-rem q0 2))
             q0
             (if (eq (minusp r0) (minusp q)) (+ q0 1) (- q0 1)))))))

;; ── Top-level polymorphic dispatchers ────────────────────────────────
;;
;; Every branch ends with a literal `(values q r)` expression so
;; `instrument_tail_for_mv` can recognise it and leave it alone.
;; Helper CALLS (%i-floor etc.) are inside `let` bindings (non-tail
;; position); the float-shim calls (FLOOR-FLOAT etc.) are likewise
;; bound to `q` before the tail `(values …)`.

(defun truncate (a &optional (b 1))
  "Divide A by B, round toward zero. Returns (values quotient remainder).
   Single-arg form: (truncate A) ≡ (truncate A 1)."
  (cond
    ;; ── float path ────────────────────────────────────────────────
    ((or (floatp a) (floatp b))
     (let* ((af (float a))
            (bf (float b))
            (q  (truncate-float (/ af bf))))
       (values q (- af (* q bf)))))
    ;; ── ratio path ────────────────────────────────────────────────
    ((or (ratiop a) (ratiop b))
     (let* ((quot (/ a b)))
       (if (ratiop quot)
           (let ((q (%i-truncate (numerator quot) (denominator quot))))
             (values q (- a (* q b))))
           (values quot 0))))
    ;; ── integer path ──────────────────────────────────────────────
    (t
     (let ((q (%i-truncate a b)))
       (values q (- a (* q b)))))))

(defun floor (a &optional (b 1))
  "Divide A by B, round toward -∞. Returns (values quotient remainder)."
  (cond
    ((or (floatp a) (floatp b))
     (let* ((af (float a))
            (bf (float b))
            (q  (floor-float (/ af bf))))
       (values q (- af (* q bf)))))
    ((or (ratiop a) (ratiop b))
     (let* ((quot (/ a b)))
       (if (ratiop quot)
           (let ((q (%i-floor (numerator quot) (denominator quot))))
             (values q (- a (* q b))))
           (values quot 0))))
    (t
     (let ((q (%i-floor a b)))
       (values q (- a (* q b)))))))

(defun ceiling (a &optional (b 1))
  "Divide A by B, round toward +∞. Returns (values quotient remainder)."
  (cond
    ((or (floatp a) (floatp b))
     (let* ((af (float a))
            (bf (float b))
            (q  (ceiling-float (/ af bf))))
       (values q (- af (* q bf)))))
    ((or (ratiop a) (ratiop b))
     (let* ((quot (/ a b)))
       (if (ratiop quot)
           (let ((q (%i-ceiling (numerator quot) (denominator quot))))
             (values q (- a (* q b))))
           (values quot 0))))
    (t
     (let ((q (%i-ceiling a b)))
       (values q (- a (* q b)))))))

(defun round (a &optional (b 1))
  "Divide A by B, round-half-to-even. Returns (values quotient remainder).
   Banker's rounding: exact ties prefer the even quotient."
  (cond
    ((or (floatp a) (floatp b))
     (let* ((af (float a))
            (bf (float b))
            (q  (round-float (/ af bf))))
       (values q (- af (* q bf)))))
    ((or (ratiop a) (ratiop b))
     (let* ((quot (/ a b)))
       (if (ratiop quot)
           (let ((q (%i-round (numerator quot) (denominator quot))))
             (values q (- a (* q b))))
           (values quot 0))))
    (t
     (let ((q (%i-round a b)))
       (values q (- a (* q b)))))))

;; REM and MOD always take two args (CL spec). Each returns a single
;; value — the remainder paired with TRUNCATE (rem) or FLOOR (mod).

(defun rem (a b)
  "(rem a b) — remainder paired with TRUNCATE. Sign matches A."
  (cond
    ((or (floatp a) (floatp b))
     (let* ((af (float a))
            (bf (float b))
            (q  (truncate-float (/ af bf))))
       (- af (* q bf))))
    ((or (ratiop a) (ratiop b))
     (let* ((quot (/ a b)))
       (if (ratiop quot)
           (let ((q (%i-truncate (numerator quot) (denominator quot))))
             (- a (* q b)))
           0)))
    (t (%i-rem a b))))

(defun mod (a b)
  "(mod a b) — remainder paired with FLOOR. Sign matches B."
  (cond
    ((or (floatp a) (floatp b))
     (let* ((af (float a))
            (bf (float b))
            (q  (floor-float (/ af bf))))
       (- af (* q bf))))
    ((or (ratiop a) (ratiop b))
     (let* ((quot (/ a b)))
       (if (ratiop quot)
           (let ((q (%i-floor (numerator quot) (denominator quot))))
             (- a (* q b)))
           0)))
    (t
     (let ((q (%i-floor a b)))
       (- a (* q b))))))

;;; ── Floating-point rounding ops (FFLOOR, FCEILING, FTRUNCATE, FROUND) ──────
;;
;; Like FLOOR/CEILING/ROUND/TRUNCATE but always return a float quotient and a
;; remainder.  (CL says the quotient type matches MAX(type(A),type(B))-as-float
;; and the remainder preserves type; we keep it simple: quotient is always a
;; float, remainder has the natural type.)

;; NOTE: each fXXX function MUST end with a literal (values ...) form so
;; the compiler's instrument_tail_for_mv pass recognises it as multi-valued
;; and leaves the MV slot intact.  Factoring through a helper that calls
;; (values ...) at tail position would lose the second value.

(defun ffloor (number &optional (divisor 1))
  "(ffloor a [d]) — like floor but returns (values float-quotient remainder)."
  (let* ((pair (multiple-value-list (floor number divisor)))
         (q    (car pair))
         (r    (cadr pair)))
    (values (* 1.0 q) r)))

(defun fceiling (number &optional (divisor 1))
  "(fceiling a [d]) — like ceiling but returns (values float-quotient remainder)."
  (let* ((pair (multiple-value-list (ceiling number divisor)))
         (q    (car pair))
         (r    (cadr pair)))
    (values (* 1.0 q) r)))

(defun ftruncate (number &optional (divisor 1))
  "(ftruncate a [d]) — like truncate but returns (values float-quotient remainder)."
  (let* ((pair (multiple-value-list (truncate number divisor)))
         (q    (car pair))
         (r    (cadr pair)))
    (values (* 1.0 q) r)))

(defun fround (number &optional (divisor 1))
  "(fround a [d]) — like round but returns (values float-quotient remainder)."
  (let* ((pair (multiple-value-list (round number divisor)))
         (q    (car pair))
         (r    (cadr pair)))
    (values (* 1.0 q) r)))

;;; ── Inverse hyperbolic functions ─────────────────────────────────────────────
;;
;; NCL has SINH/COSH/TANH as native (Rust) but lacks the inverses.
;; Standard identities from CLHS, valid for all real x.

(defun asinh (x)
  "Inverse hyperbolic sine: (log (+ x (sqrt (+ 1 (* x x)))))."
  (log (+ x (sqrt (+ 1.0 (* x x))))))

(defun acosh (x)
  "Inverse hyperbolic cosine: (log (+ x (sqrt (- (* x x) 1)))).
X must be >= 1."
  (log (+ x (sqrt (- (* x x) 1.0)))))

(defun atanh (x)
  "Inverse hyperbolic tangent: (/ (log (/ (+ 1 x) (- 1 x))) 2).
X must be in (-1, 1)."
  (/ (log (/ (+ 1.0 x) (- 1.0 x))) 2.0))

;;; ── Standard numeric constants ───────────────────────────────────────────
;;
;; PI and the implementation-limit constants every CL program expects.
;; FIXNUM bounds match NCL's tagged-word layout: a Word is 64 bits with a
;; 3-bit tag, leaving 61 signed value bits, so the range is
;;   [-2^60, 2^60-1] = [-1152921504606846976, 1152921504606846975].
;; (Confirmed empirically: (+ most-positive-fixnum 1) promotes to bignum.)
;; Floats are IEEE-754 binary64 (Rust f64); SINGLE-FLOAT is the same
;; representation as DOUBLE-FLOAT here, so the *-single-float limits alias
;; the double ones.

(defconstant pi 3.141592653589793d0)

(defconstant most-positive-fixnum 1152921504606846975)
(defconstant most-negative-fixnum -1152921504606846976)

(defconstant most-positive-double-float 1.7976931348623157d308)
(defconstant most-negative-double-float -1.7976931348623157d308)
(defconstant least-positive-double-float 2.2250738585072014d-308)
(defconstant least-negative-double-float -2.2250738585072014d-308)
(defconstant double-float-epsilon 1.1102230246251568d-16)
(defconstant double-float-negative-epsilon 1.1102230246251568d-16)

;; SINGLE-FLOAT == DOUBLE-FLOAT in NCL (both f64).
(defconstant most-positive-single-float most-positive-double-float)
(defconstant most-negative-single-float most-negative-double-float)
(defconstant least-positive-single-float least-positive-double-float)
(defconstant least-negative-single-float least-negative-double-float)
(defconstant single-float-epsilon double-float-epsilon)
(defconstant single-float-negative-epsilon double-float-negative-epsilon)

(provide 'numbers)
nil
