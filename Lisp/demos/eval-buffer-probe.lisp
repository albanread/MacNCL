;;;; eval-buffer-probe.lisp — does the F5 / Ctrl+R chain reach the
;;;; language thread?
;;;;
;;;; Opens a small pane, parks in event-loop-for. Every event seen
;;;; gets a line in the iGui log. Press F5 in the editor pane next
;;;; door; if the chain is wired, you'll see
;;;;
;;;;     [probe] :eval-buffer (NNN chars)
;;;;
;;;; in the log. If you don't, the language thread isn't draining
;;;; EvalBuffer events from the mailbox.
;;;;
;;;;   ncl --windows -l Lisp/demos/eval-buffer-probe.lisp \
;;;;       --eval "(run-probe)"

(defparameter *probe-id* nil)
(defparameter *eval-count* 0)

(defun run-probe ()
  (igui-start)
  (setq *eval-count* 0)
  (setq *probe-id* (open-child-sized "eval-buffer probe" 360 140))
  (cond
    ((null *probe-id*) :failed)
    (t
     (log-write "[probe] running — open ledit (Tools menu) and press F5 there")
     (set-redraw-rate *probe-id* 200)
     (with-batch *probe-id*
       (clear (rgb 30 40 60))
       (draw-text 12 12 "eval-buffer probe" 16 (rgb 230 230 230))
       (draw-text 12 36 "open ledit, press F5/Ctrl+R" 14 (rgb 180 180 180))
       (draw-text 12 56 "watch Ctrl+Shift+L log" 14 (rgb 180 180 180))
       (draw-text 12 84 "every event logs its :kind"
                  12 (rgb 140 140 140)))
     ;; Listen for :eval-buffer EXPLICITLY (override the auto-
     ;; installed handler) plus log every other kind we see, so a
     ;; failure shows up either as missing events or as wrong-kind
     ;; events.
     (event-loop-for *probe-id*
       (:frame-close (return :done))
       (:close       (return :done))
       (:eval-buffer (setq *eval-count* (+ *eval-count* 1))
                     (let ((src (getf ev :source)))
                       (log-write
                         (format nil "[probe] EVAL-BUFFER #~A — ~A chars; head: ~A"
                                 *eval-count*
                                 (if (stringp src) (length src) "no-source")
                                 (if (stringp src)
                                     (subseq src 0 (min 60 (length src)))
                                     "n/a")))
                       (when (stringp src)
                         (let ((result
                                (handler-case (eval-string src)
                                  (error (c)
                                    (format nil "ERROR: ~A" c)))))
                           (log-write
                             (format nil "[probe] result: ~A"
                                     (cond
                                       ((null result) "nil")
                                       ((stringp result)
                                        ;; Print one line; flag if multi-line.
                                        (if (position #\Newline result)
                                            (format nil "(~A chars, multi-line) ~A..."
                                                    (length result)
                                                    (subseq result 0
                                                            (min 60 (length result))))
                                            result))
                                       (t (format nil "~A" result))))))) ))
       (:tick        nil)         ; ignore the chatty ones
       (:mouse       nil)
       (:focus       nil)
       (:resize      nil)
       (t            (log-write
                       (format nil "[probe] other event: ~A"
                               (getf ev :kind))))))))
