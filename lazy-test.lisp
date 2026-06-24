(require 'lazy)

(format t "naturals 1..10: ~A~%" (lazy-take 10 *naturals*))
(format t "first 15 primes: ~A~%" (lazy-take 15 *primes*))
(format t "100th prime: ~A~%" (lazy-nth 99 *primes*))

;; Self-referential Fibonacci ? lambda-wrap + since `#'+` isn't installed yet.
(defun %plus (a b) (+ a b))
(defparameter *fibs*
  (lazy-cons 0
    (lazy-cons 1
      (lazy-zipwith #'%plus *fibs* (lazy-cdr *fibs*)))))

(format t "first 15 fibs: ~A~%" (lazy-take 15 *fibs*))
(format t "30th fib: ~A~%" (lazy-nth 30 *fibs*))

(defun %dbl (n) (* n 2))
(format t "powers of 2: ~A~%"
        (lazy-take 12 (lazy-iterate #'%dbl 1)))
