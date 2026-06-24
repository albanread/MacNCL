;;;; Lisp/Library/win32-threading.lisp
;;;;
;;;; Marshal Lisp code from worker threads to the Win32 UI thread.
;;;; Loaded only when (windows-enabled-p) is true — see init.lisp.
;;;; Spec: docs/WINDOWS_FFI.md, Phase 2.
;;;;
;;;; Why this exists
;;;; ───────────────
;;;; With `--windows`, NCL launches on thread 0 which becomes the
;;;; Win32 UI thread (it runs GetMessage/DispatchMessage). Lisp eval
;;;; happens on a worker thread named "ncl-lisp-worker". USER32 /
;;;; GDI32 / COMCTL32 require their calls to come from the thread
;;;; that owns the HWND/DC — for windows we create via the FFI,
;;;; that's thread 0. So worker-thread code that wants to call those
;;;; APIs has to marshal the call across.
;;;;
;;;; The Rust side provides (%ui-execute closure) which does the
;;;; SendMessage round-trip. This file wraps it in idiomatic macros.
;;;;
;;;; Usage
;;;; ─────
;;;;   (on-ui-thread BODY...)      ; synchronous; returns BODY's value
;;;;   (post-to-ui-thread BODY...) ; fire-and-forget (PostMessage)
;;;;
;;;; (on-ui-thread …) is the workhorse: it checks (ui-thread-p) and
;;;; short-circuits when already on the UI thread, so nested calls
;;;; don't pay a SendMessage round-trip per level.
;;;;
;;;; Deadlock rule
;;;; ─────────────
;;;; Don't make a blocking SendMessage from inside an on-ui-thread
;;;; body to a thread that is itself waiting on the UI thread. The
;;;; resulting circular wait is the standard Win32 SendMessage
;;;; deadlock. SendMessage to your own thread short-circuits, so a
;;;; nested (on-ui-thread (on-ui-thread …)) is safe by construction.

(provide 'win32-threading)

;;; ── Synchronous dispatch ───────────────────────────────────────────

(defmacro on-ui-thread (&rest body)
  "Run BODY on the Win32 UI thread, returning its primary value.

   If the caller is already on the UI thread, BODY runs in place
   (no SendMessage round-trip). Otherwise BODY is packaged as a
   0-arg closure and dispatched to thread 0 via SendMessage; the
   call blocks until the UI thread returns. If BODY signals or
   panics on the UI thread, (on-ui-thread …) re-raises on the
   caller.

   Errors with `Windows surface not enabled` when called without
   `--windows`. Use `(when (windows-enabled-p) (on-ui-thread …))`
   if your code needs to work either way."
  `(if (ui-thread-p)
       (progn ,@body)
       (%ui-execute (lambda () ,@body))))

;;; ── Fire-and-forget dispatch (Phase 2 stub) ────────────────────────
;;;
;;; post-to-ui-thread will use PostMessage with a heap-allocated
;;; closure carrier so the caller doesn't block. The handler frees
;;; the carrier after running. Not yet implemented — punt to the
;;; synchronous version with a TODO comment so user code that uses
;;; the name today still runs correctly.

(defmacro post-to-ui-thread (&rest body)
  "Run BODY on the Win32 UI thread without waiting for it to
   return. Phase 2 implementation: temporarily aliased to
   `on-ui-thread` (synchronous). Phase 3 will switch to a real
   PostMessage-based fire-and-forget mechanism."
  `(on-ui-thread ,@body))

nil
