;;;; test-mv.lisp — verify multiple-values propagate through recursive
;;;; tail calls after the instrument_tail_for_mv fix.

(defparameter *pass* 0)
(defparameter *fail* 0)

(defun check (label got expected)
  (if (equal got expected)
      (progn
        (setq *pass* (+ *pass* 1))
        (format t "PASS  ~A~%" label))
      (progn
        (setq *fail* (+ *fail* 1))
        (format t "FAIL  ~A: got ~S, expected ~S~%" label got expected))))

;; ── Test 1: basic (values …) in tail position ────────────────────────
(defun two-vals (x)
  (values x (+ x 1)))

(multiple-value-bind (a b) (two-vals 10)
  (check "basic values" (list a b) '(10 11)))

;; ── Test 2: recursive function that returns multiple values ───────────
;; Classic accumulator that counts elements and sums them. The recursive
;; call is in tail position — previously EnsureSingleMv would clobber b.
(defun sum-and-count (lst acc cnt)
  (if (null lst)
      (values acc cnt)
      (sum-and-count (cdr lst)
                     (+ acc (car lst))
                     (+ cnt 1))))

(multiple-value-bind (total n)
    (sum-and-count '(1 2 3 4 5) 0 0)
  (check "recursive mv" (list total n) '(15 5)))

;; ── Test 3: mutual recursion ─────────────────────────────────────────
;; Two mutually recursive functions, each returning two values.
(defun mv-even-p (n)
  (if (= n 0)
      (values t 0)
      (mv-odd-p (- n 1))))

(defun mv-odd-p (n)
  (if (= n 0)
      (values nil 0)
      (mv-even-p (- n 1))))

(multiple-value-bind (yes depth) (mv-even-p 6)
  (check "mutual-rec even" (list yes depth) '(t 0)))

;; odd(5) → even(4) → odd(3) → even(2) → odd(1) → even(0) → (values t 0)
;; The chain correctly says "5 is odd" — every step delegates to the
;; parity of (n-1), ending at even(0) which returns T (0 is even ⟹ 1
;; is odd ⟹ … ⟹ 5 is odd).
(multiple-value-bind (yes depth) (mv-odd-p 5)
  (check "mutual-rec odd" (list yes depth) '(t 0)))

;; ── Test 4: (values …) through an IF branch ──────────────────────────
(defun minmax (a b)
  (if (< a b) (values a b) (values b a)))

(multiple-value-bind (lo hi) (minmax 7 3)
  (check "minmax 7 3" (list lo hi) '(3 7)))

(multiple-value-bind (lo hi) (minmax 2 9)
  (check "minmax 2 9" (list lo hi) '(2 9)))

;; ── Test 5: floor (Lisp function, two-value) ─────────────────────────
(multiple-value-bind (q r) (floor 17 5)
  (check "floor 17 5" (list q r) '(3 2)))

(multiple-value-bind (q r) (truncate -17 5)
  (check "truncate -17 5" (list q r) '(-3 -2)))

;; ── Summary ──────────────────────────────────────────────────────────
(format t "~%Results: ~A passed, ~A failed~%" *pass* *fail*)
(if (= *fail* 0)
    (format t "ALL TESTS PASSED~%")
    (format t "SOME TESTS FAILED~%"))
nil
