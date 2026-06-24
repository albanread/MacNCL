;;;; bench/bench.lisp — NCL performance benchmark suite.
;;;;
;;;; Run:  ./target/release/ncl.exe --load bench/bench.lisp
;;;;
;;;; Each benchmark prints one parseable line:  BENCH <name> <ms> ms
;;;; Every thunk runs once untimed first (warm the JIT, steady-state
;;;; the heap), then once timed. Sizes are chosen so each timed run
;;;; lands roughly in the 0.1–3 s band on the unoptimized build.
;;;;
;;;; Coverage map (why each bench exists):
;;;;   cons-churn   — alloc_cons fast path: safepoint poll + TLAB bump
;;;;   tak, fib     — Lisp→Lisp call overhead + inline fixnum arithmetic
;;;;   loop-sum     — compiled extended-LOOP numeric iteration
;;;;   loop-collect — LOOP accumulation (cons-heavy)
;;;;   loop-parse   — LOOP macroexpansion cost (%plan-add-* appends)
;;;;   upcase       — string-upcase (string-append-char O(n²) pattern)
;;;;   sort-10k     — list merge sort: funcall + < comparisons
;;;;   apply-small  — ncl_apply buffer build (heap Vec per call)
;;;;   num-cmp      — eql/=/< on mixed fixnum/float (tag-decode paths)
;;;;   mandel       — float kernel + per-pixel shim call (canvas poke)

(defun %bench-ms (ns) (truncate ns 1000000))

(defun run-bench (name thunk)
  (funcall thunk)                       ; warmup
  ;; Collect before timing so the bytes-since-GC trigger doesn't
  ;; fire mid-measurement: WHERE the periodic minor lands depends on
  ;; how much the *upstream* benches allocated, which made
  ;; sort/mandel numbers swing ±400ms run-to-run as fixes changed
  ;; upstream allocation volume. Pause cost is reported separately
  ;; via the minors/max-pause columns.
  (gc)
  (let ((g0 (getf (gc-stats) :minor-gcs))
        (t0 (get-internal-real-time)))
    (funcall thunk)
    (let ((ms (%bench-ms (- (get-internal-real-time) t0)))
          (g1 (gc-stats)))
      (format t "BENCH ~A ~D ms  [minors ~D max-pause-us ~D]~%"
              name ms
              (- (getf g1 :minor-gcs) g0)
              (getf g1 :max-minor-pause-us)))))

;; ── cons churn ──────────────────────────────────────────────────────────

(defun bench-cons-churn ()
  ;; 8M conses in 8000 dropped lists of 1000 — young-gen churn.
  (dotimes (i 8000)
    (let ((l nil))
      (dotimes (j 1000)
        (setq l (cons j l)))))
  nil)

;; ── call overhead ───────────────────────────────────────────────────────

(defun tak (x y z)
  (if (not (< y x))
      z
      (tak (tak (- x 1) y z)
           (tak (- y 1) z x)
           (tak (- z 1) x y))))

(defun bench-tak ()
  (dotimes (i 100) (tak 18 12 6)))

(defun fib (n)
  (if (< n 2) n (+ (fib (- n 1)) (fib (- n 2)))))

(defun bench-fib ()
  (fib 32))

;; ── LOOP ────────────────────────────────────────────────────────────────

(defun bench-loop-sum ()
  (loop for i from 0 below 30000000 sum i))

(defun bench-loop-collect ()
  (length (loop for i from 0 below 2000000 collect i)))

(defparameter *loop-parse-form*
  '(loop for a from 0 below 10
         for b from 10 above 0
         for c = (+ a b)
         with d = 1
         with e = 2
         with f = 3
         when (> c 5) collect c
         when (< c 5) collect a
         unless (= c 5) count c
         maximize c
         minimize a
         sum b
         until (> a 100)
         while (< a 200)
         finally (return t)))

(defun bench-loop-parse ()
  ;; eval → full reparse + plan build + compile, 30×.
  (dotimes (i 30) (eval *loop-parse-form*)))

;; ── strings ─────────────────────────────────────────────────────────────

(defparameter *big-string*
  (let ((s ""))
    ;; 8192 chars built once (cost not timed).
    (dotimes (i 8192)
      (setq s (string-append-char s (code-char (+ 97 (mod i 26))))))
    s))

(defun bench-upcase ()
  (length (string-upcase *big-string*)))

;; ── sort ────────────────────────────────────────────────────────────────

(defun %pseudo-list (n)
  ;; small-modulus LCG; stays comfortably inside fixnums.
  (let ((seed 12345) (l nil))
    (dotimes (i n)
      (setq seed (mod (+ (* seed 75) 74) 65537))
      (setq l (cons seed l)))
    l))

(defun bench-sort-30k ()
  (length (sort (%pseudo-list 30000) #'<)))

;; ── apply ───────────────────────────────────────────────────────────────

(defparameter *apply-tail* '(3 4))

(defun bench-apply-small ()
  (dotimes (i 500000)
    (apply #'+ 1 2 *apply-tail*)))

;; ── numeric compare / eql ───────────────────────────────────────────────

(defun bench-num-cmp ()
  (let ((acc 0))
    (dotimes (i 3000000)
      (when (eql 1.5 1.5) (setq acc (+ acc 1)))
      (when (= 1.5 1.5)   (setq acc (+ acc 1)))
      (when (< 1 2.5)     (setq acc (+ acc 1))))
    acc))

;; ── mandelbrot frame (float kernel + per-pixel shim poke) ───────────────

(defun bm-iter (cx cy max)
  (let ((zx 0.0) (zy 0.0) (n 0))
    (loop
      (let ((zx2 (* zx zx)) (zy2 (* zy zy)))
        (when (or (>= n max) (> (+ zx2 zy2) 4.0))
          (return n))
        (let ((nzx (+ (- zx2 zy2) cx))
              (nzy (+ (* 2.0 (* zx zy)) cy)))
          (setq zx nzx) (setq zy nzy)))
      (setq n (+ n 1)))))

(defun bench-mandel ()
  (let* ((w 160) (h 120) (max 60)
         (base (and (fboundp 'canvas-open) (canvas-open 9999 w h))))
    (if (null base)
        (format t ";; mandel: no canvas, skipped~%")
        (let ((x-step (/ 3.5 w)) (y-step (/ 2.5 h)))
          (dotimes (py h)
            (let ((cy (+ -1.25 (* py y-step))) (row (* py w)))
              (dotimes (px w)
                (let ((n (bm-iter (+ -2.5 (* px x-step)) cy max)))
                  (buffer-set-u32 base (* (+ row px) 4)
                                  (logand (* n 4144959) 16777215))))))))))

;; ── runner ──────────────────────────────────────────────────────────────

(format t "~%=== NCL bench ===~%")
(run-bench "cons-churn"   #'bench-cons-churn)
(run-bench "tak"          #'bench-tak)
(run-bench "fib"          #'bench-fib)
(run-bench "loop-sum"     #'bench-loop-sum)
(run-bench "loop-collect" #'bench-loop-collect)
(run-bench "loop-parse"   #'bench-loop-parse)
(run-bench "upcase"       #'bench-upcase)
(run-bench "sort-30k"     #'bench-sort-30k)
(run-bench "apply-small"  #'bench-apply-small)
(run-bench "num-cmp"      #'bench-num-cmp)
(run-bench "mandel"       #'bench-mandel)
(format t "=== bench done ===~%")
