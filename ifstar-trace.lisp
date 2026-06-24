(require 'ifstar)
(handler-case (if* (= 1 1) then :yes)
  (error (c) (format t "caught: ~A~%" c)))
