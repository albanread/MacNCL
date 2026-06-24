;;;; Lisp/Library/hot-reload.lisp
;;;;
;;;; Filesystem-watch driven hot reload. Drop a .lisp file in
;;;; the watched directory (typically `Library/`), save it from
;;;; your editor, and the REPL picks up the change between forms.
;;;;
;;;; The CL side of this is thin: the Rust `notify` crate spawns
;;;; its own watcher thread that pushes paths into a queue. We
;;;; drain the queue and `(load path)` each one at safe points.
;;;;
;;;; Function-cell semantics make this almost free: defun rewrites
;;;; the symbol's function cell atomically (see project_redefinition
;;;; _model in MEMORY.md), so callers using the old definition
;;;; finish on the old, new calls land on the new. No retirement
;;;; bookkeeping. defmacro is the same. defclass also rebuilds
;;;; the class metaobject in-place, though existing instances of
;;;; the OLD class don't auto-migrate — Stage C-style class
;;;; redefinition propagation is a future enhancement.
;;;;
;;;; Three user-facing entry points:
;;;;
;;;;   (start-hot-reload [dir])
;;;;     Start watching DIR (defaults to the first entry of
;;;;     *load-path*, which the driver sets to Library/).
;;;;
;;;;   (check-reloads)
;;;;     Drain pending changes and load each one. The driver
;;;;     calls this between REPL prompts automatically once
;;;;     start-hot-reload has been called. Manual users (scripts,
;;;;     long-running computations) can call it whenever.
;;;;
;;;;   (stop-hot-reload)
;;;;     Currently a no-op placeholder — the underlying watcher
;;;;     runs for the process lifetime. Stub kept for the API
;;;;     symmetry.

(defparameter *hot-reload-enabled* nil
  "T iff a watcher thread is active. Set by start-hot-reload.")

(defparameter *hot-reload-trace* t
  "When T, print `;;; hot-reload: …` to *standard-output* for each
   reloaded file. Set to NIL to silence.")

(defun start-hot-reload (&optional dir)
  "Begin watching DIR for .lisp file changes. DIR defaults to the
   first entry of *load-path*. Returns T on success."
  (let ((target (cond
                  (dir dir)
                  ((and *load-path* (car *load-path*)) (car *load-path*))
                  (t (error "start-hot-reload: no directory and *load-path* is empty")))))
    (%watcher-start target)
    (setq *hot-reload-enabled* t)
    (when *hot-reload-trace*
      (format t ";;; hot-reload: watching ~A~%" target))
    t))

(defun stop-hot-reload ()
  "Disable the auto-reload poll. The underlying OS watcher
   continues running (it's bound to the process); we just stop
   reading from its queue."
  (setq *hot-reload-enabled* nil)
  nil)

(defun check-reloads ()
  "Drain the watcher's pending queue and (load) each file. Safe
   to call at any time; no-op if no files have changed. The
   driver calls this between REPL prompts automatically.

   Each file is parse-validated BEFORE we load it. If the file
   doesn't parse — typo, unbalanced paren, half-saved buffer —
   we report the problem and SKIP the reload. The previous
   definitions stay in place; nothing in the running session
   is touched. The next save will retry."
  (cond
    ((null *hot-reload-enabled*) nil)
    (t
     (let ((paths (%watcher-pending)))
       (dolist (path paths)
         (let ((parsed (%file-parses-p path)))
           (cond
             ((eq parsed t)
              (when *hot-reload-trace*
                (format t ";;; hot-reload: ~A~%" path))
              ;; Even with a clean parse, evaluation can still
              ;; fail (undefined function reference, wrong arity,
              ;; etc.). Catch those too so the REPL doesn't die.
              (handler-case (load path)
                (error (c)
                  (format t ";;; hot-reload: load failed for ~A: ~A~%"
                          path c))))
             ((null parsed)
              (format t ";;; hot-reload: cannot read ~A — skipped~%" path))
             (t
              (format t ";;; hot-reload: SKIP ~A — parse error: ~A~%"
                      path parsed)))))))))

(provide 'hot-reload)
nil
