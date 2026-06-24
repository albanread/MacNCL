;;;; Lisp/Library/deriv.lisp
;;;;
;;;; Symbolic differentiation à la SICP §2.3.2.
;;;;
;;;; The argument is a Lisp form representing a mathematical
;;;; expression in the obvious way:
;;;;
;;;;   (+ a b ...)    sum
;;;;   (- a b)        binary difference  (a - b)
;;;;   (- a)          unary negation
;;;;   (* a b ...)    product
;;;;   (expt b e)     power
;;;;   (sin x)        trig
;;;;   (cos x)
;;;;   (exp x)        natural exponential e^x
;;;;   (log x)        natural log
;;;;   var-symbol     a variable
;;;;   number         a constant
;;;;
;;;; (deriv expr var) returns the symbolic derivative of expr with
;;;; respect to the variable symbol var, with a light simplifier
;;;; applied so trivial terms (multiply by 0/1, add 0, etc.) don't
;;;; clutter the output.
;;;;
;;;; Examples (entered at the REPL after `(require 'deriv)`):
;;;;
;;;;   (deriv 'x 'x)                       => 1
;;;;   (deriv 5 'x)                        => 0
;;;;   (deriv '(+ x y) 'x)                 => 1
;;;;   (deriv '(* x y) 'x)                 => Y
;;;;   (deriv '(expt x 3) 'x)              => (* 3 (expt x 2))
;;;;   (deriv '(sin x) 'x)                 => (cos x)
;;;;   (deriv '(cos x) 'x)                 => (- (sin x))
;;;;   (deriv '(* x (sin x)) 'x)           => (+ (sin x) (* x (cos x)))
;;;;   (deriv '(exp (* 2 x)) 'x)           => (* 2 (exp (* 2 x)))
;;;;   (deriv '(+ x (* 3 (expt x 2))) 'x)  => (+ 1 (* 3 (* 2 x)))
;;;;
;;;; The simplifier is intentionally minimal — it folds constants
;;;; pairwise but doesn't try to flatten nested products or gather
;;;; like terms. `(* 3 (* 2 x))` stays as written rather than
;;;; collapsing to `(* 6 x)`. A real CAS layer goes on top later.

;; ── Helpers ────────────────────────────────────────────────────────────

(defun variablep (e) (symbolp e))

(defun same-variablep (a b)
  (and (variablep a) (variablep b) (eq a b)))

(defun =number (e n) (and (numberp e) (= e n)))

;; ── Expression shape predicates and accessors ──────────────────────────

(defun sump (e) (and (consp e) (eq (car e) '+)))
(defun productp (e) (and (consp e) (eq (car e) '*)))
(defun differencep (e)
  (and (consp e) (eq (car e) '-) (consp (cdr e)) (consp (cddr e))))
(defun negationp (e)
  (and (consp e) (eq (car e) '-) (consp (cdr e)) (null (cddr e))))
(defun powerp (e) (and (consp e) (eq (car e) 'expt)))
(defun sinp (e) (and (consp e) (eq (car e) 'sin)))
(defun cosp (e) (and (consp e) (eq (car e) 'cos)))
(defun expp (e) (and (consp e) (eq (car e) 'exp)))
(defun logp (e) (and (consp e) (eq (car e) 'log)))

(defun addend (e) (car (cdr e)))
(defun augend (e)
  "Right tail of an n-ary sum. For `(+ a b)` returns `b`;
   for `(+ a b c …)` returns `(+ b c …)` so recursion can chip
   away one term at a time."
  (cond
    ((null (cdr (cdr (cdr e)))) (car (cdr (cdr e))))
    (t (cons '+ (cdr (cdr e))))))

(defun multiplier (e) (car (cdr e)))
(defun multiplicand (e)
  "Right tail of an n-ary product, same idea as augend."
  (cond
    ((null (cdr (cdr (cdr e)))) (car (cdr (cdr e))))
    (t (cons '* (cdr (cdr e))))))

(defun minuend (e)    (car (cdr e)))
(defun subtrahend (e) (car (cdr (cdr e))))
(defun negation-operand (e) (car (cdr e)))

(defun base-of (e)     (car (cdr e)))
(defun exponent-of (e) (car (cdr (cdr e))))
(defun unary-arg (e)   (car (cdr e)))

;; ── Smart constructors (the simplifier lives here) ─────────────────────

(defun make-sum (a b)
  (cond
    ((=number a 0) b)
    ((=number b 0) a)
    ((and (numberp a) (numberp b)) (+ a b))
    (t (list '+ a b))))

(defun make-product (a b)
  (cond
    ((or (=number a 0) (=number b 0)) 0)
    ((=number a 1) b)
    ((=number b 1) a)
    ((and (numberp a) (numberp b)) (* a b))
    (t (list '* a b))))

(defun make-difference (a b)
  (cond
    ((=number b 0) a)
    ((=number a 0) (make-negation b))
    ((and (numberp a) (numberp b)) (- a b))
    (t (list '- a b))))

(defun make-negation (a)
  (cond
    ((=number a 0) 0)
    ((numberp a) (- 0 a))
    (t (list '- a))))

(defun make-power (b e)
  (cond
    ((=number e 0) 1)
    ((=number e 1) b)
    ((=number b 0) 0)
    ((=number b 1) 1)
    ((and (numberp b) (numberp e)) (expt b e))
    (t (list 'expt b e))))

;; ── The differentiator ────────────────────────────────────────────────

(defun deriv (expr var)
  "Symbolic derivative of EXPR with respect to the variable
   symbol VAR. EXPR is a number, a variable symbol, or a list
   whose head is one of `+ - * expt sin cos exp log`."
  (cond
    ((numberp expr) 0)
    ((variablep expr)
     (cond
       ((same-variablep expr var) 1)
       (t 0)))
    ((sump expr)
     (make-sum (deriv (addend expr) var)
               (deriv (augend expr) var)))
    ((productp expr)
     (make-sum (make-product (multiplier expr)
                             (deriv (multiplicand expr) var))
               (make-product (deriv (multiplier expr) var)
                             (multiplicand expr))))
    ((negationp expr)
     (make-negation (deriv (negation-operand expr) var)))
    ((differencep expr)
     (make-difference (deriv (minuend expr) var)
                      (deriv (subtrahend expr) var)))
    ((powerp expr)
     (cond
       ((numberp (exponent-of expr))
        ;; d/dx [b^n] = n * b^(n-1) * db/dx
        (make-product
         (make-product (exponent-of expr)
                       (make-power (base-of expr)
                                   (- (exponent-of expr) 1)))
         (deriv (base-of expr) var)))
       (t
        (error "deriv: variable exponent not yet supported: ~S" expr))))
    ((sinp expr)
     ;; d/dx [sin u] = (cos u) * du/dx
     (make-product (list 'cos (unary-arg expr))
                   (deriv (unary-arg expr) var)))
    ((cosp expr)
     ;; d/dx [cos u] = -(sin u) * du/dx
     (make-negation
      (make-product (list 'sin (unary-arg expr))
                    (deriv (unary-arg expr) var))))
    ((expp expr)
     ;; d/dx [exp u] = (exp u) * du/dx
     (make-product (list 'exp (unary-arg expr))
                   (deriv (unary-arg expr) var)))
    ((logp expr)
     ;; d/dx [log u] = (du/dx) / u  →  represented as (* du (expt u -1))
     (make-product (deriv (unary-arg expr) var)
                   (make-power (unary-arg expr) -1)))
    (t (error "deriv: unknown form: ~S" expr))))

(provide 'deriv)
nil
