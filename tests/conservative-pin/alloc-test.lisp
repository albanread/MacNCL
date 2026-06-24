(require 'threads)

;; Real-world Lisp: workers allocate cons cells (and format strings,
;; intermediate values, closure envs) every iteration. Without
;; conservative pin, a peer's GC firing mid-loop would leave a
;; <forward:...> in this worker's JIT locals on resume.

(defparameter *result* (make-atomic-counter 0))

(defun allocating-worker (n)
  (dotimes (i n)
    (thread-safepoint)
    ;; This iteration allocates: list/cons, format, integer ops.
    ;; If conservative pin works, no <forward:...> panics.
    (let ((pair (cons i (+ i 1))))
      (atomic-incf *result* (+ (car pair) (cdr pair))))))

(defparameter *n-threads* 8)
(defparameter *n-iters* 2000)
(defparameter *tids* nil)

(dotimes (i *n-threads*)
  (push (create-thread
         (let ((k *n-iters*))
           (lambda () (allocating-worker k))))
        *tids*))
(dolist (tid *tids*) (join-thread tid))

(format t "expected ~A, got ~A~%"
        ;; sum over 8 threads of sum(i + (i+1)) for i in 0..n
        ;; = 8 * (2*sum(i) + n) for i in 0..n
        ;; = 8 * (2 * n*(n-1)/2 + n)
        ;; = 8 * (n*n)
        (* 8 (* *n-iters* *n-iters*))
        (atomic-get *result*))
