;;;; minesweepers-gui.lisp — neuroevolution demo, ported to NCL iGui.
;;;;
;;;; A port of cormanlisp/examples/minesweepers.lisp (itself a Lisp port of
;;;; Mat Buckland's "Smart Minesweepers" from *AI Techniques for Game
;;;; Programming*). A population of 30 tanks, each driven by a small
;;;; feed-forward neural net, learns to find mines. A genetic algorithm
;;;; evolves the net weights between generations: fitter sweepers (those
;;;; that collected more mines) are more likely to breed, with crossover,
;;;; mutation, and elitism. Over generations the swarm visibly gets better
;;;; at seeking mines.
;;;;
;;;; What changed from the Corman original:
;;;;   * The Win32/GDI shell (WndProc, WinMain, back-buffer bitmap, GDI
;;;;     pens, MoveTo/LineTo/TextOut, WM_TIMER) is replaced by NCL's iGui:
;;;;     a retained-mode `with-batch` re-emitted each `:tick`, driven by
;;;;     `set-redraw-rate`. The whole AI + simulation core is otherwise a
;;;;     faithful port.
;;;;   * `(make-array 0 :fill-pointer t)` + `vector-push-extend` (the NN
;;;;     layers and GA chromosomes) use the `adjustable-vector` module —
;;;;     NCL's make-array is fixed-size only. ALL vectors here are av-*
;;;;     so element access never mixes fixed/adjustable.
;;;;
;;;; This is a workout for NCL: the double-float numeric tower (vector and
;;;; matrix math, sigmoid, transcendentals), heavy struct + vector
;;;; allocation churn every frame (good GC stress), closures, and the iGui
;;;; animation path.
;;;;
;;;; Controls:  Q quit   R reset   F cycle speed (1x/4x/16x)   SPACE pause
;;;;
;;;; Usage:
;;;;   ncl --load Lisp/demos/minesweepers-gui.lisp --eval "(minesweepers)"

(require 'adjustable-vector)

;;; ───────────────────────── parameters ──────────────────────────────────

(defvar *width*  720)                    ; world / window size, tracked on resize
(defvar *height* 720)
(defvar *two-pi* (* pi 2))

;; neural net shape
(defvar *num-inputs* 4)
(defvar *num-hidden* 1)
(defvar *neurons-per-hidden-layer* 6)
(defvar *num-outputs* 2)
(defvar *activation-response* 1.0)       ; sigmoid steepness
(defvar *bias* -1.0)

;; sweeper dynamics
(defvar *max-turn-rate* 0.3)
(defvar *max-speed* 2.0)
(defvar *sweeper-scale* 5)

;; world population
(defvar *num-sweepers* 30)
(defvar *num-mines* 40)
(defvar *num-ticks* 1000)                ; sim steps per generation
(defvar *mine-scale* 2.0)

;; genetic algorithm
(defvar *crossover-rate* 0.7)
(defvar *mutation-rate* 0.1)
(defvar *max-perturbation* 0.3)
(defvar *num-elite* 4)
(defvar *num-copies-elite* 1)

(defvar *controller* nil)
(defvar *steps-per-frame* 1)             ; F cycles 1 / 4 / 16
(defvar *paused* nil)

;;; ───────────────────────── object model ────────────────────────────────

(defstruct point x y)
(defstruct vector2d x y)

(defstruct neuron num-inputs weights)
(defstruct neuron-layer num-neurons neurons)
(defstruct neural-net
  num-inputs num-outputs num-hidden-layers neurons-per-hidden-layer layers)

(defstruct genome weights fitness)
(defstruct genetic-algorithm
  population population-size chromo-length
  total-fitness best-fitness average-fitness worst-fitness fittest-genome
  mutation-rate crossover-rate generation-counter)

(defstruct minesweeper
  brain position direction rotation speed ltrack rtrack fitness scale closest-mine)

(defstruct controller
  population sweepers mines genetic-algorithm
  num-sweepers num-mines num-weights-in-nn sweeper-shape mine-shape
  average-fitness-vector best-fitness-vector
  fast-render-mode ticks generation-counter window-width window-height)

;;; ───────────────────────── shapes ──────────────────────────────────────
;;; A tank: two tracks + a body/turret, 16 vertices (model space, ±1).

(defun sweeper-vertices ()
  (list
   (make-point :x -1.0   :y -1.0)  (make-point :x -1.0   :y 1.0)
   (make-point :x -0.5   :y 1.0)   (make-point :x -0.5   :y -1.0)
   (make-point :x 0.5    :y -1.0)  (make-point :x 1.0    :y -1.0)
   (make-point :x 1.0    :y 1.0)   (make-point :x 0.5    :y 1.0)
   (make-point :x -0.5   :y -0.5)  (make-point :x 0.5    :y -0.5)
   (make-point :x -0.5   :y 0.5)   (make-point :x -0.25  :y 0.5)
   (make-point :x -0.25  :y 1.75)  (make-point :x 0.25   :y 1.75)
   (make-point :x 0.25   :y 0.5)   (make-point :x 0.5    :y 0.5)))

(defun mine-vertices ()
  (list
   (make-point :x -1.0 :y -1.0) (make-point :x -1.0 :y 1.0)
   (make-point :x 1.0  :y 1.0)  (make-point :x 1.0  :y -1.0)))

;; Edge lists (index pairs) for stroking the shapes as line segments —
;; this is exactly the connectivity the original MoveTo/LineTo code drew.
(defvar *sweeper-edges*
  '((0 . 1) (1 . 2) (2 . 3) (3 . 0)        ; left track
    (4 . 5) (5 . 6) (6 . 7) (7 . 4)        ; right track
    (8 . 9)                                ; body line
    (10 . 11) (11 . 12) (12 . 13) (13 . 14) (14 . 15))) ; turret
(defvar *mine-edges* '((0 . 1) (1 . 2) (2 . 3) (3 . 0)))

;;; ───────────────────────── rng / util ──────────────────────────────────

(defun rand-int (x y) (+ x (random (+ (- y x) 1))))
(defun rand-float () (random 1.0))
(defun random-clamped () (- (rand-float) (rand-float)))   ; in (-1, 1)

(defun clamp (arg lo hi)
  (cond ((< arg lo) lo) ((> arg hi) hi) (t arg)))

(defun sigmoid (netinput response)
  (/ 1 (+ 1 (exp (/ (- netinput) response)))))

;;; ───────────────────────── vector2d math ───────────────────────────────

(defun vector2d-length (v)
  (let ((x (vector2d-x v)) (y (vector2d-y v)))
    (sqrt (+ (* x x) (* y y)))))

(defun vector2d-normalize (v)
  (let ((len (vector2d-length v)))
    ;; Guard against a zero-length vector (sweeper exactly on a mine) —
    ;; NCL signals on float division by zero, so return the zero vector.
    (if (= len 0)
        (make-vector2d :x 0.0 :y 0.0)
        (make-vector2d :x (/ (vector2d-x v) len) :y (/ (vector2d-y v) len)))))

(defun vector2d+ (a b)
  (make-vector2d :x (+ (vector2d-x a) (vector2d-x b))
                 :y (+ (vector2d-y a) (vector2d-y b))))
(defun vector2d- (a b)
  (make-vector2d :x (- (vector2d-x a) (vector2d-x b))
                 :y (- (vector2d-y a) (vector2d-y b))))
(defun vector2d-multiply (v c)
  (make-vector2d :x (* (vector2d-x v) c) :y (* (vector2d-y v) c)))

;;; ───────────────────────── matrix transforms ───────────────────────────

(defstruct matrix _11 _12 _13 _21 _22 _23 _31 _32 _33)

(defun create-matrix ()
  (make-matrix :_11 1 :_12 0 :_13 0  :_21 0 :_22 1 :_23 0  :_31 0 :_32 0 :_33 1))

(defun matrix-multiply (a b)
  (make-matrix
   :_11 (+ (* (matrix-_11 a) (matrix-_11 b)) (* (matrix-_12 a) (matrix-_21 b)) (* (matrix-_13 a) (matrix-_31 b)))
   :_12 (+ (* (matrix-_11 a) (matrix-_12 b)) (* (matrix-_12 a) (matrix-_22 b)) (* (matrix-_13 a) (matrix-_32 b)))
   :_13 (+ (* (matrix-_11 a) (matrix-_13 b)) (* (matrix-_12 a) (matrix-_23 b)) (* (matrix-_13 a) (matrix-_33 b)))
   :_21 (+ (* (matrix-_21 a) (matrix-_11 b)) (* (matrix-_22 a) (matrix-_21 b)) (* (matrix-_23 a) (matrix-_31 b)))
   :_22 (+ (* (matrix-_21 a) (matrix-_12 b)) (* (matrix-_22 a) (matrix-_22 b)) (* (matrix-_23 a) (matrix-_32 b)))
   :_23 (+ (* (matrix-_21 a) (matrix-_13 b)) (* (matrix-_22 a) (matrix-_23 b)) (* (matrix-_23 a) (matrix-_33 b)))
   :_31 (+ (* (matrix-_31 a) (matrix-_11 b)) (* (matrix-_32 a) (matrix-_21 b)) (* (matrix-_33 a) (matrix-_31 b)))
   :_32 (+ (* (matrix-_31 a) (matrix-_12 b)) (* (matrix-_32 a) (matrix-_22 b)) (* (matrix-_33 a) (matrix-_32 b)))
   :_33 (+ (* (matrix-_31 a) (matrix-_13 b)) (* (matrix-_32 a) (matrix-_23 b)) (* (matrix-_33 a) (matrix-_33 b)))))

(defun matrix-translate (m x y)
  (matrix-multiply m (make-matrix :_11 1 :_12 0 :_13 0 :_21 0 :_22 1 :_23 0 :_31 x :_32 y :_33 1)))
(defun matrix-scale (m sx sy)
  (matrix-multiply m (make-matrix :_11 sx :_12 0 :_13 0 :_21 0 :_22 sy :_23 0 :_31 0 :_32 0 :_33 1)))
(defun matrix-rotate (m rot)
  (let ((s (sin rot)) (c (cos rot)))
    (matrix-multiply m (make-matrix :_11 c :_12 s :_13 0 :_21 (- s) :_22 c :_23 0 :_31 0 :_32 0 :_33 1))))

;; transform an av of points in place
(defun transform-points (points m)
  (dotimes (i (av-length points))
    (let* ((p (av-ref points i))
           (px (point-x p)) (py (point-y p))
           (tx (+ (* (matrix-_11 m) px) (* (matrix-_21 m) py) (matrix-_31 m)))
           (ty (+ (* (matrix-_12 m) px) (* (matrix-_22 m) py) (matrix-_32 m))))
      (setf (point-x p) tx)
      (setf (point-y p) ty))))

(defun world-transform (points pos scale rotation)
  (let ((m (create-matrix)))
    (setq m (matrix-scale m scale scale))
    (if (/= rotation 0) (setq m (matrix-rotate m rotation)))
    (setq m (matrix-translate m (vector2d-x pos) (vector2d-y pos)))
    (transform-points points m)
    points))

;;; ───────────────────────── neural net ──────────────────────────────────

(defun create-neuron (num-inputs)
  ;; +1 weight for the bias
  (let* ((n (+ num-inputs 1))
         (neuron (make-neuron :num-inputs n :weights (av-make-filled n 0.0))))
    (dotimes (i n) (setf (av-ref (neuron-weights neuron) i) (random-clamped)))
    neuron))

(defun create-neuron-layer (num-neurons num-inputs-per-neuron)
  (let ((layer (make-neuron-layer :num-neurons num-neurons
                                  :neurons (av-make-filled num-neurons nil))))
    (dotimes (i num-neurons)
      (setf (av-ref (neuron-layer-neurons layer) i)
            (create-neuron num-inputs-per-neuron)))
    layer))

(defun create-neural-net ()
  (let ((nn (make-neural-net :num-inputs *num-inputs* :num-outputs *num-outputs*
                             :num-hidden-layers *num-hidden*
                             :neurons-per-hidden-layer *neurons-per-hidden-layer*
                             :layers (av-make))))
    (if (> *num-hidden* 0)
        (let ((layers (neural-net-layers nn)))
          (av-push-extend (create-neuron-layer *neurons-per-hidden-layer* *num-inputs*) layers)
          (dotimes (i (- *num-hidden* 1))
            (av-push-extend (create-neuron-layer *neurons-per-hidden-layer* *neurons-per-hidden-layer*) layers))
          (av-push-extend (create-neuron-layer *num-outputs* *neurons-per-hidden-layer*) layers))
        (av-push-extend (create-neuron-layer *num-outputs* *num-inputs*) (neural-net-layers nn)))
    nn))

(defun neural-net-number-of-weights (nn)
  (let ((weights 0))
    (dotimes (i (av-length (neural-net-layers nn)))
      (let ((layer (av-ref (neural-net-layers nn) i)))
        (dotimes (j (av-length (neuron-layer-neurons layer)))
          (setq weights (+ weights (av-length (neuron-weights (av-ref (neuron-layer-neurons layer) j))))))))
    weights))

(defun neural-net-set-weights (nn vec-weights)
  (let ((cweight 0))
    (dotimes (i (+ 1 (neural-net-num-hidden-layers nn)))
      (let ((layer (av-ref (neural-net-layers nn) i)))
        (dotimes (j (neuron-layer-num-neurons layer))
          (let ((neuron (av-ref (neuron-layer-neurons layer) j)))
            (dotimes (k (neuron-num-inputs neuron))
              (setf (av-ref (neuron-weights neuron) k) (av-ref vec-weights cweight))
              (setq cweight (+ cweight 1)))))))
    nil))

;; Feed-forward. NOTE: this faithfully replicates the Corman/Buckland Lisp
;; port, which loops only over the hidden layers (=1 here) and never runs
;; the output layer, reads the bias at index (1- *num-inputs*), and resets
;; the input cursor each neuron. The GA evolves whatever weights produce
;; mine-seeking from outputs 0/1, so the known-good behaviour is preserved.
(defun neural-net-update (nn inputs)
  (let ((outputs (av-make)) (weight 0))
    (if (/= (av-length inputs) *num-inputs*)
        (return-from neural-net-update outputs))
    (dotimes (i (neural-net-num-hidden-layers nn))
      (let ((layer (av-ref (neural-net-layers nn) i)))
        (if (> i 0) (setq inputs outputs))
        (av-set-fill outputs 0)
        (setq weight 0)
        (dotimes (j (neuron-layer-num-neurons layer))
          (let* ((netinput 0)
                 (neuron (av-ref (neuron-layer-neurons layer) j))
                 (num-inputs (neuron-num-inputs neuron)))
            (dotimes (k (- num-inputs 1))
              (setq netinput (+ netinput (* (av-ref (neuron-weights neuron) k) (av-ref inputs weight))))
              (setq weight (+ weight 1)))
            (setq netinput (+ netinput (* (av-ref (neuron-weights neuron) (- *num-inputs* 1)) *bias*)))
            (av-push-extend (sigmoid netinput *activation-response*) outputs)
            (setq weight 0)))))
    outputs))

;;; ───────────────────────── genetic algorithm ───────────────────────────

(defun create-genome (weights fitness) (make-genome :weights weights :fitness fitness))

(defun create-genetic-algorithm (pop-size mutation-rate crossover-rate num-weights)
  (let ((ga (make-genetic-algorithm
             :population (av-make-filled pop-size nil) :population-size pop-size
             :mutation-rate mutation-rate :crossover-rate crossover-rate
             :chromo-length num-weights :total-fitness 0.0 :best-fitness 0.0
             :average-fitness 0.0 :worst-fitness 99999999.0 :fittest-genome 0
             :generation-counter 0)))
    (dotimes (i pop-size)
      (let ((weights (av-make-filled num-weights 0.0)))
        (dotimes (j num-weights) (setf (av-ref weights j) (random-clamped)))
        (setf (av-ref (genetic-algorithm-population ga) i) (create-genome weights 0.0))))
    ga))

(defun genetic-algorithm-reset (ga)
  (setf (genetic-algorithm-total-fitness ga) 0)
  (setf (genetic-algorithm-best-fitness ga) 0)
  (setf (genetic-algorithm-worst-fitness ga) 9999999)
  (setf (genetic-algorithm-average-fitness ga) 0))

(defun genetic-algorithm-calculate-fitness (ga)
  (setf (genetic-algorithm-total-fitness ga) 0)
  (let ((highest 0) (lowest 9999999))
    (dotimes (i (genetic-algorithm-population-size ga))
      (let* ((genome (av-ref (genetic-algorithm-population ga) i))
             (f (genome-fitness genome)))
        (when (> f highest)
          (setq highest f)
          (setf (genetic-algorithm-fittest-genome ga) i)
          (setf (genetic-algorithm-best-fitness ga) highest))
        (when (< f lowest)
          (setq lowest f)
          (setf (genetic-algorithm-worst-fitness ga) lowest))
        (setf (genetic-algorithm-total-fitness ga) (+ (genetic-algorithm-total-fitness ga) f))))
    (setf (genetic-algorithm-average-fitness ga)
          (/ (genetic-algorithm-total-fitness ga) (genetic-algorithm-population-size ga)))))

(defun genetic-algorithm-grab-n-best (ga nbest num-copies population)
  (dotimes (c num-copies)
    (let ((n nbest))
      (dotimes (k nbest)
        (setq n (- n 1))
        (av-push-extend (av-ref (genetic-algorithm-population ga)
                                (- (- (genetic-algorithm-population-size ga) 1) n))
                        population)))))

(defun genetic-algorithm-crossover (ga mom dad baby1 baby2)
  (let ((cp 0))
    (if (or (> (rand-float) (genetic-algorithm-crossover-rate ga)) (eq mom dad))
        (setq cp (av-length mom))
        (setq cp (rand-int 0 (- (genetic-algorithm-chromo-length ga) 1))))
    (dotimes (i cp)
      (av-push-extend (av-ref mom i) baby1)
      (av-push-extend (av-ref dad i) baby2))
    (let ((i cp))
      (dotimes (k (- (av-length mom) cp))
        (av-push-extend (av-ref dad i) baby1)
        (av-push-extend (av-ref mom i) baby2)
        (setq i (+ i 1))))))

(defun genetic-algorithm-get-chromo-roulette (ga)
  (let ((slice (* (rand-float) (genetic-algorithm-total-fitness ga)))
        (fitness-so-far 0))
    (dotimes (i (genetic-algorithm-population-size ga))
      (setq fitness-so-far (+ fitness-so-far (genome-fitness (av-ref (genetic-algorithm-population ga) i))))
      (when (>= fitness-so-far slice)
        (return-from genetic-algorithm-get-chromo-roulette (av-ref (genetic-algorithm-population ga) i))))
    ;; fitness all zero (first generation) — just pick the last
    (av-ref (genetic-algorithm-population ga) (- (genetic-algorithm-population-size ga) 1))))

(defun genetic-algorithm-mutate (ga chromosomes)
  (dotimes (i (av-length chromosomes))
    (if (< (rand-float) (genetic-algorithm-mutation-rate ga))
        (setf (av-ref chromosomes i) (+ (av-ref chromosomes i) (* (random-clamped) *max-perturbation*))))))

(defun genetic-algorithm-epoch (ga old-population)
  (setf (genetic-algorithm-population ga) (av-sort old-population #'< :key #'genome-fitness))
  (genetic-algorithm-reset ga)
  (genetic-algorithm-calculate-fitness ga)
  (let ((new-population (av-make)))
    (if (evenp (* *num-copies-elite* *num-elite*))
        (genetic-algorithm-grab-n-best ga *num-elite* *num-copies-elite* new-population))
    (loop
      (when (>= (av-length new-population) (genetic-algorithm-population-size ga))
        (return))
      (let ((mom (genetic-algorithm-get-chromo-roulette ga))
            (dad (genetic-algorithm-get-chromo-roulette ga))
            (baby1 (av-make)) (baby2 (av-make)))
        (genetic-algorithm-crossover ga (genome-weights mom) (genome-weights dad) baby1 baby2)
        (genetic-algorithm-mutate ga baby1)
        (genetic-algorithm-mutate ga baby2)
        (av-push-extend (make-genome :weights baby1 :fitness 0) new-population)
        (av-push-extend (make-genome :weights baby2 :fitness 0) new-population)))
    new-population))

;;; ───────────────────────── minesweeper agent ───────────────────────────

(defun create-minesweeper ()
  (let ((ms (make-minesweeper :rotation (* (rand-float) *two-pi*) :ltrack 0.16 :rtrack 0.16
                              :fitness 0 :speed 0 :scale *sweeper-scale* :closest-mine 0
                              :brain (create-neural-net))))
    (setf (minesweeper-position ms) (make-vector2d :x (* (rand-float) *width*) :y (* (rand-float) *height*)))
    (setf (minesweeper-direction ms) (make-vector2d :x 0 :y 0))
    ms))

(defun reset-minesweeper (ms)
  (setf (minesweeper-position ms) (make-vector2d :x (* (rand-float) *width*) :y (* (rand-float) *height*)))
  (setf (minesweeper-fitness ms) 0.0)
  (setf (minesweeper-rotation ms) (* (rand-float) *two-pi*)))

(defun minesweeper-get-closest-mine (ms mines)
  (let ((closest-so-far 99999.0)
        (closest-object (make-vector2d :x 0 :y 0)))
    (dotimes (i (av-length mines))
      (let ((d (vector2d-length (vector2d- (av-ref mines i) (minesweeper-position ms)))))
        (when (< d closest-so-far)
          (setq closest-so-far d)
          (setq closest-object (vector2d- (minesweeper-position ms) (av-ref mines i)))
          (setf (minesweeper-closest-mine ms) i))))
    closest-object))

(defun minesweeper-update (ms mines)
  (let ((inputs (av-make))
        (closest-mine (vector2d-normalize (minesweeper-get-closest-mine ms mines))))
    (av-push-extend (vector2d-x closest-mine) inputs)
    (av-push-extend (vector2d-y closest-mine) inputs)
    (av-push-extend (vector2d-x (minesweeper-direction ms)) inputs)
    (av-push-extend (vector2d-y (minesweeper-direction ms)) inputs)
    (let ((output (neural-net-update (minesweeper-brain ms) inputs)))
      (if (< (av-length output) *num-outputs*)
          (return-from minesweeper-update nil))
      (setf (minesweeper-ltrack ms) (av-ref output 0))
      (setf (minesweeper-rtrack ms) (av-ref output 1))
      (let ((rot-force (clamp (- (minesweeper-ltrack ms) (minesweeper-rtrack ms))
                              (- *max-turn-rate*) *max-turn-rate*)))
        (setf (minesweeper-rotation ms) (+ (minesweeper-rotation ms) rot-force))
        (setf (minesweeper-speed ms) (+ (minesweeper-ltrack ms) (minesweeper-rtrack ms)))
        (setf (minesweeper-direction ms)
              (make-vector2d :x (- (sin (minesweeper-rotation ms))) :y (cos (minesweeper-rotation ms))))
        (setf (minesweeper-position ms)
              (vector2d+ (minesweeper-position ms)
                         (vector2d-multiply (minesweeper-direction ms) (minesweeper-speed ms))))
        (let ((pos (minesweeper-position ms)))
          (if (> (vector2d-x pos) *width*)  (setf (vector2d-x pos) 0))
          (if (< (vector2d-x pos) 0)        (setf (vector2d-x pos) *width*))
          (if (> (vector2d-y pos) *height*) (setf (vector2d-y pos) 0))
          (if (< (vector2d-y pos) 0)        (setf (vector2d-y pos) *height*)))
        t))))

(defun minesweeper-check-for-mine (ms mines size)
  (let ((d (vector2d- (minesweeper-position ms) (av-ref mines (minesweeper-closest-mine ms)))))
    (if (< (vector2d-length d) (+ size 5)) (minesweeper-closest-mine ms) nil)))

(defun minesweeper-put-weights (ms weights)
  (neural-net-set-weights (minesweeper-brain ms) weights))

;;; ───────────────────────── controller ──────────────────────────────────

(defun create-controller ()
  (let ((controller (make-controller
                     :fast-render-mode nil :ticks 0 :num-mines *num-mines*
                     :num-sweepers *num-sweepers* :generation-counter 0
                     :window-width *width* :window-height *height*
                     :sweepers (av-make-filled *num-sweepers* nil)
                     :mines (av-make-filled *num-mines* nil)
                     :average-fitness-vector (av-make) :best-fitness-vector (av-make))))
    (let ((sweepers (controller-sweepers controller)))
      (dotimes (i *num-sweepers*) (setf (av-ref sweepers i) (create-minesweeper)))
      (setf (controller-num-weights-in-nn controller)
            (neural-net-number-of-weights (minesweeper-brain (av-ref sweepers 0))))
      (setf (controller-genetic-algorithm controller)
            (create-genetic-algorithm *num-sweepers* *mutation-rate* *crossover-rate*
                                       (controller-num-weights-in-nn controller)))
      (setf (controller-population controller)
            (genetic-algorithm-population (controller-genetic-algorithm controller)))
      (dotimes (i *num-sweepers*)
        (neural-net-set-weights (minesweeper-brain (av-ref sweepers i))
                                (genome-weights (av-ref (controller-population controller) i))))
      (dotimes (i *num-mines*)
        (setf (av-ref (controller-mines controller) i)
              (make-vector2d :x (* (rand-float) *width*) :y (* (rand-float) *height*)))))
    controller))

;; Advance the simulation by one tick. Returns NIL when a generation just
;; ended and the GA produced a new population (so the caller can note it).
(defun controller-update (controller)
  (cond
    ((< (controller-ticks controller) *num-ticks*)
     (setf (controller-ticks controller) (+ (controller-ticks controller) 1))
     (dotimes (i (controller-num-sweepers controller))
       (let ((sweeper (av-ref (controller-sweepers controller) i))
             (genome (av-ref (controller-population controller) i)))
         (minesweeper-update sweeper (controller-mines controller))
         (let ((hit (minesweeper-check-for-mine sweeper (controller-mines controller) *mine-scale*)))
           (when hit
             (setf (minesweeper-fitness sweeper) (+ (minesweeper-fitness sweeper) 1))
             (setf (av-ref (controller-mines controller) hit)
                   (make-vector2d :x (* (rand-float) *width*) :y (* (rand-float) *height*))))
           (setf (genome-fitness genome) (minesweeper-fitness sweeper)))))
     t)
    (t
     ;; generation complete — run the GA
     (let ((ga (controller-genetic-algorithm controller)))
       (av-push-extend (genetic-algorithm-average-fitness ga) (controller-average-fitness-vector controller))
       (av-push-extend (genetic-algorithm-best-fitness ga) (controller-best-fitness-vector controller))
       (setf (controller-generation-counter controller) (+ (controller-generation-counter controller) 1))
       (setf (controller-ticks controller) 0)
       (setf (controller-population controller)
             (genetic-algorithm-epoch ga (controller-population controller)))
       (dotimes (i (controller-num-sweepers controller))
         (let ((sweeper (av-ref (controller-sweepers controller) i)))
           (minesweeper-put-weights sweeper (genome-weights (av-ref (controller-population controller) i)))
           (reset-minesweeper sweeper))))
     nil)))

;;; ───────────────────────── rendering (iGui) ────────────────────────────

(defvar +bg+      (rgb 250 250 250))
(defvar +mine+    (rgb 0 150 0))
(defvar +elite+   (rgb 210 40 40))       ; the best *num-elite* sweepers
(defvar +sweeper+ (rgb 70 70 80))
(defvar +hud+     (rgb 20 20 30))
(defvar +best-ln+ (rgb 210 40 40))
(defvar +avg-ln+  (rgb 50 90 200))
(defvar +panel+   (rgb 235 235 240))

(defun flr (x) (floor x))

;; Stroke a transformed shape (av of points) along EDGES (index pairs).
(defun draw-shape (pts edges color)
  (dolist (e edges)
    (let ((a (av-ref pts (car e))) (b (av-ref pts (cdr e))))
      (draw-line (flr (point-x a)) (flr (point-y a))
                 (flr (point-x b)) (flr (point-y b)) 1 color))))

;; A small best/avg fitness-history line graph in the corner.
(defun draw-graph (controller)
  (let* ((bestv (controller-best-fitness-vector controller))
         (avgv (controller-average-fitness-vector controller))
         (n (av-length bestv))
         (gw 180) (gh 90) (gx (- *width* gw 12)) (gy 12))
    (fill-rect gx gy gw gh +panel+)
    (when (> n 1)
      (let ((maxv 1.0))
        (dotimes (i n) (if (> (av-ref bestv i) maxv) (setq maxv (av-ref bestv i))))
        (let ((dx (/ gw (- n 1))))
          (dotimes (i (- n 1))
            (let ((x0 (+ gx (flr (* i dx)))) (x1 (+ gx (flr (* (+ i 1) dx)))))
              ;; best (red)
              (draw-line x0 (+ gy (- gh (flr (* gh (/ (av-ref bestv i) maxv)))))
                         x1 (+ gy (- gh (flr (* gh (/ (av-ref bestv (+ i 1)) maxv))))) 1 +best-ln+)
              ;; average (blue)
              (draw-line x0 (+ gy (- gh (flr (* gh (/ (av-ref avgv i) maxv)))))
                         x1 (+ gy (- gh (flr (* gh (/ (av-ref avgv (+ i 1)) maxv))))) 1 +avg-ln+))))))))

(defun render-controller (controller id)
  (with-batch id
    (clear +bg+)
    ;; mines
    (dotimes (i (controller-num-mines controller))
      (let ((pts (world-transform (list->av (mine-vertices))
                                  (av-ref (controller-mines controller) i) *mine-scale* 0)))
        (draw-shape pts *mine-edges* +mine+)))
    ;; sweepers — first *num-elite* (the carried-over best) in red
    (dotimes (i (controller-num-sweepers controller))
      (let* ((sweeper (av-ref (controller-sweepers controller) i))
             (color (if (< i *num-elite*) +elite+ +sweeper+))
             (pts (world-transform (list->av (sweeper-vertices))
                                   (minesweeper-position sweeper) *sweeper-scale*
                                   (minesweeper-rotation sweeper))))
        (draw-shape pts *sweeper-edges* color)))
    ;; fitness-history graph
    (draw-graph controller)
    ;; HUD
    (let ((ga (controller-genetic-algorithm controller)))
      (draw-text 8 6  (format nil "Generation ~D"
                              (controller-generation-counter controller)) 16 +hud+)
      (draw-text 8 28 (format nil "Tick ~D / ~D   Speed ~Dx~A"
                              (controller-ticks controller) *num-ticks* *steps-per-frame*
                              (if *paused* "  [PAUSED]" "")) 13 +hud+)
      (draw-text 8 46 (format nil "Best ~,1F   Avg ~,2F"
                              (genetic-algorithm-best-fitness ga)
                              (genetic-algorithm-average-fitness ga)) 13 +hud+)
      (draw-text 8 (- *height* 22)
                 "Q quit   R reset   F speed   SPACE pause" 12 +hud+))))

;;; ───────────────────────── driver ──────────────────────────────────────

(defun step-controller (controller n)
  "Advance the sim N steps (or until a generation boundary)."
  (dotimes (k n) (controller-update controller)))

(defun minesweepers ()
  "Run the minesweepers neuroevolution demo in an iGui window."
  (igui-start)
  (let ((id (open-child-sized "Smart Minesweepers" *width* *height*)))
    (cond
      ((null id) (format t "** minesweepers: open-child failed~%") :failed)
      (t
       (setq *paused* nil)
       (setq *steps-per-frame* 1)
       (setq *controller* (create-controller))
       (render-controller *controller* id)
       (set-redraw-rate id 16)            ; ~60 fps tick
       (event-loop-for id
         (:frame-close (return :done))
         (:close       (return :done))
         (:resize      (setq *width*  (max (getf ev :width) 1))
                       (setq *height* (max (getf ev :height) 1)))
         (:tick        (unless *paused*
                         (step-controller *controller* *steps-per-frame*))
                       (render-controller *controller* id))
         (:char
          (let ((ch (getf ev :char)))
            (cond
              ((or (eql ch #\q) (eql ch #\Q)) (return :done))
              ((or (eql ch #\r) (eql ch #\R))
               (setq *controller* (create-controller))
               (render-controller *controller* id))
              ((or (eql ch #\f) (eql ch #\F))
               (setq *steps-per-frame*
                     (cond ((= *steps-per-frame* 1) 4)
                           ((= *steps-per-frame* 4) 16)
                           (t 1))))
              ((eql ch #\Space)
               (setq *paused* (not *paused*)))))))))))
