;; Smaller test: just hammer next-event with park-on-block enabled,
;; minimal allocation per iteration. If retire_tlab itself is the
;; segfault, we'll hit it quickly.
(require 'threads)
(igui-start)
(let ((id (open-child "park-smoke")))
  (format t "opened child ~A~%" id)
  (create-thread
    (lambda ()
      (sleep 5)
      (format t "** watchdog: 5s **~%")
      (igui-quit))
    :report-when-finished nil)
  (let ((n 0))
    (loop
      (let ((ev (next-event -1)))
        (cond
          ((null ev) nil)
          ((eq (getf ev :kind) :frame-close)
           (format t "** ended after ~A events **~%" n)
           (return :done))
          (t
           (setq n (+ n 1))
           (when (= (rem n 50) 0)
             (format t "  ~A events~%" n))))))))
nil
