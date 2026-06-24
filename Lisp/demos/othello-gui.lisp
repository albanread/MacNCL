;;;; othello-gui.lisp — iGui port of cormanlisp/examples/gui/othello-gui.lisp.
;;;;
;;;; Roger's Othello-with-an-AI. The game logic and the evaluation
;;;; functions are Norvig's "Paradigms of AI Programming" / PAIP
;;;; chapter 18, lifted verbatim from the corman source (move
;;;; legality, capture geometry, minimax with alpha-beta,
;;;; weighted-squares heuristic with the `*weights*` table Roger
;;;; tuned via GA). The GUI substrate moves from Win32 GDI +
;;;; <main-menu-mixin> to iGui's drawing batch + event mailbox.
;;;;
;;;; You play Black; the computer plays White. The board uses
;;;; Norvig's 100-element-array layout with a border of
;;;; out-of-bounds squares — interior squares are 11..88 with the
;;;; ones digit in 1..8.
;;;;
;;;; Controls:
;;;;
;;;;   click  place a black disc (only legal moves accepted)
;;;;   n      start a new game
;;;;   b      switch AI to BEGINNER (random)
;;;;   i      switch AI to INTERMEDIATE (minimax ply 3, weighted)
;;;;   a      switch AI to ADVANCED (minimax ply 4, smart-weighted)
;;;;   esc    quit
;;;;
;;;; Usage:
;;;;
;;;;   ncl --windows -l Lisp/demos/othello-gui.lisp --eval "(run-othello-gui)"

;; ── Board encoding (Norvig PAIP §18) ──────────────────────────────

(defparameter +empty+         0)
(defparameter +black+         1)
(defparameter +white+         2)
(defparameter +out-of-bounds+ 3)

(defparameter +all-directions+ '(-11 -10 -9 -1 1 9 10 11))

;; The interior 8×8 board lives in cells 11..88, ones digit 1..8.
;; Border cells (0..10, 90..99, every cell with ones digit 0 or 9)
;; carry +out-of-bounds+ so the directional walks self-terminate.
(defparameter +all-squares+
  (let ((acc nil))
    (do ((i 11 (+ i 1)))
        ((> i 88) (nreverse acc))
      (let ((ones (mod i 10)))
        (when (and (>= ones 1) (<= ones 8))
          (setq acc (cons i acc)))))))

(defun opponent (player)
  (if (eq player +black+) +white+ +black+))

(defun cells-ref (cells sq)
  (svref cells sq))

(defun set-cell (cells sq val)
  (setf (svref cells sq) val))

(defun copy-cells (cells)
  (let ((n (length cells))
        (out (make-array 100 :initial-element +out-of-bounds+)))
    (dotimes (i n)
      (setf (svref out i) (svref cells i)))
    out))

(defparameter +initial-white-moves+ '(44 55))
(defparameter +initial-black-moves+ '(45 54))

(defun initialize-cells ()
  (let ((cells (make-array 100 :initial-element +out-of-bounds+)))
    (dolist (sq +all-squares+)
      (set-cell cells sq +empty+))
    (set-cell cells (first  +initial-white-moves+) +white+)
    (set-cell cells (second +initial-white-moves+) +white+)
    (set-cell cells (first  +initial-black-moves+) +black+)
    (set-cell cells (second +initial-black-moves+) +black+)
    cells))

;; ── Move legality / capture (verbatim from PAIP §18) ─────────────

(defun find-bracket-square (sq player cells dir)
  "Walk from SQ outward by DIR until a PLAYER piece (return SQ)
   or an empty / out-of-bounds cell (return NIL)."
  (cond
    ((eq (cells-ref cells sq) player) sq)
    ((eq (cells-ref cells sq) (opponent player))
     (find-bracket-square (+ sq dir) player cells dir))
    (t nil)))

(defun would-flip? (move player cells dir)
  "If MOVE-by-PLAYER captures along DIR, return the bracketing PLAYER
   square; else NIL."
  (let ((c (+ move dir)))
    (and (eq (cells-ref cells c) (opponent player))
         (find-bracket-square (+ c dir) player cells dir))))

(defun make-flips (move player cells dir)
  "Flip captured pieces along DIR. Side-effects CELLS."
  (let ((bracket (would-flip? move player cells dir)))
    (when bracket
      (let ((sq (+ move dir)))
        (loop
          (when (eq sq bracket) (return))
          (set-cell cells sq player)
          (setq sq (+ sq dir)))))))

(defun valid-move-p (move)
  (and (integerp move)
       (>= move 11) (<= move 88)
       (let ((ones (mod move 10)))
         (and (>= ones 1) (<= ones 8)))))

(defun legal-move-p (move player cells)
  (and (valid-move-p move)
       (eq (cells-ref cells move) +empty+)
       (some (lambda (dir) (would-flip? move player cells dir))
             +all-directions+)))

(defun make-move (move player cells)
  "Place PLAYER's disc at MOVE and flip captures in all directions.
   Side-effects CELLS; returns CELLS for chaining."
  (set-cell cells move player)
  (dolist (dir +all-directions+)
    (make-flips move player cells dir))
  cells)

(defun legal-moves (player cells)
  (let ((out nil))
    (dolist (sq +all-squares+ (nreverse out))
      (when (legal-move-p sq player cells)
        (setq out (cons sq out))))))

(defun any-legal-move? (player cells)
  (some (lambda (sq) (legal-move-p sq player cells))
        +all-squares+))

(defun next-to-play (cells previous-player)
  "Standard Othello turn order: opponent moves if able, else the
   previous player moves again if able, else nobody — game over."
  (let ((opp (opponent previous-player)))
    (cond
      ((any-legal-move? opp cells)              opp)
      ((any-legal-move? previous-player cells)  previous-player)
      (t                                        nil))))

(defun count-cells (val cells)
  (let ((n 0))
    (dolist (sq +all-squares+ n)
      (when (eq (cells-ref cells sq) val)
        (setq n (+ n 1))))))

(defun count-difference (player cells)
  (- (count-cells player cells)
     (count-cells (opponent player) cells)))

;; ── Norvig's weight table (PAIP §18.6, verbatim) ─────────────────

(defparameter *weights*
  #(0   0   0  0  0  0  0   0   0  0
    0 120 -20 20  5  5 20 -20 120  0
    0 -20 -40 -5 -5 -5 -5 -40 -20  0
    0  20  -5 15  3  3 15  -5  20  0
    0   5  -5  3  3  3  3  -5   5  0
    0   5  -5  3  3  3  3  -5   5  0
    0  20  -5 15  3  3 15  -5  20  0
    0 -20 -40 -5 -5 -5 -5 -40 -20  0
    0 120 -20 20  5  5 20 -20 120  0
    0   0   0  0  0  0  0   0   0  0))

(defun weighted-squares (player cells)
  "Sum of *weights* over PLAYER's squares minus OPP's squares."
  (let ((opp (opponent player)) (acc 0))
    (dolist (i +all-squares+ acc)
      (let ((piece (cells-ref cells i)))
        (cond
          ((eq piece player) (setq acc (+ acc (aref *weights* i))))
          ((eq piece opp)    (setq acc (- acc (aref *weights* i)))))))))

(defparameter *neighbor-table*
  (let ((tbl (make-array 100 :initial-element nil)))
    (dolist (sq +all-squares+)
      (let ((ns nil))
        (dolist (d +all-directions+)
          (when (valid-move-p (+ sq d))
            (setq ns (cons (+ sq d) ns))))
        (setf (svref tbl sq) ns)))
    tbl))

(defun neighbors (sq)
  (svref *neighbor-table* sq))

(defun smart-weighted-squares (player cells)
  "Weighted-squares with a corner-stability bonus: once a corner is
   occupied, its neighbours' weights flip sign for whichever side
   sits next to it. Caps the loss for the opponent of a corner-holder."
  (let ((w (weighted-squares player cells)))
    (dolist (corner '(11 18 81 88))
      (when (not (eq (cells-ref cells corner) +empty+))
        (dolist (nb (neighbors corner))
          (when (not (eq (cells-ref cells nb) +empty+))
            (setq w (+ w (* (- 5 (aref *weights* nb))
                            (if (eq (cells-ref cells nb) player) 1 -1))))))))
    w))

;; ── Alpha-beta minimax (PAIP §18.7) ──────────────────────────────

(defparameter +winning-value+  1000000)
(defparameter +losing-value+  -1000000)

(defun signum-fix (n)
  (cond ((> n 0) 1) ((< n 0) -1) (t 0)))

(defun final-value (player cells)
  (let ((s (signum-fix (count-difference player cells))))
    (cond ((= s -1) +losing-value+)
          ((= s  0) 0)
          (t        +winning-value+))))

(defun minimax (player cells alpha beta ply eval-fn)
  "Negamax with alpha-beta cutoff. Returns (values score best-move).
   At PLY=0 the score is EVAL-FN's value for PLAYER on CELLS. A
   side with no moves passes; both sides stuck ends the game."
  (cond
    ((= ply 0)
     (values (funcall eval-fn player cells) nil))
    (t
     (let ((moves (legal-moves player cells)))
       (cond
         ((null moves)
          (cond
            ((any-legal-move? (opponent player) cells)
             (multiple-value-bind (v _m)
                 (minimax (opponent player) cells (- beta) (- alpha) (- ply 1) eval-fn)
               (declare (ignore _m))
               (values (- v) nil)))
            (t (values (final-value player cells) nil))))
         (t
          (let ((best-move (first moves)) (a alpha))
            (block scan
              (dolist (mv moves)
                (let ((cells2 (make-move mv player (copy-cells cells))))
                  (multiple-value-bind (v _m)
                      (minimax (opponent player) cells2 (- beta) (- a) (- ply 1) eval-fn)
                    (declare (ignore _m))
                    (let ((val (- v)))
                      (when (> val a)
                        (setq a val)
                        (setq best-move mv))
                      (when (>= a beta) (return-from scan nil)))))))
            (values a best-move))))))))

(defun random-strategy (player cells)
  (let ((moves (legal-moves player cells)))
    (nth (random (length moves)) moves)))

(defun minimax-strategy (ply eval-fn)
  (lambda (player cells)
    (multiple-value-bind (_v move)
        (minimax player cells +losing-value+ +winning-value+ ply eval-fn)
      (declare (ignore _v))
      move)))

;; ── AI difficulty switch ─────────────────────────────────────────

(defparameter *ai-name*     "beginner")
(defparameter *ai-strategy* nil)

(defun pick-beginner ()
  (setq *ai-name* "beginner")
  (setq *ai-strategy* #'random-strategy))

(defun pick-intermediate ()
  (setq *ai-name* "intermediate")
  (setq *ai-strategy* (minimax-strategy 3 #'weighted-squares)))

(defun pick-advanced ()
  (setq *ai-name* "advanced")
  (setq *ai-strategy* (minimax-strategy 4 #'smart-weighted-squares)))

;; ── Game state ───────────────────────────────────────────────────

(defparameter *cells*           nil)
(defparameter *previous-player* nil)
(defparameter *current-player*  nil)
(defparameter *status*          "")
(defparameter *ai-pending*      nil)    ; ticks until AI moves; nil = not pending
(defparameter *win-w*           520)
(defparameter *win-h*           560)

(defun new-game ()
  (setq *cells*           (initialize-cells))
  (setq *previous-player* +white+)
  (setq *current-player*  +black+)
  (setq *ai-pending*      nil)
  (setq *status*          (format nil "Your move (Black) — AI: ~A" *ai-name*)))

(defun advance-after-move ()
  "Called after a player commits a move. Updates *previous-player*
   and *current-player* (or stops on game-over)."
  (let ((next (next-to-play *cells* *current-player*)))
    (setq *previous-player* *current-player*)
    (setq *current-player*  next)
    (cond
      ((null next)
       (let ((b (count-cells +black+ *cells*))
             (w (count-cells +white+ *cells*)))
         (setq *status*
               (cond
                 ((> b w) (format nil "Game over — BLACK wins  ~A:~A" b w))
                 ((< b w) (format nil "Game over — WHITE wins  ~A:~A" b w))
                 (t       (format nil "Game over — tie         ~A:~A" b w))))))
      ((eq next +black+)
       (setq *status* (format nil "Your move (Black) — AI: ~A" *ai-name*)))
      (t
       (setq *status* (format nil "AI thinking… (~A)" *ai-name*))
       ;; Three :tick ticks (~150-300 ms) before the AI plays —
       ;; long enough for the user to see their own move land.
       (setq *ai-pending* 3)))))

;; ── Rendering ────────────────────────────────────────────────────

(defparameter +felt+        (rgb  45 110  60))
(defparameter +felt-line+   (rgb  20  70  40))
(defparameter +disc-black+  (rgb  10  10  10))
(defparameter +disc-white+  (rgb 240 240 235))
(defparameter +hint+        (rgb  90 170 100))
(defparameter +text+        (rgb 250 250 240))
(defparameter +banner+      (rgb  25  35  30))

(defun board-pixel-side ()
  "Square play area is the smaller of the two window dimensions
   minus the status strip at the bottom."
  (max (min *win-w* (- *win-h* 36)) 64))

(defun cell-pixels ()
  (truncate (board-pixel-side) 8))

(defun draw-disc (id x y px py size player)
  (declare (ignore id x y))
  (let ((d (- size 6)))
    (fill-oval (+ px 3) (+ py 3) d d
               (if (eq player +black+) +disc-black+ +disc-white+))))

(defun paint-board (id)
  (with-batch id
    (clear +felt+)
    (let* ((size (cell-pixels))
           (board (* 8 size)))
      ;; Grid: nine vertical lines, nine horizontal lines.
      (dotimes (i 9)
        (fill-rect (* i size) 0 1 board +felt-line+)
        (fill-rect 0 (* i size) board 1 +felt-line+))
      ;; Discs and legal-move hints.
      (let ((hint-moves
              (if (and *current-player* (eq *current-player* +black+))
                  (legal-moves +black+ *cells*)
                  nil)))
        (dotimes (y 8)
          (dotimes (x 8)
            (let* ((sq    (+ (+ x 1) (* 10 (+ y 1))))
                   (piece (cells-ref *cells* sq))
                   (px    (* x size))
                   (py    (* y size)))
              (cond
                ((eq piece +black+) (draw-disc id x y px py size +black+))
                ((eq piece +white+) (draw-disc id x y px py size +white+))
                ((member sq hint-moves)
                 (let ((d (truncate size 4)))
                   (fill-oval (+ px (truncate (- size d) 2))
                              (+ py (truncate (- size d) 2))
                              d d +hint+))))))))
      ;; Status strip.
      (fill-rect 0 board *win-w* 36 +banner+)
      (draw-text 10 (+ board 8) *status* 16 +text+)
      (let ((b (count-cells +black+ *cells*))
            (w (count-cells +white+ *cells*)))
        (draw-text (- *win-w* 130) (+ board 8)
                   (format nil "B:~A  W:~A" b w) 16 +text+)))))

;; ── Click → move ─────────────────────────────────────────────────

(defun try-human-move (px py)
  (when (and *current-player* (eq *current-player* +black+))
    (let* ((size (cell-pixels))
           (cx (truncate px size))
           (cy (truncate py size)))
      (when (and (>= cx 0) (< cx 8) (>= cy 0) (< cy 8))
        (let ((sq (+ (+ cx 1) (* 10 (+ cy 1)))))
          (when (legal-move-p sq +black+ *cells*)
            (make-move sq +black+ *cells*)
            (advance-after-move)
            t))))))

(defun do-ai-move ()
  (let ((mv (funcall *ai-strategy* +white+ *cells*)))
    (when (and mv (legal-move-p mv +white+ *cells*))
      (make-move mv +white+ *cells*))
    (advance-after-move)))

;; ── Entry point ──────────────────────────────────────────────────

(defun run-othello-gui ()
  (pick-beginner)
  (new-game)
  (igui-start)
  (let ((id (open-child-sized "Othello" 520 560)))
    (cond
      ((null id)
       (format t "** open-child failed (is --windows enabled?)~%")
       :failed)
      (t
       (paint-board id)
       ;; ~60 fps so the AI countdown ticks promptly.
       (set-redraw-rate id 50)
       (event-loop-for id
         (:frame-close (return :done))
         (:close       (return :done))
         (:resize      (setq *win-w* (max (getf ev :width)  64))
                       (setq *win-h* (max (getf ev :height) 64)))
         (:mouse       (when (and (eq (getf ev :op) :left-down)
                                  (null *ai-pending*))
                         (when (try-human-move (getf ev :x) (getf ev :y))
                           (paint-board id))))
         (:tick        (when *ai-pending*
                         (setq *ai-pending* (- *ai-pending* 1))
                         (when (<= *ai-pending* 0)
                           (setq *ai-pending* nil)
                           (do-ai-move)))
                       (paint-board id))
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
                       (paint-board id)))))))
