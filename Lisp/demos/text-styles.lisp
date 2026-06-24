;;;; text-styles.lisp — exercise styled text rendering.

(defun paint-styles (id width height)
  (with-batch id
    (clear +slate+)
    (draw-text 12 8 "text styles" 18 +white+)

    ;; Plain default text.
    (draw-text 20 60 "Default (Segoe UI 14)" 14 +white+)

    ;; Different weights.
    (draw-text-styled 20 90  "Light"   14 +white+ :weight 300)
    (draw-text-styled 20 110 "Regular" 14 +white+ :weight 400)
    (draw-text-styled 20 130 "Bold"    14 +white+ :weight 700)
    (draw-text-styled 20 150 "Black"   14 +white+ :weight 900)

    ;; Italic and oblique.
    (draw-text-styled 200 90  "Italic"  14 +yellow+ :style :italic)
    (draw-text-styled 200 110 "Oblique" 14 +yellow+ :style :oblique)

    ;; Different family — Consolas (a Windows monospace font).
    (draw-text-styled 20 190 "(defun foo (x) (* x 2))" 14 +green+
                      :family "Consolas")
    (draw-text-styled 20 210 "(defun bar () \"hi\")"   14 +green+
                      :family "Consolas" :weight 700)

    ;; Big serif title.
    (draw-text-styled 20 250 "NewCormanLisp" 24 +red+
                      :family "Georgia" :weight 700 :style :italic)

    ;; Live size readout.
    (draw-text 12 (- height 24)
               (format nil "~A × ~A" width height)
               12 +white+)))

(defun run-text-styles ()
  (igui-start)
  (let ((id (open-child "text styles")))
    (paint-styles id 600 320)
    (event-loop-for id
      (:frame-close (return :done))
      (:close       (close-child id) (return :done))
      (:resize      (paint-styles id (getf ev :width) (getf ev :height))))))
