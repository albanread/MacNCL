;;;; canvas-demo.lisp — animated direct pixel access via the NCL canvas API.
;;;;
;;;; A Demos-menu entry: select it and the IDE evaluates (run-canvas-demo).
;;;;
;;;; Where mandelbrot.lisp pokes one static frame, this demo redraws every
;;;; tick to show that the host-owned framebuffer is fast enough to animate:
;;;; a scrolling XOR "plasma" written pixel-by-pixel, then a single GPU blit
;;;; per frame. All pixel math is integer fixnum work — no float, no per-
;;;; pixel boundary crossing.
;;;;
;;;;   (canvas-open child-id w h)   -> base address of a BGRA32 buffer
;;;;   (buffer-set-u32 base off v)  -> poke one pixel: off = (y*w + x)*4
;;;;   (canvas-present child-id)     -> draw the frame (one GPU blit)
;;;;   (set-redraw-rate id ms)       -> request a :tick every ms

(defparameter *cv-w* 256)
(defparameter *cv-h* 256)
(defparameter *cv-base* nil)   ; framebuffer base address (fixnum), or nil
(defparameter *cv-tick* 0)     ; animation phase

(defun draw-plasma (base w h phase)
  "Scrolling XOR plasma. The classic (x xor y) texture, with the red and
   green channels scrolled by PHASE so the whole field drifts each frame."
  (dotimes (py h)
    (let ((row (* py w)))
      (dotimes (px w)
        (let ((r (logand (+ px phase) 255))
              (g (logand (+ py phase) 255))
              (b (logand (logxor px py) 255)))
          (buffer-set-u32 base (* (+ row px) 4)
                          (logior #xFF000000 (ash r 16) (ash g 8) b)))))))

(defun run-canvas-demo ()
  (igui-start)
  (let ((id (open-child-sized "Canvas — animated pixel poke" *cv-w* *cv-h*)))
    (cond
      ((null id)
       (format t "** open-child failed~%")
       :failed)
      (t
       (setq *cv-base* (canvas-open id *cv-w* *cv-h*))
       (setq *cv-tick* 0)
       (cond
         ((null *cv-base*)
          (format t "** canvas-open failed~%")
          :failed)
         (t
          ;; First frame, then ask for ~30 fps of ticks. Win32 coalesces
          ;; pending WM_TIMERs, so a backed-up language thread still sees
          ;; at most one :tick per drain — the animation degrades to a
          ;; lower frame rate instead of spiralling.
          (draw-plasma *cv-base* *cv-w* *cv-h* 0)
          (canvas-present id)
          (set-redraw-rate id 33)
          (event-loop-for id
            (:frame-close (return :done))
            (:close       (return :done))
            (:tick        (setq *cv-tick* (+ *cv-tick* 2))
                          (draw-plasma *cv-base* *cv-w* *cv-h* *cv-tick*)
                          (canvas-present id)))))))))
