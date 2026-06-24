;;;; bouncing.lisp — port of cormanlisp/examples/gui/bouncing.lisp.
;;;;
;;;; A red ball bounces around inside a window. Click anywhere to
;;;; teleport the ball to the cursor. Resize the window and the
;;;; ball (and its velocity) scales with it.
;;;;
;;;; The original Corman demo manually erased the previous ball
;;;; rect with a white fill before drawing the new one — necessary
;;;; with HDC GDI where there's no notion of "redraw the whole
;;;; pane." With our retained-mode batch model we just emit a fresh
;;;; pane each tick: clear → draw ball.
;;;;
;;;; Usage:
;;;;   ncl --load Lisp/demos/bouncing.lisp --eval "(run-bouncing)"

(defparameter +bg+   (rgb 245 245 245))
(defparameter +ball+ (rgb 220 50 50))

;; Ball state — a single mutable plist held in a defparameter is
;; enough for a one-window demo. Position is the top-left of the
;; ball's bounding box; width/height are the box dimensions;
;; velocity is the per-tick delta in pixels.
(defparameter *ball-x* 0)
(defparameter *ball-y* 0)
(defparameter *ball-w* 30)
(defparameter *ball-h* 30)
(defparameter *vx* 4)
(defparameter *vy* 4)

;; Last seen window dimensions, refreshed on every :resize event.
;; Used by the tick handler for wall collisions.
(defparameter *win-w* 300)
(defparameter *win-h* 300)

(defun resize-ball (w h)
  "Match the original Corman demo: ball dimensions are 1/20 of the
   window in each axis, with a minimum of 1; velocity equals the
   per-axis dimension so motion always crosses one ball-radius per
   tick (looks the same at any window size)."
  (setq *win-w* w)
  (setq *win-h* h)
  (setq *ball-w* (max (truncate w 20) 1))
  (setq *ball-h* (max (truncate h 20) 1))
  (setq *vx* *ball-w*)
  (setq *vy* *ball-h*))

(defun advance-ball ()
  "Move one tick + bounce off walls. Mirror exactly what the
   original did: when the new position would step off-screen, flip
   the velocity and clamp the position to the wall."
  (setq *ball-x* (+ *ball-x* *vx*))
  (setq *ball-y* (+ *ball-y* *vy*))
  (when (< *ball-x* 0)
    (setq *vx* (- 0 *vx*))
    (setq *ball-x* 0))
  (when (> (+ *ball-x* *ball-w*) *win-w*)
    (setq *vx* (- 0 *vx*))
    (setq *ball-x* (- *win-w* *ball-w*)))
  (when (< *ball-y* 0)
    (setq *vy* (- 0 *vy*))
    (setq *ball-y* 0))
  (when (> (+ *ball-y* *ball-h*) *win-h*)
    (setq *vy* (- 0 *vy*))
    (setq *ball-y* (- *win-h* *ball-h*))))

(defun paint-ball (id)
  (with-batch id
    (clear +bg+)
    (fill-oval *ball-x* *ball-y* *ball-w* *ball-h* +ball+)))

(defun run-bouncing ()
  (igui-start)
  (let ((id (open-child "bouncing ball")))
    (cond
      ((null id)
       (format t "** open-child failed~%")
       :failed)
      (t
       ;; Start at the centre of the default window so the first few
       ;; ticks don't trip the wall-clamp before we've seen a :resize.
       (setq *ball-x* 100)
       (setq *ball-y* 100)
       (resize-ball *win-w* *win-h*)
       (paint-ball id)
       ;; ~60 fps. Win32 coalesces pending WM_TIMERs, so a backed-up
       ;; language thread still sees at most one :tick per drain.
       (set-redraw-rate id 16)
       ;; The new event-loop-for: install a persistent filter on
       ;; the bouncing-ball child, then dispatch by :kind. Events
       ;; for other children (a log view, REPL pane, etc.) park
       ;; in the stash. No more (= (getf ev :child-id) id) on
       ;; every clause.
       (event-loop-for id
         (:frame-close (return :done))
         (:close       (return :done))
         (:resize      (resize-ball (getf ev :width) (getf ev :height)))
         (:tick        (advance-ball)
                       (paint-ball id))
         (:mouse       (when (eq (getf ev :op) :LEFT-DOWN)
                         (setq *ball-x* (getf ev :x))
                         (setq *ball-y* (getf ev :y))
                         (paint-ball id))))))))
