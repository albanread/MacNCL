;; Bouncing with a 3-minute watchdog. Verifies GC stability under
;; sustained 60Hz allocation pressure with park-on-block active.
(require 'threads)
(load "E:/CL/NewCormanLisp/Lisp/demos/bouncing.lisp")
(create-thread
  (lambda ()
    (sleep 180)
    (format t "~%** watchdog: 180s elapsed, calling (igui-quit) **~%")
    (igui-quit))
  :report-when-finished nil)
(let ((r (run-bouncing)))
  (format t "~%** bouncing returned ~A **~%" r))
nil
