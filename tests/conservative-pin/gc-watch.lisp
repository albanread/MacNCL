;;;; gc-watch.lisp — let workers pound the heap, watch GC stats move.
;;;;
;;;; Run: ncl.exe --load tests/conservative-pin/gc-watch.lisp
;;;;
;;;; 8 workers, each in a tight loop allocating a 3-cell cons chain
;;;; per iteration. Main sleeps for 5 s (parked via enter_blocked so
;;;; the GC trigger doesn't wait for it). After joining, one
;;;; one-line `(gc-stats)` dump.
;;;;
;;;; What you should see on a 16 MiB young / 64 MiB old config:
;;;;   * tens of millions of worker iterations
;;;;   * ~70+ minor GC cycles
;;;;   * ~1 GiB cumulative bytes promoted (young swept ~70 times)
;;;;   * pinned-objects near zero (very low false-positive rate for
;;;;     this workload — workers don't keep heap pointers in
;;;;     stack-resident locals long enough to be scanned)
;;;;   * peak-young = 16 MiB
;;;;
;;;; Caveats (separate follow-ups):
;;;;   * Calling FORMAT from a worker hangs — v1 CLOS dispatch
;;;;     isn't thread-safe (shared method caches under defgeneric).
;;;;     For the demo we keep workers cons-only and do all
;;;;     formatting from main, after the workers stop.
;;;;   * Main doing significant Lisp work mid-run (interim stats
;;;;     dumps via format) hits the same path. So: stats are
;;;;     dumped once at the end.

(require 'threads)

(defparameter *stop* nil)
(defparameter *ctr* (make-atomic-counter 0))

(defun worker ()
  (loop
    (when *stop* (return))
    (thread-safepoint)
    (let ((c (cons 1 (cons 2 (cons 3 nil)))))
      (atomic-incf *ctr* (length c)))))

(defparameter *tids* nil)
(dotimes (i 8) (push (create-thread #'worker) *tids*))

(sleep 5)

(setq *stop* t)
(dolist (tid *tids*) (join-thread tid))

(let ((s (gc-stats)))
  (format t "~%GC stats after 5 s of 8-worker allocation pressure:~%")
  (format t "  minor-gcs              = ~A~%" (getf s :minor-gcs))
  (format t "  bytes-promoted-total   = ~A~%" (getf s :bytes-promoted-total))
  (format t "  objects-pinned-total   = ~A~%" (getf s :objects-pinned-total))
  (format t "  pinned-residual-cells  = ~A~%" (getf s :pinned-residual-cells))
  (format t "  peak-young-bytes       = ~A~%" (getf s :peak-young-bytes))
  (format t "  young-used / cap       = ~A / ~A~%"
          (getf s :young-used) (getf s :young-cap))
  (format t "  old-used   / cap       = ~A / ~A~%"
          (getf s :old-used) (getf s :old-cap)))
(format t "~%total worker iterations: ~A~%" (atomic-get *ctr*))
