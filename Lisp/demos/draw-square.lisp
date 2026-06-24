;;;; Lisp/demos/draw-square.lisp — single-window static-scene demo.
;;;;
;;;; Port of NewBCPL's examples/draw-square.bcl. Opens an MDI child
;;;; titled "Shapes", paints a static scene (rectangle, circle,
;;;; line, caption), then loops until the user closes the window.
;;;;
;;;; Smallest possible "an iGui window with content" example. Use
;;;; it as the template when you're starting a new visual demo.
;;;;
;;;; Run:
;;;;
;;;;   ncl --load Lisp/demos/draw-square.lisp --eval "(run-draw-square)"
;;;;
;;;; Or from inside a session:
;;;;
;;;;   (load "Lisp/demos/draw-square.lisp")
;;;;   (run-draw-square)

(defparameter +bg+   (rgb 30 36 46))     ; dark slate
(defparameter +red+  (rgb 235 76 76))
(defparameter +cyan+ (rgb 76 217 242))
(defparameter +green+ (rgb 102 230 102))
(defparameter +white+ (rgb 255 255 255))

(defun paint-shapes (id width height)
  (with-batch id
    ;; Background.
    (clear +bg+)

    ;; Filled red square, 100×100, top-left at (40, 40).
    (fill-rect 40 40 100 100 +red+)

    ;; Cyan stroked rectangle with a circle inside.
    (stroke-rect 200 40 120 100 3 +cyan+)
    (fill-circle 260 90 38 +cyan+)

    ;; Green diagonal line across the lower band.
    (draw-line 40 200 320 280 4 +green+)

    ;; White caption.
    (draw-text 40 320 "Hello from NewCormanLisp!" 22 +white+)

    ;; Live size readout in the bottom-right.
    (draw-text 12 (- height 24)
               (format nil "~A x ~A" width height)
               12 +white+)))

(defun run-draw-square ()
  "Open the iGui frame if it isn't already, open a child titled
   'Shapes', paint it, run an event loop until the window closes.
   Returns :done on clean exit."
  (igui-start)
  (let ((id (open-child "Shapes")))
    (cond
      ((null id)
       (format t "** open-child failed (is iGui running?)~%")
       :failed)
      (t
       (paint-shapes id 480 360)
       (event-loop-for id
         (:frame-close (return :done))
         (:close       (return :done))
         (:resize      (paint-shapes id
                                     (getf ev :width)
                                     (getf ev :height))))))))
