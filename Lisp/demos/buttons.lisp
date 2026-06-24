;;;; buttons.lisp — composable button widget using measure-text.
;;;;
;;;; Each button is drawn as a filled rounded-ish rectangle plus
;;;; the label text centered horizontally and vertically. We use
;;;; (measure-text ...) to get the text's actual width/height/
;;;; ascent so the label sits in the middle no matter what we put
;;;; in it.
;;;;
;;;; Mouse-hover and click handling against the button's bounding
;;;; box: simple point-in-rect math against the events as they
;;;; come in. Hovered buttons highlight; clicked buttons log.

(defparameter +btn-bg+         (rgb 50 60 75))
(defparameter +btn-bg-hover+   (rgb 70 90 120))
(defparameter +btn-bg-pressed+ (rgb 100 130 170))
(defparameter +btn-fg+         (rgb 240 240 240))

(defun point-in-rect (px py x y w h)
  (and (>= px x) (< px (+ x w))
       (>= py y) (< py (+ y h))))

(defun draw-button (child-id x y w h label hover pressed)
  "Render one button at (X, Y) of size (W × H) with LABEL centered.
   HOVER and PRESSED tweak the background color."
  (let ((bg (cond
              (pressed +btn-bg-pressed+)
              (hover +btn-bg-hover+)
              (t +btn-bg+))))
    (fill-rect x y w h bg)
    (stroke-rect x y w h 1 +btn-fg+)
    (let ((m (measure-text child-id label 14)))
      (when m
        (let ((tw (getf m :width))
              (th (getf m :height)))
          (draw-text (+ x (truncate (- w tw) 2))
                     (+ y (truncate (- h th) 2))
                     label 14 +btn-fg+))))))

;; Three buttons, laid out horizontally.
(defparameter *buttons*
  ;; (x y w h label)
  '((40  120 120 36 "Click me")
    (180 120 120 36 "Or me")
    (320 120 120 36 "Quit")))

(defun btn-rect (b) b)  ; helper alias for clarity
(defun btn-x (b) (car b))
(defun btn-y (b) (cadr b))
(defun btn-w (b) (car (cddr b)))
(defun btn-h (b) (cadr (cddr b)))
(defun btn-label (b) (car (cddr (cddr b))))

(defun button-at (mx my)
  "Return the button under (mx my), or nil."
  (let ((found nil))
    (mapc (lambda (b)
            (when (and (null found)
                       (point-in-rect mx my (btn-x b) (btn-y b) (btn-w b) (btn-h b)))
              (setq found b)))
          *buttons*)
    found))

(defun paint-buttons (id width height hovered pressed)
  (with-batch id
    (clear +slate+)
    (draw-text 12 8 "buttons (measure-text + draw-text)" 18 +white+)
    (mapc (lambda (b)
            (draw-button id (btn-x b) (btn-y b) (btn-w b) (btn-h b)
                         (btn-label b)
                         (and hovered (equal hovered b))
                         (and pressed (equal pressed b))))
          *buttons*)
    (draw-text 12 (- height 24)
               (format nil "~A × ~A" width height)
               12 +white+)))

(defun run-buttons ()
  (igui-start)
  (let ((id (open-child "buttons"))
        (hovered nil)
        (pressed nil)
        (last-w 480)
        (last-h 320))
    (paint-buttons id last-w last-h hovered pressed)
    (event-loop-for id
      (:frame-close (return :done))
      (:close       (close-child id) (return :done))
      (:resize      (setq last-w (getf ev :width))
                    (setq last-h (getf ev :height))
                    (paint-buttons id last-w last-h hovered pressed))
      (:mouse
       (let ((b (button-at (getf ev :x) (getf ev :y)))
             (op (getf ev :op)))
         (cond
           ((eq op :left-down)
            (setq pressed b)
            (paint-buttons id last-w last-h hovered pressed))
           ((eq op :left-up)
            (when (and pressed (equal pressed b))
              (format t "clicked: ~A~%" (btn-label b))
              (when (equal (btn-label b) "Quit")
                (close-child id)
                (return :done)))
            (setq pressed nil)
            (paint-buttons id last-w last-h hovered pressed))
           ((eq op :move)
            (unless (equal hovered b)
              (setq hovered b)
              (paint-buttons id last-w last-h hovered pressed)))))))))
(run-buttons)