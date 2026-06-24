;;;; Lisp/demos/click-counter.lisp — minimal interactive iGui app.
;;;;
;;;; Port of NewBCPL's examples/click-counter.bcl. Opens a window
;;;; with a square; every left click cycles the square through six
;;;; colours and bumps a counter. The count is logged to stdout.
;;;;
;;;; The minimum pattern for an interactive iGui app:
;;;;   1. open the window
;;;;   2. paint the initial state
;;;;   3. event-loop-for the window
;;;;   4. on input, mutate state + repaint
;;;;   5. on :close or :frame-close, return out of the loop
;;;;
;;;; Run:
;;;;
;;;;   ncl --load Lisp/demos/click-counter.lisp --eval "(run-click-counter)"

(defparameter +cc-bg+ (rgb 20 26 36))
(defparameter +cc-fg+ (rgb 217 217 217))

;; Six colours, cycled by (rem count 6).
(defparameter +cc-colors+
  (list (rgb 242 76 76)      ; red
        (rgb 242 166 50)     ; orange
        (rgb 242 235 50)     ; yellow
        (rgb 76 217 76)      ; green
        (rgb 76 140 242)     ; blue
        (rgb 204 102 242)))  ; violet

(defun %cc-color (n)
  (nth (rem n 6) +cc-colors+))

(defun paint-click-counter (id count)
  (with-batch id
    (clear +cc-bg+)
    (draw-text 20 30
               "Click the square. Close the window to exit."
               16 +cc-fg+)
    ;; Square at (50, 60) – (250, 260).
    (fill-rect 50 60 200 200 (%cc-color count))
    ;; Counter readout above the square.
    (draw-text 50 280
               (format nil "clicks: ~D" count)
               14 +cc-fg+)))

(defun run-click-counter ()
  "Open a 'Click Counter' child, paint it, loop on events. Every
   :LEFT-DOWN bumps the counter, recolours the square, and logs."
  (igui-start)
  (let ((id (open-child "Click Counter"))
        (count 0))
    (cond
      ((null id)
       (format t "** open-child failed~%")
       :failed)
      (t
       (paint-click-counter id count)
       (format t "[counter] window open; clicks = 0~%")
       (event-loop-for id
         (:frame-close (return :done))
         (:close       (return :done))
         (:mouse       (when (eq (getf ev :op) :LEFT-DOWN)
                         (setq count (+ count 1))
                         (paint-click-counter id count)
                         (format t "[counter] click ~D at (~D, ~D)~%"
                                 count (getf ev :x) (getf ev :y))))
         (:resize      (paint-click-counter id count)))
       (format t "[counter] exited at ~D clicks~%" count)))))
