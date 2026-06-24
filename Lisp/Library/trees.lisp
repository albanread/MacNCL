;;;; Lisp/Library/trees.lisp — tree-walking primitives.
;;;;
;;;; Ported from Roger Corman's Sys/trees.lisp, Sys/lists.lisp, and
;;;; Sys/misc.lisp. The CL spec calls these "trees" — they treat
;;;; a cons-cell tree as the unit of recursion, descending into both
;;;; car and cdr at every node.
;;;;
;;;; Surface:
;;;;
;;;;   subst        copy with substitutions, :test #'eql by default
;;;;   subst-if     copy with substitutions, predicate-driven
;;;;   subst-if-not    "                     " inverted predicate
;;;;   nsubst       destructive variant of subst (rplacs in place)
;;;;   nsubst-if          "
;;;;   nsubst-if-not      "
;;;;   sublis       substitute via an alist of (old . new) pairs
;;;;   tree-equal   deep equality over cons trees, custom :test
;;;;   copy-tree    recursively-fresh cons cells, atoms shared
;;;;   revappend    (revappend list tail) ≡ (append (reverse list) tail)
;;;;
;;;; All accept the standard CL `:test` / `:test-not` / `:key`
;;;; keyword vocabulary. Internally we normalise `:test-not` to a
;;;; complemented `:test` via #'complement (already in core.lisp),
;;;; the same idiom Corman uses.
;;;;
;;;; Performance note: the non-destructive variants follow Corman's
;;;; eql-check on the recursive results — if neither half of the
;;;; reconstructed cons differs from the original, we reuse the
;;;; original cons rather than allocate a fresh one. For
;;;; substitution patterns where most of the tree is untouched this
;;;; is the difference between O(matches) and O(tree-size)
;;;; allocations.

;; ── subst, subst-if, subst-if-not ───────────────────────────────────────

(defun subst (new old tree &key key (test #'eql) test-not)
  "Return a copy of TREE in which every node EQL to OLD (or, with
   :KEY supplied, every node whose key matches OLD under TEST) has
   been replaced with NEW. Recurses into both car and cdr."
  (when test-not (setq test (complement test-not)))
  (cond
    ((funcall test old (if key (funcall key tree) tree)) new)
    ((consp tree)
     ;; Reuse the original cons when both halves are eql to the
     ;; originals — saves an allocation when nothing inside this
     ;; node was substituted.
     (let ((a (subst new old (car tree) :key key :test test))
           (d (subst new old (cdr tree) :key key :test test)))
       (if (and (eql a (car tree)) (eql d (cdr tree)))
           tree
           (cons a d))))
    (t tree)))

(defun subst-if (new predicate tree &key key)
  "Like SUBST but match by an arbitrary predicate of one argument."
  (cond
    ((funcall predicate (if key (funcall key tree) tree)) new)
    ((consp tree)
     (let ((a (subst-if new predicate (car tree) :key key))
           (d (subst-if new predicate (cdr tree) :key key)))
       (if (and (eql a (car tree)) (eql d (cdr tree)))
           tree
           (cons a d))))
    (t tree)))

(defun subst-if-not (new predicate tree &key key)
  "Like SUBST-IF but with the predicate inverted."
  (cond
    ((not (funcall predicate (if key (funcall key tree) tree))) new)
    ((consp tree)
     (let ((a (subst-if-not new predicate (car tree) :key key))
           (d (subst-if-not new predicate (cdr tree) :key key)))
       (if (and (eql a (car tree)) (eql d (cdr tree)))
           tree
           (cons a d))))
    (t tree)))

;; ── nsubst, nsubst-if, nsubst-if-not ────────────────────────────────────
;;
;; Destructive variants. The spec leaves the recursion shape mostly
;; implementation-defined; Corman's version walks the same way as
;; SUBST and rplacs the car/cdr at every consp node. The whole
;; subtree at a matching node is replaced — meaning the *containing*
;; cons's car or cdr is rewritten via setf. Since the entry point
;; can't reach up to its own caller's cons, the outer call returns
;; the replacement; callers should assign the result back if they
;; might be substituting at the root.

(defun nsubst (new old tree &key key (test #'eql) test-not)
  "Like SUBST but destructively modifies TREE rather than copying.
   Returns the (possibly new) root."
  (when test-not (setq test (complement test-not)))
  (cond
    ((funcall test old (if key (funcall key tree) tree)) new)
    ((consp tree)
     (setf (car tree) (nsubst new old (car tree) :key key :test test))
     (setf (cdr tree) (nsubst new old (cdr tree) :key key :test test))
     tree)
    (t tree)))

(defun nsubst-if (new predicate tree &key key)
  (cond
    ((funcall predicate (if key (funcall key tree) tree)) new)
    ((consp tree)
     (setf (car tree) (nsubst-if new predicate (car tree) :key key))
     (setf (cdr tree) (nsubst-if new predicate (cdr tree) :key key))
     tree)
    (t tree)))

(defun nsubst-if-not (new predicate tree &key key)
  (cond
    ((not (funcall predicate (if key (funcall key tree) tree))) new)
    ((consp tree)
     (setf (car tree) (nsubst-if-not new predicate (car tree) :key key))
     (setf (cdr tree) (nsubst-if-not new predicate (cdr tree) :key key))
     tree)
    (t tree)))

;; ── sublis ──────────────────────────────────────────────────────────────
;;
;; sublis is alist-driven substitution: each cons (OLD . NEW) in
;; ALIST means "everywhere a node equals OLD, write NEW." It walks
;; the same tree shape as SUBST but does one assoc-lookup per node
;; instead of testing against a single OLD value.
;;
;; Our local assoc doesn't (yet) take :test-not, so we forward only
;; :test and :key into it. :test-not at the sublis level is folded
;; into :test via complement before we ever reach assoc.

(defun sublis (alist tree &key key (test #'eql) test-not)
  "Substitute via an alist of (old . new) pairs. Walks TREE the
   same way SUBST does; at each node, looks up the node (or its
   key) in ALIST under TEST. Hit → return the cdr of the matching
   pair; miss → recurse."
  ;; Fast path: eql test, no key, no test-not. Recurse without re-marshalling
  ;; &key on every node, and let the (now fast) eql/identity assoc do the
  ;; lookup. Structure sharing preserved exactly as the general body.
  (if (and (eq test #'eql) (null key) (null test-not))
      (let ((match (assoc tree alist)))
        (cond
          (match (cdr match))
          ((consp tree)
           (let ((a (sublis alist (car tree)))
                 (d (sublis alist (cdr tree))))
             (if (and (eql a (car tree)) (eql d (cdr tree)))
                 tree
                 (cons a d))))
          (t tree)))
      (progn
        (when test-not (setq test (complement test-not)))
        (let ((match (if key
                         (assoc (funcall key tree) alist :test test)
                         (assoc tree alist :test test))))
          (cond
            (match (cdr match))
            ((consp tree)
             (let ((a (sublis alist (car tree) :key key :test test))
                   (d (sublis alist (cdr tree) :key key :test test)))
               (if (and (eql a (car tree)) (eql d (cdr tree)))
                   tree
                   (cons a d))))
            (t tree))))))

;; ── tree-equal ──────────────────────────────────────────────────────────

(defun tree-equal (tree-1 tree-2 &key (test #'eql) test-not)
  "True iff TREE-1 and TREE-2 have the same cons-cell shape AND
   every leaf pair satisfies TEST. Atoms compare under TEST;
   non-matching shapes (one cons, one atom) return NIL."
  (when test-not (setq test (complement test-not)))
  (cond
    ((and (atom tree-1) (atom tree-2))
     (if (funcall test tree-1 tree-2) t nil))
    ((and (consp tree-1) (consp tree-2)
          (tree-equal (car tree-1) (car tree-2) :test test)
          (tree-equal (cdr tree-1) (cdr tree-2) :test test))
     t)
    (t nil)))

;; ── copy-tree ───────────────────────────────────────────────────────────

(defun copy-tree (tree)
  "Recursively copy every cons in TREE. Atoms (numbers, symbols,
   strings, etc.) are shared with the source — we copy cons
   structure only. The result is `equal` to TREE but shares no
   conses with it; mutating the copy never visibly changes the
   original."
  (if (consp tree)
      (cons (copy-tree (car tree)) (copy-tree (cdr tree)))
      tree))

;; ── revappend ───────────────────────────────────────────────────────────
;;
;; CL's revappend is (append (reverse list) tail) but in O(n) with a
;; single pass and no intermediate. core.lisp has %revappend as the
;; internal building block of REVERSE; we expose it under its
;; standard name.

(defun revappend (list tail)
  "(revappend list tail) ≡ (append (reverse list) tail), but in
   one pass with no intermediate cons. Useful for accumulating
   in reverse and joining the result with an existing list in
   one go."
  (let ((result tail))
    (dolist (x list)
      (push x result))
    result))

(provide 'trees)
nil
