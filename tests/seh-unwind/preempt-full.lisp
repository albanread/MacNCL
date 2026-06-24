(require 'threads)

(defparameter *ticks* 0)
(defun tick-worker ()
  (loop
    (thread-safepoint)
    (setq *ticks* (+ *ticks* 1))))

;; --- preemptive terminate-thread ---
(let ((tid (create-thread #'tick-worker)))
  (sleep 0.05)
  (format t "before terminate: ticks=~A~%" *ticks*)
  (terminate-thread tid)
  (join-thread tid)
  (format t "after terminate+join: ticks=~A~%" *ticks*)
  (format t "thread-handle = ~A (NIL=cleaned up)~%" (thread-handle tid)))

;; --- exit-thread from worker ---
(defparameter *seen* nil)
(let ((tid (create-thread
            (lambda ()
              (setq *seen* :before-exit)
              (exit-thread)
              (setq *seen* :AFTER-EXIT)))))
  (join-thread tid)
  (format t "after exit-thread: *seen* = ~A (expect :BEFORE-EXIT)~%" *seen*))

;; --- deep recursion through a REAL panic (test-panic, not flag) ---
(defun rec (n)
  (cond
    ((= n 0) (%test-panic))
    (t (+ 1 (rec (- n 1))))))

(defparameter *deep-result* nil)
(let ((tid (create-thread
            (lambda ()
              (setq *deep-result* :worker-survived)
              (rec 100)
              (setq *deep-result* :AFTER-PANIC)))))
  (join-thread tid)
  (format t "after deep-panic: *deep-result* = ~A (expect :WORKER-SURVIVED)~%"
          *deep-result*))

(format t "DONE~%")
