;;;; bench/gauntlet.lisp — fast correctness gauntlet for performance work.
;;;;
;;;; Run:  ./target/release/ncl.exe --load bench/gauntlet.lisp
;;;;       (or the release nclterm.exe — any console build).
;;;;
;;;; Prints PASS/FAIL per check and a final "GAUNTLET ALL-PASS" / "N FAILS".
;;;;
;;;; A *targeted* regression gate that complements the broad ANSI suite
;;;; (demos/ansi-runner.lisp): it exercises exactly the surfaces the perf
;;;; changes touch, so a behavioural regression shows up in <1s instead of
;;;; in a 600-case conformance run. Keep it fast, and extend it whenever a
;;;; perf change lands. Sections below map 1:1 to landed work:
;;;;   LOOP plan        -> LOOP append->cons+nreverse rewrite
;;;;   strings          -> O(n) string-upcase/downcase/capitalize
;;;;   eql / cmp tower  -> "decode tags once" in eql_values / ncl_cmp_full
;;;;   apply            -> ncl_apply <=16-arg stack buffer (17/20 cross it)
;;;;   alloc + GC churn -> cheap safepoint poll + inline root stack
;;;; Next to add: unboxed-float arithmetic — see
;;;; docs/performance-unbox-float.md (a float-correctness section is the
;;;; gate for that work's Sprint 1).

(defparameter *fails* 0)
(defun chk (name got want)
  (cond
    ((equal got want) (format t "PASS ~A~%" name))
    (t (setq *fails* (+ *fails* 1))
       (format t "FAIL ~A: got ~S want ~S~%" name got want))))

;; ── LOOP plan ordering ──────────────────────────────────────────────
(chk "loop-collect-order" (loop for i from 1 to 5 collect i) '(1 2 3 4 5))
(chk "loop-multi-with"
     (loop with a = 1 with b = 2 with c = 3 for i from 0 below 1 collect (list a b c))
     '((1 2 3)))
(chk "loop-body-order"
     (let ((acc nil))
       (loop for i from 1 to 3 do (setq acc (cons i acc)) do (setq acc (cons (* i 10) acc)))
       (reverse acc))
     '(1 10 2 20 3 30))
(chk "loop-when-collect" (loop for i from 1 to 10 when (evenp i) collect i) '(2 4 6 8 10))
(chk "loop-unless-collect" (loop for i from 1 to 6 unless (evenp i) collect i) '(1 3 5))
(chk "loop-last-accum-wins" (loop for i from 1 to 4 collect i sum i) 10)
(chk "loop-sum-then-collect" (loop for i from 1 to 4 sum i collect i) '(1 2 3 4))
(chk "loop-into-finally"
     (loop for i from 1 to 4 collect i into xs finally (return (reverse xs)))
     '(4 3 2 1))
(chk "loop-post-until"
     (let ((n 0)) (loop do (setq n (+ n 1)) until (>= n 5)) n)
     5)
(chk "loop-initially"
     (let ((acc nil))
       (loop initially (setq acc (cons :init acc))
             for i from 1 to 2 do (setq acc (cons i acc)))
       (reverse acc))
     '(:init 1 2))
(chk "loop-max-min" (list (loop for i in '(3 1 4 1 5) maximize i)
                          (loop for i in '(3 1 4 1 5) minimize i))
     '(5 1))
(chk "loop-while-steps"
     (let ((x 0)) (loop for i from 0 below 100 while (< i 3) do (setq x i)) x)
     2)
(chk "loop-append" (loop for x in '((1 2) (3) (4 5)) append x) '(1 2 3 4 5))
(chk "loop-count" (loop for i from 1 to 10 count (evenp i)) 5)
(chk "loop-named"
     (loop named outer for i from 0 below 10
           do (when (= i 3) (return-from outer (* i 7))))
     21)

;; ── strings ─────────────────────────────────────────────────────────
(chk "upcase" (string-upcase "hello world") "HELLO WORLD")
(chk "downcase" (string-downcase "HeLLo") "hello")
(chk "upcase-range" (string-upcase "abcdef" :start 2 :end 4) "abCDef")
(chk "downcase-range" (string-downcase "ABCDEF" :start 1 :end 3) "AbcDEF")
(chk "capitalize" (string-capitalize "hello world 3rd time") "Hello World 3rd Time")
(chk "capitalize-range" (string-capitalize "hello world" :start 6) "hello World")
(chk "upcase-empty" (string-upcase "") "")
(chk "upcase-symbol" (string-upcase 'foo) "FOO")

;; ── eql / cmp numeric tower ─────────────────────────────────────────
(chk "eql-float" (eql 1.5 1.5) t)
(chk "eql-float-ne" (eql 1.5 2.5) nil)
(chk "eql-cross-type" (eql 3 3.0) nil)
(chk "eql-bignum" (eql (expt 2 100) (expt 2 100)) t)
(chk "eql-ratio" (eql 1/3 1/3) t)
(chk "eql-ratio-ne" (eql 1/3 2/3) nil)
(chk "eql-complex" (eql #c(3 -4) #c(3 -4)) t)
(chk "eql-complex-ne" (eql #c(3 -4) #c(3 4)) nil)
(chk "eql-fixnum" (eql 42 42) t)
(chk "eql-char" (eql #\a #\a) t)
(chk "eql-string-not" (eql "ab" "ab") nil)
(chk "=-mixed" (= 1 1.0) t)
(chk "<-mixed" (< 1 2.5 3) t)
(chk "=-big-ratio" (= (/ (expt 2 100) (expt 2 99)) 2) t)
(chk "cmp-complex-eq" (= #c(2 3) #c(2 3)) t)
(chk "equal-float" (equal 3.0 3.0) t)

;; ── apply: stack path + heap path ───────────────────────────────────
(chk "apply-small" (apply #'+ 1 2 '(3 4)) 10)
(chk "apply-empty-tail" (apply #'+ 1 2 '()) 3)
(chk "apply-no-prefix" (apply #'list '(1 2 3)) '(1 2 3))
(chk "apply-17-args"
     (apply #'+ 1 2 3 4 5 6 7 8 '(9 10 11 12 13 14 15 16 17))
     153)
(chk "apply-20-args" (apply #'list 1 2 3 '(4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20))
     '(1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20))

;; ── alloc + GC under churn (safepoint early-out + inline root stack) ─
(chk "cons-churn-gc"
     (let ((keep nil))
       (dotimes (i 200)
         (let ((l nil))
           (dotimes (j 5000) (setq l (cons j l)))
           (when (= (mod i 50) 0) (setq keep (cons (length l) keep)))))
       keep)
     '(5000 5000 5000 5000))
(chk "sort-after-churn"
     (sort (list 5 3 9 1 7 2 8 4 6) #'<)
     '(1 2 3 4 5 6 7 8 9))

;; ── unboxed-float arithmetic (Sprint 1) ─────────────────────────────
;; Native f64 fast path for float-typed expressions; the value stays
;; unboxed across the expression and boxes once at the escape. See
;; docs/performance-unbox-float.md.
(chk "float-mul"          (* 2.0 3.0) 6.0)
(chk "float-add"          (+ 1.5 2.5) 4.0)
(chk "float-sub"          (- 5.0 1.5) 3.5)
(chk "float-chain"        (+ (* 2.0 3.0) 1.0) 7.0)
(chk "float-nested"       (* (+ 1.0 1.0) (- 4.0 1.0)) 6.0)
;; int→float contagion with a fixnum constant operand
(chk "float-contagion+"   (+ 2.0 3) 5.0)
(chk "float-contagion*"   (* 2 1.5) 3.0)
(chk "float-contagion-"   (- 10 0.5) 9.5)
;; comparisons — native ordered fcmp
(chk "float->"            (> 3.0 2.0) t)
(chk "float-<"            (< 1.0 2.0) t)
(chk "float->="           (>= 4.0 4.0) t)
(chk "float-<="           (<= 4.0 4.0) t)
(chk "float-="            (= 1.0 1.0) t)
(chk "float->-false"      (> 2.0 3.0) nil)
(chk "float-cmp-mixed"    (< 1 2.0) t)
(chk "float-=-mixed"      (= 2 2.0) t)
;; -0.0 = 0.0 under IEEE ordered-equal
(chk "float-negzero-="    (= 0.0 -0.0) t)
;; a float that escapes to a Word (boxed in a list) then re-enters
;; arithmetic via the generic/contagion path
(chk "float-box-roundtrip" (+ (car (list 1.5)) 0.5) 2.0)
(chk "float-eql"          (eql 2.0 2.0) t)
(chk "eq-int-vs-float"    (eq 3 3.0) nil)
;; integers MUST stay integers — no accidental float contagion
(chk "int-add-stays-int"  (+ 1 2) 3)
(chk "int-mul-stays-int"  (* 3 4) 12)
(chk "int-cmp-stays"      (< 1 2) t)
(chk "int-=-stays"        (= 3 3) t)
(chk "bignum-still-ok"    (* 1000000000000 1000000000000) 1000000000000000000000000)

;; ── declared double-float parameters (Sprint 3) ─────────────────────
;; Params declared (double-float ...) are read unboxed; arithmetic on
;; them is native f64 with one box at the escape.
(defun g-dist2 (x y) (declare (double-float x y)) (+ (* x x) (* y y)))
(defun g-pid   (x)   (declare (double-float x)) x)
(defun g-lerp  (a b) (declare (double-float a b)) (+ a (* 0.5 (- b a))))
(defun g-poly  (x)   (declare (double-float x)) (+ (* x (* x x)) (* 2.0 (* x x)) (* 3.0 x) 4.0))
(defun g-cmpf  (x y) (declare (double-float x y)) (if (> x y) :x :y))
;; (type double-float ...) long form, and an undeclared param mixing in.
(defun g-scale (x k) (declare (type double-float x)) (* x k))
(chk "fparam-dist2"   (g-dist2 3.0 4.0) 25.0)
(chk "fparam-pid"     (g-pid 3.5) 3.5)
(chk "fparam-lerp"    (g-lerp 0.0 10.0) 5.0)
(chk "fparam-poly"    (g-poly 2.0) 26.0)
(chk "fparam-cmp"     (g-cmpf 3.0 2.0) :x)
(chk "fparam-cmp2"    (g-cmpf 1.0 2.0) :y)
(chk "fparam-type-lf" (g-scale 2.0 3.0) 6.0)
;; an integer arg to an undeclared mate still contaminates correctly
(chk "fparam-mix-int" (g-scale 2.0 3) 6.0)

;; ── declared double-float locals (Sprint 2) ─────────────────────────
;; A (declare (double-float ..)) let-local is stored unboxed in an f64
;; stack slot. Mutable locals captured by a loop-lambda safely fall back
;; to the boxed representation — still correct, just not unboxed.
(defun g-floc ()  (let ((x 2.0)) (declare (double-float x)) (* x x)))
(defun g-floc2 () (let ((a 2.0)) (declare (double-float a))
                    (let ((b (* a a))) (declare (double-float b)) (+ a b))))
(defun g-floc-word () (let ((x 1.5)) (declare (double-float x)) (list x)))
(defun g-floc-int-init () (let ((x 3)) (declare (double-float x)) (* x 2.0)))
(defun g-facc (n) (let ((acc 0.0) (i 0)) (declare (double-float acc))
                    (loop (when (>= i n) (return acc))
                          (setq acc (+ acc 1.0)) (setq i (+ i 1)))))
(chk "flocal-immut"     (g-floc) 4.0)
(chk "flocal-nested"    (g-floc2) 6.0)
(chk "flocal-word-ctx"  (g-floc-word) '(1.5))
(chk "flocal-int-init"  (g-floc-int-init) 6.0)
(chk "flocal-loop-acc"  (g-facc 100) 100.0)

;; ── fast-loop: inline loop, unboxed loop carries (the real lever) ───
;; (fast-loop TEST RESULT BODY...) — no capturing lambda, so declared
;; double-float loop variables stay in f64 stack slots across iterations.
(defun g-countup (n) (let ((i 0)) (fast-loop (>= i n) i (setq i (+ i 1)))))
(defun g-fl-acc (n) (let ((acc 0.0) (i 0)) (declare (double-float acc))
                      (fast-loop (>= i n) acc (setq acc (+ acc 2.0)) (setq i (+ i 1)))))
(defun g-fl-empty () (let ((i 42)) (fast-loop t i)))   ; test true on entry
(defun g-mbi (cx cy max)
  (declare (double-float cx cy))
  (let ((zx 0.0) (zy 0.0) (n 0))
    (declare (double-float zx zy))
    (fast-loop (or (>= n max) (> (+ (* zx zx) (* zy zy)) 4.0)) n
      (let ((nx (+ (- (* zx zx) (* zy zy)) cx)) (ny (+ (* 2.0 (* zx zy)) cy)))
        (declare (double-float nx ny)) (setq zx nx) (setq zy ny))
      (setq n (+ n 1)))))
(chk "floop-countup"   (g-countup 100000) 100000)
(chk "floop-facc"      (g-fl-acc 50) 100.0)
(chk "floop-empty"     (g-fl-empty) 42)
(chk "floop-mbi-inset" (g-mbi 0.0 0.0 100) 100)
(chk "floop-mbi-esc"   (g-mbi 2.0 2.0 100) 1)
(chk "floop-mbi-mid"   (g-mbi -0.5 0.0 100) 100)
;; nested fast-loop + GC churn alongside (cons-allocating loop body)
(defun g-fl-nested ()
  (let ((sum 0) (py 0))
    (fast-loop (>= py 50) sum
      (let ((px 0)) (fast-loop (>= px 50) nil (setq sum (+ sum 1)) (setq px (+ px 1))))
      (setq py (+ py 1)))))
(chk "floop-nested"    (g-fl-nested) 2500)

(format t "~%GAUNTLET ~A~%" (if (= *fails* 0) "ALL-PASS" (format nil "~A FAILS" *fails*)))
