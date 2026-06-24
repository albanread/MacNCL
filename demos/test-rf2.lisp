(defun f (x) (if x (return-from f 99)) 7)
(format t "f(nil)=~S f(t)=~S~%" (f nil) (f t))   ; 7 99
(defun g (n) (dotimes (i 10) (when (= i n) (return-from g i))) -1)
(format t "g(3)=~S g(20)=~S~%" (g 3) (g 20))       ; 3 -1
(format t "done~%")
