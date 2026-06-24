;;;; life-gui.lisp — port of cormanlisp/examples/gui/life-gui.lisp.
;;;;
;;;; Conway's Game of Life with a 15×15 grid, click-to-toggle, and a
;;;; 100 ms tick. Live cells are drawn as rainbow-cycled filled
;;;; ellipses on a white background; dead cells are blank squares.
;;;; The original demo used Win32 GDI directly with a black grid +
;;;; SetTimer / KillTimer; we render through iGui's retained-mode
;;;; batch model and let `set-redraw-rate` drive the ticks.
;;;;
;;;; The original had a File menu (Clear / Pause / Resume). iGui
;;;; doesn't carry menus, so the same controls land on the keyboard:
;;;;
;;;;   space  pause / resume
;;;;   c      clear the board
;;;;   r      reset to the three-cell seed (horizontal blinker)
;;;;
;;;; Usage:
;;;;
;;;;   ncl --windows -l Lisp/demos/life-gui.lisp --eval "(run-life-gui)"
;;;;
;;;; Single-call form for transcript convenience:
;;;;
;;;;   (run-life-gui)
;;;;
;;;; The faithful-tribute upstream is `examples/gui/life-gui.lisp` in
;;;; the corman tree. Game logic, grid dimensions, and seed pattern
;;;; preserved; the GUI substrate is iGui not MFC.

;; ── Board state ─────────────────────────────────────────────────────

(defparameter +horiz+ 15)
(defparameter +vert+  15)

;; 1D row-major storage. (cell-index x y) → fixnum; (svref *cells* idx)
;; is T for live, NIL for dead. `*counters*` is the same shape and
;; holds neighbour counts for the current advance pass.
(defparameter *cells*    (make-array (* +horiz+ +vert+) :initial-element nil))
(defparameter *counters* (make-array (* +horiz+ +vert+) :initial-element 0))

(defparameter *paused*  nil)
(defparameter *win-w*   300)
(defparameter *win-h*   300)

;; Rainbow counter: ticks through the RGB cube one cell at a time so
;; live cells visibly change colour as the simulation advances.
(defparameter *colour-r* 0)
(defparameter *colour-g* 0)
(defparameter *colour-b* 0)

(defun cell-index (x y)
  (+ x (* y +horiz+)))

(defun cell-at (x y)
  (svref *cells* (cell-index x y)))

(defun set-cell (x y value)
  (setf (svref *cells* (cell-index x y)) value))

(defun toggle-cell (x y)
  (set-cell x y (not (cell-at x y))))

;; ── Colours ─────────────────────────────────────────────────────────

(defparameter +bg+   (rgb 255 255 255))
(defparameter +grid+ (rgb 0   0   0  ))

(defun next-colour ()
  "Advance through the RGB cube in 16-step increments. Mirrors the
   corman demo's `get-rgb` which produced 4096 distinct colours
   before wrapping. Returns a packed RGB fixnum."
  (let ((r *colour-r*) (g *colour-g*) (b *colour-b*))
    (setq *colour-r* (+ *colour-r* 16))
    (when (>= *colour-r* 256)
      (setq *colour-r* 0)
      (setq *colour-b* (+ *colour-b* 16)))
    (when (>= *colour-b* 256)
      (setq *colour-b* 0)
      (setq *colour-g* (+ *colour-g* 16)))
    (when (>= *colour-g* 256)
      (setq *colour-g* 0))
    (rgb r g b)))

;; ── Drawing ─────────────────────────────────────────────────────────

(defun paint (id)
  "Redraw the entire pane. with-batch makes this latest-wins, so we
   re-emit everything every tick — no diff bookkeeping needed."
  (let* ((cell-w (max (truncate *win-w* +horiz+) 1))
         (cell-h (max (truncate *win-h* +vert+)  1)))
    (with-batch id
      (clear +bg+)
      ;; Vertical grid lines (one extra past the last column).
      (dotimes (i (+ +horiz+ 1))
        (fill-rect (* cell-w i) 0 1 (* cell-h +vert+) +grid+))
      ;; Horizontal grid lines.
      (dotimes (i (+ +vert+ 1))
        (fill-rect 0 (* cell-h i) (* cell-w +horiz+) 1 +grid+))
      ;; Live cells as filled ovals, rainbow-cycled.
      (dotimes (y +vert+)
        (dotimes (x +horiz+)
          (when (cell-at x y)
            (fill-oval
              (+ 2 (* x cell-w))
              (+ 2 (* y cell-h))
              (- cell-w 3)
              (- cell-h 3)
              (next-colour))))))))

;; ── Conway step ─────────────────────────────────────────────────────

(defun neighbour-count (x y)
  "How many of (x, y)'s eight neighbours are live? Edges don't wrap —
   this is the same finite-board interpretation the corman demo uses."
  (let ((n 0))
    (when (and (> x 0)            (> y 0)            (cell-at (- x 1) (- y 1))) (setq n (+ n 1)))
    (when (and                    (> y 0)            (cell-at    x    (- y 1))) (setq n (+ n 1)))
    (when (and (< x (- +horiz+ 1))(> y 0)            (cell-at (+ x 1) (- y 1))) (setq n (+ n 1)))
    (when (and (> x 0)                               (cell-at (- x 1)    y))    (setq n (+ n 1)))
    (when (and (< x (- +horiz+ 1))                   (cell-at (+ x 1)    y))    (setq n (+ n 1)))
    (when (and (> x 0)            (< y (- +vert+ 1)) (cell-at (- x 1) (+ y 1))) (setq n (+ n 1)))
    (when (and                    (< y (- +vert+ 1)) (cell-at    x    (+ y 1))) (setq n (+ n 1)))
    (when (and (< x (- +horiz+ 1))(< y (- +vert+ 1)) (cell-at (+ x 1) (+ y 1))) (setq n (+ n 1)))
    n))

(defun advance-board ()
  "One Conway step: standard B3/S23 — a live cell with 2 or 3
   neighbours survives, a dead cell with exactly 3 neighbours is
   born. Two-phase update: first snapshot all the counts, then
   apply the deaths and births."
  ;; Pass 1: snapshot counts.
  (dotimes (y +vert+)
    (dotimes (x +horiz+)
      (setf (svref *counters* (cell-index x y))
            (neighbour-count x y))))
  ;; Pass 2: apply the rule.
  (dotimes (y +vert+)
    (dotimes (x +horiz+)
      (let ((n (svref *counters* (cell-index x y))))
        (cond
          ((and (cell-at x y) (or (< n 2) (> n 3)))
           (set-cell x y nil))
          ((and (not (cell-at x y)) (= n 3))
           (set-cell x y t)))))))

;; ── Board lifecycle ─────────────────────────────────────────────────

(defun clear-board ()
  (dotimes (i (* +horiz+ +vert+))
    (setf (svref *cells* i) nil)))

(defun seed-board ()
  "Drop the corman demo's three-cell starter: a horizontal blinker
   at the centre of the grid. Period-2 oscillator — alternates
   between horizontal and vertical orientation each tick."
  (clear-board)
  (set-cell 6 7 t)
  (set-cell 7 7 t)
  (set-cell 8 7 t))

(defun toggle-cell-at-pixel (px py)
  "Click handler. Convert the pixel coordinate to a (cell-x, cell-y)
   pair, clamp to the grid, toggle. Matches the corman demo's
   `toggle-cell-at-position`."
  (let* ((cell-w (max (truncate *win-w* +horiz+) 1))
         (cell-h (max (truncate *win-h* +vert+)  1))
         (cx     (min (truncate px cell-w) (- +horiz+ 1)))
         (cy     (min (truncate py cell-h) (- +vert+ 1))))
    (toggle-cell cx cy)))

;; ── Entry point ────────────────────────────────────────────────────

(defun run-life-gui ()
  (igui-start)
  (let ((id (open-child "Life")))
    (cond
      ((null id)
       (format t "** open-child failed (is --windows enabled?)~%")
       :failed)
      (t
       (seed-board)
       (paint id)
       ;; 100 ms tick to match the corman demo's *refresh-milliseconds*.
       (set-redraw-rate id 100)
       (event-loop-for id
         (:frame-close (return :done))
         (:close       (return :done))
         (:resize      (setq *win-w* (max (getf ev :width)  1))
                       (setq *win-h* (max (getf ev :height) 1)))
         (:tick        (unless *paused* (advance-board))
                       (paint id))
         (:mouse       (when (eq (getf ev :op) :left-down)
                         (toggle-cell-at-pixel (getf ev :x) (getf ev :y))
                         (paint id)))
         (:char        (let ((ch (getf ev :char)))
                         (cond
                           ((or (eq ch #\space) (eq ch #\Space))
                            (setq *paused* (not *paused*)))
                           ((or (eq ch #\c) (eq ch #\C))
                            (clear-board)
                            (paint id))
                           ((or (eq ch #\r) (eq ch #\R))
                            (seed-board)
                            (paint id))))))))))
