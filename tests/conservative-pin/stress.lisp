(require 'threads)
(defparameter *r* (make-atomic-counter 0))

;; Heavy mixed allocation: cons cells, format strings, integer math.
(defun heavy-worker (n)
  (dotimes (i n)
    (thread-safepoint)
    (let ((a (cons i nil))
          (b (cons (+ i 1) (cons (+ i 2) nil)))
          (msg (format nil "iter ~A" i)))
      ;; Use them so they don't get DCE'd
      (atomic-incf *r* (car a))
      (atomic-incf *r* (car b))
      (atomic-incf *r* (length msg)))))

(defparameter *tids* nil)
(dotimes (i 16)
  (push (create-thread (let ((k 1500)) (lambda () (heavy-worker k))))
        *tids*))
(dolist (tid *tids*) (join-thread tid))

(format t "16 threads x 1500 heavy iters: final counter = ~A~%" (atomic-get *r*))
(format t "all threads joined cleanly~%")
