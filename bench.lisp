(require 'time)
(require 'memoize)

(defun fib (n) (if (<= n 1) n (+ (fib (- n 1)) (fib (- n 2)))))

(format t "~%?? fib 25 (unmemoized recursive) ??~%")
(time (fib 25))

(memoize fib)
(format t "~%?? fib 25 (memoized, first call populates cache) ??~%")
(time (fib 25))
(format t "~%?? fib 25 (memoized, second call all cache hits) ??~%")
(time (fib 25))
(format t "~%?? fib 100 (memoized) ??~%")
(time (fib 100))

(unmemoize fib)

;; Tak: classic Lisp micro-benchmark (Gabriel benchmarks).
(defun tak (x y z)
  (if (< y x)
      (tak (tak (- x 1) y z)
           (tak (- y 1) z x)
           (tak (- z 1) x y))
      z))
(format t "~%?? (tak 18 12 6) ??~%")
(time (tak 18 12 6))
