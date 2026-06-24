;;;; test-symbols.lisp — regression tests for symbols.lisp
;;;; Covers: property lists, prog1/prog2, destructuring-bind, and advice.

(defparameter *pass* 0)
(defparameter *fail* 0)

(defun check (label got expected)
  (if (equal got expected)
      (progn (setq *pass* (+ *pass* 1))
             (format t "PASS  ~A~%" label))
      (progn (setq *fail* (+ *fail* 1))
             (format t "FAIL  ~A: got ~S, expected ~S~%" label got expected))))

;; ── symbol-plist / get / setf get / remprop ─────────────────────────────────

(let ((s (gensym "T")))
  (check "plist initial nil"   (symbol-plist s)          nil)
  (check "get absent default"  (get s 'foo 99)            99)
  (check "get absent nil"      (get s 'foo)               nil)

  (setf (get s 'color) 'red)
  (check "setf get / get"      (get s 'color)             'red)

  (setf (get s 'size)  42)
  (check "second prop"         (get s 'size)              42)
  (check "first still there"   (get s 'color)             'red)

  (setf (get s 'color) 'blue)
  (check "overwrite prop"      (get s 'color)             'blue)

  (check "remprop found"       (remprop s 'color)         t)
  (check "get after remprop"   (get s 'color)             nil)
  (check "other prop survives" (get s 'size)              42)
  (check "remprop absent"      (remprop s 'color)         nil)

  (setf (symbol-plist s) nil)
  (check "clear plist"         (symbol-plist s)           nil))

;; putprop
(let ((s (gensym "P")))
  (putprop s 'alpha 'name)
  (check "putprop / get"       (get s 'name)              'alpha))

;; ── prog1 / prog2 ────────────────────────────────────────────────────────────

(check "prog1 returns first"
       (let ((x 0))
         (prog1 (progn (setq x (+ x 1)) x)
                (setq x (+ x 1))
                (setq x (+ x 1))))
       1)

(check "prog2 returns second"
       (let ((x 0))
         (prog2 (setq x (+ x 10))
                (setq x (+ x 1))
                (setq x (+ x 100))))
       11)

;; ── destructuring-bind ───────────────────────────────────────────────────────

;; required only
(destructuring-bind (a b c) '(1 2 3)
  (check "dbb required"        (list a b c)               '(1 2 3)))

;; &rest
(destructuring-bind (a &rest r) '(1 2 3 4)
  (check "dbb &rest"           (list a r)                 '(1 (2 3 4))))

;; dotted rest
(destructuring-bind (a . r) '(1 2 3)
  (check "dbb dotted"          (list a r)                 '(1 (2 3))))

;; &optional with default
(destructuring-bind (a &optional (b 99) c) '(1 2)
  (check "dbb &optional"       (list a b c)               '(1 2 nil)))

(destructuring-bind (a &optional (b 99)) '(1)
  (check "dbb &optional default" (list a b)               '(1 99)))

;; nested sub-pattern
(destructuring-bind ((x y) z) '((10 20) 30)
  (check "dbb nested"          (list x y z)               '(10 20 30)))

;; nested + &rest
(destructuring-bind ((x &rest rest) z) '((1 2 3) 4)
  (check "dbb nested &rest"    (list x rest z)            '(1 (2 3) 4)))

;; &key
(destructuring-bind (a &key x (y 0)) '(1 :x 10 :y 20)
  (check "dbb &key"            (list a x y)               '(1 10 20)))

(destructuring-bind (a &key (z 77)) '(1)
  (check "dbb &key default"    (list a z)                 '(1 77)))

;; ── advice ───────────────────────────────────────────────────────────────────

(defun %double (n) (* n 2))

;; add logging advice: return triple instead
(advise %double (n)
  (* n 3))

(check "advice fires"          (%double 5)                15)
;; symbol-function-advised-p returns the saved function (truthy) when advised
(check "advised-p t"           (if (symbol-function-advised-p '%double) t nil) t)

;; unadvise restores
(unadvise %double)
(check "after unadvise"        (%double 5)                10)
(check "advised-p nil"         (symbol-function-advised-p '%double) nil)

;; call-advised-function works
(defun %add1 (n) (+ n 1))
(advise %add1 (n)
  (* (call-advised-function) 10))   ; wrap: (* (original n) 10)

(check "call-advised-fn"       (%add1 3)                  40)
(unadvise %add1)
(check "add1 restored"         (%add1 3)                   4)

;; advise with no args returns list
(check "advise no-args"        (listp (advise))           t)

;; ── Summary ──────────────────────────────────────────────────────────────────

(format t "~%Results: ~A passed, ~A failed~%" *pass* *fail*)
(if (= *fail* 0)
    (format t "ALL TESTS PASSED~%")
    (format t "SOME TESTS FAILED~%"))
nil
