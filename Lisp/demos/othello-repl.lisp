;;;; othello-repl.lisp — Othello + live REPL, two concurrent threads.
;;;;
;;;; Demonstrates the dispatcher-based event model introduced alongside
;;;; this file: `event-loop-for` in two different OS threads blocks on
;;;; two independent per-child queues — neither thread starves the other.
;;;;
;;;; Windows:
;;;;   Left  — the Othello board (mouse, keyboard, animated AI)
;;;;   Right — a live REPL you can type into while the game runs
;;;;
;;;; Controls (board window):
;;;;   click  place a black disc (legal moves only)
;;;;   n      new game
;;;;   b/i/a  beginner / intermediate / advanced AI
;;;;   Esc    quit
;;;;
;;;; REPL — type any Lisp expression and press Enter. While you play,
;;;; try:
;;;;   *current-player*               => current player constant
;;;;   (count-cells +black+ *cells*)  => black disc count
;;;;   (pick-advanced)                => switch AI mid-game
;;;;   (new-game)                     => restart from REPL
;;;;
;;;; Usage:
;;;;   ncl -l Lisp/demos/othello-repl.lisp --eval "(run-othello-repl)"

(require 'threads)

;; Load the core game logic (board, AI, rendering, all the defparameters).
;; We reuse everything from the single-thread demo except the top-level
;; entry point.
(load "Lisp/demos/othello-gui.lisp")

;; ── Board thread ─────────────────────────────────────────────────────────

(defun board-thread-fn (board-id)
  "Run the Othello board event loop. Blocking — this is the thread body."
  (new-game)
  (paint-board board-id)
  (set-redraw-rate board-id 50)       ; ~20fps tick for AI countdown
  (event-loop-for board-id
    (:frame-close (return :done))
    (:close       (return :done))
    (:resize      (setq *win-w* (max (getf ev :width)  64))
                  (setq *win-h* (max (getf ev :height) 64)))
    (:mouse       (when (and (eq (getf ev :op) :left-down)
                             (null *ai-pending*))
                    (when (try-human-move (getf ev :x) (getf ev :y))
                      (paint-board board-id))))
    (:tick        (when *ai-pending*
                    (setq *ai-pending* (- *ai-pending* 1))
                    (when (<= *ai-pending* 0)
                      (setq *ai-pending* nil)
                      (do-ai-move)))
                  (paint-board board-id))
    (:char        (let ((ch (getf ev :char)))
                    (cond
                      ((or (eq ch #\n) (eq ch #\N))
                       (new-game))
                      ((or (eq ch #\b) (eq ch #\B))
                       (pick-beginner)
                       (setq *status*
                             (format nil "AI now: ~A — your move (Black)" *ai-name*)))
                      ((or (eq ch #\i) (eq ch #\I))
                       (pick-intermediate)
                       (setq *status*
                             (format nil "AI now: ~A — your move (Black)" *ai-name*)))
                      ((or (eq ch #\a) (eq ch #\A))
                       (pick-advanced)
                       (setq *status*
                             (format nil "AI now: ~A — your move (Black)" *ai-name*)))
                      ((or (eq ch #\Escape) (eq ch #\Esc))
                       (return :done))))
                  (paint-board board-id))))

;; ── REPL thread ──────────────────────────────────────────────────────────

(defun repl-thread-fn (repl-id)
  "Run the graphical REPL event loop. Blocking — this is the thread body."
  (event-loop-for repl-id
    (:frame-close (return :done))
    (:close       (return :done))
    (:repl-submit
     (let ((src (repl-pop-input repl-id)))
       (when src
         (handler-case
             (let ((result (eval-string src)))
               (repl-output repl-id
                 (if result
                     (format nil "=> ~A" result)
                     "=> nil")))
           (error (c)
             (repl-error repl-id
               (format nil "Error: ~A" c)))))))))

;; ── Entry point ──────────────────────────────────────────────────────────

(defun run-othello-repl ()
  "Open an Othello board and a live REPL side by side.
   The board runs in a background thread; the main thread drives the REPL.
   Both event loops block on independent per-child queues — the dispatcher
   model ensures neither window's events get lost or delayed by the other."
  (pick-beginner)
  (igui-start)
  (let* ((board-id (open-child-sized "Othello" 520 560))
         (repl-id  (open-repl-window "Othello REPL")))
    (cond
      ((null board-id)
       (format t "** open-child-sized failed — is --windows enabled?~%")
       :failed)
      ((null repl-id)
       (format t "** open-repl-window failed~%")
       :failed)
      (t
       ;; Board gets its own OS thread.  The REPL runs on the main Lisp
       ;; thread — it returns when the user closes the REPL window or the
       ;; frame is closed.
       (create-thread
         (let ((bid board-id))
           (lambda () (board-thread-fn bid)))
         :report-when-finished nil)
       ;; Main thread drives the REPL.
       (repl-thread-fn repl-id)))))
