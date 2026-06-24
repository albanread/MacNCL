;;;; Lisp/Library/types.lisp — basic CL type system helpers.
;;;;
;;;; SUBTYPEP — the missing complement to TYPEP. CL's full
;;;; subtype calculus is undecidable in general (SATISFIES alone
;;;; makes it so), but the practical 80% boils down to a finite
;;;; hierarchy of named primitive types — INTEGER ⊂ RATIONAL ⊂
;;;; REAL ⊂ NUMBER, NULL ⊂ LIST ⊂ SEQUENCE, etc.
;;;;
;;;; This file implements the symbol-vs-symbol case via an
;;;; explicit ancestor table. Compound type specs (`(integer 1 3)`,
;;;; `(or A B)`, `(satisfies P)`, …) fall through as
;;;; (values nil nil) — "I don't know" — which is the spec-permitted
;;;; conservative answer. CL code that checks the SECOND value
;;;; (certainty) handles that gracefully; code that only checks the
;;;; first treats unknown as a negative.
;;;;
;;;; Returns are (values result certainty):
;;;;   (T T)     — TYPE1 is definitely a subtype of TYPE2
;;;;   (NIL T)   — TYPE1 is definitely NOT a subtype of TYPE2
;;;;   (NIL NIL) — undecided (compound spec, satisfies, etc.)
;;;;
;;;; Lives in Library/ (loaded by init.lisp) rather than core.lisp
;;;; because the ancestor table is large and only needed once the
;;;; CL surface is being exercised.

;; ── Ancestor table ─────────────────────────────────────────────────
;;
;; Each entry: (TYPE . LIST-OF-DIRECT-SUPERTYPES). T is the implicit
;; supertype of every type (handled separately). NIL is the implicit
;; subtype of every type (handled separately).
;;
;; Mirrors the type-recognition table inside `typep_shim`
;; (src/ncl-runtime/src/abi.rs ~ "match name.as_ref()"). Keep them
;; in sync: a type SUBTYPEP recognises should also TYPEP-check.

(defparameter *subtype-supertypes*
  '(;; numeric tower
    (fixnum integer)
    (bignum integer)
    (integer rational)
    (ratio rational)
    (rational real)
    (short-float float)
    (single-float float)
    (double-float float)
    (long-float float)
    (float real)
    (real number)
    (complex number)
    ;; symbols + booleans
    (null symbol list)
    (boolean symbol)
    (keyword symbol)
    ;; sequences
    (cons list)
    (list sequence)
    (string vector)
    (simple-string string)
    (simple-vector vector)
    (vector array)
    (vector sequence)
    (array sequence) ;; not strictly CL — arrays aren't always seqs;
                     ;; safe for the corpus.
    ;; functions
    (compiled-function function)
    (generic-function function)
    (standard-generic-function generic-function)
    (standard-object t)
    (built-in-class class)
    (standard-class class)
    (structure-class class)
    (class standard-object)
    (method standard-object)
    (standard-method method)
    ;; chars + strings
    (standard-char character)
    (base-char character)
    (extended-char character)
    ;; atoms
    (atom t)
    ;; hash table
    (hash-table t)
    ;; package
    (package t)))

(defun %type-ancestors (type)
  "Return TYPE plus all transitive supertypes per *subtype-supertypes*.
   Always includes T at the tail (every type is a subtype of T)."
  (cond
    ((eq type 't) '(t))
    ((eq type 'nil) '())  ; NIL is the bottom; its ancestor set is
                          ; "everything" but we represent that
                          ; through the (eq type1 'nil) short-circuit
                          ; in subtypep itself.
    (t
     (let ((result (list type))
           (frontier (list type)))
       (loop
         (cond
           ((null frontier) (return (append (reverse result) '(t))))
           (t
            (let* ((current (car frontier))
                   (parents (cdr (assoc current *subtype-supertypes*))))
              (setq frontier (cdr frontier))
              (dolist (p parents)
                (unless (member p result)
                  (setq result (cons p result))
                  (setq frontier (cons p frontier))))))))))))

(defun subtypep (type1 type2)
  "Return (VALUES RESULT CERTAINTY). See file header for semantics."
  (cond
    ;; Compound type specs — leave the analysis to a future
    ;; canonicaliser. Returning unknown is spec-conformant.
    ((or (consp type1) (consp type2))
     (values nil nil))
    ;; Same name → trivially a subtype.
    ((eq type1 type2) (values t t))
    ;; NIL is a subtype of every type.
    ((eq type1 'nil) (values t t))
    ;; T is a supertype of every type.
    ((eq type2 't) (values t t))
    ;; T is a subtype only of T (the (eq type1 type2) clause covered
    ;; that), so (subtypep 't OTHER) is NIL for any OTHER ≠ T.
    ((eq type1 't) (values nil t))
    ;; Walk ancestors of type1.
    (t
     (cond
       ((member type2 (%type-ancestors type1)) (values t t))
       ;; Both are named primitives but unrelated → definitely not
       ;; subtype. We could refine with sibling-relation analysis;
       ;; for now any named-non-ancestor is treated as a clean
       ;; "definitely not."
       (t (values nil t))))))

;; ── coerce-friendly type-of refinement ───────────────────────────
;;
;; The runtime TYPE-OF returns broad names (FIXNUM / BIGNUM / FLOAT /
;; SIMPLE-VECTOR / …). CL spec sometimes wants more specific names —
;; chapter 4 tests `(type-of (expt 2 40))` => BIGNUM and we already
;; return that, so no further refinement is needed here. Future
;; tightening can layer on top.

;; ── ccase ────────────────────────────────────────────────────────────
;;
;; (ccase KEYPLACE CLAUSE*) — correctable CASE.
;; Like ECASE, but the CL spec says a mismatch should signal a
;; correctable error with a STORE-VALUE restart so the caller can
;; supply a new key. NCL does not yet have interactive restarts, so
;; we degrade gracefully to a non-continuable error — same as ECASE —
;; while preserving the correct macro syntax so code that uses CCASE
;; at least compiles and runs.

(defmacro ccase (keyplace &rest clauses)
  "Like CASE but signals a correctable error if no clause matches.
   (NCL: restarts not yet supported; signals a non-continuable error.)"
  (let ((k (gensym "CCASE-KEY")))
    `(let ((,k ,keyplace))
       (cond
         ,@(mapcar (lambda (clause) (case-clause-expand clause k))
                   clauses)
         (t (error "ccase: no matching clause for ~S" ,k))))))

;; ── deftype / extended typep ──────────────────────────────────────────
;;
;; (deftype NAME LAMBDA-LIST &body BODY) — register a type expander.
;;
;; The ANSI CL built-in `typep` in NCL only handles simple symbol type
;; names — compound specifiers like `(integer 0 100)`, `(or A B)`,
;; etc. all return NIL. We replace `typep` with a Lisp wrapper that:
;;
;;   1. Expands user-defined types registered via DEFTYPE.
;;   2. Handles the common compound built-in specifiers directly.
;;   3. Falls through to the original Rust shim for simple symbols.
;;
;; Architecture note: we save the original in a `defparameter` BEFORE
;; redefining typep, rather than using a `(let ((orig #'typep)) (defun
;; typep ...))` closure. The closure approach causes an infinite loop
;; in NCL because the `defun` form updates the function cell while the
;; let-frame reference may alias it.
;;
;; Lambda-list uses DESTRUCTURING-BIND semantics (from symbols.lisp).

(defvar *type-expanders* (make-hash-table :test 'eq)
  "Maps user-defined type names to their expander lambdas.")

;; Save the native typep BEFORE we shadow it.
(defparameter *%original-typep%* #'typep)

(defmacro deftype (name lambda-list &body body)
  "Define a derived type specifier NAME with LAMBDA-LIST and BODY.
   The body should return a type specifier (a symbol or compound form).
   The lambda-list is applied to the arguments of the compound type
   specifier: `(typep x '(NAME arg1 arg2 ...))` calls the expander
   with args `(arg1 arg2 ...)`."
  (let ((form-g (gensym "DTFORM")))
    `(progn
       (setf (gethash ',name *type-expanders*)
             (lambda (,form-g)
               (destructuring-bind ,lambda-list
                   (if (consp ,form-g) (cdr ,form-g) '())
                 ,@body)))
       ',name)))

;; ── Helpers for compound type dispatch ──────────────────────────────

(defun %typep-in-range (n lo hi)
  "Return T if N satisfies LO <= N <= HI. * means unbounded."
  (and (or (eq lo '*) (>= n lo))
       (or (eq hi '*) (<= n hi))))

;; ── Extended TYPEP implementation ────────────────────────────────────
;;
;; CRITICAL ARCHITECTURE NOTE — avoiding symbolp→typep→%new-typep loop
;; ─────────────────────────────────────────────────────────────────────
;; core.lisp defines  (defun symbolp (x) (typep x 'symbol))  and
;; similar predicates for integerp, floatp, stringp, etc.  After
;; types.lisp redefines typep as a Lisp wrapper, every call to
;; (symbolp x) goes through typep → %new-typep.
;;
;; Therefore %new-typep MUST NOT call symbolp (or any predicate that
;; calls typep) in its top-level dispatch test, or it will recurse
;; infinitely: symbolp → typep → %new-typep → symbolp → …
;;
;; Solution: use (consp type) — a compiler INTRINSIC that emits a
;; direct tag-check and never calls typep — as the first branch.
;; Everything that is not a cons falls into the catch-all (t …)
;; branch, which handles symbol type names and nil.  Calling
;; (integerp object) etc. inside the compound branches is safe
;; because those calls eventually reach the Rust shim in O(1) via
;;   integerp → typep → %new-typep → (consp 'integer) = NO
;;                                 → (t) → gethash → funcall shim
;;
;; The `case` macro used for built-in compound dispatch is also safe:
;; it expands (at macro-expand / compile time) to cond+eql, and both
;; cond and eql are compiler intrinsics that never touch typep.

(defun %new-typep (object type)
  "Internal implementation for the extended TYPEP.
   Dispatch order: compound cons specs first, symbol/nil catch-all
   second.  Never calls SYMBOLP to avoid a typep-recursion loop."
  (cond
    ;; ── Compound type specs: (head arg…) ─────────────────────────────
    ;; consp is a compiler intrinsic — no typep call.
    ((consp type)
     (let* ((head (car type))
            (args (cdr type))
            (expander (gethash head *type-expanders*)))
       (if expander
           ;; User-defined compound: expand and recurse.
           (%new-typep object (funcall expander type))
           ;; Built-in compound specifiers.
           (case head
             ;; Numeric ranges
             ((integer)
              (and (integerp object)
                   (%typep-in-range object
                                    (if args (car args) '*)
                                    (if (cdr args) (cadr args) '*))))
             ((float single-float double-float short-float long-float)
              (and (floatp object)
                   (or (null args)
                       (%typep-in-range object (car args)
                                        (if (cdr args) (cadr args) '*)))))
             ((real)
              (and (or (integerp object) (floatp object))
                   (or (null args)
                       (%typep-in-range object (car args)
                                        (if (cdr args) (cadr args) '*)))))
             ((rational)
              (or (integerp object)
                  (and (numberp object)
                       (not (floatp object))
                       (not (integerp object)))))
             ;; Logical combinators
             ((or)
              (let ((ok nil))
                (dolist (t2 args) (when (%new-typep object t2) (setq ok t)))
                ok))
             ((and)
              (let ((ok t))
                (dolist (t2 args) (unless (%new-typep object t2) (setq ok nil)))
                ok))
             ((not)
              (not (%new-typep object (car args))))
             ;; Structural types
             ((cons)
              (and (consp object)
                   (or (null args)       (eq (car args) '*)
                       (%new-typep (car object) (car args)))
                   (or (null (cdr args)) (eq (cadr args) '*)
                       (%new-typep (cdr object) (cadr args)))))
             ;; Membership / equality
             ((member)
              (not (null (member object args))))
             ((eql)
              (eql object (car args)))
             ;; Predicate
             ((satisfies)
              (not (null (funcall (car args) object))))
             ;; String/vector with optional length
             ((string simple-string)
              (and (stringp object)
                   (or (null args) (eq (car args) '*)
                       (= (length object) (car args)))))
             ((vector simple-vector array)
              (and (vectorp object)
                   (or (null args) (eq (car args) '*)
                       (= (length object) (car args)))))
             ;; Unknown compound — try just the head as a symbol.
             (t (funcall *%original-typep%* object head))))))
    ;; ── The universal / empty type names ─────────────────────────────
    ;; (typep x t)   is always true  — T is the type of every object.
    ;; (typep x nil) is always false — NIL is the empty type.
    ;; The native shim doesn't special-case these, so handle them here.
    ;; (Note: the NULL *type* is the symbol NULL, distinct from NIL, and
    ;; falls through to the native shim below.)
    ((eq type t) t)
    ((eq type 'nil) nil)
    ;; ── Symbol type names — catch-all ────────────────────────────────
    ;; Anything reaching here is a symbol.  We do NOT use (symbolp type)
    ;; — see note above.
    (t
     (let ((expander (gethash type *type-expanders*)))
       (if expander
           ;; User-defined: expand and recurse.
           (%new-typep object (funcall expander type))
           ;; Built-in named type: delegate to the Rust shim.
           (funcall *%original-typep%* object type))))))

;; CL: a compiled function. NCL JIT-compiles every function at
;; definition, so every function is a compiled function.
(defun compiled-function-p (object)
  "Return T if OBJECT is a compiled function. In NCL every function is
   JIT-compiled, so this is equivalent to FUNCTIONP."
  (functionp object))

;; Wrap with optional environment parameter for CL compliance.
(defun typep (object type &optional environment)
  "Return T if OBJECT is of the given TYPE specifier.
   Handles user-defined types (DEFTYPE), compound built-in specs
   (integer, or, and, not, cons, member, eql, satisfies, …), and
   delegates simple symbol types to the native type checker."
  (declare (ignore environment))
  (%new-typep object type))

(provide 'types)
nil
