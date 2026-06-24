;;;; shapes.lisp — exercise the round drawing primitives.

(defun paint-shapes (id width height)
  (with-batch id
    (clear +slate+)
    (draw-text 12 8 "shapes" 18 +white+)

    ;; Filled ovals across the top row.
    (fill-oval  60 60 80 50 +red+)
    (fill-oval 160 60 80 50 +green+)
    (fill-oval 260 60 80 50 +blue+)

    ;; Stroked ovals below.
    (stroke-oval  60 130 80 50 2 +red+)
    (stroke-oval 160 130 80 50 2 +green+)
    (stroke-oval 260 130 80 50 2 +blue+)

    ;; Filled and stroked circles.
    (fill-circle    100 240 30 +yellow+)
    (stroke-circle  200 240 30 3 +yellow+)

    ;; A few arcs at varying rotations / apertures.
    (draw-arc 320 240 30   0  90 3 +white+)  ; 0° rotated, 90° span
    (draw-arc 400 240 30  90 180 3 +white+)  ; half-circle pointing down

    ;; Live size readout in the corner.
    (draw-text 12 (- height 24)
               (format nil "~A × ~A" width height)
               12 +white+)))

(defun run-shapes ()
  (igui-start)
  (let ((id (open-child "shapes")))
    (paint-shapes id 480 320)
    (event-loop-for id
      (:frame-close (return :done))
      (:close       (close-child id) (return :done))
      (:resize      (paint-shapes id (getf ev :width) (getf ev :height))))))
