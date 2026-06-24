;;;; conses.lisp — cons-allocation benchmark.
;;;;
;;;; Builds and walks a fresh N-element list every iteration. Each
;;;; iteration allocates N cons cells. Tests `ncl_alloc_cons`
;;;; throughput directly — no closures, no strings, no numerics
;;;; outside fixnum range.

(defparameter *iterations* 100000)
(defparameter *list-length* 50)

(defun build-list (n)
  (let ((acc nil))
    (dotimes (i n)
      (setq acc (cons i acc)))
    acc))

(defun list-length-walk (lst)
  (let ((count 0))
    (dolist (x lst)
      (declare (ignore x))
      (setq count (+ count 1)))
    count))

(defun run-conses (iters n)
  (let ((sum 0))
    (dotimes (i iters)
      (setq sum (+ sum (list-length-walk (build-list n)))))
    (format t "~%-- conses complete: ~A iters, sum=~A --~%" iters sum)
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

(format t "Cons benchmark: ~A iterations of build+walk a ~A-element list~%"
        *iterations* *list-length*)
(report-gc "(baseline)")
(run-conses *iterations* *list-length*)
(report-gc "(final)")
