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

   pattern.

   NOTE: this BLOCKS the calling (language) thread inside its loop, so it
   monopolises the one Lisp thread for as long as it runs. That's fine for
   a standalone `ncl --windows -l app.lisp` launch, but inside the hosted
   IDE it would freeze the REPL and every other pane. For IDE-hosted apps
   prefer the cooperative ON-WINDOW below: register a handler and return,
   and the host's central loop drives every pane on the shared thread."
  `(with-events-from ,window-form
     (event-loop ,@clauses)))

;; ── Cooperative dispatch: many panes, one language thread ───────────────
;;
;; The blocking model above gives each pane its own (next-event …) loop —
;; which means each pane needs its own thread, or it starves the others.
;; With a single language thread (the GC mutator is thread-bound) that
;; doesn't scale: the first app's loop freezes the REPL and blocks every
;; other pane.
;;
;; The cooperative model inverts it. The HOST runs exactly one central
;; loop (see ncl-driver) that drains every event and calls %DISPATCH-EVENT
;; once per event. Each pane registers a handler with (on-window …) /
;; (on-event …) and returns immediately; its handler runs to completion on
;; the shared thread and yields. No pane ever blocks, so N panes — and the
;; live REPL — coexist on one thread, while the AppKit UI thread only ever
;; posts into the mailbox.

(defparameter *pane-handlers* (make-hash-table)
  "Window-id → handler function (of one arg, the event plist). Populated
   by (on-event …); consulted by %DISPATCH-EVENT to route per-pane events.")

(defparameter *global-handlers* nil
  "Functions called for every *global* event — one with no :child-id:
   frame-close, theme-change, menu, eval-buffer. See (on-global-event …).")

(defun on-event (win handler)
  "Register HANDLER as the cooperative event handler for window WIN,
   replacing any previous one. HANDLER is called as (funcall HANDLER ev)
   with the event plist. Returns WIN."
  (setf (gethash win *pane-handlers*) handler)
  win)

(defun off-event (win)
  "Remove WIN's cooperative handler; events for WIN are then dropped.
   Returns WIN."
  (remhash win *pane-handlers*)
  win)

(defun on-global-event (handler)
  "Add HANDLER to the list invoked for every global event (frame-close,
   theme-change, menu, eval-buffer). Returns HANDLER."
  (unless (member handler *global-handlers*)
    (setq *global-handlers* (cons handler *global-handlers*)))
  handler)

(defun stop-pane (win)
  "Cooperative teardown for a pane opened with (on-window …): unregister
   its handler and close its window. Returns WIN."
  (off-event win)
  (close-child win)
  win)

(defun %event-log (msg)
  "Best-effort log for the event system. Writes to the iGui log overlay
   when it's present (the GUI build installs LOG-WRITE); otherwise falls
   back to standard output, so the routing layer also works headless."
  (if (fboundp 'log-write)
      (log-write msg)
      (progn (princ msg) nil)))

(defun %safe-call-handler (handler ev)
  "Funcall HANDLER on EV, trapping any Lisp condition so one pane's bug
   can't take down the central loop or another pane."
  (handler-case (funcall handler ev)
    (error (c)
      (%event-log (format nil "[event] handler error: ~A~%" c)))))

(defun %dispatch-event (ev)
  "Host entry point — the central event loop calls this once per event.
   Routes EV to the handler registered for its :child-id; an event with
   no :child-id (a global) goes to every global handler. Returns nil.

   Called from Rust (igui_mac::shims::dispatch_event); keep the name and
   one-arg shape stable."
  (let* ((win (getf ev :child-id))
         (handler (and win (gethash win *pane-handlers*))))
    (cond
      (handler   (%safe-call-handler handler ev))
      ((null win) (dolist (g *global-handlers*)
                    (%safe-call-handler g ev))))
    nil))

(defmacro on-window (window-form &rest clauses)
  "Register a COOPERATIVE event handler for WINDOW-FORM and return its id.

   CLAUSES use the same (KIND body...) shape as EVENT-LOOP, but instead of
   looping, the expansion builds a one-shot handler the host's central
   loop calls once per event for this window. Inside each clause body the
   event plist is bound to `ev' and the window id to `self'.

   Cooperative contract: a clause body MUST return promptly — it must not
   call (next-event …) or run its own loop, which would re-monopolise the
   shared language thread this whole design exists to free up.

   Teardown: (stop-pane self) unregisters and closes; (off-event self)
   keeps the window but stops handling. A :CLOSE clause that calls
   stop-pane is auto-added when you don't supply one, so the window's
   close box just works.

   :FRAME-CLOSE is app-global (the whole IDE quitting) and is handled by
   the host — register it via (on-global-event …) if you need to observe
   it, not as an on-window clause.

   Example:

     (let ((id (open-child-sized \"Paint\" 480 360)))
       (on-window id
         (:mouse (when (eq (getf ev :op) :left-down)
                   (dab (getf ev :x) (getf ev :y) id)))
         (:char  (when (eq (getf ev :char) #\\Escape) (stop-pane self))))
       id)"
  (let ((win  (gensym "WIN-"))
        (evg  (gensym "EV-"))
        (kind (gensym "K-"))
        (has-close (some (lambda (c) (eq (car c) :close)) clauses)))
    `(let ((,win ,window-form))
       (on-event ,win
         (lambda (,evg)
           (let ((,kind (getf ,evg :kind))
                 (ev    ,evg)
                 (self  ,win))
             (cond
               ,@(unless has-close
                   (list `((eq ,kind :close) (stop-pane self))))
               ,@(mapcar
                   (lambda (clause)
                     (let ((head (car clause))
                           (body (cdr clause)))
                       (if (eq head t)
                           `(t ,@body)
                           `((eq ,kind ',head) ,@body))))
                   clauses)))))
       ,win)))

(provide 'events)
nil
