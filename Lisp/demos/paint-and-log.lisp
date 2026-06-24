;;;; Lisp/demos/paint-and-log.lisp — canvas-owned events + log sink.
;;;;
;;;; Two windows, one event loop on the canvas.
;;;;
;;;;   * a graphics canvas where left-clicks drop coloured dots
;;;;   * a passive text-pane log the canvas writes click numbers
;;;;     and coordinates into via `text-write`
;;;;
;;;; Design — "the app listens for its own events":
;;;;
;;;; The canvas is the app. It registers as the filter window via
;;;; `event-loop-for`; events for any other child (including the log
;;;; child) stash and are ignored. The log is treated like stdout —
;;;; the canvas writes to it, but the log doesn't drive anything.
;;;;
;;;; If the user closes the log child on its own, subsequent
;;;; (text-write log …) calls return NIL silently and the canvas
;;;; keeps running. To exit, close the canvas (or the whole frame).
;;;;
;;;; Run:
;;;;
;;;;   ncl --eval "(igui-start)" --load Lisp/demos/paint-and-log.lisp \
;;;;       --eval "(run-paint-and-log)"

(defparameter +canvas-bg+ (rgb 20 25 41))      ; very dark blue
(defparameter +log-bg+    (rgb 32 40 64))
(defparameter +log-fg+    (rgb 102 224 224))   ; soft cyan
(defparameter +log-banner-bg+ (rgb 32 40 64))

;; Six dot colours cycled through on each click.
(defparameter +dot-colors+
  (list (rgb 242 90 60)     ; warm orange-red
        (rgb 250 180 60)    ; amber
        (rgb 240 235 80)    ; yellow
        (rgb 120 220 100)   ; green
        (rgb 80 160 240)    ; sky blue
        (rgb 200 120 240))) ; lavender

(defun %dot-color (n)
  (nth (rem n 6) +dot-colors+))

(defun paint-and-log-banner (log)
  (text-set-pen log +log-fg+ +log-banner-bg+)
  (text-write log "+--------------------------+~%")
  (text-write log "|   paint-and-log demo     |~%")
  (text-write log "+--------------------------+~%")
  (text-reset-pen log)
  (text-write log "Click the canvas to drop dots.~%")
  (text-write log "Close the canvas to exit.~%~%"))

(defun run-paint-and-log ()
  "Open the canvas and log windows. The canvas is the app; the
   log is a passive sink it writes click messages into. Returns
   :done when the canvas (or the whole frame) closes."
  (igui-start)
  (let ((canvas (open-child "Canvas"))
        (log    (open-text-window "Log")))
    (cond
      ((or (null canvas) (null log))
       (format t "** failed to open windows (canvas=~A log=~A)~%" canvas log)
       :failed)
      (t
       ;; Initial canvas paint.
       (with-batch canvas
         (clear +canvas-bg+))
       ;; Banner in the log.
       (paint-and-log-banner log)
       (let ((count 0))
         (event-loop-for canvas
           (:frame-close (return :done))
           (:close       (close-child canvas) (return :done))
           (:mouse
            (when (eq (getf ev :op) :LEFT-DOWN)
              (setq count (+ count 1))
              ;; Draw a new dot on the canvas.
              (with-batch canvas
                (fill-circle (getf ev :x) (getf ev :y) 7
                             (%dot-color count)))
              ;; Log the click into the text pane.
              (text-write log
                          (format nil "  click ~D at (~D, ~D)~%"
                                  count
                                  (getf ev :x)
                                  (getf ev :y))))))
         (format t "[paint-and-log] exited after ~D clicks~%" count))))))
