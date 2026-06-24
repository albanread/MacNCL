(require 'time)

(format t "internal-time-units-per-second = ~A~%" internal-time-units-per-second)

(let* ((a (get-internal-real-time))
       (b (get-internal-real-time)))
  (format t "two reads delta (ns): ~A~%" (- b a)))

(time (+ 1 2 3))

(defun build-list (n)
  (let ((acc nil))
    (dotimes (i n) (setq acc (cons i acc)))
    acc))

(time (length (build-list 100000)))

(format t "bench avg ns (1000 calls of (+ 1 2 3)): ~A~%"
        (bench (lambda () (+ 1 2 3)) :repeats 1000))

(format t "values  -> ~A~%" (multiple-value-list (funcall #'values 10 20 30)))
(format t "v-list  -> ~A~%" (multiple-value-list (apply #'values '(7 8 9))))
nil
