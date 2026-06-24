(format t "10 draws of (random 100):~%  ")
(dotimes (i 10) (format t "~A " (random 100)))
(format t "~%")

(format t "6000 draws of (random 6) — distribution check (expect 1000 each):~%")
(let ((buckets (make-array 6 :initial-element 0)))
  (dotimes (i 6000)
    (let ((b (random 6)))
      (setf (aref buckets b) (+ 1 (aref buckets b)))))
  (format t "  ~A~%" buckets))

(format t "fastest call?~%")
(require 'time)
(format t "  bench avg ns (1000 × (random 1000)): ~A~%"
        (bench (lambda () (random 1000)) :repeats 1000))
nil
