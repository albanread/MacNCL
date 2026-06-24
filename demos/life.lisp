;;;; demos/life.lisp — deliberate GC stress test in the shape of
;;;; Conway's Game of Life.
;;;;
;;;; **This is not a Life implementation.** It's a workload chosen
;;;; specifically to provoke the garbage collector. Every design
;;;; choice that looks inefficient is on purpose:
;;;;
;;;;   - Board state is a LIST of `(x . y)` conses, not the 2D
;;;;     array Roger Corman's examples/life.lisp uses. An array-
;;;;     based Life mutates in place and allocates effectively
;;;;     nothing per tick; that's useless for GC testing. The
;;;;     list-based version allocates a fresh live-cell list every
;;;;     generation.
;;;;
;;;;   - `member-cell` is a linear scan called from inside a
;;;;     triple-nested loop. Each call constructs candidate
;;;;     `(cons nx ny)` for comparison. Bad asymptotically; that's
;;;;     the point. Per-tick allocation count scales
;;;;     superlinearly with live population.
;;;;
;;;;   - `candidate-positions` builds ~9 conses per live cell,
;;;;     most of which are duplicates the dedup step discards.
;;;;     Maximising pressure rather than minimising work.
;;;;
;;;;   - The R-pentomino seed produces a 1100-step chaotic
;;;;     evolution before settling — the longest natural runtime
;;;;     among small Life patterns. Plenty of churn.
;;;;
;;;; Cumulative allocation across 300 generations runs to tens of
;;;; megabytes — enough to force multiple real GC cycles on the
;;;; default heap. Faithful Corman porting would allocate ~10 KB
;;;; total and provoke zero GCs. The inefficient port is the test.
;;;;
;;;; What this is testing:
;;;;
;;;;   - The GC handles sustained allocation pressure without
;;;;     panicking or losing references.
;;;;   - Per-cycle survivor volume is bounded under steady-state
;;;;     Life evolution (live-cell count plateaus after the
;;;;     transient).
;;;;   - `(gc-stats)` numbers (MINOR-GCS, BYTES-PROMOTED, peak
;;;;     young, pause times) reflect reasonable steady-state
;;;;     behaviour, not pathological climbing.
;;;;
;;;; Run with:
;;;;
;;;;   ncl -l demos/life.lisp
;;;;
;;;; Prints the board every ~25 generations, plus `(gc-stats)`
;;;; every ~50 generations. Auto-exits after `*max-generations*`.
;;;;
;;;; The numbers to watch in the output:
;;;;
;;;;   - MINOR-GCS climbing steadily (a few per 50 generations).
;;;;   - PEAK-YOUNG-BYTES bounded across the run (not climbing
;;;;     toward YOUNG-CAP).
;;;;   - OLD-USED stabilising after the transient.
;;;;   - BYTES-PROMOTED-TOTAL / MINOR-GCS small (hundreds of KB
;;;;     per cycle; a large value indicates over-pinning, leaked
;;;;     rooting, or marked-but-shouldn't-be-live objects).
;;;;
;;;; Live-cell count over generations is also a correctness oracle:
;;;; if the GC ever silently corrupts a cons cell, the board's
;;;; evolution diverges from R-pentomino's well-known trajectory.

(defparameter *width* 50)
(defparameter *height* 25)
(defparameter *max-generations* 300)
(defparameter *print-every* 25)
(defparameter *stats-every* 50)

;; R-pentomino seed, placed near the middle of the grid.
(defparameter *initial-cells*
  '((25 . 12) (26 . 12)
    (24 . 13) (25 . 13)
    (25 . 14)))

;; ── State helpers ─────────────────────────────────────────────────────

(defun in-bounds-p (x y)
  (and (>= x 0) (< x *width*) (>= y 0) (< y *height*)))

(defun cell-equal (a b)
  (and (= (car a) (car b)) (= (cdr a) (cdr b))))

(defun member-cell (cell cells)
  ;; Linear search — O(n) per call, deliberately inefficient so the
  ;; per-tick work scales and the GC sees real pressure.
  (cond ((null cells) nil)
        ((cell-equal cell (car cells)) t)
        (t (member-cell cell (cdr cells)))))

(defun count-neighbors (cells x y)
  (let ((count 0))
    (dotimes (dy 3)
      (dotimes (dx 3)
        (let ((nx (+ x dx -1)) (ny (+ y dy -1)))
          (unless (and (= nx x) (= ny y))
            (when (member-cell (cons nx ny) cells)
              (setq count (+ count 1)))))))
    count))

(defun candidate-positions (cells)
  ;; Every position that could change this tick: every live cell
  ;; plus every neighbor of a live cell. Allocates ~9 conses per
  ;; live cell. Duplicates allowed; the dedup happens in `step`.
  (let ((result nil))
    (dolist (cell cells)
      (let ((x (car cell)) (y (cdr cell)))
        (dotimes (dy 3)
          (dotimes (dx 3)
            (let ((nx (+ x dx -1)) (ny (+ y dy -1)))
              (when (in-bounds-p nx ny)
                (setq result (cons (cons nx ny) result))))))))
    result))

(defun step-generation (cells)
  ;; Build the next generation from `cells`. Returns a fresh list;
  ;; the input list is logically discarded.
  (let ((candidates (candidate-positions cells))
        (next nil))
    (dolist (pos candidates)
      (let* ((x (car pos))
             (y (cdr pos))
             (alive (member-cell pos cells))
             (n     (count-neighbors cells x y)))
        (when (or (and alive (or (= n 2) (= n 3)))
                  (and (not alive) (= n 3)))
          (unless (member-cell pos next)
            (setq next (cons (cons x y) next))))))
    next))

;; ── Display ───────────────────────────────────────────────────────────

(defun cell-at (cells x y)
  (cond ((null cells) nil)
        ((and (= (caar cells) x) (= (cdar cells) y)) t)
        (t (cell-at (cdr cells) x y))))

(defun print-board (cells gen)
  (format t "~%-- generation ~A, ~A live cells --~%"
          gen (length cells))
  (dotimes (y *height*)
    (dotimes (x *width*)
      (if (cell-at cells x y)
          (format t "#")
          (format t ".")))
    (format t "~%"))
  (force-output))

(defun print-gc-stats (label)
  (format t "~%---- gc-stats ~A ----~%" label)
  (let ((s (gc-stats)))
    (loop
      (when (null s) (return))
      (format t "  ~A ~A~%" (car s) (car (cdr s)))
      (setq s (cdr (cdr s)))))
  (format t "---- end ----~%")
  (force-output))

;; ── Main loop ─────────────────────────────────────────────────────────

(defun run-life ()
  (format t "Conway's Life — ~Ax~A grid, ~A generations~%"
          *width* *height* *max-generations*)
  (print-gc-stats "(baseline)")
  (let ((cells *initial-cells*)
        (gen 0))
    (loop
      (when (>= gen *max-generations*) (return nil))
      (when (zerop (mod gen *print-every*))
        (print-board cells gen))
      (when (and (> gen 0) (zerop (mod gen *stats-every*)))
        (print-gc-stats (format nil "after gen ~A" gen)))
      (setq cells (step-generation cells))
      (setq gen (+ gen 1))))
  (print-gc-stats "(final)"))

(run-life)
