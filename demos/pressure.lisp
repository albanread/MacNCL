;;;; pressure.lisp — sane GC pressure test.
;;;;
;;;; Goal: exercise the collector under steady, predictable load
;;;; that touches every major allocating type. Less insane than
;;;; the old `member-cell` blowup (which created millions of
;;;; closures per tick); more thorough than `Life` (which after
;;;; the BLOCK-skip optimisation barely allocates anything).
;;;;
;;;; Each tick allocates:
;;;;   - a small chain of CONS cells
;;;;   - a young-heap STRING via `format`
;;;;   - a RATIO (CL division of two positive integers — bytes
;;;;     into the rational tower)
;;;;   - a BIGNUM (`expt 2 <small>` for a value past 2^60)
;;;;   - a FLOAT (mixed-tower division)
;;;;
;;;; A configurable fraction of ticks is RETAINED (consed onto a
;;;; rolling list); the rest is allowed to die. This lets the
;;;; user dial survival ratio: low = mostly garbage = young
;;;; reclaim path; high = mostly survives = promotion path.
;;;;
;;;; Sweep size with the GC's young-cap env vars:
;;;;   ncl --load pressure.lisp                 ; default young (256 MB)
;;;;   NCL_YOUNG_MB=16 ncl --load pressure.lisp ; modest pressure
;;;;   NCL_YOUNG_MB=2  ncl --load pressure.lisp ; heavy pressure

;; ── Configuration ────────────────────────────────────────────────────

(defparameter *iterations* 50000)
(defparameter *retain-every* 100)
(defparameter *report-every* 10000)

;; ── Per-tick allocation ──────────────────────────────────────────────

(defun pressure-tick (i)
  ;; Build five heap objects of distinct types. Each is held in
  ;; a `let*` local across subsequent allocs, so the GC sees them
  ;; live in the precise root list until this function returns
  ;; the assembled list.
  (let* ((cell (cons i (cons (* i 2) (cons (* i 3) nil))))
         (str  (format nil "tick ~A label" i))
         ;; (i+1)/(i+2) — rational, never reducible to an integer
         ;; for i >= 0, so always exercises the Ratio path.
         (rat  (/ (+ i 1) (+ i 2)))
         ;; expt 2 <n> for n grown by i, modded so it stays in a
         ;; reasonable Bignum size (~2^90 max).
         (big  (expt 2 (+ 70 (mod i 20))))
         ;; Floating-point ratio.
         (flo  (/ (+ i 1.0) 7.0)))
    (list cell str rat big flo)))

;; ── Driver ───────────────────────────────────────────────────────────

(defun run-pressure (iters)
  (let ((retained nil)
        (kept 0)
        (next-report *report-every*))
    (dotimes (i iters)
      (let ((sample (pressure-tick i)))
        (when (zerop (mod i *retain-every*))
          (setq retained (cons sample retained))
          (setq kept (+ kept 1))))
      (when (= i next-report)
        (format t "  tick ~A  retained=~A~%" i kept)
        (force-output)
        (setq next-report (+ next-report *report-every*))))
    (format t "~%-- pressure complete: ~A ticks, ~A retained --~%"
            iters kept)
    ;; Light verification: walk the retained list, touch each piece.
    ;; Catches any latent forwarding-pointer corruption inside the
    ;; survivors — a stale cell would crash here, not silently.
    (let ((walked 0))
      (dolist (sample retained)
        ;; Sample is (cell str rat big flo). Touch fields enough to
        ;; force any stale reads to surface.
        (let ((c (car sample))
              (s (car (cdr sample))))
          ;; cons chain: walk it
          (when (consp c)
            (length c))
          ;; string: probe it
          (when (stringp s)
            (length s)))
        (setq walked (+ walked 1)))
      (format t "verified ~A retained samples~%" walked))
    retained))

;; ── GC stats reporter (copied from life.lisp) ────────────────────────

(defun report-gc (label)
  (format t "~%---- gc-stats ~A ----~%" label)
  (let ((s (gc-stats)))
    (loop
      (when (null s) (return))
      (format t "  ~A ~A~%" (car s) (car (cdr s)))
      (setq s (cdr (cdr s)))))
  (format t "---- end ----~%")
  (force-output))

;; ── Main ─────────────────────────────────────────────────────────────

(format t "Pressure test: ~A iterations, retain every ~A, report every ~A~%"
        *iterations* *retain-every* *report-every*)
(report-gc "(baseline)")
(run-pressure *iterations*)
(report-gc "(final)")
