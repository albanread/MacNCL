;;;; Lisp/Library/sequences.lisp — generic CL sequence operations.
;;;;
;;;; The CL spec defines position / find / count / remove / search
;;;; / mismatch / etc. as polymorphic over lists, vectors, and
;;;; strings. core.lisp shipped list-only versions of the common
;;;; ones. This file:
;;;;
;;;;   * adds the generic accessor (elt seq i)
;;;;   * adds copy-seq, fill, replace, concatenate
;;;;   * adds map (multi-sequence), reduce, search, mismatch
;;;;   * adds substitute / delete
;;;;   * adds vector support to subseq (was list + string only)
;;;;   * REDEFINES position / find / count / remove / find-if /
;;;;     position-if / remove-if / remove-if-not so they dispatch
;;;;     by sequence type
;;;;
;;;; Strategy: the redefined functions check (listp seq) first
;;;; and call the original list-only path (preserving performance
;;;; for the common case), else use the generic elt/length walker.
;;;; The generic walker is a few lines slower per call but
;;;; correct for every type.

;; ── Generic accessors ─────────────────────────────────────────────────────

(defun elt (seq i)
  "Return the I-th element of SEQ. Works on lists, vectors, and
   strings. Bounds-error is the underlying primitive's choice
   (currently: segfault for vectors out of range — bounds-check
   lands when arrays grow up)."
  (cond
    ((listp seq)   (nth i seq))
    ((stringp seq) (char seq i))
    ((vectorp seq) (svref seq i))
    (t (error "elt: not a sequence: ~A" seq))))

(defun %set-elt (seq i val)
  "Mutate seq[i]. Mirrors (setf (elt ...)). Returns val."
  (cond
    ((listp seq)
     (setf (car (nthcdr i seq)) val)
     val)
    ((stringp seq)
     (setf (char seq i) val)
     val)
    ((vectorp seq)
     (setf (svref seq i) val)
     val)
    (t (error "(setf elt): not a sequence: ~A" seq))))

;; CL spec: (setf (elt seq i) v) — handled via the %setf-elt
;; generic-setf-fallback name.
(defun %setf-elt (val seq i)
  (%set-elt seq i val))

;; ── do-seq: iteration helper ─────────────────────────────────────────────
;;
;; Calls FN with each element of SEQ in order, regardless of
;; sequence type. Used internally by everything below.

(defun %seq-foreach (fn seq)
  (cond
    ((listp seq)
     (let ((p seq))
       (loop
         (cond
           ((null p) (return nil))
           (t (funcall fn (car p))
              (setq p (cdr p)))))))
    (t
     (let ((i 0) (n (length seq)))
       (loop
         (cond
           ((>= i n) (return nil))
           (t (funcall fn (elt seq i))
              (setq i (+ i 1)))))))))

;; ── copy-seq / subseq / concatenate ──────────────────────────────────────

(defun copy-seq (seq)
  "Shallow copy of SEQ, preserving type."
  (cond
    ((listp seq) (copy-list seq))
    ((stringp seq) (substring seq 0 (length seq)))
    ((vectorp seq)
     (let* ((n (length seq))
            (v (make-array n)))
       (let ((i 0))
         (loop
           (cond
             ((>= i n) (return v))
             (t (setf (svref v i) (svref seq i))
                (setq i (+ i 1))))))))
    (t (error "copy-seq: not a sequence: ~A" seq))))

;; Replace the list+string subseq from core.lisp with a vector-
;; aware version. The defparameter capture-then-redefine pattern
;; isn't needed since the old version had a (t error) fall-through
;; for vectors; we're just adding that branch.
(defun subseq (seq start &optional end)
  "Substring/sublist/subvector from START (inclusive) to END
   (exclusive, defaults to (length seq))."
  (let ((real-end (cond ((null end) (length seq)) (t end))))
    (cond
      ((stringp seq) (substring seq start real-end))
      ((listp seq)
       (let ((lst (nthcdr start seq))
             (n (- real-end start)))
         (cond
           ((<= n 0) nil)
           (t (%list-take lst n)))))
      ((vectorp seq)
       (let* ((n (- real-end start))
              (v (make-array n)))
         (let ((i 0))
           (loop
             (cond
               ((>= i n) (return v))
               (t (setf (svref v i) (svref seq (+ start i)))
                  (setq i (+ i 1))))))))
      (t (error "subseq: not a sequence: ~A" seq)))))

(defun %list-take (lst n)
  (cond
    ((or (null lst) (<= n 0)) nil)
    (t (cons (car lst) (%list-take (cdr lst) (- n 1))))))

(defun concatenate (result-type &rest seqs)
  "Concatenate the given sequences into a fresh sequence of
   RESULT-TYPE (one of 'list, 'string, 'vector)."
  (cond
    ((eq result-type 'list)
     (let ((out nil))
       (dolist (s seqs)
         (%seq-foreach (lambda (x) (setq out (cons x out))) s))
       (reverse out)))
    ((eq result-type 'string)
     ;; Two-pass: size the result once (sum of input lengths), then fill
     ;; by index. The old `string-append-char` accumulator rebuilt the
     ;; whole string every char — O(n^2). make-string + (setf char) is O(n).
     (let ((total 0))
       (dolist (s seqs) (setq total (+ total (length s))))
       (let ((out (make-string total))
             (i 0))
         (dolist (s seqs)
           (%seq-foreach (lambda (c)
                           (setf (char out i) c)
                           (setq i (+ i 1)))
                         s))
         out)))
    ((eq result-type 'vector)
     (let ((total 0))
       (dolist (s seqs) (setq total (+ total (length s))))
       (let ((v (make-array total))
             (i 0))
         (dolist (s seqs)
           (%seq-foreach (lambda (x)
                           (setf (svref v i) x)
                           (setq i (+ i 1)))
                         s))
         v)))
    (t (error "concatenate: unsupported result-type ~A" result-type))))

;; ── Iteration / aggregation ──────────────────────────────────────────────

(defun map (result-type fn &rest seqs)
  "Apply FN to each parallel tuple of elements across SEQS.
   RESULT-TYPE selects what to build:
     'list     → fresh list of results
     'string   → fresh string from char results
     'vector   → fresh vector
     NIL       → discard results, return NIL (side-effect form)

   Stops at the shortest input."
  (let ((out
          (if (and seqs (every (lambda (s) (listp s)) seqs))
              ;; All-lists fast path: walk parallel cons cursors (O(total
              ;; elements)). The general path's `(elt s i)` is O(i) on a
              ;; list, making map O(m*n^2); here each step is car/cdr.
              ;; Stops at the shortest input (some cursor reaches nil).
              (let ((cursors seqs)
                    (acc nil))
                (loop
                  (cond
                    ((some (lambda (c) (null c)) cursors) (return acc))
                    (t
                     (let ((r (apply fn (mapcar (lambda (c) (car c)) cursors))))
                       (cond ((null result-type) nil)
                             (t (setq acc (cons r acc)))))
                     (setq cursors (mapcar (lambda (c) (cdr c)) cursors))))))
              ;; Vector / mixed path: elt is O(1) on vectors/strings.
              (let* ((lengths (mapcar (lambda (s) (length s)) seqs))
                     (n (cond ((null lengths) 0)
                              (t (%list-min lengths))))
                     (acc nil)
                     (i 0))
                (loop
                  (cond
                    ((>= i n) (return acc))
                    (t
                     (let ((r (apply fn (mapcar (lambda (s) (elt s i)) seqs))))
                       (cond ((null result-type) nil)
                             (t (setq acc (cons r acc)))))
                     (setq i (+ i 1)))))))))
    (cond
      ((null result-type) nil)
      ((eq result-type 'list)   (reverse out))
      ((eq result-type 'vector)
       (let* ((rev (reverse out))
              (v (make-array (length rev)))
              (i 0))
         (dolist (x rev)
           (setf (svref v i) x)
           (setq i (+ i 1)))
         v))
      ((eq result-type 'string)
       ;; O(n) two-pass build (was string-append-char → O(n^2)).
       (let* ((rev (reverse out))
              (s (make-string (length rev)))
              (i 0))
         (dolist (c rev)
           (setf (char s i) c)
           (setq i (+ i 1)))
         s))
      (t (error "map: unsupported result-type ~A" result-type)))))

(defun %list-min (lst)
  (cond
    ((null (cdr lst)) (car lst))
    (t (let ((rest (%list-min (cdr lst))))
         (cond ((< (car lst) rest) (car lst)) (t rest))))))

;; Sentinel for "no :initial-value supplied". Our compiler doesn't
;; yet support the &key (var default supplied-p) shape, so we use
;; a unique cons that the user can't pass.
(defparameter %reduce-no-init (cons 'no 'init))

(defun reduce (fn seq &key (initial-value %reduce-no-init) (from-end nil)
                              (key #'identity) (start 0) end)
  "Combine elements of SEQ via FN. Without :initial-value, the
   first element is the seed; with it, INITIAL-VALUE is the
   seed and every element is folded in."
  (declare (ignore from-end))   ; right-fold deferred
  (cond
    ((listp seq)
     ;; List fast path: walk the spine with a moving cursor so element
     ;; access is O(1) per step instead of O(i) via `elt` — which made
     ;; the whole fold O(n^2) on lists. Honours :start (skip via nthcdr)
     ;; and :end (index limit); :key applied per element; identical seed
     ;; semantics to the vector path below.
     (let* ((cur (nthcdr start seq))
            (idx start)
            (acc (cond
                   ((eq initial-value %reduce-no-init)
                    (cond
                      ((or (null cur) (and end (>= start end)))
                       (error "reduce: empty sequence and no :initial-value"))
                      (t (let ((v (funcall key (car cur))))
                           (setq cur (cdr cur))
                           (setq idx (+ idx 1))
                           v))))
                   (t initial-value))))
       (loop
         (cond
           ((or (null cur) (and end (>= idx end))) (return acc))
           (t (setq acc (funcall fn acc (funcall key (car cur))))
              (setq cur (cdr cur))
              (setq idx (+ idx 1)))))))
    (t
     ;; Vector / string: elt is O(1).
     (let* ((real-end (cond ((null end) (length seq)) (t end)))
            (i start)
            (acc (cond
                   ((eq initial-value %reduce-no-init)
                    (cond
                      ((>= start real-end)
                       (error "reduce: empty sequence and no :initial-value"))
                      (t (let ((v (funcall key (elt seq start))))
                           (setq i (+ start 1))
                           v))))
                   (t initial-value))))
       (loop
         (cond
           ((>= i real-end) (return acc))
           (t (setq acc (funcall fn acc (funcall key (elt seq i))))
              (setq i (+ i 1)))))))))

;; ── Search / position / find / count (generic) ──────────────────────────

;; List fast-path is inline (a local LOOP) rather than a funcall back
;; to the captured original. The earlier shape captured the pre-
;; redefinition `find` in `%list-find` and the polymorphic shim then
;; funcall'd into it. That triggered a stack blow-up in heavy CLOS
;; method dispatch: the captured original's body had a recursive
;; `(find item (cdr lst) ...)` call which resolves through the
;; symbol-function cell at runtime — so each step trampolined
;; polymorphic → %list-find → polymorphic → %list-find, doubling
;; the stack growth per element. On Windows' 1 MB default thread
;; stack, a few hundred subclassp calls during a SHARED-INITIALIZE
;; :AFTER dispatch sufficed to overflow (chapter 7 of the corman
;; ANSI hyperspec-examples).
;;
;; Folding the list walk inline keeps the recursion contained inside
;; one function activation and removes the funcall + symbol-cell
;; hop on every node.

(defun find (item seq &key (test #'eql) (key #'identity))
  (cond
    ((listp seq)
     (let ((lst seq) (result nil) (found nil))
       (loop
         (cond
           (found (return result))
           ((null lst) (return nil))
           ((funcall test item (funcall key (car lst)))
            (setq result (car lst)) (setq found t))
           (t (setq lst (cdr lst)))))))
    (t
     (let ((i 0) (n (length seq)) (result nil) (found nil))
       (loop
         (cond
           (found (return result))
           ((>= i n) (return nil))
           (t (let ((el (elt seq i)))
                (cond
                  ((funcall test item (funcall key el))
                   (setq result el)
                   (setq found t))
                  (t (setq i (+ i 1))))))))))))

(defun position (item seq &key (test #'eql) (key #'identity))
  (cond
    ((listp seq)
     ;; Same rationale as FIND above: inline list walk so the heavy
     ;; CLOS dispatch path doesn't trampoline through %list-position.
     (let ((lst seq) (i 0) (result nil) (found nil))
       (loop
         (cond
           (found (return result))
           ((null lst) (return nil))
           ((funcall test item (funcall key (car lst)))
            (setq result i) (setq found t))
           (t (setq lst (cdr lst)) (setq i (+ i 1)))))))
    (t
     (let ((i 0) (n (length seq)) (result nil) (found nil))
       (loop
         (cond
           (found (return result))
           ((>= i n) (return nil))
           (t (cond
                ((funcall test item (funcall key (elt seq i)))
                 (setq result i)
                 (setq found t))
                (t (setq i (+ i 1)))))))))))

(defun count (item seq &key (test #'eql) (key #'identity))
  "Count elements equal to ITEM under TEST + KEY."
  (let ((c 0))
    (%seq-foreach
     (lambda (el)
       (when (funcall test item (funcall key el))
         (setq c (+ c 1))))
     seq)
    c))

(defun remove (item seq &key (test #'eql) (key #'identity))
  "Return a fresh sequence with elements equal to ITEM removed.
   Result preserves SEQ's type."
  (cond
    ((listp seq)
     ;; Inline list walk — same rationale as the FIND fix above.
     ;; The captured %list-remove recurses through symbol-function,
     ;; which trampolines back into this polymorphic shim and
     ;; doubles stack growth per element.
     (let ((acc nil) (lst seq))
       (loop
         (cond
           ((null lst)
            (return (let ((rev nil))
                      (loop
                        (cond
                          ((null acc) (return rev))
                          (t (setq rev (cons (car acc) rev))
                             (setq acc (cdr acc))))))))
           ((funcall test item (funcall key (car lst)))
            (setq lst (cdr lst)))
           (t (setq acc (cons (car lst) acc))
              (setq lst (cdr lst)))))))
    ((stringp seq)
     ;; Two-pass: count kept, allocate, fill — string-append-char was O(n^2).
     (let ((kept 0))
       (%seq-foreach
        (lambda (c)
          (unless (funcall test item (funcall key c))
            (setq kept (+ kept 1))))
        seq)
       (let ((out (make-string kept))
             (i 0))
         (%seq-foreach
          (lambda (c)
            (unless (funcall test item (funcall key c))
              (setf (char out i) c)
              (setq i (+ i 1))))
          seq)
         out)))
    ((vectorp seq)
     ;; Two-pass: count kept, allocate, fill.
     (let ((kept 0))
       (%seq-foreach
        (lambda (el)
          (unless (funcall test item (funcall key el))
            (setq kept (+ kept 1))))
        seq)
       (let ((v (make-array kept))
             (i 0))
         (%seq-foreach
          (lambda (el)
            (unless (funcall test item (funcall key el))
              (setf (svref v i) el)
              (setq i (+ i 1))))
          seq)
         v)))
    (t (error "remove: not a sequence: ~A" seq))))

;; delete is the destructive variant of remove. Our remove is
;; already non-shared for vectors and strings; for lists CL allows
;; modifying cons cells but our list-remove builds fresh. Same
;; observable behaviour either way; delete is just an alias.
(defun delete (item seq &key (test #'eql) (key #'identity))
  (remove item seq :test test :key key))

(defun substitute (new-item old-item seq
                            &key (test #'eql) (key #'identity))
  "Fresh sequence with every element equal to OLD-ITEM replaced
   by NEW-ITEM. Type-preserving."
  (cond
    ((listp seq)
     (let ((out nil))
       (%seq-foreach
        (lambda (el)
          (setq out
                (cons (cond
                        ((funcall test old-item (funcall key el)) new-item)
                        (t el))
                      out)))
        seq)
       (reverse out)))
    ((stringp seq)
     ;; Length is preserved → size once and fill by index (was O(n^2)).
     (let* ((out (make-string (length seq)))
            (i 0))
       (%seq-foreach
        (lambda (c)
          (setf (char out i)
                (cond
                  ((funcall test old-item (funcall key c)) new-item)
                  (t c)))
          (setq i (+ i 1)))
        seq)
       out))
    ((vectorp seq)
     (let* ((n (length seq))
            (v (make-array n))
            (i 0))
       (%seq-foreach
        (lambda (el)
          (setf (svref v i)
                (cond
                  ((funcall test old-item (funcall key el)) new-item)
                  (t el)))
          (setq i (+ i 1)))
        seq)
       v))
    (t (error "substitute: not a sequence: ~A" seq))))

;; ── Predicate variants (find-if / position-if / remove-if) ──────────────

(defparameter %list-find-if (symbol-function 'find-if))
(defparameter %list-remove-if (symbol-function 'remove-if))
(defparameter %list-remove-if-not (symbol-function 'remove-if-not))

(defun find-if (pred seq &key (key #'identity))
  (find t seq :test (lambda (_ x) (funcall pred x)) :key key))

(defun position-if (pred seq &key (key #'identity))
  (position t seq :test (lambda (_ x) (funcall pred x)) :key key))

(defun count-if (pred seq &key (key #'identity))
  (count t seq :test (lambda (_ x) (funcall pred x)) :key key))

(defun remove-if (pred seq &key (key #'identity))
  (remove t seq :test (lambda (_ x) (funcall pred x)) :key key))

(defun remove-if-not (pred seq &key (key #'identity))
  (remove t seq :test (lambda (_ x) (not (funcall pred x))) :key key))

;; ── mismatch / search ────────────────────────────────────────────────────

(defun mismatch (seq1 seq2 &key (test #'eql) (key #'identity))
  "Return the position of the first element-by-element difference
   between SEQ1 and SEQ2, or NIL if they're equal up to the
   shorter one's length. The mismatch position is in SEQ1's
   indexing scheme."
  (if (and (listp seq1) (listp seq2))
      ;; Both lists: walk cursors (O(min n)) — paired `elt` was O(n^2).
      ;; Same result: index of first differing element; if one is a strict
      ;; prefix of the other, the index one past the shorter; else nil.
      (let ((c1 seq1) (c2 seq2) (i 0) (result nil) (done nil))
        (loop
          (cond
            (done (return result))
            ((and (null c1) (null c2)) (setq done t))             ; equal → nil
            ((or (null c1) (null c2)) (setq result i) (setq done t))
            ((funcall test (funcall key (car c1)) (funcall key (car c2)))
             (setq c1 (cdr c1)) (setq c2 (cdr c2)) (setq i (+ i 1)))
            (t (setq result i) (setq done t)))))
      (let* ((n1 (length seq1))
             (n2 (length seq2))
             (n  (cond ((< n1 n2) n1) (t n2)))
             (i 0)
             (result nil)
             (done nil))
        (loop
          (cond
            (done (return result))
            ((>= i n)
             ;; Walked through the shorter; if lengths differ, mismatch
             ;; is at index n (one past the shorter end). Else nil.
             (setq result (cond ((= n1 n2) nil) (t n)))
             (setq done t))
            (t (cond
                 ((funcall test
                           (funcall key (elt seq1 i))
                           (funcall key (elt seq2 i)))
                  (setq i (+ i 1)))
                 (t (setq result i)
                    (setq done t)))))))))

(defun search (sub-seq seq &key (test #'eql) (key #'identity))
  "Return the index in SEQ of the first occurrence of SUB-SEQ,
   or NIL if absent. O(n*m) — naive scan, sufficient for the
   typical short-needle case."
  ;; Coerce list operands to vectors once so the inner index reads are
  ;; O(1). A list `elt` inside the nested scan made search O(n^2*m);
  ;; vector indices match the original list indices, so the result is
  ;; identical.
  (let ((sub-seq (cond ((listp sub-seq) (coerce sub-seq 'vector)) (t sub-seq)))
        (seq (cond ((listp seq) (coerce seq 'vector)) (t seq))))
   (let* ((nsub (length sub-seq))
         (nseq (length seq))
         (limit (- nseq nsub))
         (start 0)
         (result nil)
         (done nil))
    (cond
      ((> nsub nseq) nil)
      (t
       (loop
         (cond
           (done (return result))
           ((> start limit)
            (setq done t))                              ; not found
           (t (let ((ok t) (j 0))
                (loop
                  (cond
                    ((>= j nsub) (return nil))
                    ((not (funcall test
                                   (funcall key (elt sub-seq j))
                                   (funcall key (elt seq (+ start j)))))
                     (setq ok nil)
                     (return nil))
                    (t (setq j (+ j 1)))))
                (cond
                  (ok (setq result start) (setq done t))
                  (t (setq start (+ start 1)))))))))))))

;; ── fill / replace ──────────────────────────────────────────────────────

(defun fill (seq item &key (start 0) end)
  "Destructively replace each element of SEQ in [START, END) with
   ITEM. Returns SEQ. For lists this mutates car cells; for
   vectors/strings it writes via setf elt."
  (let ((real-end (cond ((null end) (length seq)) (t end)))
        (i start))
    (loop
      (cond
        ((>= i real-end) (return seq))
        (t (%set-elt seq i item)
           (setq i (+ i 1)))))))

(defun replace (target source
                       &key (start1 0) end1 (start2 0) end2)
  "Destructively copy elements of SOURCE into TARGET. Returns
   TARGET. Bounds are clamped at the shorter of the two
   sub-ranges."
  (let* ((re1 (cond ((null end1) (length target)) (t end1)))
         (re2 (cond ((null end2) (length source)) (t end2)))
         (n   (cond ((< (- re1 start1) (- re2 start2))
                     (- re1 start1))
                    (t (- re2 start2))))
         (i 0))
    (loop
      (cond
        ((>= i n) (return target))
        (t (%set-elt target (+ start1 i) (elt source (+ start2 i)))
           (setq i (+ i 1)))))))

;; ── Negated predicate variants ──────────────────────────────────────────

(defun find-if-not (pred seq &key (key #'identity))
  "Return the first element of SEQ for which PRED returns NIL."
  (find-if (complement pred) seq :key key))

(defun position-if-not (pred seq &key (key #'identity))
  "Return the index of the first element for which PRED returns NIL."
  (position-if (complement pred) seq :key key))

(defun count-if-not (pred seq &key (key #'identity))
  "Count elements of SEQ for which PRED returns NIL."
  (count-if (complement pred) seq :key key))

;; ── Substitute predicate variants ──────────────────────────────────────

(defun substitute-if (new pred seq &key (key #'identity))
  "Return a fresh sequence with each element satisfying PRED
   replaced by NEW. KEY is applied to each element before testing."
  (substitute new nil seq
              :test (lambda (_ ek) (funcall pred ek))
              :key key))

(defun substitute-if-not (new pred seq &key (key #'identity))
  "Return a fresh sequence with each element NOT satisfying PRED
   replaced by NEW."
  (substitute new nil seq
              :test (lambda (_ ek) (not (funcall pred ek)))
              :key key))

;; ── Delete predicate variants ───────────────────────────────────────────
;; NCL's delete is already non-destructive (it delegates to remove).
;; delete-if / delete-if-not follow the same pattern.

(defun delete-if (pred seq &key (key #'identity))
  "Remove elements of SEQ satisfying PRED. May destructively modify
   SEQ (currently delegates to REMOVE-IF)."
  (remove-if pred seq :key key))

(defun delete-if-not (pred seq &key (key #'identity))
  "Remove elements of SEQ NOT satisfying PRED. May destructively
   modify SEQ (currently delegates to REMOVE-IF-NOT)."
  (remove-if-not pred seq :key key))

;; ── Sort with :key — override core.lisp's two-arg sort ─────────────────
;;
;; core.lisp's SORT takes only (lst cmp) with no :key support. We
;; replace it here with a version that handles :key and also works on
;; vectors and strings (via list round-trip).
;;
;; The merge sort is stable: equal elements from the left sequence
;; (first argument to %sort-merge) always appear before equal elements
;; from the right, preserving the original relative order.

(defun %sort-merge (a b cmp key)
  "Merge two sorted lists A and B under CMP (with KEY applied)."
  (cond
    ((null a) b)
    ((null b) a)
    ((funcall cmp (funcall key (car a)) (funcall key (car b)))
     (cons (car a) (%sort-merge (cdr a) b cmp key)))
    (t (cons (car b) (%sort-merge a (cdr b) cmp key)))))

(defun %sort-list (lst cmp key)
  "Recursive merge-sort of LST under CMP/KEY."
  (cond
    ((or (null lst) (null (cdr lst))) lst)
    (t (let* ((split (%split-list lst))
              (left  (%sort-list (car split) cmp key))
              (right (%sort-list (cdr split) cmp key)))
         (%sort-merge left right cmp key)))))

;; coerce vector/string → list without depending on core's coerce
;; (which doesn't have the vector→list path yet).
(defun %seq-to-list (seq)
  (cond
    ((listp seq) seq)
    (t (let ((n (length seq)) (acc nil) (i 0))
         (loop
           (when (>= i n) (return (nreverse acc)))
           (setq acc (cons (elt seq i) acc))
           (setq i (+ i 1)))))))

(defun sort (seq cmp &key (key #'identity))
  "Sort SEQ (a list, vector, or string) by CMP. CMP is called with two
   elements (after KEY is applied) and should return true if the first
   should precede the second. Stable for lists (merge sort). For
   vectors/strings, sorts elements in-place via a list round-trip."
  (cond
    ((listp seq)
     (%sort-list seq cmp key))
    ((vectorp seq)
     (let* ((sorted (%sort-list (%seq-to-list seq) cmp key))
            (i 0))
       (dolist (x sorted seq)
         (setf (svref seq i) x)
         (setq i (+ i 1)))))
    ((stringp seq)
     (let* ((sorted (%sort-list (%seq-to-list seq) cmp key))
            (i 0))
       (dolist (c sorted seq)
         (setf (char seq i) c)
         (setq i (+ i 1)))))
    (t (error "sort: not a sequence: ~S" seq))))

(defun stable-sort (seq cmp &key (key #'identity))
  "Sort SEQ stably under CMP. In NCL, SORT is already a stable merge
   sort, so this is an alias."
  (sort seq cmp :key key))

;; ── merge ────────────────────────────────────────────────────────────────

(defun merge (result-type seq1 seq2 pred &key (key #'identity))
  "Merge two sequences SEQ1 and SEQ2 (assumed sorted by PRED/KEY)
   into a single sorted sequence of RESULT-TYPE. RESULT-TYPE must
   be LIST, VECTOR, or STRING."
  (let* ((lst1 (%seq-to-list seq1))
         (lst2 (%seq-to-list seq2))
         (merged (%sort-merge lst1 lst2 pred key)))
    (cond
      ((eq result-type 'list) merged)
      ((or (eq result-type 'vector) (eq result-type 'simple-vector))
       (coerce merged 'vector))
      ((eq result-type 'string)
       (coerce merged 'string))
      (t (error "merge: unsupported result-type ~S" result-type)))))

;; ── delete-duplicates ────────────────────────────────────────────────────
;;
;; CL spec allows destructive modification; for lists this is equivalent
;; to remove-duplicates. Both names are provided so portable code works.

(defun delete-duplicates (seq &key (test #'eql) (key #'identity))
  "Like REMOVE-DUPLICATES but may modify SEQ in place.
   In NCL, this is an alias (same semantics as REMOVE-DUPLICATES)."
  (remove-duplicates seq :test test :key key))

;; ── nsubstitute / nsubstitute-if / nsubstitute-if-not ────────────────────
;;
;; In-place versions of substitute. Walk the sequence and replace matching
;; elements by mutation; return the (possibly modified) sequence.

(defun %nsubstitute-seq-if (newitem predicate sequence
                             start end from-end count key)
  "Internal: in-place substitution via predicate over any sequence."
  (let* ((len (length sequence))
         (s   (or start 0))
         (e   (or end len))
         (replaced 0))
    (if from-end
        ;; Walk backwards
        (let ((i (- e 1)))
          (loop
            (when (or (< i s) (and count (>= replaced count))) (return))
            (let ((elem (elt sequence i)))
              (when (funcall predicate (funcall key elem))
                (setf (elt sequence i) newitem)
                (setq replaced (+ replaced 1))))
            (setq i (- i 1))))
        ;; Walk forwards
        (let ((i s))
          (loop
            (when (or (>= i e) (and count (>= replaced count))) (return))
            (let ((elem (elt sequence i)))
              (when (funcall predicate (funcall key elem))
                (setf (elt sequence i) newitem)
                (setq replaced (+ replaced 1))))
            (setq i (+ i 1)))))
    sequence))

(defun nsubstitute (newitem olditem sequence
                    &key (test #'eql) (key #'identity)
                         start end from-end count)
  "Destructively replace all occurrences of OLDITEM in SEQUENCE
   with NEWITEM. Returns the modified sequence."
  (%nsubstitute-seq-if newitem
                       (lambda (x) (funcall test x olditem))
                       sequence start end from-end count key))

(defun nsubstitute-if (newitem test sequence
                       &key (key #'identity) start end from-end count)
  "Destructively replace every element of SEQUENCE satisfying TEST
   with NEWITEM. Returns the modified sequence."
  (%nsubstitute-seq-if newitem test sequence start end from-end count key))

(defun nsubstitute-if-not (newitem test sequence
                            &key (key #'identity) start end from-end count)
  "Destructively replace every element of SEQUENCE NOT satisfying TEST
   with NEWITEM. Returns the modified sequence."
  (%nsubstitute-seq-if newitem
                       (lambda (x) (not (funcall test x)))
                       sequence start end from-end count key))

;; ── (setf subseq) ─────────────────────────────────────────────────────────
;;
;; The compiler's generic-setf fallback rewrites
;;   (setf (subseq seq start end) val)
;; into
;;   (%setf-subseq val seq start end)
;; so we only need (defun %setf-subseq ...).  DO NOT also define
;; (defun (setf subseq) ...) — that would mangle to the same %SETF-SUBSEQ
;; symbol and replace this implementation with an infinitely-recursive
;; wrapper.

(defun %setf-subseq (value sequence start &optional end)
  "Copy elements from VALUE into SEQUENCE[start…end].
   VALUE is truncated if shorter than the slice; extra slice positions
   are left unchanged.  Returns VALUE (CL setf convention)."
  (let* ((len (length sequence))
         (s   (or start 0))
         (e   (if end (min end len) len))
         (vlen (length value))
         (n   (min (- e s) vlen))
         (i   0))
    (loop
      (when (>= i n) (return value))
      (setf (elt sequence (+ s i)) (elt value i))
      (setq i (+ i 1)))))

(provide 'sequences)
nil
