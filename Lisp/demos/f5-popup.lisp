;;;; f5-popup.lisp — F5 me. I open a window.
;;;;
;;;; The point of this demo is to verify the F5/Ctrl+R chain actually
;;;; works END-TO-END from the editor, including spawning a new
;;;; iGui window from inside a nested event-loop. Most demo files
;;;; only DEFINE their runner; you have to F5 the (run-foo) call
;;;; separately. This one self-invokes at the bottom — paste it
;;;; into ledit, press F5, and a small pane opens immediately.

(igui-start)

(let ((id (open-child-sized "F5 worked" 280 100)))
  (when id
    (with-batch id
      (clear (rgb 30 50 70))
      (draw-text 16 16 "F5 from ledit reached here." 16 (rgb 230 230 230))
      (draw-text 16 40 "Close this window to continue."
                 14 (rgb 180 200 220)))
    ;; Park briefly so the window is visible. We don't enter a
    ;; full event-loop here because that would block the outer
    ;; eval-buffer handler. The user closes the window from the X.
    (event-loop-for id
      (:frame-close (return :done))
      (:close       (return :done))
      (:tick        nil))))
