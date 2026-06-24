;;;; mandelbrot.lisp — direct pixel access via the NCL canvas API.
;;;;
;;;; A Demos-menu entry: select it and the IDE evaluates (run-mandelbrot).
;;;;
;;;; The host owns a BGRA32 framebuffer per child window. Lisp pokes each
;;;; pixel STRAIGHT into that buffer — no per-pixel boundary crossing — then
;;;; presents the whole frame in a single GPU blit. The Mandelbrot set is
;;;; the canonical showcase: every pixel is an independent escape-time
;;;; computation.
;;;;
;;;;   (canvas-open child-id w h)   -> base address of a BGRA32 buffer
;;;;   (buffer-set-u32 base off v)  -> poke one pixel: off = (y*w + x)*4
;;;;   (canvas-present child-id)     -> draw the frame (one GPU blit)

(defun mb-argb (r g b)
  "Opaque BGRA32 pixel 0xAARRGGBB (alpha ignored by the canvas)."
  (logior #xFF000000
          (ash (logand r 255) 16)
          (ash (logand g 255) 8)
          (logand b 255)))

(defun mb-iter (cx cy max)
  "Escape-time iteration count for the point c = cx + i*cy.
   Returns MAX for points that never escape (in the set).

   The (declare (double-float …))s are what make this fast: with them,
   the compiler keeps zx/zy/zx2/… as unboxed f64 in registers, and the
   plain (loop …) is auto-inlined (its body calls no user functions), so
   the whole iteration runs as native float code with no per-op boxing
   and no per-call closure — no special loop syntax required. Drop the
   declarations and it still computes the same image, just ~25x slower
   (every loop-carried float round-trips through a heap box)."
  (declare (double-float cx cy))
  (let ((zx 0.0) (zy 0.0) (n 0))
    (declare (double-float zx zy))
    (loop
      (let ((zx2 (* zx zx))
            (zy2 (* zy zy)))
        (declare (double-float zx2 zy2))
        (when (or (>= n max) (> (+ zx2 zy2) 4.0))
          (return n))
        (let ((new-zx (+ (- zx2 zy2) cx))
              (new-zy (+ (* 2.0 (* zx zy)) cy)))
          (declare (double-float new-zx new-zy))
          (setq zx new-zx)
          (setq zy new-zy)))
      (setq n (+ n 1)))))

(defun mb-color (n max)
  "Map an escape count to a colour: black inside the set, a cyclic
   banded palette outside.

   We mask with (logand x 255) rather than (mod x 256): the inputs are
   non-negative and 256 is a power of two, so the two are identical here
   — but logand/logior/ash lower to inline machine ops, whereas mod is a
   polymorphic Lisp function (it handles floats/ratios) and stays a real
   call. With the inline ops the whole per-pixel colour costs ~nothing;
   the three mods were ~15x the cost of the fractal iteration itself."
  (if (>= n max)
      #xFF000000
      (mb-argb (logand (* n 5) 255)
               (logand (* n 7) 255)
               (logand (+ 64 (* n 11)) 255))))

(defun render-mandelbrot (base w h max)
  "Compute the whole set and poke each pixel directly into the buffer."
  (let* ((x-min -2.5) (x-max 1.0)
         (y-min -1.25) (y-max 1.25)
         (x-step (/ (- x-max x-min) w))
         (y-step (/ (- y-max y-min) h)))
    (dotimes (py h)
      (let ((cy  (+ y-min (* py y-step)))
            (row (* py w)))
        (dotimes (px w)
          (let* ((cx (+ x-min (* px x-step)))
                 (n  (mb-iter cx cy max)))
            (buffer-set-u32 base (* (+ row px) 4) (mb-color n max))))))))

(defun run-mandelbrot ()
  (igui-start)
  (let* ((w 480) (h 360) (max 100)
         (id (open-child-sized "Mandelbrot — direct pixel access" w h)))
    (cond
      ((null id)
       (format t "** open-child failed~%")
       :failed)
      (t
       (let ((base (canvas-open id w h)))
         (cond
           ((null base)
            (format t "** canvas-open failed~%")
            :failed)
           (t
            ;; One full render, one blit. The host retains the blit batch
            ;; and replays it on every WM_PAINT, so the image survives
            ;; resizes/repaints with no work from us.
            (format t "rendering ~Ax~A Mandelbrot (max-iter ~A) ...~%" w h max)
            (render-mandelbrot base w h max)
            (canvas-present id)
            (format t "done — close the window to exit.~%")
            (event-loop-for id
              (:frame-close (return :done))
              (:close       (return :done))))))))))
