;;;; closures.lisp — closure-allocation benchmark.
;;;;
;;;; Each iteration creates a closure that captures two local
;;;; variables and immediately funcalls it. The closure is dead by
;;;; the time the next iteration starts, so every iteration allocates
;;;; one fresh Function + env Vector. Pure stress on
;;;; `ncl_make_closure`'s fast path.

(defparameter *iterations* 1000000)

(defun make-and-call (a b)
  ;; Build a closure capturing (a, b), then invoke it. The closure
  ;; itself does trivial work — the alloc dominates.
  (let ((f (lambda (x) (+ x a b))))
    (funcall f 1)))

(defun run-closures (iters)
  (let ((sum 0))
    (dotimes (i iters)
      (setq sum (+ sum (make-and-call i (+ i 1)))))
    (format t "~%-- closures complete: ~A iters, sum=~A --~%" iters sum)
    sum))

(defun report-gc (label)
  (format t "~%---- gc-stats ~A ----~%" label)
  (let ((s (gc-stats)))
    (loop
      (when (null s) (return))
      (format t "  ~A ~A~%" (car s) (car (cdr s)))
      (setq s (cdr (cdr s)))))
  (format t "---- end ----~%")
  (force-output))

(format t "Closure benchmark: ~A iterations of make-and-call~%" *iterations*)
(report-gc "(baseline)")
(run-closures *iterations*)
(report-gc "(final)")
