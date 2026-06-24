(require 'ifstar)

;; Basic: if-then
(format t "test1: ~A~%" (if* (= 1 1) then :yes))
;; Expected: :YES

;; if-then-else
(format t "test2: ~A~%" (if* (= 1 2) then :yes else :no))
;; Expected: :NO

;; if-elseif-else
(defun classify (n)
  (if* (< n 0) then :negative
       elseif (= n 0) then :zero
       elseif (< n 10) then :small
       else :big))
(format t "classify -3 = ~A~%" (classify -3))
(format t "classify 0  = ~A~%" (classify 0))
(format t "classify 5  = ~A~%" (classify 5))
(format t "classify 42 = ~A~%" (classify 42))

;; Multiple consequents per branch
(defparameter *log* nil)
(defun process (op val)
  (if* (eq op :push) then
         (setq *log* (cons val *log*))
         :pushed
       elseif (eq op :reset) then
         (setq *log* nil)
         :reset
       else
         (format t "unknown op ~A~%" op)
         :unknown))
(format t "process :push 1 = ~A~%" (process :push 1))
(format t "process :push 2 = ~A~%" (process :push 2))
(format t "process :reset nil = ~A~%" (process :reset nil))
(format t "process :foo nil = ~A~%" (process :foo nil))

;; thenret: consequent IS the test value
(format t "thenret demo: ~A~%"
        (if* (member 3 '(1 2 3 4 5)) thenret))
;; Expected: (3 4 5)
