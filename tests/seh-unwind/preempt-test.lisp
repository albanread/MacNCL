(require 'threads)
;; A worker that does NOT cooperatively bail when terminate-thread is
;; called. The OLD cooperative model required the worker's loop to
;; check the safepoint return value. The NEW preemptive model
;; just panics out from inside thread-safepoint.
(defparameter *ticks* 0)
(defun tick-worker ()
  (loop
    (thread-safepoint)        ; <-- no (when ... (return)), no early-exit
    (setq *ticks* (+ *ticks* 1))))

(let ((tid (create-thread #'tick-worker)))
  (sleep 0.05)
  (format t "before terminate: *ticks*=~A~%" *ticks*)
  (terminate-thread tid)
  (sleep 0.05)
  (join-thread tid)
  (format t "after terminate + join: *ticks*=~A~%" *ticks*)
  (format t "thread-handle = ~A (should be NIL)~%" (thread-handle tid)))

(format t "DONE~%")
