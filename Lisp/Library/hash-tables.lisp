;;;; Lisp/Library/hash-tables.lisp — hash-table utilities.
;;;;
;;;; Provides:
;;;;
;;;;   WITH-HASH-TABLE-ITERATOR  — sequential iterator over a hash table
;;;;   SXHASH                    — implementation-independent hash code
;;;;
;;;; The NCL runtime already provides the core hash-table API:
;;;;   make-hash-table  gethash  (setf gethash)
;;;;   remhash  clrhash  maphash  hash-table-p
;;;;   hash-table-count  hash-table-test
;;;;
;;;; This module adds the standard iterator macro and the hash function
;;;; that CL code can call from Lisp.

;; ── with-hash-table-iterator ─────────────────────────────────────────
;;
;; (with-hash-table-iterator (NAME HASH-TABLE) &body body)
;;
;; Establishes a local macro NAME that, when called with no arguments,
;; returns three values:
;;
;;   (values T     KEY VALUE)    — another entry is available
;;   (values NIL   NIL NIL)     — the table has been exhausted
;;
;; ORDER IS UNSPECIFIED by the standard, so this can snapshot all
;; entries at macro-entry time. Using maphash keeps us independent of
;; the internal hash-table layout.
;;
;; Example:
;;   (with-hash-table-iterator (next ht)
;;     (loop
;;       (multiple-value-bind (more? k v) (next)
;;         (unless more? (return))
;;         (format t "~A => ~A~%" k v))))

(defmacro with-hash-table-iterator (name-and-table &body body)
  "Iterate over a hash table via a local function named by the first
   element of NAME-AND-TABLE.  Syntax: (NAME HASH-TABLE).
   Each call to (NAME) returns (values more? key value)."
  ;; NCL's defmacro doesn't support nested destructuring lambda-lists,
  ;; so we destructure manually at macroexpansion time.
  (let* ((iter-name  (car  name-and-table))
         (hash-table (cadr name-and-table))
         (pairs-g    (gensym "HTPAIRS"))
         (cur-g      (gensym "HTCUR")))
    `(let ((,pairs-g nil))
       ;; Snapshot all entries at macro-entry time so iteration is
       ;; stable even if the table is modified in the body.
       (maphash (lambda (k v)
                  (setq ,pairs-g (cons (cons k v) ,pairs-g)))
                ,hash-table)
       (let ((,cur-g (nreverse ,pairs-g)))
         (flet ((,iter-name ()
                  (if ,cur-g
                      (let ((pair (car ,cur-g)))
                        (setq ,cur-g (cdr ,cur-g))
                        (values t (car pair) (cdr pair)))
                      (values nil nil nil))))
           ,@body)))))

;; ── sxhash ────────────────────────────────────────────────────────────
;;
;; (sxhash OBJECT) — return a non-negative fixnum hash code for OBJECT.
;;
;; Required properties (ANSI CL §18.1.2):
;;
;;   1. (equal X Y) ⟹ (= (sxhash X) (sxhash Y))
;;   2. Consistent within a Lisp session (not across images).
;;   3. Result is a non-negative fixnum.
;;
;; This implementation is correct and portable; it does NOT need to
;; agree with NCL's internal hash-table hash (which uses the :test
;; function's hash, not sxhash, for EQ/EQL/EQUAL/EQUALP tables).
;; Users who need a hash consistent with a specific hash-table should
;; use that table's internal keys.
;;
;; Algorithm: djb2-style mixing, 29-bit result.

(defun %sxhash-mix (h v)
  "Mix hash accumulator H with additional value V (both non-negative)."
  (mod (+ (mod (* h 31) 536870912) v) 536870912))   ; 536870912 = 2^29

(defun sxhash (object)
  "Return a non-negative fixnum hash code for OBJECT (ANSI CL §18.1.2).
   Objects that are EQUAL always return the same code within a session."
  (cond
    ;; Atoms with cheap identity hash.
    ((null object)      0)
    ((eq object t)      1)
    ;; Symbols — hash their print-name string.
    ((symbolp object)
     (%sxhash-mix 3 (sxhash (symbol-name object))))
    ;; Integers — fold to positive, mix with 7.
    ((integerp object)
     (let ((n (if (minusp object) (- -1 object) object)))
       (mod (+ 7 (mod n 536870912)) 536870912)))
    ;; Characters — hash by code point.
    ((characterp object)
     (%sxhash-mix 11 (char-code object)))
    ;; Floats — hash via integer representation (0.0 and -0.0 agree).
    ((floatp object)
     (if (zerop object)
         13
         ;; Scale to get a stable integer; handle NaN/Inf gracefully.
         (let ((n (ignore-errors (round (* (abs object) 65536)))))
           (%sxhash-mix 17 (if n (mod n 536870912) 0)))))
    ;; Strings — djb2 over char codes.
    ((stringp object)
     (let ((h 5381))
       (dotimes (i (length object))
         (setq h (%sxhash-mix h (char-code (char object i)))))
       h))
    ;; Lists (and conses) — mix car and cdr hashes.
    ((consp object)
     (let ((h (sxhash (car object)))
           (t-depth 5))           ; limit CDR recursion depth
       (do ((cell (cdr object) (cdr cell))
            (d    0              (1+ d)))
           ((or (null cell) (atom cell) (>= d t-depth))
            (if (atom cell)
                (%sxhash-mix h (sxhash cell))
                h))
         (setq h (%sxhash-mix h (sxhash (car cell)))))
       h))
    ;; Vectors/arrays — hash first few elements.
    ((vectorp object)
     (let ((h 5381)
           (len (min (length object) 8)))
       (dotimes (i len)
         (setq h (%sxhash-mix h (sxhash (aref object i)))))
       h))
    ;; Anything else — use a fixed constant (EQ objects are EQUAL so ok).
    (t 42)))

(provide 'hash-tables)
nil
