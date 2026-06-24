(defun recurse (n)
  (cond
    ((= n 0) (error "boom"))
    (t (+ 1 (recurse (- n 1))))))

(format t "depth 200:~%")
(handler-case (recurse 200)
  (error (c) (format t "  caught~%")))

(format t "depth 500:~%")
(handler-case (recurse 500)
  (error (c) (format t "  caught~%")))

(format t "depth 1000:~%")
(handler-case (recurse 1000)
  (error (c) (format t "  caught~%")))

(format t "DONE~%")
