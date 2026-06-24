;;;; hello-igui.lisp — minimal "pixels on screen" demo.
;;;;
;;;; Opens an MDI child, paints a panel with some shapes, runs an
;;;; event loop that re-paints on every :resize and exits when
;;;; the frame is closed.
;;;;
;;;; Run with:  ncl --eval "(load \"Lisp/demos/hello-igui.lisp\")"
;;;; (the loader is still TODO — for now, paste this into a session
;;;; that's already loaded the stdlib, or invoke the body via --eval.)

(defun paint-hello (child-id width height)
  "Paint a simple scene into CHILD-ID's surface, sized to WIDTH × HEIGHT."
  (with-batch child-id
    (clear +slate+)
    ;; Header bar with title text.
    (fill-rect 0 0 width 40 +panel+)
    (draw-text 12 8 "hello, NewCL" 18 +white+)
    ;; Centred-ish swatches with labels.
    (fill-rect 60 80 100 60 +red+)
    (fill-rect 200 80 100 60 +green+)
    (fill-rect 340 80 100 60 +blue+)
    (draw-text 76 142 "red" 13 +white+)
    (draw-text 212 142 "green" 13 +white+)
    (draw-text 354 142 "blue" 13 +white+)
    ;; A cross of lines through the swatch row.
    (draw-line 0 110 width 110 1 +white+)
    (draw-line 250 60 250 160 1 +white+)
    ;; An outlined rectangle around the whole drawing area.
    (stroke-rect 4 4 (- width 8) (- height 8) 2 +yellow+)
    ;; Footer with the size readout.
    (draw-text 12 (- height 24) (format nil "~A × ~A" width height) 12 +white+)))

(defun run-hello-igui ()
  "Open a child, paint it, drive the event loop until the frame closes."
  (igui-start)
  (let ((id (open-child "hello-igui")))
    ;; Initial paint at a guessed size — the first :RESIZE event
    ;; will repaint at the actual size.
    (paint-hello id 480 320)
    (event-loop-for id
      (:frame-close (return :done))
      (:close       (close-child id) (return :done))
      (:resize      (paint-hello id (getf ev :width) (getf ev :height))))))

;; Calling code:
;;   (run-hello-igui)
