;; Bouncing with a 10-second watchdog to test whether it keeps running.
(require 'threads)
(load "E:/CL/NewCormanLisp/Lisp/demos/bouncing.lisp")
(create-thread
  (lambda ()
    (sleep 10)
    (format t "~%** watchdog: 10s elapsed, calling (igui-quit) **~%")
    (igui-quit))
  :report-when-finished nil)
(let ((r (run-bouncing)))
  (format t "~%** bouncing returned ~A **~%" r))
nil
