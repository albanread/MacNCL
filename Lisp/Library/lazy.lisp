;;;; Lisp/Library/lazy.lisp
;;;;
;;;; Lazy lists — Scheme/SICP §3.5 style "streams" without the
;;;; name clash with our existing I/O `streams.lisp`. The car of
;;;; a lazy cons is computed eagerly; the cdr is a memoising thunk
;;;; that's forced on demand and caches the result.
;;;;
;;;; Two-line essence:
;;;;
;;;;   (lazy-cons head tail-expr)   ;; tail-expr captured in a closure
;;;;   (lazy-cdr s)                 ;; runs the closure once, caches it
;;;;
;;;; That's enough to express infinite sequences finitely:
;;;;
;;;;   (defun integers-from (n) (lazy-cons n (integers-from (+ n 1))))
;;;;   (lazy-take 5 (integers-from 1))  ; => (1 2 3 4 5)
;;;;
;;;; The self-referential Fibonacci trick falls out for free:
;;;;
;;;;   (defun add2 (a b) (+ a b))
;;;;   (defparameter *fibs*
;;;;     (lazy-cons 0
;;;;       (lazy-cons 1
;;;;         (lazy-zipwith #'add2 *fibs* (lazy-cdr *fibs*)))))
;;;;   (lazy-take 10 *fibs*)
;;;;   ;; => (0 1 1 2 3 5 8 13 21 34)
;;;;
;;;; (Note: `#'+` doesn't work yet — `+` is a special form in the
;;;; compiler, so its symbol-function cell isn't installed. Wrap
;;;; in a defun like `add2` above. Same story for `*`, `<`, etc.)
;;;;
;;;; The recursive `*fibs*` reference is safe because the cdr of
;;;; the second cell is a *promise*, not an evaluated form. By
;;;; the time anyone forces it, `*fibs*` is fully bound.

;; ── promise = memoising thunk ───────────────────────────────────────────
;;
;; Represented as `(cons FLAG VALUE-OR-THUNK)`:
;;   ('unforced . thunk)  — not yet evaluated
;;   ('forced   . value)  — evaluated; cached result in the cdr
;;
;; Mutating the cons in place avoids a second allocation per
;; force. Symbol comparison with `eq` is the cheapest tag check
;; we have.

(defun %make-promise (thunk)
  (cons 'unforced thunk))

(defun %force (promise)
  (cond
    ((eq (car promise) 'forced)
     (cdr promise))
    (t
     (let ((val (funcall (cdr promise))))
       (setf (car promise) 'forced)
       (setf (cdr promise) val)
       val))))

;; ── lazy-cons + accessors ───────────────────────────────────────────────

(defmacro lazy-cons (head tail-form)
  "Build a lazy cons. HEAD is evaluated immediately; TAIL-FORM is
   wrapped in a memoising thunk and only evaluated on the first
   `lazy-cdr`."
  `(cons ,head (%make-promise (lambda () ,tail-form))))

(defun lazy-car (s)
  "Head of S. No thunk forcing — the head was always eager."
  (car s))

(defun lazy-cdr (s)
  "Tail of S. Forces the underlying thunk on first call; later
   calls return the cached result."
  (%force (cdr s)))

(defun lazy-null (s) (null s))

(defparameter *the-empty-lazy* nil
  "Canonical empty lazy list — `nil`, same as a regular list.
   Provided for documentation; you can just use NIL.")

;; ── Operations ──────────────────────────────────────────────────────────

(defun lazy-take (n s)
  "Eagerly consume the first N elements of S and return them as
   an ordinary list."
  (cond
    ((or (zerop n) (lazy-null s)) nil)
    (t (cons (lazy-car s) (lazy-take (- n 1) (lazy-cdr s))))))

(defun lazy-nth (n s)
  "Return the Nth element (0-indexed). Forces along the spine."
  (cond
    ((lazy-null s) nil)
    ((zerop n) (lazy-car s))
    (t (lazy-nth (- n 1) (lazy-cdr s)))))

(defun lazy-map (fn s)
  "Apply FN to every element of S, lazily."
  (cond
    ((lazy-null s) nil)
    (t (lazy-cons (funcall fn (lazy-car s))
                  (lazy-map fn (lazy-cdr s))))))

(defun lazy-filter (pred s)
  "Lazy list of elements of S satisfying PRED. Skips eagerly
   through unmatched prefix, then defers the rest."
  (cond
    ((lazy-null s) nil)
    ((funcall pred (lazy-car s))
     (lazy-cons (lazy-car s) (lazy-filter pred (lazy-cdr s))))
    (t (lazy-filter pred (lazy-cdr s)))))

(defun lazy-zipwith (fn s1 s2)
  "Lazy pointwise combination via FN. Stops when either input
   ends. The classic ingredient for self-referential Fibonacci."
  (cond
    ((or (lazy-null s1) (lazy-null s2)) nil)
    (t (lazy-cons (funcall fn (lazy-car s1) (lazy-car s2))
                  (lazy-zipwith fn (lazy-cdr s1) (lazy-cdr s2))))))

(defun lazy-cons-list (head-list tail-stream)
  "Front the eager list HEAD-LIST onto the lazy TAIL-STREAM."
  (cond
    ((null head-list) tail-stream)
    (t (lazy-cons (car head-list)
                  (lazy-cons-list (cdr head-list) tail-stream)))))

;; ── Standard infinite streams ──────────────────────────────────────────

(defun integers-from (n)
  "The infinite stream N, N+1, N+2, …"
  (lazy-cons n (integers-from (+ n 1))))

(defparameter *naturals* (integers-from 1)
  "1, 2, 3, …")

(defun lazy-iterate (fn x)
  "x, (fn x), (fn (fn x)), …"
  (lazy-cons x (lazy-iterate fn (funcall fn x))))

;; The classic prime sieve from SICP. Filters the infinite stream
;; of integers ≥ 2 by removing multiples of each one's head as
;; you encounter it.

(defun sieve (s)
  (lazy-cons (lazy-car s)
             (sieve (lazy-filter
                     (lambda (n)
                       (not (zerop (rem n (lazy-car s)))))
                     (lazy-cdr s)))))

(defparameter *primes* (sieve (integers-from 2))
  "Lazy stream of primes via the SICP sieve.
   `(lazy-take 10 *primes*)` => (2 3 5 7 11 13 17 19 23 29)")

(provide 'lazy)
nil
