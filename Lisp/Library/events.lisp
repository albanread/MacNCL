;;;; Lisp/Library/events.lisp
;;;;
;;;; Idiomatic iGui event-loop macros. Builds on the runtime
;;;; primitives shipped by the new channels filter machinery:
;;;;
;;;;   (next-event timeout-ms)
;;;;   (next-event-for window-id timeout-ms)
;;;;   (filter-on-window window-id)
;;;;   (unfilter-window window-id)
;;;;   (clear-event-filter)
;;;;   (discard-stashed-events)
;;;;
;;;; Without these macros, every demo writes the same boilerplate:
;;;;
;;;;   (loop
;;;;     (let ((ev (next-event -1)))
;;;;       (cond
;;;;         ((eq (getf ev :kind) :frame-close) (return :done))
;;;;         ((and (eq (getf ev :kind) :resize)
;;;;               (= (getf ev :child-id) win))
;;;;          (handle-resize ...))
;;;;         ((and (eq (getf ev :kind) :mouse)
;;;;               (= (getf ev :child-id) win))
;;;;          (handle-mouse ...)))))
;;;;
;;;; The (= (getf ev :child-id) win) repeats on every clause. With
;;;; with-events-from + event-loop:
;;;;
;;;;   (with-events-from win
;;;;     (event-loop
;;;;       (:frame-close (return :done))
;;;;       (:resize      (handle-resize ev))
;;;;       (:mouse       (handle-mouse ev))))
;;;;
;;;; The filter is set once; the dispatch is a single keyword case;
;;;; the event itself is bound to `ev` for handlers that need it.

;; ── with-events-from: scoped persistent filter ──────────────────────────

(defmacro with-events-from (window-form &rest body)
  "Set up the event-filter so subsequent (next-event ...) calls
   inside BODY only see events for WINDOW-FORM (plus globals
   like :FRAME-CLOSE). The filter is cleared on exit, in both
   the normal-return and the unwound-by-condition cases.

   Our `loop` doesn't unwind so this can't use unwind-protect;
   instead, the cleanup runs as the body's last form. A condition
   inside the body that escapes the form will leak the filter
   entry, but that's a transient state that (clear-event-filter)
   resolves."
  (let ((win (gensym "WIN-"))
        (result (gensym "RES-")))
    `(let ((,win ,window-form))
       (filter-on-window ,win)
       (let ((,result (progn ,@body)))
         (unfilter-window ,win)
         ,result))))

;; ── event-loop: dispatch by :kind ───────────────────────────────────────

(defmacro event-loop (&rest clauses)
  "Block on (next-event -1) and dispatch by :kind. Each clause is

       (KEYWORD ...body...)

   where KEYWORD is one of :KEY :CHAR :MOUSE :FOCUS :RESIZE :TICK
   :CLOSE :FRAME-CLOSE :MENU :DPI-CHANGE — i.e. the event-kind
   keywords returned in the event plist's :KIND slot.

   The current event is bound to `ev' for the duration of each
   clause body, so handlers can extract whatever fields they
   need via (getf ev :width) etc.

   A single special clause head `t` is the wildcard: it fires on
   any kind not otherwise listed.

   The loop runs forever; clause bodies typically (return …) to
   exit. As with all our loops, (return) doesn't unwind — put it
   at the end of the clause body.

   Example:

     (event-loop
       (:frame-close (return :done))
       (:close       (return :done))
       (:resize      (resize-pane (getf ev :width) (getf ev :height)))
       (:mouse       (when (eq (getf ev :op) :left-down)
                       (handle-click (getf ev :x) (getf ev :y))))
       (:tick        (advance) (repaint))
       (t            nil))

   :eval-buffer events from the ledit pane (Ctrl+R) are
   auto-handled here — the source is evaluated via the active
   session and a single-line printed result lands in the iGui
   log overlay. User clauses can override by listing
   `(:eval-buffer ...)` explicitly."
  (let* ((ev (gensym "EV-"))
         (kind (gensym "K-"))
         (has-eval-buffer
          (some (lambda (c) (eq (car c) :eval-buffer)) clauses))
         (default-eval-clause
          (unless has-eval-buffer
            (list `((eq ,kind :eval-buffer)
                    (%handle-eval-buffer (getf ev :source)))))))
    `(loop
       (let ((,ev (next-event -1)))
         (when ,ev
           (let ((,kind (getf ,ev :kind)))
             (let ((ev ,ev))
               (cond
                 ;; Default :eval-buffer handler (Ctrl+R in ledit).
                 ;; Suppressed if the user listed their own
                 ;; (:eval-buffer ...) clause below.
                 ,@default-eval-clause
                 ,@(mapcar
                     (lambda (clause)
                       (let ((head (car clause))
                             (body (cdr clause)))
                         (cond
                           ((eq head 't) `(t ,@body))
                           (t `((eq ,kind ',head) ,@body)))))
                     clauses)))))))))

(defun %handle-eval-buffer (source)
  "Evaluate SOURCE via the active session. If the printed result
   fits on a single line, write it to the iGui log overlay; if
   it's multi-line or an error, write a short marker (the user
   can re-run from the editor with their own clause to inspect)."
  (let ((result
         (handler-case (eval-string source)
           (error (c) (format nil "error: ~A" c)))))
    (cond
      ((null result) nil)
      ((position #\Newline result)
       ;; Multi-line: just hint that it ran.
       (log-write (format nil "[eval] ~A lines~%"
                          (1+ (count #\Newline result)))))
      (t
       (log-write (format nil "[eval] ~A~%" result))))))

;; ── event-loop-for: combined filter + dispatch ─────────────────────────

(defmacro event-loop-for (window-form &rest clauses)
  "(event-loop-for WIN clauses...) — shorthand for the very common

       (with-events-from WIN
         (event-loop clauses...))

   pattern."
  `(with-events-from ,window-form
     (event-loop ,@clauses)))

(provide 'events)
nil
