;;;; test-port.lisp — regression tests for ported CL functions.

(defparameter *pass* 0)
(defparameter *fail* 0)

(defun check (label got expected)
  (if (equal got expected)
      (progn (setq *pass* (+ *pass* 1))
             (format t "PASS  ~A~%" label))
      (progn (setq *fail* (+ *fail* 1))
             (format t "FAIL  ~A: got ~S, expected ~S~%" label got expected))))

;; ── member-if / member-if-not ────────────────────────────────────────────
(check "member-if found"   (member-if #'evenp '(1 3 4 5))  '(4 5))
(check "member-if none"    (member-if #'evenp '(1 3 5))     nil)
(check "member-if-not"     (member-if-not #'oddp '(1 3 4 5)) '(4 5))

;; ── assoc-if / assoc-if-not ──────────────────────────────────────────────
(check "assoc-if"     (assoc-if #'evenp '((1 . a) (2 . b) (3 . c))) '(2 . b))
(check "assoc-if nil" (assoc-if #'evenp '((1 . a) (3 . c)))         nil)
(check "assoc-if-not" (assoc-if-not #'oddp '((1 . a) (2 . b)))      '(2 . b))

;; ── rassoc ──────────────────────────────────────────────────────────────
(check "rassoc found" (rassoc 'b '((a . 1) (b . 2) (c . 3)))  nil) ; cdr is number
(check "rassoc num"   (rassoc 2  '((a . 1) (b . 2) (c . 3)))  '(b . 2))
(check "rassoc-if"    (rassoc-if #'evenp '((a . 1) (b . 2)))  '(b . 2))
(check "rassoc-if-not" (rassoc-if-not #'oddp '((a . 1) (b . 2))) '(b . 2))

;; ── copy-alist ───────────────────────────────────────────────────────────
(let* ((al '((a . 1) (b . 2)))
       (cp (copy-alist al)))
  (check "copy-alist equal"   (equal cp al) t)
  (check "copy-alist not eq"  (eq cp al)    nil)
  (check "copy-alist cell ≠"  (eq (car cp) (car al)) nil))

;; ── adjoin / pushnew ────────────────────────────────────────────────────
(check "adjoin new"    (adjoin 4 '(1 2 3))    '(4 1 2 3))
(check "adjoin dup"    (adjoin 2 '(1 2 3))    '(1 2 3))
(check "adjoin key"    (adjoin '(4) '((1)(2)(3)) :key #'car) '((4)(1)(2)(3)))
(let ((lst '(1 2 3)))
  (pushnew 4 lst)
  (check "pushnew new" lst '(4 1 2 3)))
(let ((lst '(1 2 3)))
  (pushnew 2 lst)
  (check "pushnew dup" lst '(1 2 3)))

;; ── subsetp ──────────────────────────────────────────────────────────────
(check "subsetp t"   (subsetp '(1 2) '(1 2 3))  t)
(check "subsetp nil" (subsetp '(1 4) '(1 2 3))  nil)
(check "subsetp empty" (subsetp '() '(1 2 3))   t)

;; ── set-exclusive-or ────────────────────────────────────────────────────
(check "sxor" (sort (set-exclusive-or '(1 2 3) '(2 3 4)) #'<) '(1 4))
(check "sxor eq" (set-exclusive-or '(1 2) '(1 2)) nil)

;; ── n-variants (alias non-destructive) ──────────────────────────────────
(check "nintersection" (sort (nintersection '(1 2 3) '(2 3 4)) #'<) '(2 3))
(check "nset-difference" (nset-difference '(1 2 3) '(2 3)) '(1))
(check "nunion" (sort (nunion '(1 2) '(2 3)) #'<) '(1 2 3))

;; ── find-if-not / position-if-not / count-if-not ────────────────────────
(check "find-if-not"     (find-if-not #'oddp '(1 3 4 5))   4)
(check "find-if-not nil" (find-if-not #'oddp '(1 3 5))     nil)
(check "position-if-not" (position-if-not #'oddp '(1 3 4)) 2)
(check "count-if-not"    (count-if-not #'oddp '(1 2 3 4))  2)

;; ── substitute-if / substitute-if-not ────────────────────────────────────
(check "substitute-if"     (substitute-if 0 #'evenp '(1 2 3 4)) '(1 0 3 0))
(check "substitute-if-not" (substitute-if-not 0 #'evenp '(1 2 3 4)) '(0 2 0 4))

;; ── delete-if / delete-if-not ───────────────────────────────────────────
(check "delete-if"     (delete-if #'evenp '(1 2 3 4)) '(1 3))
(check "delete-if-not" (delete-if-not #'evenp '(1 2 3 4)) '(2 4))

;; ── sort with :key ───────────────────────────────────────────────────────
(check "sort basic"   (sort '(3 1 2) #'<) '(1 2 3))
(check "sort :key"    (sort '((b 2)(a 1)(c 3)) #'< :key #'cadr) '((a 1)(b 2)(c 3)))
(let ((v (make-array 3)))
  (setf (svref v 0) 3) (setf (svref v 1) 1) (setf (svref v 2) 2)
  (sort v #'<)
  (check "sort vector" (list (svref v 0) (svref v 1) (svref v 2)) '(1 2 3)))

;; ── stable-sort ──────────────────────────────────────────────────────────
(check "stable-sort"  (stable-sort '(3 1 2) #'<) '(1 2 3))

;; ── merge ────────────────────────────────────────────────────────────────
(check "merge list"   (merge 'list '(1 3 5) '(2 4 6) #'<) '(1 2 3 4 5 6))
(check "merge empty"  (merge 'list '() '(1 2) #'<)         '(1 2))

;; ── Summary ──────────────────────────────────────────────────────────────
(format t "~%Results: ~A passed, ~A failed~%" *pass* *fail*)
(if (= *fail* 0)
    (format t "ALL TESTS PASSED~%")
    (format t "SOME TESTS FAILED~%"))
nil
