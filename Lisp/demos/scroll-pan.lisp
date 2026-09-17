;;;; Lisp/demos/scroll-pan.lisp — mouse-wheel / trackpad scrolling demo.
;;;;
;;;; Opens a viewport onto a 1600×1000 star field (roughly 3× the window)
;;;; and pans it with scroll input:
;;;;
;;;;   - wheel / two-finger vertical   pans up and down
;;;;   - two-finger horizontal         pans left and right (:wheel-dx)
;;;;
;;;; Exercises the scroll pipeline end to end: the AppKit event monitor
;;;; reads scrollingDeltaX/Y (converting pixel-precise trackpad deltas to
;;;; lines), the event plist carries :wheel-delta / :wheel-lines /
;;;; :wheel-dx, and this pane consumes them. The readout in the corner
;;;; shows the viewport origin so motion is unambiguous.
;;;;
;;;; Run:
;;;;
;;;;   ncl --load Lisp/demos/scroll-pan.lisp --eval "(run-scroll-pan)"
;;;;
;;;; Or from the IDE: Examples ▸ scroll-pan, then ⌘R.

(defparameter +field-w+ 1600)
(defparameter +field-h+ 1000)

(defparameter +bg+     (rgb 12 16 28))
(defparameter +grid+   (rgb 34 44 68))
(defparameter +star+   (rgb 224 232 244))
(defparameter +gold+   (rgb 235 190 76))
(defparameter +dim+    (rgb 110 120 140))

;; Viewport state: the field coordinate of the viewport's top-left corner,
;; plus the last known window size (wheel repaints happen outside resize).
(defparameter *ox* 0)
(defparameter *oy* 0)
(defparameter *vw* 560)
(defparameter *vh* 400)

;; Deterministic star field: (x y r) triples from a small LCG, so the
;; pattern is identical on every run and the pan is easy to judge.
(defun make-stars (n)
  (let ((seed 42) (stars nil))
    (dotimes (i n (nreverse stars))
      (setq seed (mod (+ (* seed 1103515245) 12345) 2147483648))
      (let ((x (mod seed +field-w+)))
        (setq seed (mod (+ (* seed 1103515245) 12345) 2147483648))
        (let ((y (mod seed +field-h+)))
          (setq seed (mod (+ (* seed 1103515245) 12345) 2147483648))
          (push (list x y (+ 1 (mod seed 3))) stars))))))

(defparameter *stars* (make-stars 220))

(defun clamp-origin ()
  (setq *ox* (max 0 (min *ox* (- +field-w+ *vw*))))
  (setq *oy* (max 0 (min *oy* (- +field-h+ *vh*)))))

(defun paint-scroll (id w h)
  (setq *vw* w *vh* h)
  (clamp-origin)
  (with-batch id
    (clear +bg+)

    ;; Field grid every 100px, offset by the origin — the motion cue.
    (let ((x (- (mod *ox* 100))))
      (loop while (< x w)
            do (draw-line x 0 x h 1 +grid+)
               (incf x 100)))
    (let ((y (- (mod *oy* 100))))
      (loop while (< y h)
            do (draw-line 0 y w y 1 +grid+)
               (incf y 100)))

    ;; Stars inside the viewport only.
    (dolist (s *stars*)
      (let ((sx (- (first s)  *ox*))
            (sy (- (second s) *oy*))
            (r  (third s)))
        (when (and (<= -4 sx (+ w 4)) (<= -4 sy (+ h 4)))
          (fill-circle sx sy r (if (= r 3) +gold+ +star+)))))

    ;; Readout + hint, over a scrim so it stays legible.
    (fill-rect 0 (- h 30) w 30 +bg+)
    (draw-text 12 (- h 21)
               (format nil "origin ~A,~A   field ~A×~A   — scroll pans, sideways scroll moves :wheel-dx"
                       *ox* *oy* +field-w+ +field-h+)
               12 +dim+)))

(defun run-scroll-pan ()
  "Open the 'Scroll Pan' viewport pane and drive it from scroll events
   until its window closes. Returns :done on clean exit."
  (igui-start)
  (let ((id (open-child-sized "Scroll Pan" 560 400)))
    (if (null id)
        (progn
          (format t "** open-child failed (is iGui running?)~%")
          :failed)
        (progn
          (paint-scroll id 560 400)
          (on-window id
            (:mouse
             (when (eq (getf ev :op) :wheel)
               ;; dy > 0 scrolls toward the field's start; horizontal
               ;; dx follows the same convention on its axis.
               (decf *oy* (getf ev :wheel-delta 0))
               (decf *ox* (getf ev :wheel-dx 0))
               (paint-scroll id *vw* *vh*)))
            (:resize
             (paint-scroll id
                           (max 1 (getf ev :width))
                           (max 1 (getf ev :height)))))))))
