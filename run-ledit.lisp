(igui-start)
(format t "iGui frame up. Open Tools ? ledit (or Ctrl+Shift+E).~%")
(format t "Type or paste Lisp; F5 / Ctrl+R / Edit menu ? Run Buffer.~%")
(format t "Results land in Tools ? Log (Ctrl+Shift+L).~%")
(event-loop
  (:frame-close (return :done))
  (:close nil))        ; child-window close must not kill the session
