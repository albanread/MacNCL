;;;; Lisp/Library/lists.lisp — CL list-traversal extras.
;;;;
;;;; Ported from Roger Corman's Sys/lists.lisp. Provides:
;;;;
;;;;   * The mapping family — MAPL, MAPLIST, MAPCAN, MAPCON. (MAPCAR
;;;;     and MAPC are already in core.lisp; this module adds the
;;;;     four cdr-chain / concatenating variants.)
;;;;   * Alist helpers — PAIRLIS, ACONS.
;;;;   * Tail-sharing helpers — TAILP, LDIFF.
;;;;
;;;; All four mapping operators are variadic over the input lists:
;;;; (MAPLIST fn a b c) walks three lists in parallel and stops at
;;;; the shortest, exactly like the CL spec. The single-list case
;;;; takes a fast path (plain recursion); the multi-list case
;;;; routes through %cars-of / %cdrs-of (core.lisp helpers).

;; ── Map family ─────────────────────────────────────────────────────────
;;
;; The four operators differ along two axes:
;;
;;          element-arg          cdr-chain-arg
;;   collect    MAPCAR              MAPLIST
;;   nconc      MAPCAN              MAPCON
;;   discard    MAPC                MAPL
;;
;; MAPCAR / MAPC live in core.lisp; here we add the other four. All
;; four are variadic over the input lists in the same parallel-walk
;; shape MAPCAR uses.

;; ── mapl ────────────────────────────────────────────────────────

(defun %mapl-1 (fn lst)
  (cond
    ((null lst) nil)
    (t (funcall fn lst)
       (%mapl-1 fn (cdr lst)))))

(defun %mapl-n (fn lsts)
  (cond
    ((%any-null lsts) nil)
    (t (apply fn lsts)
       (%mapl-n fn (%cdrs-of lsts)))))

(defun mapl (fn list &rest more-lists)
  "Apply FN successively to (LIST MORE…), then to (CDR LIST) …,
   walking every list in parallel and stopping at the shortest.
   FN receives one tail per list. Called for effect; returns
   LIST unchanged."
  (cond
    ((null more-lists) (%mapl-1 fn list))
    (t (%mapl-n fn (cons list more-lists))))
  list)

;; ── maplist ────────────────────────────────────────────────────

(defun %maplist-1 (fn lst)
  (cond
    ((null lst) nil)
    (t (cons (funcall fn lst) (%maplist-1 fn (cdr lst))))))

(defun %maplist-n (fn lsts)
  (cond
    ((%any-null lsts) nil)
    (t (cons (apply fn lsts) (%maplist-n fn (%cdrs-of lsts))))))

(defun maplist (fn list &rest more-lists)
  "Apply FN to (LIST MORE…), (CDR LIST) …, …, collecting each
   result. Like MAPCAR but FN sees the tails at each step, not
   the heads."
  (cond
    ((null more-lists) (%maplist-1 fn list))
    (t (%maplist-n fn (cons list more-lists)))))

;; ── mapcan ────────────────────────────────────────────────────

(defun %mapcan-1 (fn lst)
  (cond
    ((null lst) nil)
    (t (let ((head (funcall fn (car lst))))
         (nconc head (%mapcan-1 fn (cdr lst)))))))

(defun %mapcan-n (fn lsts)
  (cond
    ((%any-null lsts) nil)
    (t (let ((head (apply fn (%cars-of lsts))))
         (nconc head (%mapcan-n fn (%cdrs-of lsts)))))))

(defun mapcan (fn list &rest more-lists)
  "Apply FN to parallel tuples of elements and NCONC the results
   together. FN must return a list at every step; NIL contributes
   no elements. The idiomatic 'filter and flatten' combinator."
  (cond
    ((null more-lists) (%mapcan-1 fn list))
    (t (%mapcan-n fn (cons list more-lists)))))

;; ── mapcon ────────────────────────────────────────────────────

(defun %mapcon-1 (fn lst)
  (cond
    ((null lst) nil)
    (t (let ((head (funcall fn lst)))
         (nconc head (%mapcon-1 fn (cdr lst)))))))

(defun %mapcon-n (fn lsts)
  (cond
    ((%any-null lsts) nil)
    (t (let ((head (apply fn lsts)))
         (nconc head (%mapcon-n fn (%cdrs-of lsts)))))))

(defun mapcon (fn list &rest more-lists)
  "Apply FN to the cdr-chains of LIST and MORE-LISTS in parallel
   and NCONC the results. Each FN call receives a tail per list."
  (cond
    ((null more-lists) (%mapcon-1 fn list))
    (t (%mapcon-n fn (cons list more-lists)))))

;; ── Alist helpers ──────────────────────────────────────────────────────

(defun acons (key datum alist)
  "Prepend (KEY . DATUM) to ALIST and return the new list. The
   one-line definition is the CL spec verbatim."
  (cons (cons key datum) alist))

(defun %pairlis-recur (keys vals tail)
  (cond
    ((or (null keys) (null vals)) tail)
    (t (cons (cons (car keys) (car vals))
             (%pairlis-recur (cdr keys) (cdr vals) tail)))))

(defun pairlis (keys vals &optional tail)
  "Zip KEYS and VALS into an alist of (key . value) cells. TAIL, if
   supplied, is the cdr of the last cell — i.e. the new pairs are
   prepended to it. The spec leaves the order of the resulting
   pairs unspecified (Corman builds them via MAPCAR + NCONC so the
   first key/value ends up at the head); we preserve that order."
  (%pairlis-recur keys vals tail))

;; ── Tail-sharing tests ─────────────────────────────────────────────────

(defun tailp (object lst)
  "T iff OBJECT is one of the cons cells reachable by repeated
   CDR from LST (including LST itself). Comparison is EQL on the
   cons cells. Useful for asking whether one list is a structural
   suffix of another."
  (cond
    ((eq object lst) t)
    ((atom lst) nil)
    (t (tailp object (cdr lst)))))

(defun %ldiff-recur (lst object acc)
  ;; Collect cars onto ACC in reverse order until we EQ-hit OBJECT
  ;; or run off the end (atom-cdr). Reverse ACC for the result;
  ;; if we ran off the end with a non-nil dotted tail, append it.
  (cond
    ((eq object lst)
     (nreverse acc))
    ((atom lst)
     ;; The list ended at an atom that isn't OBJECT. Per CL spec
     ;; LDIFF returns a *copy* — the dotted tail is appended only
     ;; if it isn't nil.
     (if (null lst)
         (nreverse acc)
         (let ((front (nreverse acc)))
           ;; Splice the dotted tail onto the end via setf on the
           ;; last cons. If FRONT is nil we just return the tail.
           (if (null front)
               lst
               (progn (setf (cdr (last front)) lst) front)))))
    (t (%ldiff-recur (cdr lst) object (cons (car lst) acc)))))

(defun ldiff (lst object)
  "Return a fresh list of the elements of LST that precede OBJECT
   (compared as cons-cell identity). If OBJECT is not a tail of
   LST, return a copy of LST. Useful for cutting a prefix off a
   list while preserving the suffix's identity."
  (%ldiff-recur lst object nil))

;; ── Member predicates ─────────────────────────────────────────────────────

(defun member-if (pred list &key (key #'identity))
  "Return the first tail of LIST whose car satisfies PRED (applied
   after KEY). Returns NIL if no element matches."
  (cond
    ((null list) nil)
    ((funcall pred (funcall key (car list))) list)
    (t (member-if pred (cdr list) :key key))))

(defun member-if-not (pred list &key (key #'identity))
  "Return the first tail of LIST whose car does NOT satisfy PRED."
  (member-if (complement pred) list :key key))

;; ── Alist search ──────────────────────────────────────────────────────────

(defun assoc-if (pred alist &key (key #'identity))
  "Return the first pair in ALIST whose car satisfies PRED (applied
   after KEY). Returns NIL if none match."
  (dolist (pair alist nil)
    (when (and (consp pair)
               (funcall pred (funcall key (car pair))))
      (return pair))))

(defun assoc-if-not (pred alist &key (key #'identity))
  "Return the first pair in ALIST whose car does NOT satisfy PRED."
  (assoc-if (complement pred) alist :key key))

(defun rassoc (item alist &key (key #'identity) (test #'eql))
  "Return the first pair in ALIST whose cdr matches ITEM under TEST
   (KEY applied to the cdr before testing)."
  (dolist (pair alist nil)
    (when (and (consp pair)
               (funcall test item (funcall key (cdr pair))))
      (return pair))))

(defun rassoc-if (pred alist &key (key #'identity))
  "Return the first pair in ALIST whose cdr satisfies PRED."
  (dolist (pair alist nil)
    (when (and (consp pair)
               (funcall pred (funcall key (cdr pair))))
      (return pair))))

(defun rassoc-if-not (pred alist &key (key #'identity))
  "Return the first pair in ALIST whose cdr does NOT satisfy PRED."
  (rassoc-if (complement pred) alist :key key))

(defun copy-alist (alist)
  "Return a fresh alist whose pairs are fresh cons cells sharing the
   original keys and values. Non-cons elements are copied as-is."
  (mapcar (lambda (pair)
            (if (consp pair)
                (cons (car pair) (cdr pair))
                pair))
          alist))

;; ── Adjoin / pushnew ─────────────────────────────────────────────────────

(defun adjoin (item list &key (key #'identity) (test #'eql))
  "Return LIST unchanged if (funcall test (funcall key item)
   (funcall key element)) is true for some element. Otherwise
   return (cons item list)."
  ;; Fast path: default (eql test, identity key) reuses member's fast path,
  ;; dropping the (funcall key item) + keyword marshalling of the general case.
  (if (and (eq test #'eql) (eq key #'identity))
      (if (member item list) list (cons item list))
      (if (member (funcall key item) list :key key :test test)
          list
          (cons item list))))

(defmacro pushnew (item place &rest keyword-args)
  "Push ITEM onto PLACE (a generalized variable holding a list) only
   if it is not already a member according to ADJOIN's :key and
   :test. Returns the updated list."
  `(setf ,place (adjoin ,item ,place ,@keyword-args)))

;; ── Set operations ────────────────────────────────────────────────────────
;;
;; intersection, union, set-difference are defined in core.lisp.
;; We add the remaining CL set functions here.

(defun subsetp (list-1 list-2 &key (key #'identity) (test #'eql))
  "Return T if every element of LIST-1 appears in LIST-2 (under
   TEST applied after KEY)."
  (every (lambda (x)
           (member (funcall key x) list-2 :key key :test test))
         list-1))

(defun set-exclusive-or (list-1 list-2 &key (key #'identity) (test #'eql))
  "Elements in LIST-1 but not LIST-2 plus elements in LIST-2 but not
   LIST-1 — the symmetric difference. Order is unspecified."
  (append (set-difference list-1 list-2 :key key :test test)
          (set-difference list-2 list-1 :key key :test test)))

(defun nset-exclusive-or (list-1 list-2 &key (key #'identity) (test #'eql))
  "Potentially destructive SET-EXCLUSIVE-OR. Currently delegates to
   the non-destructive version."
  (nconc (set-difference list-1 list-2 :key key :test test)
         (set-difference list-2 list-1 :key key :test test)))

(defun nintersection (list-1 list-2 &key (key #'identity) (test #'eql))
  "Potentially destructive INTERSECTION. Currently delegates to
   the non-destructive version."
  (intersection list-1 list-2 :key key :test test))

(defun nset-difference (list-1 list-2 &key (key #'identity) (test #'eql))
  "Potentially destructive SET-DIFFERENCE. Currently delegates to
   the non-destructive version."
  (set-difference list-1 list-2 :key key :test test))

(defun nunion (list-1 list-2 &key (key #'identity) (test #'eql))
  "Potentially destructive UNION. Currently delegates to the
   non-destructive version."
  (union list-1 list-2 :key key :test test))

;; ── nth-value ────────────────────────────────────────────────────────
;;
;; (nth-value N FORM) — return the Nth (zero-indexed) of the multiple
;; values returned by FORM. Both N and FORM are evaluated exactly once.
;; Standard in ANSI CL §5.3.3. Commonly used to pick out one value
;; from floor, decode-float, gethash, etc.
;;
;;   (nth-value 0 (floor 17 5))  => 3
;;   (nth-value 1 (floor 17 5))  => 2

(defmacro nth-value (n form)
  "Return the Nth multiple value (zero-indexed) of FORM.
   Both N and FORM are evaluated exactly once."
  (let ((n-g (gensym "NV-N"))
        (r-g (gensym "NV-R")))
    `(let* ((,n-g ,n)
            (,r-g (multiple-value-list ,form)))
       (nth ,n-g ,r-g))))

(provide 'lists)
nil
