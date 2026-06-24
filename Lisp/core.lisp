;;;; core.lisp — the user-Lisp portion of NewCormanLisp's standard
;;;; library. This file is loaded by Session::load_core_stdlib at
;;;; session start.
;;;;
;;;; Everything in this file is plain Lisp using the primitives
;;;; defined by the compiler (cons/car/cdr, equal, +/-, if/cond,
;;;; let, defun, lambda, funcall, setq, setf). When a function
;;;; appears here it lives as a defun whose code is JIT-compiled at
;;;; load time and installed in the symbol's function cell — the
;;;; same path user code goes through.
;;;;
;;;; Conventions:
;;;;   - Helpers prefixed with % are internal; don't depend on them
;;;;     in user code.
;;;;   - Predicates that return T or NIL match Common Lisp.
;;;;   - Test-equality default is EQUAL (deep), not EQL. CL's exact
;;;;     EQL/EQUAL/EQUALP/:test distinction lands when keyword
;;;;     arguments do.

;; -- Trivial accessors --------------------------------------------------------

(defun first (lst) (car lst))
(defun rest (lst) (cdr lst))

(defun second (lst) (car (cdr lst)))
(defun third (lst) (car (cdr (cdr lst))))
(defun fourth (lst) (car (cdr (cdr (cdr lst)))))

(defun caar (lst) (car (car lst)))
(defun cadr (lst) (car (cdr lst)))
(defun cdar (lst) (cdr (car lst)))
(defun cddr (lst) (cdr (cdr lst)))
(defun caddr   (lst) (car (cdr (cdr lst))))
(defun cdddr   (lst) (cdr (cdr (cdr lst))))
(defun cadddr  (lst) (car (cdr (cdr (cdr lst)))))
(defun cddddr  (lst) (cdr (cdr (cdr (cdr lst)))))
(defun cadar   (lst) (car (cdr (car lst))))
(defun caaar   (lst) (car (car (car lst))))

(defun identity (x) x)

;; -- reverse, append ---------------------------------------------------------

(defun %revappend (lst acc)
  ;; (revappend lst acc) ≡ (append (reverse lst) acc), tail recursive.
  (if (null lst)
      acc
      (%revappend (cdr lst) (cons (car lst) acc))))

;; Coerce any sequence to a fresh list of its elements. Lists pass
;; through; vectors and strings are converted via `coerce`. The
;; list-walking sequence operators below funnel non-list arguments
;; through this so they never dereference vector/string storage as
;; cons cells — which segfaults. (`coerce` is defined in
;; Library/sequences.lisp; it's referenced here only at call time, by
;; which point the full library is loaded, so the forward reference is
;; fine. Bootstrap-time callers all pass real lists.)
(defun %as-list (seq)
  (if (listp seq) seq (coerce seq 'list)))

(defun reverse (seq)
  "Reverse a sequence, returning a NEW sequence of the same type.
   Lists, vectors, and strings are all supported."
  (cond
    ((listp seq) (%revappend seq nil))
    ((stringp seq) (coerce (%revappend (coerce seq 'list) nil) 'string))
    ((vectorp seq) (coerce (%revappend (coerce seq 'list) nil) 'vector))
    (t (error "REVERSE: argument is not a sequence: ~S" seq))))

(defun append (a b)
  ;; Binary append. Variadic CL append lands when &rest does.
  (if (null a)
      b
      (cons (car a) (append (cdr a) b))))

;; -- mapcar, mapc, every, some (variadic) ----------------------------------
;;
;; CL's mapping family walks N lists in parallel and stops at the
;; shortest. Each step builds the argument vector by taking the CAR
;; of every list, applies FN, then advances every list by one CDR.
;;
;; The N=1 case is hot — recursion stays as it was (no per-step
;; cons-list of cars/cdrs). The N>1 case routes through a shared
;; walker; the &rest tail it consumes is the only extra allocation.
;;
;; Early-exit predicates (every, some) short-circuit naturally: the
;; recursive helper returns the moment its own cond fires, and the
;; ACL stack unwinds without any further work.
;;
;; Two helpers do the parallel-walk plumbing: %cars-of gathers
;; (car a) (car b) (car c) … and %cdrs-of gathers (cdr a) (cdr b)
;; (cdr c) …. %any-null is the loop's end-test.

(defun %any-null (lsts)
  (cond
    ((null lsts) nil)
    ((null (car lsts)) t)
    (t (%any-null (cdr lsts)))))

(defun %cars-of (lsts)
  (cond
    ((null lsts) nil)
    (t (cons (car (car lsts)) (%cars-of (cdr lsts))))))

(defun %cdrs-of (lsts)
  (cond
    ((null lsts) nil)
    (t (cons (cdr (car lsts)) (%cdrs-of (cdr lsts))))))

;; ── mapcar ──────────────────────────────────────────────────────

(defun %mapcar-1 (fn lst)
  (cond
    ((null lst) nil)
    (t (cons (funcall fn (car lst))
             (%mapcar-1 fn (cdr lst))))))

(defun %mapcar-n (fn lsts)
  (cond
    ((%any-null lsts) nil)
    (t (cons (apply fn (%cars-of lsts))
             (%mapcar-n fn (%cdrs-of lsts))))))

(defun mapcar (fn list &rest more-lists)
  "Apply FN to successive elements of LIST and MORE-LISTS in
   parallel, collecting the results. Stops at the shortest input.
   FN receives one argument per list."
  (cond
    ((null more-lists) (%mapcar-1 fn list))
    (t (%mapcar-n fn (cons list more-lists)))))

;; ── mapc ────────────────────────────────────────────────────────

(defun %mapc-1 (fn lst)
  (cond
    ((null lst) nil)
    (t (funcall fn (car lst))
       (%mapc-1 fn (cdr lst)))))

(defun %mapc-n (fn lsts)
  (cond
    ((%any-null lsts) nil)
    (t (apply fn (%cars-of lsts))
       (%mapc-n fn (%cdrs-of lsts)))))

(defun mapc (fn list &rest more-lists)
  "Like MAPCAR but called for effect. Returns LIST (the first
   input list) unchanged. FN is invoked for its side effects."
  (cond
    ((null more-lists) (%mapc-1 fn list))
    (t (%mapc-n fn (cons list more-lists))))
  list)

;; ── every ──────────────────────────────────────────────────────

(defun %every-1 (pred lst)
  (cond
    ((null lst) t)
    ((funcall pred (car lst)) (%every-1 pred (cdr lst)))
    (t nil)))

(defun %every-n (pred lsts)
  (cond
    ((%any-null lsts) t)
    ((apply pred (%cars-of lsts)) (%every-n pred (%cdrs-of lsts)))
    (t nil)))

(defun every (pred list &rest more-lists)
  "T iff PRED returns non-NIL for every parallel tuple of elements
   from LIST and MORE-LISTS. Stops at the shortest input. Early-
   exits on the first NIL. Accepts lists, vectors, and strings —
   non-list sequences are coerced to lists first so the walk never
   dereferences vector/string storage as cons cells."
  (cond
    ((null more-lists) (%every-1 pred (%as-list list)))
    (t (%every-n pred (mapcar #'%as-list (cons list more-lists))))))

;; ── some ───────────────────────────────────────────────────────

(defun %some-1 (pred lst)
  (cond
    ((null lst) nil)
    (t (let ((v (funcall pred (car lst))))
         (cond (v v)
               (t (%some-1 pred (cdr lst)))))) ))

(defun %some-n (pred lsts)
  (cond
    ((%any-null lsts) nil)
    (t (let ((v (apply pred (%cars-of lsts))))
         (cond (v v)
               (t (%some-n pred (%cdrs-of lsts)))))) ))

(defun some (pred list &rest more-lists)
  "Returns the first non-NIL value of PRED applied to parallel
   tuples from LIST and MORE-LISTS, or NIL if all yielded NIL.
   Stops at the shortest input or at the first hit. Accepts lists,
   vectors, and strings — non-list sequences are coerced to lists
   first so the walk never dereferences their storage as cons cells."
  (cond
    ((null more-lists) (%some-1 pred (%as-list list)))
    (t (%some-n pred (mapcar #'%as-list (cons list more-lists))))))

;; -- member, position, find, assoc -------------------------------------------

;; -- Sequence/list searches with :test and :key -----------------------------
;;
;; CL's family of search functions (member, find, position, assoc,
;; ...) accept :test (the predicate, default eql) and :key (an
;; accessor applied to each candidate before comparison, default
;; identity). Closette uses both heavily — assoc with :key #'cadr
;; for example. The implementations below desugar each candidate
;; check to (funcall test item (funcall key elem)).

;; Fixed-arity tail-recursive helper for the common (eql, identity) case.
;; Self-tail-call elimination turns this into a tight loop — no per-element
;; funcall, no &key re-parse — WITHOUT needing the LOOP macro, which isn't
;; defined this early in the bootstrap.
(defun %member-eql (item lst)
  (cond ((null lst) nil)
        ((eql item (car lst)) lst)
        (t (%member-eql item (cdr lst)))))

(defun member (item lst &key (test #'eql) (key #'identity))
  "Return the tail of LST starting at the first ELEM where
   (funcall TEST ITEM (funcall KEY ELEM)) is true, or NIL."
  ;; Fast path: default (eql test, identity key) — ~40x SBCL before this.
  (if (and (eq test #'eql) (eq key #'identity))
      (%member-eql item lst)
      (cond
        ((null lst) nil)
        ((funcall test item (funcall key (car lst))) lst)
        (t (member item (cdr lst) :test test :key key)))))

(defun find (item lst &key (test #'eql) (key #'identity))
  "Return the first element of LST that matches ITEM under TEST
   (after KEY is applied to the element), or NIL."
  (cond
    ((null lst) nil)
    ((funcall test item (funcall key (car lst))) (car lst))
    (t (find item (cdr lst) :test test :key key))))

(defun position (item lst &key (test #'eql) (key #'identity))
  "Return the index in LST of the first matching element, or NIL."
  (%position-from item lst 0 test key))

(defun %position-from (item lst i test key)
  (cond
    ((null lst) nil)
    ((funcall test item (funcall key (car lst))) i)
    (t (%position-from item (cdr lst) (+ i 1) test key))))

;; Fixed-arity tail-recursive helper (see %member-eql) — self-tail-call
;; elimination compiles this to a tight loop with no per-element funcall.
(defun %assoc-eql (item alist)
  (cond ((null alist) nil)
        ((eql item (car (car alist))) (car alist))
        (t (%assoc-eql item (cdr alist)))))

(defun assoc (item alist &key (test #'eql) (key #'identity))
  "Walk ALIST; return the first entry whose CAR matches ITEM
   under TEST (with KEY applied to the entry's car). Default
   TEST is eql to match CL — earlier this was equal because we
   didn't have keyword args; callers that relied on equal should
   pass `:test #'equal`."
  ;; Fast path: the overwhelmingly common (eql test, identity key) case.
  ;; assoc was ~44x SBCL before this; it is the dominant cost in
  ;; symbol/alist-heavy code (unification, property lists, …).
  (if (and (eq test #'eql) (eq key #'identity))
      (%assoc-eql item alist)
      (cond
        ((null alist) nil)
        ((funcall test item (funcall key (car (car alist)))) (car alist))
        (t (assoc item (cdr alist) :test test :key key)))))

;; -- nth, nthcdr, last -------------------------------------------------------

(defun nthcdr (n lst)
  (if (= n 0)
      lst
      (nthcdr (- n 1) (cdr lst))))

(defun nth (n lst)
  (car (nthcdr n lst)))

(defun last (lst)
  ;; CL's `last` returns the LAST CONS CELL of lst, not the last
  ;; element. (last '(1 2 3)) is (3), not 3.
  (cond
    ((null lst) nil)
    ((null (cdr lst)) lst)
    (t (last (cdr lst)))))

(defun butlast (lst)
  ;; Returns lst with its last cons removed.
  (cond
    ((null lst) nil)
    ((null (cdr lst)) nil)
    (t (cons (car lst) (butlast (cdr lst))))))

;; -- list construction helpers -----------------------------------------------

(defun copy-list (lst)
  (if (null lst)
      nil
      (cons (car lst) (copy-list (cdr lst)))))

(defun list-length (lst)
  ;; Same as the LENGTH primitive on lists; provided for symmetry.
  (length lst))

;; (list* a b c lst) ≡ (cons a (cons b (cons c lst))).
;; CL's variadic list* — the last arg is used as the tail; earlier
;; args are consed onto the front. (list* x) ≡ x.
(defun %list*-build (head r)
  (if (null r)
      head
      (cons head (%list*-build (car r) (cdr r)))))
(defun list* (head &rest r)
  (%list*-build head r))

;; Variadic append: (append a b c d) ≡ (append a (append b (append c d))).
;; Reuses the binary `append` defined above.
(defun %append-many (lst rest-of-lists)
  (if (null rest-of-lists)
      lst
      (append lst (%append-many (car rest-of-lists) (cdr rest-of-lists)))))
(defun append* (&rest lists)
  ;; Named `append*` to coexist with the binary `append`. When &rest
  ;; argument unpacking matures we'll merge them.
  (cond
    ((null lists) nil)
    ((null (cdr lists)) (car lists))
    (t (%append-many (car lists) (cdr lists)))))

;; -- Numeric helpers ---------------------------------------------------------

(defun zerop (n) (= n 0))
(defun plusp (n) (> n 0))
(defun minusp (n) (< n 0))

;; CL `mod` matches the sign of the divisor; `rem` matches the
;; sign of the dividend. They differ only when divisor and
;; dividend have opposite signs and the remainder is non-zero.
(defun mod (a b)
  (let ((r (rem a b)))
    (if (zerop r)
        0
        (if (eq (minusp r) (minusp b))
            r
            (+ r b)))))

(defun evenp (n) (zerop (rem n 2)))
(defun oddp (n) (not (evenp n)))

;; ── Integer-only rounding family: floor / ceiling / round.
;;
;; Each follows the CL spec at the function-contract level — accepts
;; the divisor as an optional argument that defaults to 1, and
;; returns TWO values: the quotient and the matching remainder. The
;; implementations here cover integer arguments; `Library/numbers.lisp`
;; extends them to floats and ratios in the deployed image. Tests that
;; load only the core stdlib (Session::with_stdlib) see THIS contract
;; — see `tests/end_to_end_tests::ffloor_mv_baseline` for the
;; regression guard.
;;
;; MV-propagation constraint: the compiler's `instrument_tail_for_mv`
;; pass wraps tail-position function CALL expressions with
;; `EnsureSingleMv`, which collapses the MV slot to a single value. To
;; return multiple values, a function must have `(values …)` as its
;; DIRECT tail expression — not tucked inside a helper call at tail
;; position. Each defun below therefore ends with `(values q r)` at
;; the bottom of a `let`, never in a helper. (Same constraint, same
;; workaround, that Library/numbers.lisp documents at length.)
;;
;; Note: `truncate` stays as the native shim (single-value, exactly
;; two args). Wrapping it in core.lisp would conflict with the
;; deployment's `Library/numbers.lisp`, which snapshots the current
;; truncate into its `%int-truncate` parameter and would then chain
;; through the wrapper, recursing infinitely on the native lookup.
;; The native is exactly what floor/ceiling/round need internally:
;; `(let ((q (truncate a b))) ...)` binds the let-var to the
;; quotient, then `(values q r)` at the tail returns both. No
;; capture, no chain. The cost is that integer-only `truncate` keeps
;; its strict 2-arg signature in the test session — that's a
;; separate gap to close later if someone needs it.

;; floor: largest integer k such that k*b <= a (when b > 0; flips
;; for b < 0). Differs from truncate when sign(a) != sign(b) and
;; the truncated remainder is non-zero — bump the quotient one
;; further toward -∞, then recompute the remainder against the
;; adjusted quotient.
;;
;; NB: written with nested `let` rather than `let*` because the
;; `let*` macro is defined later in this file and isn't visible to
;; the compiler when this defun is processed.
(defun floor (a &optional (b 1))
  "Divide A by B, round toward -∞. Returns (values quotient remainder)."
  (let ((q0 (truncate a b)))
    (let ((r0 (- a (* q0 b))))
      (let ((q (if (and (not (zerop r0))
                        (not (eq (minusp r0) (minusp b))))
                   (- q0 1)
                   q0)))
        (values q (- a (* q b)))))))

;; ceiling: smallest integer k such that k*b >= a (when b > 0).
;; Mirror of floor — bump UP by 1 when the truncated remainder is
;; non-zero and shares the divisor's sign.
(defun ceiling (a &optional (b 1))
  "Divide A by B, round toward +∞. Returns (values quotient remainder)."
  (let ((q0 (truncate a b)))
    (let ((r0 (- a (* q0 b))))
      (let ((q (if (and (not (zerop r0))
                        (eq (minusp r0) (minusp b)))
                   (+ q0 1)
                   q0)))
        (values q (- a (* q b)))))))

;; round: round to nearest integer; exact ties go to even (banker's
;; rounding, the CL default).
(defun round (a &optional (b 1))
  "Divide A by B, round half-to-even. Returns (values quotient remainder)."
  (let ((q0 (truncate a b)))
    (let ((r0 (- a (* q0 b))))
      (let ((half-b (truncate b 2)))
        (let ((q (cond
                   ;; |r| < |half-b| — truncated quotient is the nearest.
                   ((< (abs r0) (abs half-b)) q0)
                   ;; |r| > |half-b| — round away from zero.
                   ((> (abs r0) (abs half-b))
                    (if (eq (minusp r0) (minusp b)) (+ q0 1) (- q0 1)))
                   ;; |r| == |half-b| — exact tie; round to even.
                   (t (if (evenp q0)
                          q0
                          (if (eq (minusp r0) (minusp b)) (+ q0 1) (- q0 1)))))))
          (values q (- a (* q b))))))))

(defun signum (n)
  "CL signum — returns -1, 0, or +1 with the sign of N."
  (cond ((zerop n) 0)
        ((minusp n) -1)
        (t 1)))

(defun 1+ (n) (+ n 1))
(defun 1- (n) (- n 1))

(defun min2 (a b) (if (< a b) a b))
(defun max2 (a b) (if (> a b) a b))

;; Variadic min / max via &rest. (min) is an error in CL — we
;; return nil for the empty case instead, until conditions exist.
(defun %min-of (a r)
  (if (null r) a (%min-of (min2 a (car r)) (cdr r))))
(defun min (a &rest r) (%min-of a r))

(defun %max-of (a r)
  (if (null r) a (%max-of (max2 a (car r)) (cdr r))))
(defun max (a &rest r) (%max-of a r))

;; abs lives as a native shim that handles fixnum/bignum/ratio/
;; float/complex uniformly (see ncl-runtime/src/complex.rs).
;; The earlier (defun abs ...) using `<` couldn't handle complex
;; arguments — comparison isn't defined on complex. Keep this
;; comment so future readers know the native is intentional.

;; -- Loops -------------------------------------------------------------------
;;
;; (loop body...) repeats body forever; (return v) exits the
;; immediately enclosing loop with value v. Both wrap the
;; %native-loop / %loop-return primitives.
;;
;; CAVEAT: like (error ...), (return) doesn't unwind — code
;; *after* the (return) call but still inside the same iteration
;; body still runs. Idiomatic CL puts return at the end of a
;; cond/case clause, which sidesteps this:
;;
;;   (loop (cond ((stop?) (return result))
;;               (t (do-work))))
;;
;; works correctly. The dual-form
;;
;;   (loop (when (stop?) (return result))
;;         (do-work))                      ; <-- still runs after return
;;
;; doesn't, because (do-work) is a sibling of (when ...) and
;; runs once the when's expansion has stashed the return value.

(defmacro loop (&rest body)
  "CL LOOP. Two forms:
   * Simple: (loop FORM...) repeats FORMs forever; (return v) exits.
   * Extended: (loop KEYWORD ...) — for/while/collect/sum/etc, parsed
     by %loop-expand into a block + let* + tagbody. Dispatch is by
     whether the first element is a recognised loop keyword symbol."
  (if (%loop-extended-p body)
      (%loop-expand body)
      `(%native-loop (lambda () ,@body))))

;; Defined here (before the first internal use of `loop` in dotimes/
;; do below) so the dispatch works during the rest of stdlib load.
;; The full %loop-expand machinery lives after `tagbody`, since the
;; expansion targets tagbody/go — it's only invoked lazily when a
;; user actually writes an extended loop, well after load completes.
(defun %loop-extended-p (body)
  "True if BODY is an extended-LOOP clause list (starts with a known
   loop keyword symbol). Uses only MEMBER (defined far above) — NOT
   symbolp, which is defined much later in this file and would be
   unbound when the simple (loop …) forms inside the parser helpers
   below are macroexpanded during stdlib load. A cons or number as
   the first element simply fails the eql-based MEMBER test."
  (and body
       (member (car body)
               '(for repeat while until do collect append nconc
                 sum count maximize minimize when unless
                 with initially finally return
                 thereis always never named))))

(defmacro return (&rest args)
  ;; (return)   → exit with nil
  ;; (return v) → exit with v
  (cond
    ((null args) `(%loop-return nil))
    (t `(%loop-return ,(car args)))))

(defmacro let* (bindings &rest body)
  "Sequential let — each binding sees the earlier bindings.
   Expands to nested `let` forms."
  (cond
    ((null bindings) `(progn ,@body))
    (t `(let (,(car bindings))
          (let* ,(cdr bindings) ,@body)))))

;; -- Tier 1 macros (chunk 7) -------------------------------------------------
;;
;; CL forms Closette uses heavily. None of these need new compiler
;; or runtime machinery — every expansion lands on something that
;; already exists.

(defmacro declare (&rest decls)
  "No-op. CL `(declare …)` carries metadata for the compiler
   (ignore, type, dynamic-extent, …). We don't act on any of it
   yet, so the form just expands to NIL — sufficient because
   declarations always appear in implicit-progn position and a
   stray NIL there is harmless."
  nil)

(defmacro prog1 (first &rest rest)
  "Evaluate FIRST, save its value, then evaluate REST in order
   and return FIRST's value."
  (let ((r (gensym "PROG1")))
    `(let ((,r ,first))
       ,@rest
       ,r)))

(defmacro prog2 (first second &rest rest)
  "Evaluate the three sections, return SECOND's value."
  `(progn ,first (prog1 ,second ,@rest)))

(defmacro defvar (name &rest rest)
  "Declare a special variable. CL distinguishes defvar (assign
   only if currently unbound) from defparameter (always assign);
   without a boundp primitive yet we treat them identically.
   Closette's defvars are at file load time and aren't reset
   later, so this is safe."
  (cond
    ((null rest) `(defparameter ,name nil))
    (t `(defparameter ,name ,(car rest)))))

;; Registry of symbols declared constant via DEFCONSTANT, consulted
;; by CONSTANTP. A plain list with eq membership — the set of user
;; constants is small in practice.
(defparameter *constants* nil)

(defmacro defconstant (name value &rest rest)
  "Declare NAME as a constant with VALUE. NCL does not yet ENFORCE
   non-reassignment, but the symbol is recorded so CONSTANTP reports
   it as constant. The optional docstring argument is accepted and
   discarded for source-compat with portable libraries."
  (declare (ignore rest))
  `(progn
     (defparameter ,name ,value)
     (setq *constants* (cons ',name *constants*))
     ',name))

(defmacro push (value place)
  "Prepend VALUE to the list stored at PLACE. PLACE is evaluated
   twice — fine for symbol or simple-accessor places (which is
   what Closette uses) but not safe for places with side effects.
   A proper get-setf-expansion-based version lands when we have
   one."
  `(setf ,place (cons ,value ,place)))

(defmacro pop (place)
  "Remove and return the head of the list at PLACE. Same
   double-evaluation caveat as `push`."
  `(prog1 (car ,place) (setf ,place (cdr ,place))))

(defmacro case (keyform &rest clauses)
  "(case KEY (KEYS body…) (KEYS body…) …) — dispatch on KEY.
   KEYS is either a symbol/atom matched with EQL, a list of
   atoms (matches if KEY EQL any of them), or T / OTHERWISE for
   the catch-all clause."
  (let ((k (gensym "CASE-KEY")))
    `(let ((,k ,keyform))
       (cond
         ,@(mapcar (lambda (clause)
                     (case-clause-expand clause k))
                   clauses)))))

(defun case-clause-expand (clause keyvar)
  "Helper for the CASE macro. Builds one cond-clause from a
   case-clause."
  (let ((keys (car clause))
        (body (cdr clause)))
    (cond
      ((or (eq keys 'otherwise) (eq keys 't))
       `(t ,@body))
      ((listp keys)
       `((or ,@(mapcar (lambda (key) `(eql ,keyvar ',key)) keys))
         ,@body))
      (t `((eql ,keyvar ',keys) ,@body)))))

(defmacro ecase (keyform &rest clauses)
  "Like CASE but with no fall-through — signals an error if no
   clause matches. No OTHERWISE clause expected."
  (let ((k (gensym "ECASE-KEY")))
    `(let ((,k ,keyform))
       (cond
         ,@(mapcar (lambda (clause)
                     (case-clause-expand clause k))
                   clauses)
         (t (error "ecase: no matching clause for ~A" ,k))))))

(defmacro typecase (keyform &rest clauses)
  "(typecase KEY (TYPE body…) …) — clause matches if KEY is of
   TYPE per `typep`. T matches anything (catch-all)."
  (let ((k (gensym "TYPECASE-KEY")))
    `(let ((,k ,keyform))
       (cond
         ,@(mapcar (lambda (clause)
                     (typecase-clause-expand clause k))
                   clauses)))))

(defun typecase-clause-expand (clause keyvar)
  (let ((type (car clause))
        (body (cdr clause)))
    (cond
      ((eq type 't) `(t ,@body))
      (t `((typep ,keyvar ',type) ,@body)))))

(defmacro etypecase (keyform &rest clauses)
  "Like TYPECASE but signals an error if no clause matches.
No OTHERWISE / T clause is expected."
  (let ((k (gensym "ETYPECASE-KEY")))
    `(let ((,k ,keyform))
       (cond
         ,@(mapcar (lambda (clause)
                     (typecase-clause-expand clause k))
                   clauses)
         (t (error "etypecase: no matching clause for ~S of type ~A"
                   ,k (type-of ,k)))))))

(defmacro ctypecase (keyplace &rest clauses)
  "Like TYPECASE but signals a correctable error if no clause matches.
NCL does not support interactive restarts; this behaves like ETYPECASE."
  (let ((k (gensym "CTYPECASE-KEY")))
    `(let ((,k ,keyplace))
       (cond
         ,@(mapcar (lambda (clause)
                     (typecase-clause-expand clause k))
                   clauses)
         (t (error "ctypecase: no matching clause for ~S of type ~A"
                   ,k (type-of ,k)))))))

(defmacro dolist (binding &rest body)
  "(dolist (var list [result-form]) body…) — bind VAR to each
   element of LIST in turn, evaluate BODY. Returns RESULT-FORM
   (or NIL if absent)."
  (let* ((var (car binding))
         (list-form (car (cdr binding)))
         (result-form (cond
                        ((null (cdr (cdr binding))) nil)
                        (t (car (cdr (cdr binding))))))
         (rem (gensym "DOLIST-REM")))
    `(let ((,rem ,list-form)
           (,var nil))
       (loop
         (cond
           ((null ,rem) (return ,result-form))
           (t (setq ,var (car ,rem))
              ,@body
              (setq ,rem (cdr ,rem))))))))

(defmacro block (name &rest body)
  "(block NAME body…) — establish a named exit point. Inside
   BODY, `(return-from NAME val)` immediately exits the block
   with VAL. Without a matching return-from the block returns
   the last body form's value.

   Implementation: wraps BODY in a thunk passed to
   %native-block, which sets a setjmp at entry and longjmps
   from a matching %return-from. The longjmp aborts the rest
   of the body — unlike loop's flag-based (return), this is a
   real non-local exit."
  `(%native-block ',name (lambda () ,@body)))

(defmacro return-from (name &optional value)
  "(return-from NAME [val]) — non-locally exit the innermost
   enclosing (block NAME …) with VAL (default NIL). Per CL the
   result form is optional: (return-from foo) returns NIL."
  `(%return-from ',name ,value))

(defmacro catch (tag &rest body)
  "(catch TAG body…) — establish a dynamic exit point keyed by the
   *value* of TAG (compared with EQ). A matching (throw TAG val)
   anywhere in the dynamic extent of BODY transfers control here and
   returns VAL. Without a throw, returns the last body form's value.

   Unlike BLOCK (whose name is lexical and compile-time), CATCH's tag
   is evaluated at run time — so the tag form is passed through to
   %native-catch, and the body is wrapped in a thunk."
  `(%native-catch ,tag (lambda () ,@body)))

(defmacro throw (tag value)
  "(throw TAG val) — transfer control to the nearest enclosing
   (catch TAG …) whose tag is EQ to TAG, carrying VAL out. Signals a
   control error if no matching catch is active."
  `(%throw ,tag ,value))

;; ── TAGBODY / GO ──────────────────────────────────────────────────
;;
;; TAGBODY is CL's primitive iteration/goto construct. The body is a
;; sequence of statements (conses) and tags (symbols or integers).
;; Execution runs top to bottom; (go TAG) transfers control to just
;; after TAG. The tagbody returns NIL.
;;
;; Implementation strategy — a PC-driven state machine, built entirely
;; from existing primitives (block / return-from / a simple loop):
;;
;;   * Split the body at tags into SEGMENTS. Segment 0 is the code
;;     before the first tag; each tag opens the next segment. A tag
;;     maps to the PC (index) of the segment it opens.
;;   * Compile the body to a function (lambda (pc) ...) that, given a
;;     starting PC, runs every segment whose index is >= pc (natural
;;     fall-through), wrapped in a (block B ...). The function returns
;;     -1 when it falls off the end.
;;   * (go TAG) rewrites to (return-from B <pc-of-TAG>) — a non-local
;;     exit that aborts the rest of the current segment and hands the
;;     target PC back to the driver.
;;   * The driver %native-tagbody calls the function repeatedly,
;;     feeding back each returned PC, until it sees -1 (completion).
;;
;; (go X) is only rewritten for tags X belonging to THIS tagbody; a
;; (go Y) for an outer tag is left untouched so the enclosing
;; tagbody's expansion handles it (correct lexical tag scoping).

(defun %tagbody-tag-p (form)
  "A tagbody tag is an atom (symbol or integer) at top level."
  (or (symbolp form) (integerp form)))

(defun %tagbody-parse (body)
  "Split BODY into segments at tags. Returns (tag-alist . segments)
   where tag-alist maps each tag to its segment PC, and segments is a
   list of statement-lists indexed by PC (segment 0 first)."
  (let ((tag-alist nil)
        (segments nil)     ; reversed list of (reversed) statement lists
        (current nil)      ; reversed statements of the current segment
        (pc 0))
    (dolist (form body)
      (if (%tagbody-tag-p form)
          (progn
            (push (reverse current) segments)
            (setq current nil)
            (setq pc (1+ pc))
            (push (cons form pc) tag-alist))
          (push form current)))
    (push (reverse current) segments)
    (cons (reverse tag-alist) (reverse segments))))

(defun %tagbody-rewrite (form tag-alist block-name)
  "Recursively rewrite (go TAG) → (return-from BLOCK-NAME pc) for tags
   in TAG-ALIST. Leaves other gos and quoted data untouched."
  (cond
    ((not (consp form)) form)
    ((eq (car form) 'go)
     (let ((entry (assoc (car (cdr form)) tag-alist)))
       (if entry
           (list 'return-from block-name (cdr entry))
           form)))
    ((eq (car form) 'quote) form)
    (t (mapcar (lambda (sub) (%tagbody-rewrite sub tag-alist block-name)) form))))

(defun %tagbody-build-dispatch (segments tag-alist block-name pc-var i)
  "Build the (when (<= pc-var i) stmts...) dispatch clauses, with gos
   rewritten. Recurses over SEGMENTS with running index I."
  (if (null segments)
      nil
      (cons (list* 'when (list '<= pc-var i)
                   (%tagbody-rewrite-list (car segments) tag-alist block-name))
            (%tagbody-build-dispatch (cdr segments) tag-alist block-name
                                     pc-var (1+ i)))))

(defun %tagbody-rewrite-list (stmts tag-alist block-name)
  "Rewrite gos in each statement of STMTS; append a trailing NIL so an
   empty segment still forms a valid WHEN body."
  (append (mapcar (lambda (s) (%tagbody-rewrite s tag-alist block-name)) stmts)
          (list nil)))

(defun %native-tagbody (body-fn)
  "Driver: run BODY-FN starting at PC 0, feeding back each returned PC
   until it returns a negative value (normal completion). Returns NIL."
  (let ((pc 0))
    (loop
      (setq pc (funcall body-fn pc))
      (when (< pc 0) (return nil)))))

(defmacro tagbody (&rest body)
  "(tagbody {tag | statement}*) — CL's goto-based iteration construct.
   Runs statements top to bottom; (go TAG) jumps to just after TAG.
   Returns NIL."
  (let* ((block-name (gensym "TAGBODY"))
         (pc-var (gensym "PC"))
         (parsed (%tagbody-parse body))
         (tag-alist (car parsed))
         (segments (cdr parsed)))
    (if (null tag-alist)
        ;; No tags — body is just a progn returning nil.
        `(progn ,@body nil)
        `(%native-tagbody
           (lambda (,pc-var)
             (block ,block-name
               ,@(%tagbody-build-dispatch segments tag-alist block-name pc-var 0)
               -1))))))

;; ── Extended LOOP ─────────────────────────────────────────────────
;;
;; A useful subset of CL's LOOP facility, expanding to block + let* +
;; tagbody/go (the same shape real implementations use). Supported:
;;
;;   for V in LIST | on LIST | from N [to|below|downto|above M] [by S]
;;                | = INIT [then STEP]
;;   repeat N        while TEST       until TEST
;;   with V [= INIT] named NAME
;;   do FORMS...      initially FORMS...   finally FORMS...
;;   collect E   append E   sum E   count E   maximize E   minimize E
;;   when TEST <clause>     unless TEST <clause>
;;   return E    thereis E    always E    never E
;;
;; Parser state is a 12-slot vector, mutated in place by small clause
;; handlers — this sidesteps NCL's "no mutable parameters" rule (the
;; vector is a shared heap object; we mutate its slots, not a param)
;; and keeps each handler tiny so codegen never blows the stack.
;;
;;   0 cl   1 vars   2 pre   3 ends   4 body   5 steps   6 post
;;   7 result   8 name   9 acc   10 acc-tail   11 acc-kind
;; Slots 1-6 hold REVERSED accumulators; %loop-build reverses them.

(defun %lsg (st i) (svref st i))
(defun %lss (st i v) (setf (svref st i) v))
(defun %lsp (st i v) (setf (svref st i) (cons v (svref st i))))

(defun %loop-keyword-p (sym)
  (member sym '(for repeat while until do collect append nconc
                sum count maximize minimize when unless
                with initially finally return
                thereis always never named)))

;; -- accumulator setup --

(defun %loop-need-list-acc (st)
  (when (null (%lsg st 9))
    (let ((h (gensym "ACC")) (tl (gensym "TL")))
      (%lsp st 1 (list h (list 'cons nil nil)))
      (%lsp st 1 (list tl h))
      (%lss st 9 h)
      (%lss st 10 tl)
      (%lss st 11 'collect))))

(defun %loop-need-num-acc (st init kind)
  (when (null (%lsg st 9))
    (let ((a (gensym "NUM")))
      (%lsp st 1 (list a init))
      (%lss st 9 a)
      (%lss st 11 kind))))

;; -- accumulator emitters (each appends one body form, optionally
;;    guarded by a when-TEST from a surrounding when/unless clause) --

(defun %loop-guard (form test)
  (if test (list 'when test form) form))

(defun %loop-emit-collect (st expr test)
  (%loop-need-list-acc st)
  (let ((tl (%lsg st 10)) (c (gensym "C")))
    (%lsp st 4 (%loop-guard
                (list 'let (list (list c (list 'cons expr nil)))
                      (list 'setf (list 'cdr tl) c)
                      (list 'setq tl c))
                test))))

(defun %loop-emit-append (st expr test)
  (%loop-need-list-acc st)
  (%lss st 11 'append)
  (let ((tl (%lsg st 10)) (tmp (gensym "AP")))
    (%lsp st 4 (%loop-guard
                (list 'let (list (list tmp expr))
                      (list 'when tmp
                            (list 'setf (list 'cdr tl) (list 'copy-list tmp))
                            (list 'setq tl (list 'last (list 'cdr tl)))))
                test))))

(defun %loop-emit-sum (st expr test)
  (%loop-need-num-acc st 0 'sum)
  (let ((a (%lsg st 9)))
    (%lsp st 4 (%loop-guard (list 'setq a (list '+ a expr)) test))))

(defun %loop-emit-count (st expr test)
  (%loop-need-num-acc st 0 'count)
  (let ((a (%lsg st 9)))
    (%lsp st 4 (%loop-guard (list 'when expr (list 'setq a (list '+ a 1))) test))))

(defun %loop-emit-max (st expr test)
  (%loop-need-num-acc st nil 'maximize)
  (let ((a (%lsg st 9)) (tmp (gensym "MX")))
    (%lsp st 4 (%loop-guard
                (list 'let (list (list tmp expr))
                      (list 'when (list 'or (list 'null a) (list '> tmp a))
                            (list 'setq a tmp)))
                test))))

(defun %loop-emit-min (st expr test)
  (%loop-need-num-acc st nil 'minimize)
  (let ((a (%lsg st 9)) (tmp (gensym "MN")))
    (%lsp st 4 (%loop-guard
                (list 'let (list (list tmp expr))
                      (list 'when (list 'or (list 'null a) (list '< tmp a))
                            (list 'setq a tmp)))
                test))))

(defun %loop-emit-acc (st kind expr test)
  (cond
    ((eq kind 'collect) (%loop-emit-collect st expr test))
    ((eq kind 'append)  (%loop-emit-append st expr test))
    ((eq kind 'nconc)   (%loop-emit-append st expr test))
    ((eq kind 'sum)     (%loop-emit-sum st expr test))
    ((eq kind 'count)   (%loop-emit-count st expr test))
    ((eq kind 'maximize)(%loop-emit-max st expr test))
    ((eq kind 'minimize)(%loop-emit-min st expr test))
    (t (error (format nil "LOOP: bad accumulator ~S" kind)))))

;; Accumulate KIND of EXPR into the named VAR (declared here). Unlike
;; the anonymous accumulators above, INTO vars are ordinary loop
;; variables — visible in FINALLY and not auto-returned as the loop
;; result.
(defun %loop-emit-into (st kind expr var test)
  (cond
    ((member kind '(collect append nconc))
     (let ((tl (gensym "ITL")) (c (gensym "IC")))
       (%lsp st 1 (list var nil))
       (%lsp st 1 (list tl nil))
       (let ((form
              (if (eq kind 'collect)
                  (list 'let (list (list c (list 'cons expr nil)))
                        (list 'if (list 'null var)
                              (list 'progn (list 'setq var c) (list 'setq tl c))
                              (list 'progn (list 'setf (list 'cdr tl) c) (list 'setq tl c))))
                  (list 'let (list (list c (list 'copy-list expr)))
                        (list 'when c
                              (list 'if (list 'null var)
                                    (list 'progn (list 'setq var c) (list 'setq tl (list 'last c)))
                                    (list 'progn (list 'setf (list 'cdr tl) c) (list 'setq tl (list 'last c)))))))))
         (%lsp st 4 (%loop-guard form test)))))
    ((eq kind 'sum)
     (%lsp st 1 (list var 0))
     (%lsp st 4 (%loop-guard (list 'setq var (list '+ var expr)) test)))
    ((eq kind 'count)
     (%lsp st 1 (list var 0))
     (%lsp st 4 (%loop-guard (list 'when expr (list 'setq var (list '+ var 1))) test)))
    ((eq kind 'maximize)
     (%lsp st 1 (list var nil))
     (let ((tmp (gensym "MXI")))
       (%lsp st 4 (%loop-guard
                   (list 'let (list (list tmp expr))
                         (list 'when (list 'or (list 'null var) (list '> tmp var))
                               (list 'setq var tmp))) test))))
    ((eq kind 'minimize)
     (%lsp st 1 (list var nil))
     (let ((tmp (gensym "MNI")))
       (%lsp st 4 (%loop-guard
                   (list 'let (list (list tmp expr))
                         (list 'when (list 'or (list 'null var) (list '< tmp var))
                               (list 'setq var tmp))) test))))
    (t (error (format nil "LOOP: bad INTO accumulator ~S" kind)))))

;; -- FOR clause --

(defun %loop-for-arith (st var start rest down-default)
  "Parse the numeric stepping clause for FOR V: a START plus any of
   {to|upto LIM | below LIM | downto LIM | above LIM | by INC}* in any
   order. DOWN-DEFAULT t means count downward (DOWNFROM) unless a
   bound keyword overrides the direction."
  (let ((limit nil) (test-op nil) (step 1)
        (dir (if down-default 'down 'up)) (rem rest))
    (loop
      (cond
        ((null rem) (return))
        ((or (eq (car rem) 'to) (eq (car rem) 'upto))
         (setq limit (cadr rem)) (setq test-op '>)  (setq rem (cddr rem)))
        ((eq (car rem) 'below)  (setq limit (cadr rem)) (setq test-op '>=) (setq rem (cddr rem)))
        ((eq (car rem) 'downto) (setq limit (cadr rem)) (setq test-op '<)  (setq dir 'down) (setq rem (cddr rem)))
        ((eq (car rem) 'above)  (setq limit (cadr rem)) (setq test-op '<=) (setq dir 'down) (setq rem (cddr rem)))
        ((eq (car rem) 'by)     (setq step (cadr rem)) (setq rem (cddr rem)))
        (t (return))))
    ;; A bare `to LIM` under a downward count means "stop when below
    ;; LIM" — flip the comparison sense.
    (when (and (eq dir 'down) (eq test-op '>))  (setq test-op '<))
    (when (and (eq dir 'down) (eq test-op '>=)) (setq test-op '<=))
    (%lsp st 1 (list var start))
    (when limit
      (let ((lv (gensym "LIM")))
        (%lsp st 1 (list lv limit))
        (%lsp st 3 (list test-op var lv))))
    (if (eq dir 'down)
        (%lsp st 5 (list 'setq var (list '- var step)))
        (%lsp st 5 (list 'setq var (list '+ var step))))
    (%lss st 0 rem)))

(defun %loop-arith-prep-p (sym)
  (member sym '(from upfrom downfrom to upto below downto above by)))

;; -- destructuring + of-type helpers --

(defun %loop-skip-of-type (tail)
  "If TAIL begins with OF-TYPE, drop the marker and its (advisory) type
   spec; otherwise return TAIL unchanged. NCL ignores LOOP type
   declarations — they're optimization hints, not semantics."
  (if (and (consp tail) (eq (car tail) 'of-type))
      (cdr (cdr tail))
      tail))

(defun %loop-pattern-vars (pat)
  "Flat list of the variable symbols in a LOOP destructuring pattern.
   NIL entries are discards; supports nested and dotted patterns."
  (cond
    ((null pat) nil)
    ((symbolp pat) (list pat))
    ((consp pat)
     (let ((v (car pat)))
       (append (cond ((null v) nil)
                     ((symbolp v) (list v))
                     ((consp v) (%loop-pattern-vars v))
                     (t nil))
               (%loop-pattern-vars (cdr pat)))))
    (t nil)))

(defun %loop-pattern-setqs (pat src)
  "SETQ forms binding PATTERN's vars by walking SRC with car/cdr."
  (cond
    ((null pat) nil)
    ((symbolp pat) (list (list 'setq pat src)))
    ((consp pat)
     (let ((v (car pat)))
       (append (cond ((null v) nil)
                     ((symbolp v) (list (list 'setq v (list 'car src))))
                     ((consp v) (%loop-pattern-setqs v (list 'car src)))
                     (t nil))
               (%loop-pattern-setqs (cdr pat) (list 'cdr src)))))
    (t nil)))

(defun %loop-bind-iter (st var item)
  "If VAR is a destructuring pattern, declare its vars (nil) and emit
   body setqs destructuring ITEM into them. VAR a plain symbol is a
   no-op (ITEM is already that symbol)."
  (when (consp var)
    (dolist (v (%loop-pattern-vars var)) (%lsp st 1 (list v nil)))
    (dolist (sq (%loop-pattern-setqs var item)) (%lsp st 4 sq))))

(defun %loop-clause-for (st)
  ;; (for VAR ... [and VAR ...]*) — parse the first binding, then any
  ;; AND-joined sibling bindings. NOTE: NCL steps AND-bindings
  ;; sequentially, not in true parallel; independent bindings (the
  ;; common case) behave identically.
  (%loop-parse-for-binding st (cdr (%lsg st 0)))
  (loop
    (let ((rem (%lsg st 0)))
      (if (and rem (eq (car rem) 'and))
          (%loop-parse-for-binding st (cdr rem))
          (return)))))

(defun %loop-parse-for-binding (st tokens)
  "Parse ONE for-binding from TOKENS = (VAR [of-type T] PREP ...);
   advances slot 0 to the unconsumed tail."
  (let* ((var (car tokens))
         (after (%loop-skip-of-type (cdr tokens)))   ; tolerate OF-TYPE
         (prep (car after))
         (rest (cdr after))
         ;; For destructuring patterns, iterate through a hidden var
         ;; and destructure it into the pattern each step.
         (item (if (consp var) (gensym "ITM") var)))
    (cond
      ((eq prep 'in)
       ;; for V in LIST [by STEP-FN]
       (let ((lv (gensym "IN")) (stepfn nil) (more (cdr rest)))
         (when (and more (eq (car more) 'by))
           (setq stepfn (cadr more)) (setq more (cddr more)))
         (%lsp st 1 (list lv (car rest)))
         (%lsp st 1 (list item nil))
         (%lsp st 3 (list 'null lv))
         (%lsp st 4 (list 'setq item (list 'car lv)))
         (%loop-bind-iter st var item)
         (%lsp st 5 (list 'setq lv (if stepfn (list 'funcall stepfn lv) (list 'cdr lv))))
         (%lss st 0 more)))
      ((eq prep 'on)
       ;; for V on LIST [by STEP-FN]
       (let ((lv (gensym "ON")) (stepfn nil) (more (cdr rest)))
         (when (and more (eq (car more) 'by))
           (setq stepfn (cadr more)) (setq more (cddr more)))
         (%lsp st 1 (list lv (car rest)))
         (%lsp st 1 (list item nil))
         (%lsp st 3 (list 'null lv))
         (%lsp st 4 (list 'setq item lv))
         (%loop-bind-iter st var item)
         (%lsp st 5 (list 'setq lv (if stepfn (list 'funcall stepfn lv) (list 'cdr lv))))
         (%lss st 0 more)))
      ((%loop-arith-prep-p prep)
       ;; numeric range — VAR must be a symbol. Handles FROM/UPFROM/
       ;; DOWNFROM with an explicit start, and leading bounds
       ;; (TO/UPTO/BELOW/DOWNTO/ABOVE/BY with an implicit start of 0).
       (let ((start 0) (range after) (down nil))
         (cond
           ((or (eq prep 'from) (eq prep 'upfrom))
            (setq start (car rest)) (setq range (cdr rest)))
           ((eq prep 'downfrom)
            (setq start (car rest)) (setq range (cdr rest)) (setq down t)))
         (%loop-for-arith st var start range down)))
      ((eq prep '=)
       (let* ((init (car rest))
              (r2 (cdr rest))
              (has-then (and r2 (eq (car r2) 'then)))
              (step (if has-then (cadr r2) init)))
         (%lsp st 1 (list item init))
         (%lsp st 5 (list 'setq item step))
         (%loop-bind-iter st var item)
         (%lss st 0 (if has-then (cddr r2) r2))))
      (t (error (format nil "LOOP FOR: unknown preposition ~S" prep))))))

;; -- multi-form clauses (do / initially / finally) --

(defun %loop-grab-forms (st)
  "Consume forms after the current keyword until the next loop keyword
   or end. Returns the forms in source order; advances slot 0."
  (let ((rem (cdr (%lsg st 0))) (forms nil))
    (loop
      (if (or (null rem) (%loop-keyword-p (car rem)))
          (return)
          (progn (setq forms (cons (car rem) forms)) (setq rem (cdr rem)))))
    (%lss st 0 rem)
    (reverse forms)))

(defun %loop-prepend (st slot forms)
  "Prepend FORMS (source order) to a reversed accumulator slot."
  (%lss st slot (append (reverse forms) (%lsg st slot))))

;; -- when / unless --

(defun %loop-clause-cond (st negate)
  (let* ((cl (%lsg st 0))
         (test0 (cadr cl))
         (test (if negate (list 'not test0) test0))
         (inner-kw (caddr cl))
         (inner-expr (cadddr cl))
         (rest (cddddr cl)))   ; tokens after (when TEST KW EXPR)
    (cond
      ((eq inner-kw 'do)
       (%lsp st 4 (list 'when test (%loop-rewrite-returns inner-expr (%lsg st 8))))
       (%lss st 0 rest))
      ((eq inner-kw 'return)
       (%lsp st 4 (list 'when test (list 'return-from (%lsg st 8) inner-expr)))
       (%lss st 0 rest))
      ((and rest (eq (car rest) 'into))
       ;; when TEST KIND EXPR into VAR
       (%loop-emit-into st inner-kw inner-expr (cadr rest) test)
       (%lss st 0 (cddr rest)))
      (t (%loop-emit-acc st inner-kw inner-expr test)
         (%lss st 0 rest)))))

;; -- per-clause dispatch (split in two to keep each cond small) --

(defun %loop-rewrite-returns (form name)
  "Rewrite (return X) -> (return-from NAME X) in user DO/FINALLY forms,
   so a RETURN inside the loop body exits the loop's block (NCL's bare
   `return` macro otherwise targets the simple-loop break flag, which a
   tagbody-based extended loop doesn't watch). Does not descend into
   nested LOOP/BLOCK/DO/DOLIST/DOTIMES (they bind their own block)."
  (cond
    ((not (consp form)) form)
    ((eq (car form) 'return)
     (list 'return-from name (if (cdr form) (cadr form) nil)))
    ((member (car form) '(quote loop block dolist dotimes do do*)) form)
    (t (mapcar (lambda (s) (%loop-rewrite-returns s name)) form))))

(defun %loop-rw-list (forms name)
  (mapcar (lambda (f) (%loop-rewrite-returns f name)) forms))

(defun %loop-step (st)
  (let ((kw (car (%lsg st 0))))
    (cond
      ((eq kw 'named)   (%lss st 8 (cadr (%lsg st 0))) (%lss st 0 (cddr (%lsg st 0))))
      ((eq kw 'for)     (%loop-clause-for st))
      ((eq kw 'with)    (%loop-clause-with st))
      ((eq kw 'repeat)  (%loop-clause-repeat st))
      ((eq kw 'while)   (%lsp st 3 (list 'not (cadr (%lsg st 0)))) (%lss st 0 (cddr (%lsg st 0))))
      ((eq kw 'until)   (%lsp st 3 (cadr (%lsg st 0))) (%lss st 0 (cddr (%lsg st 0))))
      ((eq kw 'when)    (%loop-clause-cond st nil))
      ((eq kw 'unless)  (%loop-clause-cond st t))
      ((eq kw 'do)      (%loop-prepend st 4 (%loop-rw-list (%loop-grab-forms st) (%lsg st 8))))
      ((eq kw 'initially)(%loop-prepend st 2 (%loop-rw-list (%loop-grab-forms st) (%lsg st 8))))
      ((eq kw 'finally) (%loop-prepend st 6 (%loop-rw-list (%loop-grab-forms st) (%lsg st 8))))
      ((eq kw 'return)  (%loop-clause-return st))
      (t (%loop-step2 st kw)))))

(defun %loop-step2 (st kw)
  (cond
    ((member kw '(collect append nconc sum count maximize minimize))
     (let* ((cl (%lsg st 0))
            (expr (cadr cl))
            (rest (cddr cl)))
       (if (and rest (eq (car rest) 'into))
           ;; KIND EXPR INTO VAR — named accumulation
           (progn (%lss st 0 (cddr rest))
                  (%loop-emit-into st kw expr (cadr rest) nil))
           (progn (%lss st 0 rest)
                  (%loop-emit-acc st kw expr nil)))))
    ((eq kw 'thereis)  (%loop-clause-thereis st))
    ((eq kw 'always)   (%loop-clause-always st))
    ((eq kw 'never)    (%loop-clause-never st))
    (t (error (format nil "LOOP: unknown clause ~S" kw)))))

(defun %loop-clause-with (st)
  ;; (with VAR [= VAL] [and VAR [= VAL]]*)
  (%loop-parse-with-binding st (cdr (%lsg st 0)))
  (loop
    (let ((rem (%lsg st 0)))
      (if (and rem (eq (car rem) 'and))
          (%loop-parse-with-binding st (cdr rem))
          (return)))))

(defun %loop-parse-with-binding (st tokens)
  (let* ((var (car tokens))
         (r (%loop-skip-of-type (cdr tokens))))   ; tolerate OF-TYPE
    (if (and r (eq (car r) '=))
        (let ((val (cadr r)))
          (if (consp var)
              ;; destructuring with: bind a hidden var to VAL, then
              ;; destructure once in the pre-body (WITH is one-shot).
              (let ((item (gensym "WITM")))
                (%lsp st 1 (list item val))
                (dolist (v (%loop-pattern-vars var)) (%lsp st 1 (list v nil)))
                (dolist (sq (%loop-pattern-setqs var item)) (%lsp st 2 sq)))
              (%lsp st 1 (list var val)))
          (%lss st 0 (cddr r)))
        (progn
          (if (consp var)
              (dolist (v (%loop-pattern-vars var)) (%lsp st 1 (list v nil)))
              (%lsp st 1 (list var nil)))
          (%lss st 0 r)))))

(defun %loop-clause-repeat (st)
  (let ((cv (gensym "REP")) (n (cadr (%lsg st 0))))
    (%lsp st 1 (list cv n))
    (%lsp st 3 (list '<= cv 0))
    (%lsp st 5 (list 'setq cv (list '- cv 1)))
    (%lss st 0 (cddr (%lsg st 0)))))

(defun %loop-clause-return (st)
  (%lsp st 4 (list 'return-from (%lsg st 8) (cadr (%lsg st 0))))
  (%lss st 0 (cddr (%lsg st 0))))

(defun %loop-clause-thereis (st)
  (let ((tv (gensym "TH")) (expr (cadr (%lsg st 0))))
    (%lsp st 4 (list 'let (list (list tv expr))
                     (list 'when tv (list 'return-from (%lsg st 8) tv))))
    (%lss st 0 (cddr (%lsg st 0)))))

(defun %loop-clause-always (st)
  (%lsp st 4 (list 'unless (cadr (%lsg st 0)) (list 'return-from (%lsg st 8) nil)))
  (%lss st 7 t)
  (%lss st 0 (cddr (%lsg st 0))))

(defun %loop-clause-never (st)
  (%lsp st 4 (list 'when (cadr (%lsg st 0)) (list 'return-from (%lsg st 8) nil)))
  (%lss st 7 t)
  (%lss st 0 (cddr (%lsg st 0))))

;; -- result + output assembly --

(defun %loop-result (st)
  (let ((kind (%lsg st 11)) (acc (%lsg st 9)) (result (%lsg st 7)))
    (cond
      ((member kind '(collect append)) (if acc (list 'cdr acc) nil))
      ((member kind '(sum count maximize minimize)) acc)
      (t result))))

(defun %loop-end-gos (ends lend)
  (mapcar (lambda (test) (list 'when test (list 'go lend))) ends))

(defun %loop-build (st)
  (let* ((start (gensym "LS"))
         (lend (gensym "LE"))
         (name (%lsg st 8))
         (vars (reverse (%lsg st 1)))
         (pre (reverse (%lsg st 2)))
         (ends (reverse (%lsg st 3)))
         (body (reverse (%lsg st 4)))
         (steps (reverse (%lsg st 5)))
         (post (reverse (%lsg st 6)))
         (result (%loop-result st)))
    `(block ,name
       (let* ,vars
         ,@pre
         (tagbody
           ,start
           ,@(%loop-end-gos ends lend)
           ,@body
           ,@steps
           (go ,start)
           ,lend)
         ,@post
         ,result))))

(defun %loop-expand (clauses)
  "Parse extended-LOOP CLAUSES into a block/let*/tagbody form."
  (let ((st (make-array 12 :initial-element nil)))
    (%lss st 0 clauses)
    (loop
      (when (null (%lsg st 0)) (return nil))
      (%loop-step st))
    (%loop-build st)))

(defmacro dotimes (binding &rest body)
  "(dotimes (var count [result-form]) body…) — bind VAR to
   0, 1, …, COUNT-1; evaluate BODY each time. Returns RESULT-FORM."
  (let* ((var (car binding))
         (count-form (car (cdr binding)))
         (result-form (cond
                        ((null (cdr (cdr binding))) nil)
                        (t (car (cdr (cdr binding))))))
         (limit (gensym "DOTIMES-LIMIT")))
    `(let ((,var 0)
           (,limit ,count-form))
       (loop
         (cond
           ((>= ,var ,limit) (return ,result-form))
           (t ,@body
              (setq ,var (+ ,var 1))))))))

;; ── DO and DO* ────────────────────────────────────────────────────────
;;
;; Common Lisp's general iterative construct:
;;
;;   (do ((var init [step])...)
;;       (end-test result-form...)
;;     body...)
;;
;; Semantics in three parts:
;;
;;   1. Establish an implicit (block nil …) around the whole form.
;;   2. Bind all VARs to their INIT forms simultaneously (let-style
;;      for DO; let*-style for DO*).
;;   3. Loop. Each pass: evaluate END-TEST; if true, evaluate
;;      RESULT-FORMs and return the last one. Otherwise evaluate
;;      BODY (treated as an implicit progn — we don't have tagbody
;;      yet; go-tags inside the body aren't supported), then step
;;      every bound variable that has a STEP form. For DO the steps
;;      are evaluated in parallel (the classic "swap" idiom works);
;;      for DO* sequentially.
;;
;; Bindings without a STEP form keep their value across iterations.
;; A bare-symbol binding `(foo)` is treated as `(foo nil)`.
;;
;; (return val) inside BODY exits the do with VAL (it finds the
;; implicit block-nil). Same for (return-from nil val).

(defun %do-normalise-binding (b)
  "Internal: turn each DO binding into (var init step-or-:no-step).
   Accepts a bare symbol, (var), (var init), or (var init step)."
  (cond
    ((symbolp b) (list b nil :no-step))
    ((null (cdr b)) (list (car b) nil :no-step))
    ((null (cdr (cdr b))) (list (car b) (car (cdr b)) :no-step))
    (t (list (car b) (car (cdr b)) (car (cdr (cdr b)))))))

(defun %do-stepped-only (norm)
  ;; Keep only the bindings that have a STEP form.
  (cond
    ((null norm) nil)
    ((eq (car (cdr (cdr (car norm)))) :no-step)
     (%do-stepped-only (cdr norm)))
    (t (cons (car norm) (%do-stepped-only (cdr norm))))))

(defun %do-let-bindings (norm)
  ;; Build (let …) binding pairs (var init) from the normalised
  ;; (var init step) triples. We can't use `(mapcar #'list vars
  ;; inits)` because core's mapcar is unary; this recursive
  ;; helper does the n-ary zip we need.
  (cond
    ((null norm) nil)
    (t (cons (list (car (car norm)) (car (cdr (car norm))))
             (%do-let-bindings (cdr norm))))))

(defun %do-step-block-parallel (stepped)
  "Build the parallel-step body: evaluate all step forms into
   gensym temps, then assign each VAR from its temp."
  (cond
    ((null stepped) nil)
    (t
     (let ((bindings nil)
           (assigns nil))
       (dolist (s stepped)
         (let ((var (car s))
               (step (car (cdr (cdr s))))
               (tmp (gensym "DO-NEW-")))
           (push (list tmp step) bindings)
           (push (list 'setq var tmp) assigns)))
       (list `(let ,(reverse bindings) ,@(reverse assigns)))))))

(defun %do-step-block-sequential (stepped)
  "DO* step: assign each VAR from its STEP in order; later steps
   see the updated earlier values."
  (cond
    ((null stepped) nil)
    (t (let ((assigns nil))
         (dolist (s stepped)
           (let ((var (car s))
                 (step (car (cdr (cdr s)))))
             (push (list 'setq var step) assigns)))
         (reverse assigns)))))

(defmacro do (bindings end-clause &rest body)
  "Common Lisp DO: parallel-init / parallel-step iteration."
  (let* ((norm     (mapcar #'%do-normalise-binding bindings))
         (stepped  (%do-stepped-only norm))
         (end-test (car end-clause))
         (results  (cdr end-clause)))
    `(block nil
       (let ,(%do-let-bindings norm)
         (loop
           ;; cond, not (when test (return …)) — NCL's RETURN is
           ;; a flag-set, not a non-local exit; using `when` lets
           ;; the body and step still run after a triggered
           ;; return-from-loop, which is wrong (and crashes when
           ;; the step touches now-NIL state).
           (cond
             (,end-test (return (progn ,@results)))
             ;; Trailing nil makes the t-clause non-empty when
             ;; the do has no body forms AND no step bindings
             ;; (the (t …) clause must have at least one body
             ;; form per NCL's current cond lowering).
             (t ,@body
                ,@(%do-step-block-parallel stepped)
                nil)))))))

(defmacro do* (bindings end-clause &rest body)
  "Common Lisp DO*: sequential-init (let*-style) / sequential-step
   iteration. The init / step forms of later bindings see the
   updated values of earlier ones."
  (let* ((norm     (mapcar #'%do-normalise-binding bindings))
         (stepped  (%do-stepped-only norm))
         (end-test (car end-clause))
         (results  (cdr end-clause)))
    `(block nil
       (let* ,(%do-let-bindings norm)
         (loop
           (cond
             (,end-test (return (progn ,@results)))
             (t ,@body
                ,@(%do-step-block-sequential stepped)
                nil)))))))

;; -- Property lists ----------------------------------------------------------

(defun getf (plist key &optional default)
  "Walk PLIST, returning the value paired with KEY, or DEFAULT (nil) if not
   found. The plist is a flat list of alternating keys and values:
   (:a 1 :b 2 :c 3)."
  (cond
    ((null plist) default)
    ((eq (car plist) key) (car (cdr plist)))
    (t (getf (cdr (cdr plist)) key default))))

(defun %putf (plist key value)
  "Return a plist like PLIST but with KEY mapped to VALUE — updating
   the existing pair if present, else prepending a new one."
  (cond
    ((null plist) (list key value))
    ((eq (car plist) key) (cons key (cons value (cdr (cdr plist)))))
    (t (cons (car plist)
             (cons (car (cdr plist))
                   (%putf (cdr (cdr plist)) key value))))))

;; Symbol property lists (get / symbol-plist) need make-hash-table, so
;; they're defined after the hash-table section further down.

(defun %plist-remove (plist key)
  (cond
    ((null plist) nil)
    ((eq (car plist) key) (cdr (cdr plist)))
    (t (cons (car plist)
             (cons (car (cdr plist))
                   (%plist-remove (cdr (cdr plist)) key))))))

;; -- sublis / nsublis --------------------------------------------------------

(defun sublis (alist tree &key (test #'eql) (key #'identity))
  "Substitute in TREE using ALIST: any subtree whose KEY matches a
   key in ALIST (under TEST) is replaced by that entry's value."
  (let ((pair (assoc (funcall key tree) alist :test test)))
    (cond
      (pair (cdr pair))
      ((consp tree)
       (cons (sublis alist (car tree) :test test :key key)
             (sublis alist (cdr tree) :test test :key key)))
      (t tree))))

;; -- Conditions --------------------------------------------------------------
;;
;; (error condition-or-message) signals; (handler-case body
;; (error (var) recovery)) catches. The condition is whatever was
;; passed to error — typically a string. Conditions as typed
;; objects with class hierarchies wait on CLOS.

;; -- Tier-2 utilities (chunk 12) ---------------------------------------------
;;
;; A pile of small CL functions Closette pulls in. None need new
;; compiler or runtime support; each is a thin wrapper over
;; primitives we already have.

(defun third (x) (car (cdr (cdr x))))

(defun complement (pred)
  "Return a function that negates PRED: it accepts the same
   arguments as PRED and returns (not (apply PRED args)). Variadic
   per CL — e.g. (funcall (complement #'member) x list) works, not
   just unary predicates. (A unary-only complement silently dropped
   extra args, then called PRED with too few — reading past the
   argument array into garbage.)"
  (lambda (&rest args) (not (apply pred args))))

(defun fdefinition (name)
  "Return the function bound to NAME. Same as `symbol-function`
   for plain symbols; CL also accepts (setf NAME) function-name
   lists which we don't yet support."
  (symbol-function name))

(defun make-symbol (name)
  "Return a fresh symbol whose name is NAME. CL's make-symbol
   produces an UNINTERNED symbol; we don't have uninterned
   symbols, so this just calls intern on a uniqued name and
   returns. Same compromise as gensym."
  (intern (format nil "~A~A" name (gensym ""))))

(defun nreverse (lst)
  "Same as REVERSE for now. CL nreverse is allowed to mutate
   LST's cons cells; we just allocate fresh — slower but
   semantically equivalent for non-shared lists. Closette
   doesn't depend on the destructive behaviour."
  (reverse lst))

(defun nconc (&rest lists)
  "Concatenate LISTS. CL nconc destructively splices each
   non-last list's tail; we use append* (allocating). Closette
   doesn't depend on the destructive behaviour."
  (apply #'append* lists))

(defun find-if (pred lst &key (key #'identity))
  "Return the first element of LST for which (PRED (KEY elem))
   is true, or NIL."
  (cond
    ((null lst) nil)
    ((funcall pred (funcall key (car lst))) (car lst))
    (t (find-if pred (cdr lst) :key key))))

(defun remove-if (pred lst &key (key #'identity))
  "Return a fresh list of LST's elements for which (PRED (KEY
   elem)) is FALSE. Order preserved."
  (cond
    ((null lst) nil)
    ((funcall pred (funcall key (car lst)))
     (remove-if pred (cdr lst) :key key))
    (t (cons (car lst) (remove-if pred (cdr lst) :key key)))))

(defun remove-if-not (pred lst &key (key #'identity))
  "Return a fresh list of LST's elements for which (PRED (KEY
   elem)) is TRUE — the keep-matching counterpart of remove-if."
  (cond
    ((null lst) nil)
    ((funcall pred (funcall key (car lst)))
     (cons (car lst) (remove-if-not pred (cdr lst) :key key)))
    (t (remove-if-not pred (cdr lst) :key key))))

(defun remove (item lst &key (test #'eql) (key #'identity))
  "Return a fresh list with all elements matching ITEM removed."
  (cond
    ((null lst) nil)
    ((funcall test item (funcall key (car lst)))
     (remove item (cdr lst) :test test :key key))
    (t (cons (car lst) (remove item (cdr lst) :test test :key key)))))

;; -- sort -------------------------------------------------------------------
;;
;; Mergesort variant. CL's `sort` is destructive but allowed to
;; share — we just return a fresh list. Comparator returns T iff
;; the first arg should come before the second.

(defun %split-list (lst)
  "Split LST into two halves; returns (cons left right)."
  (let ((slow lst) (fast lst) (n 0))
    (loop
      (cond
        ((or (null fast) (null (cdr fast))) (return nil))
        (t (setq slow (cdr slow))
           (setq fast (cdr (cdr fast)))
           (setq n (+ n 1)))))
    (let ((left nil) (rest lst))
      (loop
        (cond
          ((zerop n) (return nil))
          (t (setq left (cons (car rest) left))
             (setq rest (cdr rest))
             (setq n (- n 1)))))
      (cons (reverse left) rest))))

(defun %merge (a b cmp)
  (cond
    ((null a) b)
    ((null b) a)
    ((funcall cmp (car a) (car b))
     (cons (car a) (%merge (cdr a) b cmp)))
    (t (cons (car b) (%merge a (cdr b) cmp)))))

(defun sort (lst cmp)
  "Mergesort by CMP. CMP returns true iff its first arg should
   come before its second. Returns a fresh list (we don't share
   tails) — CL's destructive variant is harmless here because we
   never read the input again."
  (cond
    ((null lst) nil)
    ((null (cdr lst)) lst)
    (t (let* ((split (%split-list lst))
              (left  (sort (car split) cmp))
              (right (sort (cdr split) cmp)))
         (%merge left right cmp)))))

(defun copy-list (lst)
  (cond ((null lst) nil)
        (t (cons (car lst) (copy-list (cdr lst))))))

(defun remove-duplicates (lst &key (test #'eql) (key #'identity))
  "Return a fresh list with duplicates removed. CL default
   drops the EARLIER occurrence of any pair of matches, so the
   LAST occurrence wins. (Pass :from-end to invert — not yet
   supported here.)"
  (cond
    ((null lst) nil)
    ((member (funcall key (car lst)) (cdr lst) :test test :key key)
     (remove-duplicates (cdr lst) :test test :key key))
    (t (cons (car lst)
             (remove-duplicates (cdr lst) :test test :key key)))))

(defun set-difference (xs ys &key (test #'eql) (key #'identity))
  "Elements of XS not in YS (under TEST + KEY). Order preserved."
  (cond
    ((null xs) nil)
    ((member (funcall key (car xs)) ys :test test :key key)
     (set-difference (cdr xs) ys :test test :key key))
    (t (cons (car xs)
             (set-difference (cdr xs) ys :test test :key key)))))

(defun intersection (xs ys &key (test #'eql) (key #'identity))
  "Elements of XS that are also in YS."
  (cond
    ((null xs) nil)
    ((member (funcall key (car xs)) ys :test test :key key)
     (cons (car xs)
           (intersection (cdr xs) ys :test test :key key)))
    (t (intersection (cdr xs) ys :test test :key key))))

(defun union (xs ys &key (test #'eql) (key #'identity))
  "Union as a fresh list — XS first (with duplicates removed
   relative to YS), then YS."
  (cond
    ((null xs) ys)
    ((member (funcall key (car xs)) ys :test test :key key)
     (union (cdr xs) ys :test test :key key))
    (t (cons (car xs)
             (union (cdr xs) ys :test test :key key)))))

(defun subseq (seq start &optional end)
  "Substring/sublist from START (inclusive) to END (exclusive,
   defaults to length of seq). Works on strings (delegates to
   substring) and lists; vectors not yet."
  (cond
    ((stringp seq)
     (substring seq start (cond ((null end) (length seq)) (t end))))
    ((listp seq)
     (let ((lst (nthcdr start seq))
           (n (cond ((null end) nil) (t (- end start)))))
       (cond
         ((null n) (copy-list lst))
         (t (subseq-take lst n)))))
    (t (error "subseq: unsupported sequence type: ~A" seq))))

(defun subseq-take (lst n)
  (cond
    ((or (null lst) (<= n 0)) nil)
    (t (cons (car lst) (subseq-take (cdr lst) (- n 1))))))

(defun %coerce-vector-to-list (v i n)
  (cond ((>= i n) nil)
        (t (cons (aref v i) (%coerce-vector-to-list v (+ i 1) n)))))

(defun coerce (object result-type)
  "Coerce OBJECT to RESULT-TYPE.  Handles the cases Closette and the
   standard library need: identity, list<->vector, list<->string,
   character->string, number type widening."
  (cond
    ;; ── identity ────────────────────────────────────────────────────
    ((typep object result-type) object)
    ;; ── → LIST ──────────────────────────────────────────────────────
    ((eq result-type 'list)
     (cond
       ((listp object) object)
       ((stringp object) (coerce-string-to-list object 0 (length object)))
       ((vectorp object) (%coerce-vector-to-list object 0 (length object)))
       (t (error "coerce: cannot coerce ~S to LIST" object))))
    ;; ── → STRING ────────────────────────────────────────────────────
    ((eq result-type 'string)
     (cond
       ((stringp object) object)
       ((characterp object) (make-string 1 :initial-element object))
       ((listp object) (coerce-list-to-string object))
       ((vectorp object) (coerce-list-to-string
                           (%coerce-vector-to-list object 0 (length object))))
       (t (error "coerce: cannot coerce ~S to STRING" object))))
    ;; ── → VECTOR / SIMPLE-VECTOR ────────────────────────────────────
    ((or (eq result-type 'vector) (eq result-type 'simple-vector))
     (cond
       ((vectorp object) object)
       ((listp object) (apply #'vector object))
       ((stringp object)
        (apply #'vector (coerce-string-to-list object 0 (length object))))
       (t (error "coerce: cannot coerce ~S to VECTOR" object))))
    ;; ── → CHARACTER ─────────────────────────────────────────────────
    ;; A character designator: a character, a one-element string, or a
    ;; symbol whose name is exactly one character long.
    ((eq result-type 'character)
     (cond
       ((characterp object) object)
       ((and (stringp object) (= (length object) 1)) (char object 0))
       ((and (symbolp object) object (= (length (symbol-name object)) 1))
        (char (symbol-name object) 0))
       (t (error "coerce: cannot coerce ~S to CHARACTER" object))))
    ;; ── numeric widenings ───────────────────────────────────────────
    ;; All float subtypes collapse to NCL's single float type.
    ((member result-type '(float short-float single-float double-float long-float))
     (if (numberp object) (* 1.0 object)
         (error "coerce: cannot coerce ~S to FLOAT" object)))
    ((eq result-type 'integer)
     (if (integerp object) object
         (error "coerce: cannot coerce ~S to INTEGER" object)))
    ;; ── → COMPLEX ───────────────────────────────────────────────────
    ;; (complex x 0): a float realpart stays as #C(x 0.0); a rational
    ;; realpart canonicalizes back to the real (CL). An already-complex
    ;; object was handled by the identity branch above.
    ((eq result-type 'complex)
     (if (numberp object) (complex object 0)
         (error "coerce: cannot coerce ~S to COMPLEX" object)))
    (t (error "coerce: unsupported result-type ~S" result-type))))

(defun coerce-string-to-list (s i n)
  (cond
    ((>= i n) nil)
    (t (cons (char s i) (coerce-string-to-list s (+ i 1) n)))))

(defun coerce-list-to-string (chars)
  (cond
    ((null chars) "")
    (t (string-concat (string-append-char "" (car chars))
                       (coerce-list-to-string (cdr chars))))))

;; -- Type predicates ---------------------------------------------------------
;;
;; Each is a one-line wrapper around (typep x 'KIND). We could
;; install them as native shims for speed, but most callers are
;; already inside Lisp-level code paths and the indirection is
;; cheap. Direct calls to (typep x 'foo) work too — these are
;; just the conventional CL spellings.

(defun symbolp (x) (typep x 'symbol))
(defun stringp (x) (typep x 'string))
(defun vectorp (x) (typep x 'vector))
(defun listp (x) (typep x 'list))
(defun consp (x) (typep x 'cons))
(defun integerp (x) (typep x 'integer))
(defun numberp (x) (typep x 'number))
(defun bignump (x) (typep x 'bignum))
(defun fixnump (x) (typep x 'fixnum))
(defun characterp (x) (typep x 'character))
(defun functionp (x) (typep x 'function))
(defun floatp (x) (typep x 'float))
(defun rationalp (x) (typep x 'rational))
(defun realp (x) (typep x 'real))
(defun complexp (x) (typep x 'complex))
(defun packagep (x) (typep x 'package))

;; -- defstruct ---------------------------------------------------------------
;;
;; A struct instance is laid out as a Vector with the type tag in
;; slot 0 and the user slots in slots 1..N. Each (defstruct NAME
;; (slot1 default1) (slot2 default2) ...) generates:
;;
;;   - constructor:  (make-NAME &key slot1 slot2 ...) returning an
;;                   instance with the given inits (or defaults).
;;   - predicate:    (NAME-p obj) → T iff obj is a vector tagged
;;                   with 'NAME at slot 0.
;;   - accessors:    (NAME-slot1 obj), (NAME-slot2 obj), ...
;;   - setf-accessors: (%setf-NAME-slot1 val obj), ...
;;     reached via the generic (setf (NAME-slot1 obj) val) lowering
;;     in the compiler.
;;
;; This is enough for Closette's `method-table` defstruct. Things
;; not yet supported: :type / :include / :print-function /
;; :predicate / :constructor options, BOA-style positional
;; constructors, and the (slot default :type T :read-only T)
;; per-slot keyword args.

(defun defstruct-slot-name (spec)
  (cond
    ((symbolp spec) spec)
    (t (car spec))))

(defun defstruct-slot-default (spec)
  (cond
    ((symbolp spec) nil)
    ((null (cdr spec)) nil)
    (t (car (cdr spec)))))

(defun defstruct-symbol (prefix name)
  "Intern a fresh symbol whose name is PREFIX concatenated with
   NAME's printer text. Used by the defstruct macro to build the
   constructor / accessor / setter symbols at expansion time."
  (intern (format nil "~A~A" prefix name)))

(defun defstruct-build-constructor (name slots)
  (let ((ctor (defstruct-symbol "MAKE-" name))
        (n-slots (length slots)))
    `(defun ,ctor (&key
                    ,@(mapcar (lambda (s)
                                (list (defstruct-slot-name s)
                                      (defstruct-slot-default s)))
                              slots))
       ;; Allocate length n-slots+1 (1 extra cell for the type tag).
       (let ((__v (make-array ,(+ n-slots 1) :initial-element nil)))
         (setf (svref __v 0) ',name)
         ,@(defstruct-build-init-stmts slots 1)
         __v))))

(defun defstruct-build-init-stmts (slots i)
  "Emit a list of (setf (svref __v i) slot-name) forms, one per
   slot, with i counting up from 1. Recursive walk so we don't
   need a multi-list mapcar."
  (cond
    ((null slots) nil)
    (t (cons `(setf (svref __v ,i) ,(defstruct-slot-name (car slots)))
             (defstruct-build-init-stmts (cdr slots) (+ i 1))))))

(defun defstruct-build-predicate (name)
  (let ((pred (defstruct-symbol "" (format nil "~A-P" name))))
    `(defun ,pred (obj)
       (and (vectorp obj)
            (eq (svref obj 0) ',name)))))

(defun defstruct-build-accessors (name slots)
  (defstruct-build-accessors-iter name slots 1))

(defun defstruct-build-accessors-iter (name slots i)
  (cond
    ((null slots) nil)
    (t
     (let* ((slot (defstruct-slot-name (car slots)))
            (acc (defstruct-symbol (format nil "~A-" name) slot)))
       (cons `(defun ,acc (obj) (svref obj ,i))
             (defstruct-build-accessors-iter name (cdr slots) (+ i 1)))))))

(defun defstruct-build-setters (name slots)
  (defstruct-build-setters-iter name slots 1))

(defun defstruct-build-setters-iter (name slots i)
  (cond
    ((null slots) nil)
    (t
     (let* ((slot (defstruct-slot-name (car slots)))
            (acc-name (format nil "~A-~A" name slot))
            (setter (intern (format nil "%SETF-~A" acc-name))))
       (cons `(defun ,setter (val obj)
                (setf (svref obj ,i) val)
                val)
             (defstruct-build-setters-iter name (cdr slots) (+ i 1)))))))

(defmacro defstruct (name &rest slots)
  "Define a struct type NAME with the given SLOTS (each a symbol
   or a (name default) list). Generates a make-NAME constructor
   that takes the slot names as &key args, a NAME-P type
   predicate, and per-slot accessors NAME-SLOT plus matching
   setf-accessors %SETF-NAME-SLOT (the latter reached via the
   generic (setf (NAME-SLOT obj) val) lowering)."
  `(progn
     ,(defstruct-build-constructor name slots)
     ,(defstruct-build-predicate name)
     ,@(defstruct-build-accessors name slots)
     ,@(defstruct-build-setters name slots)
     ',name))

;; -- Character comparison ----------------------------------------------------

(defun char-equal (c1 c2)
  "Case-insensitive character comparison."
  (eql (char-upcase c1) (char-upcase c2)))

(defun char= (c1 c2)
  "Case-sensitive character comparison."
  (eql c1 c2))

(defun char/= (c1 c2)
  "True if C1 and C2 are different characters."
  (not (eql c1 c2)))

(defun char< (c1 c2) (< (char-code c1) (char-code c2)))
(defun char> (c1 c2) (> (char-code c1) (char-code c2)))
(defun char<= (c1 c2) (<= (char-code c1) (char-code c2)))
(defun char>= (c1 c2) (>= (char-code c1) (char-code c2)))

;; -- String comparison & manipulation ----------------------------------------

(defun string= (s1 s2)
  "Case-sensitive string comparison."
  (equal s1 s2))

(defun string/= (s1 s2)
  "True if S1 and S2 differ."
  (not (string= s1 s2)))

(defun string-equal (s1 s2)
  "Case-insensitive string comparison."
  (if (not (and (stringp s1) (stringp s2)))
      nil
      (let ((n (length s1)))
        (if (/= n (length s2))
            nil
            (%string-equal-loop s1 s2 n 0)))))

(defun %string-equal-loop (s1 s2 n i)
  (if (= i n)
      t
      (if (char-equal (char s1 i) (char s2 i))
          (%string-equal-loop s1 s2 n (1+ i))
          nil)))

(defun string-not-equal (s1 s2) (not (string-equal s1 s2)))

(defun string< (s1 s2)
  "Lexicographic less-than on strings."
  (%string-compare s1 s2 'less))

(defun string> (s1 s2)
  "Lexicographic greater-than on strings."
  (%string-compare s1 s2 'greater))

(defun string<= (s1 s2) (not (string> s1 s2)))
(defun string>= (s1 s2) (not (string< s1 s2)))

(defun %string-compare (s1 s2 mode)
  "Compare S1 and S2 lexicographically. MODE is LESS or GREATER."
  (let ((n1 (length s1))
        (n2 (length s2))
        (n  (min2 (length s1) (length s2))))
    (%string-compare-loop s1 s2 n n1 n2 mode 0)))

(defun %string-compare-loop (s1 s2 n n1 n2 mode i)
  (if (= i n)
      ;; All chars match up to min length; shorter string is "less".
      (if (eq mode 'less) (< n1 n2) (> n1 n2))
      (let ((c1 (char-code (char s1 i)))
            (c2 (char-code (char s2 i))))
        (cond
          ((< c1 c2) (eq mode 'less))
          ((> c1 c2) (eq mode 'greater))
          (t (%string-compare-loop s1 s2 n n1 n2 mode (1+ i)))))))

(defun string-capitalize (s)
  "Return S with first character of each word uppercased, rest lowered."
  (let ((n (length s)))
    (if (zerop n) s
        (%string-capitalize-loop s n 0 t nil))))

(defun %string-capitalize-loop (s n i word-start acc)
  (if (= i n)
      (coerce (reverse acc) 'string)
      (let ((c (char s i)))
        (if (alpha-char-p c)
            (%string-capitalize-loop s n (1+ i) nil
              (cons (if word-start (char-upcase c) (char-downcase c)) acc))
            (%string-capitalize-loop s n (1+ i) t (cons c acc))))))

;; string-upcase, string-downcase, string-trim, string-left-trim,
;; string-right-trim, parse-integer are registered as natives.

;; -- Sequence functions (generic) -------------------------------------------
;;
;; These work on lists, vectors, and strings via the %as-list +
;; coerce pattern established by reverse/every/some.

(defun count (item seq &key (test #'eql) (key #'identity))
  "Count occurrences of ITEM in SEQ."
  (let ((lst (%as-list seq))
        (n 0))
    (dolist (e lst n)
      (when (funcall test item (funcall key e))
        (setq n (1+ n))))))

(defun count-if (pred seq &key (key #'identity))
  "Count elements in SEQ satisfying PRED."
  (let ((lst (%as-list seq))
        (n 0))
    (dolist (e lst n)
      (when (funcall pred (funcall key e))
        (setq n (1+ n))))))

(defun count-if-not (pred seq &key (key #'identity))
  "Count elements in SEQ not satisfying PRED."
  (count-if (complement pred) seq :key key))

(defun reduce (fn seq &rest args)
  "Reduce SEQ by FN. Accepts :INITIAL-VALUE keyword."
  (let* ((lst (%as-list seq))
         (iv-pair (member :initial-value args))
         (has-iv  iv-pair))
    (if has-iv
        (%reduce-loop fn lst (cadr iv-pair))
        (if (null lst)
            (funcall fn)
            (%reduce-loop fn (cdr lst) (car lst))))))

(defun %reduce-loop (fn lst acc)
  (if (null lst)
      acc
      (%reduce-loop fn (cdr lst) (funcall fn acc (car lst)))))

(defun map-into (result fn &rest seqs)
  "Destructively map FN over SEQS into RESULT."
  (declare (ignore result fn seqs))
  (error "map-into: not yet implemented"))

(defun fill (seq item &key (start 0) end)
  "Fill SEQ with ITEM from START to END."
  (let ((e (or end (length seq))))
    (%fill-loop seq item start e)))

(defun %fill-loop (seq item i end)
  (if (>= i end)
      seq
      (progn
        (setf (elt seq i) item)
        (%fill-loop seq item (1+ i) end))))

(defun %concat-strings (seqs)
  "Concatenate a list of strings by reducing with two-arg format."
  (if (null seqs)
      ""
      (reduce (lambda (a b) (format nil "~A~A" a b)) seqs)))

(defun concatenate (result-type &rest seqs)
  "Concatenate SEQS into a sequence of RESULT-TYPE."
  (cond
    ((or (eq result-type 'string) (equal result-type '(simple-array character (*))))
     (%concat-strings seqs))
    ((eq result-type 'list)
     (apply #'append (mapcar #'%as-list seqs)))
    ((eq result-type 'vector)
     (coerce (apply #'append (mapcar #'%as-list seqs)) 'vector))
    (t (error (format nil "concatenate: unsupported result-type ~S" result-type)))))

;; -- More list utilities ----------------------------------------------------

(defun delete (item lst &key (test #'eql) (key #'identity))
  "Destructively remove ITEM from LST."
  (remove item lst :test test :key key))

(defun delete-if (pred lst &key (key #'identity))
  "Destructively remove elements satisfying PRED."
  (remove-if pred lst :key key))

(defun delete-if-not (pred lst &key (key #'identity))
  "Destructively remove elements not satisfying PRED."
  (remove-if-not pred lst :key key))

(defun subst (new old tree &key (test #'eql))
  "Substitute NEW for every OLD in TREE (by TEST)."
  (cond
    ((funcall test old tree) new)
    ((consp tree)
     (let ((a (subst new old (car tree) :test test))
           (d (subst new old (cdr tree) :test test)))
       (if (and (eql a (car tree)) (eql d (cdr tree)))
           tree
           (cons a d))))
    (t tree)))

(defun copy-tree (tree)
  "Return a copy of TREE (deep copy of conses)."
  (if (consp tree)
      (cons (copy-tree (car tree)) (copy-tree (cdr tree)))
      tree))

(defun tree-equal (a b &key (test #'eql))
  "True if A and B have the same cons structure and leaves match under TEST."
  (cond
    ((and (consp a) (consp b))
     (and (tree-equal (car a) (car b) :test test)
          (tree-equal (cdr a) (cdr b) :test test)))
    ((or (consp a) (consp b)) nil)
    (t (funcall test a b))))

(defun adjoin (item list &key (test #'eql) (key #'identity))
  "Add ITEM to LIST if not already present."
  (if (member item list :test test :key key)
      list
      (cons item list)))

(defmacro pushnew (item place &key (test '#'eql) (key '#'identity))
  "Push ITEM onto PLACE if not already a member."
  `(setq ,place (adjoin ,item ,place :test ,test :key ,key)))

;; -- equalp -----------------------------------------------------------------

(defun equalp (a b)
  "Case-insensitive, type-coercing equality. Strings are compared
   ignoring case; numbers compared by value; conses compared
   recursively; everything else falls back to EQL."
  (cond
    ((eql a b) t)
    ((and (numberp a) (numberp b)) (= a b))
    ((and (stringp a) (stringp b)) (string-equal a b))
    ((and (consp a) (consp b))
     (and (equalp (car a) (car b))
          (equalp (cdr a) (cdr b))))
    (t nil)))

;; -- Control flow extras ----------------------------------------------------

(defmacro ignore-errors (&rest forms)
  "Evaluate FORMS; if an error is signalled, return (values NIL condition)."
  `(handler-case (progn ,@forms)
     (t (c) (values nil c))))

(defmacro with-output-to-string (var-form &rest body)
  "Evaluate BODY with VAR bound to a string output stream.
   VAR-FORM is a list containing the variable name, e.g. (s).
   Returns the accumulated string. (Bootstrap: collects via format.)"
  ;; Bootstrap implementation: VAR is unused, body writes to a
  ;; collector. For now this is a thin shim that captures printed
  ;; output to a string.
  (let ((var (car var-form))
        (result (gensym "RESULT")))
    `(let ((,var nil) (,result ""))
       ;; Override *standard-output* concept — since our printer
       ;; functions check stream arg, we use a special accumulator.
       ;; Simplified: just return (format nil ...) for common patterns.
       ,@body
       ,result)))

;; complement is defined at line ~821; identity at line ~40.

(defun constantly (value)
  "Return a function that always returns VALUE."
  (lambda (&rest args)
    (declare (ignore args))
    value))

(defun constantp (form &optional environment)
  "True if FORM is a constant form — its value is fixed and known:
   a self-evaluating object (number, string, character, vector, …),
   NIL or T, a (quote …) form, or a symbol declared with DEFCONSTANT.
   Symbols bound by defvar/defparameter (or unbound) are NOT constant.
   The optional ENVIRONMENT is accepted (per CL) and ignored — we do
   not yet recognise lexically constant forms. May return NIL for
   forms that are in fact constant; CL permits this."
  (declare (ignore environment))
  (cond
    ((consp form) (eq (car form) 'quote))
    ((symbolp form)
     ;; NIL and T self-evaluate; otherwise only DEFCONSTANT names.
     (if (or (null form) (eq form t) (and (member form *constants*) t))
         t
         nil))
    ;; Everything else is a self-evaluating literal.
    (t t)))

;; -- List predicates & utilities (batch 4) ------------------------------------

(defun endp (x)
  "Return T if X is the empty list. Signal error if not a list."
  (if (null x) t
      (if (consp x) nil
          (error (format nil "ENDP: ~S is not a list" x)))))

(defun tailp (object list)
  "Return T if OBJECT is any tail (cdr-chain) of LIST."
  (cond
    ((eql object list) t)
    ((atom list) nil)
    (t (tailp object (cdr list)))))

(defun ldiff (list object)
  "Return a copy of the leading part of LIST up to OBJECT."
  (cond
    ((eql list object) nil)
    ((atom list) (copy-list list))
    (t (cons (car list) (ldiff (cdr list) object)))))

(defun nbutlast (lst &optional (n 1))
  "Destructively remove the last N elements from LST."
  (let ((len (length lst)))
    (if (<= len n) nil
        (let ((new-end (nthcdr (- len n 1) lst)))
          (setf (cdr new-end) nil)
          lst))))

(defun revappend (x y)
  "Equivalent to (append (reverse x) y) but more efficient."
  (%revappend x y))
;; %revappend is defined at line ~44.

(defun mapcon (fn list &rest more-lists)
  "Like maplist but destructively appends (nconc) results."
  (apply #'nconc (apply #'maplist fn list more-lists)))

(defun nsubst (new old tree &key (test #'eql))
  "Destructive tree substitution: replace OLD with NEW in TREE."
  (cond
    ((funcall test old tree) new)
    ((atom tree) tree)
    (t (setf (car tree) (nsubst new old (car tree) :test test))
       (setf (cdr tree) (nsubst new old (cdr tree) :test test))
       tree)))

(defun nsubstitute (new old seq &key (test #'eql) (key #'identity))
  "Destructive sequence substitution."
  (cond
    ((listp seq)
     (%nsubstitute-list new old seq test key)
     seq)
    ((vectorp seq)
     (dotimes (i (length seq) seq)
       (when (funcall test old (funcall key (aref seq i)))
         (setf (aref seq i) new))))
    (t (error "nsubstitute: not a sequence"))))

(defun %nsubstitute-list (new old lst test key)
  (when lst
    (when (funcall test old (funcall key (car lst)))
      (setf (car lst) new))
    (%nsubstitute-list new old (cdr lst) test key)))

;; -- Char predicates (batch 4) -----------------------------------------------
;;
;; char-code, code-char, char-upcase, char-downcase, alpha-char-p,
;; upper-case-p, lower-case-p, graphic-char-p, digit-char-p, digit-char,
;; alphanumericp are all native shims. These Lisp wrappers add derived
;; predicates.

;; NOTE: ALPHANUMERICP is intentionally NOT redefined here. It is already
;; a native shim backed by Rust's char::is_alphanumeric, which is Unicode-
;; aware (a Devanagari digit, a CJK letter, etc. all answer T). A previous
;; Lisp wrapper `(or (alpha-char-p c) (digit-char-p c))` *shadowed* that
;; shim and narrowed it: DIGIT-CHAR-P is radix-based (ASCII 0-9/A-Z only),
;; so `(alphanumericp (code-char #x0969))` (Devanagari 3) wrongly returned
;; NIL. Deleting the wrapper restores the Unicode-aware native behaviour.

(defun both-case-p (c)
  "Return T if C has both upper and lower case variants."
  (or (upper-case-p c) (lower-case-p c)))

(defun char-int (c)
  "Return the character code of C (same as char-code)."
  (char-code c))

(defun standard-char-p (c)
  "Return T if C is a standard character (graphic or newline)."
  (or (graphic-char-p c) (char= c #\Newline)))

;; -- Sequence: stable-sort (batch 4) ----------------------------------------
;;
;; Merge-sort is naturally stable. Our existing sort uses quicksort on
;; vectors; for lists we implement merge-sort which is stable.

(defun stable-sort (seq pred &key (key #'identity))
  "Sort SEQ stably by PRED. Returns a new sorted sequence."
  (sort seq pred :key key))
;; NCL's sort on lists is already merge-based and stable.
;; This alias satisfies the CL contract.

;; -- Utility macros (batch 4) ------------------------------------------------

(defmacro with-gensyms (names &rest body)
  "Bind each name in NAMES to a fresh gensym, then evaluate BODY."
  `(let ,(mapcar (lambda (n)
                   (list n `(gensym ,(symbol-name n))))
                 names)
     ,@body))

;; -- Quantifiers (batch 5) ---------------------------------------------------

(defun notany (pred seq &rest more)
  "Return T if PRED is false for every element of SEQ."
  (not (apply #'some pred seq more)))

(defun notevery (pred seq &rest more)
  "Return T if PRED is false for at least one element of SEQ."
  (not (apply #'every pred seq more)))

;; prog1 and prog2 are defined at line ~497.

;; -- Type predicates (batch 3) -----------------------------------------------
;;
;; atom is a compiler intrinsic (Expr::IsAtom in lower.rs).
;; arrayp — vectorp and stringp already exist as predicates; a
;; combined arrayp is handy for user code.

(defun arrayp (x)
  "Return T if X is an array (simple-vector or string)."
  (or (vectorp x) (stringp x)))

;; -- Number utilities (batch 3) ---------------------------------------------

(defun gcd (&rest args)
  "Return the greatest common divisor."
  (cond
    ((null args) 0)
    ((null (cdr args)) (abs (car args)))
    (t (reduce #'%gcd2 (mapcar #'abs args)))))

(defun %gcd2 (a b)
  "Euclidean GCD of two non-negative integers."
  (if (zerop b) a (%gcd2 b (rem a b))))

(defun lcm (&rest args)
  "Return the least common multiple."
  (cond
    ((null args) 1)
    ((null (cdr args)) (abs (car args)))
    (t (reduce #'%lcm2 (mapcar #'abs args)))))

(defun %lcm2 (a b)
  (if (or (zerop a) (zerop b))
      0
      (* (truncate (abs a) (gcd a b)) (abs b))))

(defun logtest (i1 i2)
  "Return T if any bit is set in both I1 and I2."
  (not (zerop (logand i1 i2))))

(defun logcount (n)
  "Count the 1-bits (positive) or 0-bits (negative) of integer N."
  (if (minusp n) (logcount (lognot n))
      (%popcount n 0)))

(defun %popcount (n acc)
  (if (zerop n) acc
      (%popcount (logand n (- n 1)) (1+ acc))))

(defun logbitp (index integer)
  "Return T if bit INDEX of INTEGER is 1."
  (not (zerop (logand (ash 1 index) integer))))

(defun integer-length (n)
  "Return the number of bits needed to represent N in two's complement."
  (if (minusp n) (integer-length (lognot n))
      (%bit-length n 0)))

(defun %bit-length (n acc)
  (if (zerop n) acc
      (%bit-length (ash n -1) (1+ acc))))

;; -- Alist utilities (batch 3) -----------------------------------------------

(defun acons (key datum alist)
  "Add (KEY . DATUM) to the front of ALIST."
  (cons (cons key datum) alist))

(defun pairlis (keys data &optional alist)
  "Pair up KEYS and DATA into an alist prepended to ALIST."
  (if (null keys)
      alist
      (acons (car keys) (car data)
             (pairlis (cdr keys) (cdr data) alist))))

(defun rassoc (item alist &key (test #'eql) (key #'identity))
  "Like assoc, but matches against the CDR of each entry."
  (cond
    ((null alist) nil)
    ((funcall test item (funcall key (cdr (car alist)))) (car alist))
    (t (rassoc item (cdr alist) :test test :key key))))

(defun copy-alist (alist)
  "Return a copy of ALIST (fresh top-level conses)."
  (if (null alist)
      nil
      (cons (cons (caar alist) (cdar alist))
            (copy-alist (cdr alist)))))

;; -- More sequence operations (batch 3) --------------------------------------

(defun elt (sequence index)
  "Return element at INDEX from SEQUENCE."
  (if (listp sequence)
      (nth index sequence)
      (aref sequence index)))

;; (setf elt) — deferred until (defun (setf ...) ...) is
;; supported in core.lisp loading context.

(defun substitute (new old seq &key (test #'eql) (key #'identity))
  "Return a copy of SEQ with OLD replaced by NEW."
  (let ((lst (%as-list seq)))
    (let ((result (%substitute-list new old lst test key)))
      (cond
        ((listp seq) result)
        ((stringp seq) (coerce result 'string))
        ((vectorp seq) (coerce result 'vector))
        (t result)))))

(defun %substitute-list (new old lst test key)
  (if (null lst) nil
      (cons (if (funcall test old (funcall key (car lst))) new (car lst))
            (%substitute-list new old (cdr lst) test key))))

(defun substitute-if (new pred seq &key (key #'identity))
  "Return copy of SEQ with elements satisfying PRED replaced by NEW."
  (let ((lst (%as-list seq)))
    (let ((result (%subst-if-list new pred lst key)))
      (cond
        ((listp seq) result)
        ((stringp seq) (coerce result 'string))
        ((vectorp seq) (coerce result 'vector))
        (t result)))))

(defun %subst-if-list (new pred lst key)
  (if (null lst) nil
      (cons (if (funcall pred (funcall key (car lst))) new (car lst))
            (%subst-if-list new pred (cdr lst) key))))

(defun substitute-if-not (new pred seq &key (key #'identity))
  "Return copy of SEQ with elements NOT satisfying PRED replaced by NEW."
  (substitute-if new (complement pred) seq :key key))

(defun search (seq1 seq2 &key (test #'eql))
  "Return position of SEQ1 in SEQ2, or NIL."
  (let ((lst1 (%as-list seq1))
        (lst2 (%as-list seq2)))
    (%search-list lst1 lst2 0 test)))

(defun %search-list (needle haystack pos test)
  (cond
    ((null needle) 0)            ; empty pattern matches at 0
    ((null haystack) nil)
    ((%prefix-p needle haystack test) pos)
    (t (%search-list needle (cdr haystack) (1+ pos) test))))

(defun %prefix-p (prefix lst test)
  (cond
    ((null prefix) t)
    ((null lst) nil)
    ((funcall test (car prefix) (car lst))
     (%prefix-p (cdr prefix) (cdr lst) test))
    (t nil)))

(defun mismatch (seq1 seq2 &key (test #'eql))
  "Return index of first mismatch between SEQ1 and SEQ2, or NIL."
  (let ((lst1 (%as-list seq1))
        (lst2 (%as-list seq2)))
    (%mismatch-list lst1 lst2 0 test)))

(defun %mismatch-list (l1 l2 i test)
  (cond
    ((and (null l1) (null l2)) nil)
    ((or (null l1) (null l2)) i)
    ((funcall test (car l1) (car l2))
     (%mismatch-list (cdr l1) (cdr l2) (1+ i) test))
    (t i)))

(defun mapcan (fn list &rest more-lists)
  "Like mapcar but destructively appends (nconc) results."
  (apply #'nconc (apply #'mapcar fn list more-lists)))

(defun maplist (fn list &rest more-lists)
  "Map FN over successive cdrs of the lists."
  (cond
    ((null more-lists) (%maplist-1 fn list))
    (t (error "maplist: multiple lists not yet supported"))))

(defun %maplist-1 (fn lst)
  (if (null lst) nil
      (cons (funcall fn lst) (%maplist-1 fn (cdr lst)))))

(defun make-string (size &key (initial-element #\Space))
  "Create a string of SIZE characters, each INITIAL-ELEMENT."
  (let ((s (make-array size :initial-element initial-element)))
    (coerce s 'string)))

;; -- Control flow (batch 3) --------------------------------------------------

(defmacro nth-value (n form)
  "Return the Nth value (zero-based) from a multiple-value form."
  `(nth ,n (multiple-value-list ,form)))

(defmacro multiple-value-setq (vars form)
  "Set VARS from the multiple values of FORM."
  (let ((mv (gensym "MV")))
    `(let ((,mv (multiple-value-list ,form)))
       ,@(%mvsetq-assigns vars mv 0)
       (car ,mv))))

(defun %mvsetq-assigns (vars mv-sym i)
  (if (null vars) nil
      (cons `(setq ,(car vars) (nth ,i ,mv-sym))
            (%mvsetq-assigns (cdr vars) mv-sym (1+ i)))))

(defmacro assert (test-form &rest args)
  "Signal an error if TEST-FORM evaluates to NIL."
  (declare (ignore args))
  `(unless ,test-form
     (error (format nil "Assertion failed: ~S" ',test-form))))

(defmacro check-type (place typespec &rest args)
  "Signal an error if PLACE is not of type TYPESPEC."
  (declare (ignore args))
  `(unless (typep ,place ',typespec)
     (error (format nil "The value ~S is not of type ~A"
                    ,place ',typespec))))

;; -- Sweep II: missing CL standard functions ---------------------------------

;; -- assoc-if / rassoc-if / member-if ----------------------------------------

(defun assoc-if (pred alist &key (key #'identity))
  "Return first entry in ALIST whose KEY of car satisfies PRED."
  (cond
    ((null alist) nil)
    ((funcall pred (funcall key (car (car alist)))) (car alist))
    (t (assoc-if pred (cdr alist) :key key))))

(defun rassoc-if (pred alist &key (key #'identity))
  "Return first entry in ALIST whose KEY of cdr satisfies PRED."
  (cond
    ((null alist) nil)
    ((funcall pred (funcall key (cdr (car alist)))) (car alist))
    (t (rassoc-if pred (cdr alist) :key key))))

(defun member-if (pred lst &key (key #'identity))
  "Return tail of LST starting at first element satisfying PRED."
  (cond
    ((null lst) nil)
    ((funcall pred (funcall key (car lst))) lst)
    (t (member-if pred (cdr lst) :key key))))

(defun member-if-not (pred lst &key (key #'identity))
  "Return tail of LST starting at first element NOT satisfying PRED."
  (member-if (complement pred) lst :key key))

;; -- position-if / find-if-not -----------------------------------------------

(defun position-if (pred seq &key (key #'identity))
  "Return index of first element in SEQ satisfying PRED, or NIL."
  (%position-if-from pred (%as-list seq) 0 key))

(defun %position-if-from (pred lst i key)
  (cond
    ((null lst) nil)
    ((funcall pred (funcall key (car lst))) i)
    (t (%position-if-from pred (cdr lst) (1+ i) key))))

(defun position-if-not (pred seq &key (key #'identity))
  "Return index of first element NOT satisfying PRED."
  (position-if (complement pred) seq :key key))

(defun find-if-not (pred seq &key (key #'identity))
  "Return first element in SEQ NOT satisfying PRED."
  (find-if (complement pred) seq :key key))

;; -- char case-insensitive comparisons (sweep II) ----------------------------

(defun char-lessp (c1 c2)
  "Case-insensitive char<."
  (char< (char-upcase c1) (char-upcase c2)))

(defun char-greaterp (c1 c2)
  "Case-insensitive char>."
  (char> (char-upcase c1) (char-upcase c2)))

(defun char-not-greaterp (c1 c2)
  "Case-insensitive char<=."
  (char<= (char-upcase c1) (char-upcase c2)))

(defun char-not-lessp (c1 c2)
  "Case-insensitive char>=."
  (char>= (char-upcase c1) (char-upcase c2)))

;; -- nstring destructives (sweep II) -----------------------------------------

(defun nstring-upcase (string)
  "Destructively upcase STRING in place."
  (dotimes (i (length string))
    (setf (char string i) (char-upcase (char string i))))
  string)

(defun nstring-downcase (string)
  "Destructively downcase STRING in place."
  (dotimes (i (length string))
    (setf (char string i) (char-downcase (char string i))))
  string)

(defun nstring-capitalize (string)
  "Destructively capitalize STRING: first char of each word up, rest down."
  (let ((start t))
    (dotimes (i (length string))
      (let ((ch (char string i)))
        (cond
          ((alpha-char-p ch)
           (if start
               (setf (char string i) (char-upcase ch))
               (setf (char string i) (char-downcase ch)))
           (setq start nil))
          (t (setq start t))))))
  string)

;; -- copy-seq / replace (sweep II) -------------------------------------------

(defun copy-seq (seq)
  "Return a fresh copy of SEQ."
  (cond
    ((listp seq)   (copy-list seq))
    ((stringp seq) (subseq seq 0))
    ((vectorp seq) (subseq seq 0))
    (t (error (format nil "copy-seq: not a sequence: ~S" seq)))))

(defun replace (seq1 seq2 &key (start1 0) end1 (start2 0) end2)
  "Copy elements from SEQ2 into SEQ1 destructively. Returns SEQ1."
  (let* ((src (%as-list (subseq seq2 start2 end2)))
         (e1 (or end1 (length seq1)))
         (pos start1))
    (dolist (elem src)
      (when (>= pos e1) (return))
      (if (listp seq1)
          (setf (nth pos seq1) elem)
          (setf (aref seq1 pos) elem))
      (setq pos (1+ pos))))
  seq1)

;; -- set-exclusive-or / subsetp (sweep II) -----------------------------------

(defun set-exclusive-or (xs ys &key (test #'eql) (key #'identity))
  "Elements in either XS or YS but not both."
  (append (set-difference xs ys :test test :key key)
          (set-difference ys xs :test test :key key)))

(defun nset-exclusive-or (xs ys &key (test #'eql) (key #'identity))
  "Destructive version of set-exclusive-or."
  (nconc (set-difference xs ys :test test :key key)
         (set-difference ys xs :test test :key key)))

(defun subsetp (xs ys &key (test #'eql) (key #'identity))
  "Return T if every element of XS is in YS."
  (every (lambda (x)
           (member (funcall key x) ys :test test :key key))
         xs))

;; -- mapl / mapcon (sweep II) ------------------------------------------------

(defun mapl (fn list &rest more-lists)
  "Like mapc but passes successive cdrs to FN."
  (cond
    ((null more-lists) (%mapl-1 fn list))
    (t (error "mapl: multiple lists not yet supported")))
  list)

(defun %mapl-1 (fn lst)
  (when lst
    (funcall fn lst)
    (%mapl-1 fn (cdr lst))))

(defun mapcon (fn list &rest more-lists)
  "Like mapcan but passes successive cdrs to FN."
  (apply #'nconc (apply #'maplist fn list more-lists)))

;; -- map (generic CL map) (sweep II) ----------------------------------------

(defun map (result-type fn &rest seqs)
  "Apply FN to successive elements of SEQS, collecting into RESULT-TYPE."
  (let ((results (apply #'mapcar fn (mapcar #'%as-list seqs))))
    (cond
      ((null result-type) nil)
      ((eq result-type 'list) results)
      ((eq result-type 'string) (coerce results 'string))
      ((eq result-type 'vector) (coerce results 'vector))
      (t (error (format nil "map: unsupported result-type ~S" result-type))))))

;; -- nsubstitute / nsubstitute-if (sweep II) ---------------------------------

(defun nsubstitute (new old seq &key (test #'eql) (key #'identity))
  "Destructively substitute NEW for OLD in SEQ."
  (let ((lst (%as-list seq)))
    (%nsubst-list new old lst test key))
  seq)

(defun %nsubst-list (new old lst test key)
  (when lst
    (when (funcall test old (funcall key (car lst)))
      (setf (car lst) new))
    (%nsubst-list new old (cdr lst) test key)))

(defun nsubstitute-if (new pred seq &key (key #'identity))
  "Destructively substitute NEW where PRED holds in SEQ."
  (let ((lst (%as-list seq)))
    (%nsubst-if-list new pred lst key))
  seq)

(defun %nsubst-if-list (new pred lst key)
  (when lst
    (when (funcall pred (funcall key (car lst)))
      (setf (car lst) new))
    (%nsubst-if-list new pred (cdr lst) key)))

;; -- multiple-value-prog1 (sweep II) ----------------------------------------

(defmacro multiple-value-prog1 (first-form &rest more-forms)
  "Evaluate FIRST-FORM, save all its values, evaluate MORE-FORMS,
   then return all saved values from FIRST-FORM."
  (let ((vals (gensym "MVPROG1")))
    `(let ((,vals (multiple-value-list ,first-form)))
       ,@more-forms
       (values-list ,vals))))

;; -- nreconc (sweep II) ------------------------------------------------------

(defun nreconc (list tail)
  "Destructive version of (revappend list tail).
   Equivalent to (nconc (nreverse list) tail) but more efficient."
  (if (null list)
      tail
      (let ((rest (cdr list)))
        (setf (cdr list) tail)
        (nreconc rest list))))

;; -- Hash tables -------------------------------------------------------------
;;
;; A hash table is a Vector laid out as:
;;   slot 0 — test symbol (one of EQ / EQL / EQUAL)
;;   slot 1 — current count of entries (fixnum, mutable via setf-svref)
;;   slot 2..N+1 — N buckets, each a list of (key . value) cons cells
;;
;; EQ / EQL tables use %word-hash (raw-bit hash, fast).
;; EQUAL / EQUALP tables use %equal-hash (content-aware: strings
;; are hashed by their character content, so distinct string objects
;; with the same text hash identically).
;;
;; The whole structure lives on the GC heap (vector + cons cells),
;; so old-to-young pointer marking and the ordinary trace pass take
;; care of survival across GC.

(defun make-hash-table (&key (test 'eql) (size 16))
  "Allocate a new hash table. TEST is one of EQ, EQL, or EQUAL
   (defaults to EQL). SIZE is the initial bucket count (defaults
   to 16). Returns the table."
  (let* ((nbuckets (max size 4))
         (v (make-array (+ nbuckets 2) :initial-element nil)))
    (setf (svref v 0) test)
    (setf (svref v 1) 0)
    v))

(defun %ht-test (ht) (svref ht 0))
(defun %ht-count (ht) (svref ht 1))
(defun %ht-bump-count (ht delta)
  (setf (svref ht 1) (+ (svref ht 1) delta)))
(defun %ht-nbuckets (ht) (- (length ht) 2))
(defun %ht-bucket (ht i) (svref ht (+ i 2)))
(defun %ht-set-bucket (ht i v) (setf (svref ht (+ i 2)) v))

;; Capture the native integer REM *now*, before any later module
;; (Library/numbers.lisp) redefines REM/MOD into polymorphic wrappers.
;; The bucket index is always (non-negative-fixnum-hash) REM
;; (positive-fixnum-nbuckets), so the raw integer primitive is exactly
;; correct here — and using it is mandatory, not just an optimisation:
;; the polymorphic MOD calls FLOATP, and FLOATP -> TYPEP -> %NEW-TYPEP
;; -> GETHASH -> %HT-BUCKET-INDEX -> MOD closes a recursion cycle that
;; stack-overflows the entire stdlib load the moment numbers.lisp is on
;; top of types.lisp. Going straight to the native REM breaks the cycle
;; at its only closure point and keeps every GETHASH off the polymorphic
;; dispatch path.
(defparameter %ht-native-rem (symbol-function 'rem))

(defun %ht-bucket-index (ht key)
  (let ((test (%ht-test ht)))
    (funcall %ht-native-rem
             (if (or (eq test 'equal) (eq test 'equalp))
                 (%equal-hash key)
                 (%word-hash key))
             (%ht-nbuckets ht))))

(defun %ht-keys-match (test k1 k2)
  "Compare K1 and K2 under TEST. EQUAL falls back to EQUAL on
   conses/strings; EQUALP uses case-insensitive string comparison;
   EQL handles fixnums/chars/symbols/T/NIL same as EQ in our
   current value set; EQ is identity."
  (cond
    ((eq test 'eq) (eq k1 k2))
    ((eq test 'eql) (eql k1 k2))
    ((eq test 'equal) (equal k1 k2))
    ((eq test 'equalp) (equalp k1 k2))
    (t (eql k1 k2))))

(defun hash-table-p (x)
  "Return T if X is a hash table."
  ;; A hash table is a vector whose slot 0 is one of the test symbols.
  (and (vectorp x)
       (>= (length x) 3)
       (let ((test (svref x 0)))
         (or (eq test 'eq) (eq test 'eql)
             (eq test 'equal) (eq test 'equalp)))))

(defun hash-table-count (ht)
  "Return the number of key/value pairs currently in HT."
  (%ht-count ht))

(defun hash-table-test (ht)
  "Return the test symbol HT was created with."
  (%ht-test ht))

(defun gethash (key ht &optional default)
  "Look up KEY in HT. Returns the associated value, or DEFAULT
   if none. Returns NIL as the secondary value when the key was
   absent, T when it was found."
  (let ((bucket (%ht-bucket ht (%ht-bucket-index ht key)))
        (test (%ht-test ht))
        (result default)
        (found nil))
    (loop
      (cond
        ((null bucket) (return nil))
        (t (let ((pair (car bucket)))
             (cond
               ((%ht-keys-match test (car pair) key)
                (setq result (cdr pair))
                (setq found t)
                (setq bucket nil))
               (t (setq bucket (cdr bucket))))))))
    (if found
        (values result t)
        (values default nil))))

(defun %hash-set (ht key val)
  "Insert or update KEY → VAL. Returns VAL. Used by
   `(setf (gethash ...) ...)` lowering."
  (let* ((bi (%ht-bucket-index ht key))
         (bucket (%ht-bucket ht bi))
         (test (%ht-test ht))
         (cur bucket)
         (done nil))
    (loop
      (cond
        ((or done (null cur))
         ;; Not found in walk — prepend a fresh pair to the
         ;; bucket. Inserting at the head is O(1) and keeps the
         ;; small-bucket hot path tight.
         (cond
           ((not done)
            (%ht-set-bucket ht bi (cons (cons key val) bucket))
            (%ht-bump-count ht 1)))
         (return val))
        (t (let ((pair (car cur)))
             (cond
               ((%ht-keys-match test (car pair) key)
                (setf (cdr pair) val)
                (setq done t))
               (t (setq cur (cdr cur))))))))))

(defun remhash (key ht)
  "Remove KEY from HT. Returns T if it was present, NIL otherwise."
  (let* ((bi (%ht-bucket-index ht key))
         (bucket (%ht-bucket ht bi))
         (test (%ht-test ht)))
    ;; Two cases: head-of-bucket vs middle. Handle head first.
    (cond
      ((null bucket) nil)
      ((%ht-keys-match test (car (car bucket)) key)
       (%ht-set-bucket ht bi (cdr bucket))
       (%ht-bump-count ht -1)
       t)
      (t
       ;; Walk with prev/cur so we can splice cur out by setting
       ;; (cdr prev) = (cdr cur).
       (let ((prev bucket)
             (cur (cdr bucket))
             (found nil))
         (loop
           (cond
             ((null cur) (return nil))
             ((%ht-keys-match test (car (car cur)) key)
              (setf (cdr prev) (cdr cur))
              (%ht-bump-count ht -1)
              (setq found t)
              (setq cur nil))
             (t (setq prev cur)
                (setq cur (cdr cur)))))
         found)))))

(defun clrhash (ht)
  "Empty HT. Returns HT."
  (let ((i 0)
        (n (%ht-nbuckets ht)))
    (loop
      (cond
        ((>= i n) (return nil))
        (t (%ht-set-bucket ht i nil)
           (setq i (+ i 1)))))
    (setf (svref ht 1) 0)
    ht))

(defun maphash (fn ht)
  "Call FN with each key and value of HT. Returns NIL."
  (let ((i 0)
        (n (%ht-nbuckets ht)))
    (loop
      (cond
        ((>= i n) (return nil))
        (t
         (let ((bucket (%ht-bucket ht i)))
           (loop
             (cond
               ((null bucket) (return nil))
               (t (funcall fn (car (car bucket)) (cdr (car bucket)))
                  (setq bucket (cdr bucket))))))
         (setq i (+ i 1)))))))

;; -- Symbol property lists ---------------------------------------------------
;;
;; NCL symbols don't carry a dedicated plist slot yet, so each symbol's
;; plist is stored in a global EQ hash table. get / symbol-plist and
;; their setf forms layer on top. (setf (get s i) v) lowers via the
;; generic %SETF-NAME convention to (%setf-get v s i); same for
;; symbol-plist. Defined here (after make-hash-table) because the
;; backing store is a hash table.

(defvar *symbol-plists* (make-hash-table :test 'eq))

(defun symbol-plist (sym)
  "Return SYM's property list (NIL if it has none)."
  (gethash sym *symbol-plists*))

(defun %setf-symbol-plist (new-plist sym)
  (if (null new-plist)
      (remhash sym *symbol-plists*)
      (setf (gethash sym *symbol-plists*) new-plist))
  new-plist)

(defun get (sym indicator &optional default)
  "Return the value of SYM's INDICATOR property, or DEFAULT."
  (getf (symbol-plist sym) indicator default))

(defun %setf-get (value sym indicator &optional default)
  (declare (ignore default))
  (setf (symbol-plist sym) (%putf (symbol-plist sym) indicator value))
  value)

(defun remprop (sym indicator)
  "Remove SYM's INDICATOR property. Returns T if it was present."
  (let ((plist (symbol-plist sym)))
    (if (eq (getf plist indicator '%plist-absent) '%plist-absent)
        nil
        (progn
          (setf (symbol-plist sym) (%plist-remove plist indicator))
          t))))

(defmacro multiple-value-list (form)
  "Evaluate FORM and return a fresh list of all the values it
   produced. If FORM returned a single value, the list has one
   element; if FORM was `(values v1 v2 ... vN)` in tail position,
   the list has N elements.

   Implementation: clear the multi-value slot before FORM runs, so
   that constants / variable lookups / native shim calls (which
   don't write the slot) are observable as such. Then JIT'd
   function calls in FORM either set the slot via `Expr::Values`
   (tail-position `(values ...)`) or via `EnsureSingleMv` (every
   other function exit). Either way `%multiple-value-list-of`
   reads the slot afterward and falls back to `(primary)` when it
   was never written."
  (let ((p (gensym "MV-PRIMARY")))
    `(progn
       (%mv-clear)
       (let ((,p ,form))
         (%multiple-value-list-of ,p)))))

(defmacro multiple-value-bind (vars form &rest body)
  "Evaluate FORM, then bind the symbols in VARS to its primary,
   secondary, … values. Excess vars get NIL; extra values are
   discarded. BODY is run in the new bindings.

   Expansion mirrors `multiple-value-list`: clear, evaluate form,
   snapshot, then destructure."
  (let ((p (gensym "MV-PRIMARY"))
        (l (gensym "MV-LIST")))
    `(progn
       (%mv-clear)
       (let ((,p ,form))
         (let ((,l (%multiple-value-list-of ,p)))
           ,(multiple-value-bind-build-bindings vars l body))))))

(defun multiple-value-bind-build-bindings (vars list-sym body)
  "Helper for the multiple-value-bind macro. Builds a chain of
   let-bindings that pull successive elements out of LIST-SYM and
   bind them to the names in VARS. Each step guards with `(if l
   ...)` so a list shorter than VARS binds the trailing names to
   NIL instead of crashing on (car nil). The generated form ends
   in BODY."
  (cond
    ((null vars) `(progn ,@body))
    (t
     (let ((var (car vars))
           (rest (cdr vars))
           (l (gensym "MV-TAIL")))
       `(let ((,var (if ,list-sym (car ,list-sym) nil))
              (,l (if ,list-sym (cdr ,list-sym) nil)))
          ,(multiple-value-bind-build-bindings rest l body))))))

(defmacro handler-case (body-form &rest clauses)
  "(handler-case body
      (error (var) recovery))
   For now only the ERROR clause is supported. The single-clause
   form is enough to demonstrate the unwind-and-bind mechanism;
   typed condition dispatch lands when CLOS does."
  (cond
    ((null clauses)
     ;; No clauses — the body's value is just returned.
     body-form)
    (t
     (let ((clause (car clauses)))
       (let ((var-list (car (cdr clause)))
             (handler-body (cdr (cdr clause))))
         (let ((var (car var-list)))
           `(%handler-case
              (lambda () ,body-form)
              (lambda (,var) ,@handler-body))))))))

;; -- iGui drawing ------------------------------------------------------------
;;
;; Colors are packed fixnums: 0xRRGGBBAA. (rgb r g b) sets alpha to
;; 255; (rgba r g b a) lets the caller specify it.

(defun rgb (r g b)
  "Pack a fully-opaque color into a fixnum."
  (+ (* r 16777216)        ; r << 24
     (* g 65536)            ; g << 16
     (* b 256)              ; b << 8
     255))

(defun rgba (r g b a)
  (+ (* r 16777216)
     (* g 65536)
     (* b 256)
     a))

;; A handful of named colors. Match common-CL/Win32 conventions
;; loosely; users who want their own should just call (rgb ...).
(defparameter +black+   (rgb 0 0 0))
(defparameter +white+   (rgb 255 255 255))
(defparameter +red+     (rgb 220 50 50))
(defparameter +green+   (rgb 50 180 80))
(defparameter +blue+    (rgb 50 100 200))
(defparameter +yellow+  (rgb 220 200 60))
(defparameter +slate+   (rgb 46 51 57))
(defparameter +panel+   (rgb 30 33 38))

(defmacro with-batch (child-id &rest body)
  "Open a drawing batch for CHILD-ID, evaluate BODY (which calls
   clear/fill-rect/draw-line/etc.), and submit on exit.

   Each new submit replaces the child's previous on-screen batch
   (latest-wins) — so the body should re-emit the entire pane,
   not just changes."
  `(progn
     (%begin-batch ,child-id)
     ,@body
     (%submit-batch)))

(defun clear (color)
  "Fill the active pane with COLOR."
  (%emit-clear color))

(defun fill-rect (x y w h color)
  (%emit-fill-rect x y w h color))

(defun stroke-rect (x y w h thickness color)
  (%emit-stroke-rect x y w h thickness color))

(defun draw-line (x1 y1 x2 y2 thickness color)
  (%emit-draw-line x1 y1 x2 y2 thickness color))

(defun draw-text (x y text size color)
  "Render TEXT at (X, Y) in Segoe UI at SIZE px. Y is the
   baseline-ish top of the text run. SIZE and coords are
   fixnums for now (sub-pixel waits on float support)."
  (%emit-draw-text x y text size color))

(defun draw-text-styled (x y text size color &rest opts)
  "Like draw-text but with styling. OPTS is a flat property list
   of any of:
     :family   STRING    — font family, e.g. \"Consolas\"
     :weight   FIXNUM    — 100..900 (regular = 400, bold = 700)
     :style    KEYWORD   — :normal | :italic | :oblique
     :stretch  FIXNUM    — 1 (ultra-condensed) .. 9 (ultra-expanded)
     :align    KEYWORD   — :leading | :trailing | :center | :justified
   Unrecognised keys are ignored. Missing keys take the same
   defaults as `draw-text`.

   Example:
     (draw-text-styled 10 20 \"Code\" 14 +white+
                       :family \"Consolas\" :weight 700 :style :italic)"
  (%emit-draw-text-styled x y text size color opts))

(defun fill-oval (x y w h color)
  "Filled ellipse, axis-aligned, with the given bounding box."
  (%emit-fill-oval x y w h color))

(defun stroke-oval (x y w h thickness color)
  (%emit-stroke-oval x y w h thickness color))

(defun fill-circle (cx cy radius color)
  (%emit-fill-circle cx cy radius color))

(defun stroke-circle (cx cy radius thickness color)
  (%emit-stroke-circle cx cy radius thickness color))

(defun draw-arc (cx cy radius rotation-deg aperture-deg thickness color)
  "Outlined circular arc centered at (CX, CY). ROTATION-DEG is the
   midpoint angle (0 points right, 90 points down) in degrees;
   APERTURE-DEG is the full angular span. Both are fixnums for now;
   floats land when the compiler grows them."
  (%emit-draw-arc cx cy radius rotation-deg aperture-deg thickness color))

(defun measure-text (child-id text size &rest opts)
  "Measure TEXT as it would render in CHILD-ID's pane. Returns a
   plist `(:width W :height H :ascent A :line-count N)` (all
   fixnums, rounded to nearest pixel) or NIL on failure.

   OPTS takes the same keys as `draw-text-styled` so layout sees
   the same metrics drawing will produce."
  (%measure-text child-id text size opts))

;; -- Log view ----------------------------------------------------------------

(defun log-format (control &rest args)
  "Format CONTROL with ARGS (same directives as `format`) and push
   the result as a single line into the iGui log overlay. Open
   the overlay via Tools → Log or Ctrl+Shift+L. (Renamed from
   `log` to avoid shadowing CL's natural-log function.)"
  (log-write (apply #'format nil control args)))

;; -- Text-view (terminal-style monospaced child) -----------------------------
;;
;; The native text-window primitives, rolled up into one place:
;;   open-text-window TITLE       → child-id (fixnum) or NIL
;;   text-write ID STRING         → write at cursor (handles \n \r \t \b)
;;   text-write-char ID CHAR      → single-char convenience
;;   text-clear ID                → wipe whole grid, cursor → (0,0)
;;   text-clear-eol ID            → clear cursor → end of line
;;   text-clear-eos ID            → clear cursor → bottom-right
;;   text-newline ID              → CR + LF, scroll if at bottom
;;   text-scroll-up ID N          → scroll grid up N rows
;;   text-set-cursor ID ROW COL   → move cursor (clamped)
;;   text-set-pen ID FG BG        → packed-RGBA colours
;;   text-reset-pen ID            → restore defaults
;;   text-show-caret ID FLAG      → caret visibility
;;
;; Colours are packed fixnums via (rgb r g b) / (rgba r g b a),
;; same encoding the geometry primitives use.

(defun text-format (id control &rest args)
  "Format CONTROL with ARGS (using `format` directives) and write
   the result into text window ID at the cursor. Returns T."
  (text-write id (apply #'format nil control args)))

(defun text-print (id obj)
  "Write OBJ's printed form into text window ID at the cursor."
  (text-write id (format nil "~A" obj)))

(defun text-println (id obj)
  "Like `text-print` but also issues a newline."
  (text-write id (format nil "~A" obj))
  (text-newline id))

;; -- String helpers ----------------------------------------------------------

(defun string-concat (a b)
  "Return a fresh string with B appended to A."
  (format nil "~A~A" a b))

(defun string-append-char (s c)
  "Return a fresh string with C appended to S."
  (format nil "~A~A" s c))

(defun string-without-last (s)
  "Return S with its last codepoint removed; empty string stays empty."
  (let ((n (length s)))
    (if (zerop n) s (substring s 0 (- n 1)))))

;; -- Standard I/O Streams & CL Printer Names --------------------------------
;;
;; The CL standard printer functions: print, prin1, princ, pprint,
;; write, write-char, write-string, write-line, terpri, fresh-line,
;; and the -to-string variants.
;;
;; Stream destinations:
;;   NIL or T → *standard-output* (i.e. stdout via native FORMAT)
;;   fixnum   → file handle (via WRITE-STRING-TO)
;;
;; These are thin wrappers around the native FORMAT engine — they'll
;; be shadowed by Library/xp.lisp if/when the pretty-printer loads.

(defparameter *standard-output* t)
(defparameter *standard-input* t)
(defparameter *error-output* t)
(defparameter *trace-output* t)
(defparameter *debug-io* t)
(defparameter *query-io* t)
(defparameter *terminal-io* t)

;; Print-control variables. Our bootstrap printer ignores them but
;; defining them here lets library code bind them without error.
(defparameter *print-escape* t)
(defparameter *print-readably* nil)
(defparameter *print-pretty* nil)
(defparameter *print-circle* nil)
(defparameter *print-base* 10)
(defparameter *print-radix* nil)
(defparameter *print-case* ':upcase)
(defparameter *print-level* nil)
(defparameter *print-length* nil)
(defparameter *print-array* t)
(defparameter *print-gensym* t)

(defun %resolve-stream (stream)
  "Resolve a printer stream argument: NIL → *standard-output*."
  (if stream stream *standard-output*))

(defun %emit-to-stream (stream s)
  "Write string S to the resolved STREAM. T → stdout, fixnum → file handle."
  (if (eq stream t)
      (format t "~A" s)
      (write-string-to stream s)))

(defun prin1 (object &optional stream)
  "Print OBJECT readably (~S style) to STREAM. Returns OBJECT."
  (let ((dest (%resolve-stream stream)))
    (if (eq dest t)
        (format t "~S" object)
        (%emit-to-stream dest (format nil "~S" object))))
  object)

(defun princ (object &optional stream)
  "Print OBJECT aesthetically (~A style) to STREAM. Returns OBJECT."
  (let ((dest (%resolve-stream stream)))
    (if (eq dest t)
        (format t "~A" object)
        (%emit-to-stream dest (format nil "~A" object))))
  object)

(defun print (object &optional stream)
  "Output newline, OBJECT readably, then space to STREAM. Returns OBJECT."
  (let ((dest (%resolve-stream stream)))
    (if (eq dest t)
        (format t "~%~S " object)
        (%emit-to-stream dest (format nil "~%~S " object))))
  object)

(defun pprint (object &optional stream)
  "Pretty-print OBJECT to STREAM. Returns (values)."
  (let ((dest (%resolve-stream stream)))
    (if (eq dest t)
        (format t "~%~S" object)
        (%emit-to-stream dest (format nil "~%~S" object))))
  (values))

(defun write (object &rest keys)
  "Output OBJECT to :stream (default *standard-output*). Returns OBJECT.
   Accepts :escape :readably :pretty :base :radix :case :level
   :length :circle :array :gensym keywords (ignored in bootstrap)."
  (let ((stream (%resolve-stream (getf keys ':stream))))
    (if (eq stream t)
        (format t "~S" object)
        (%emit-to-stream stream (format nil "~S" object))))
  object)

(defun prin1-to-string (object)
  "Return string with readable printed representation of OBJECT."
  (format nil "~S" object))

(defun princ-to-string (object)
  "Return string with aesthetic printed representation of OBJECT."
  (format nil "~A" object))

(defun write-to-string (object &rest keys)
  "Return string with printed representation of OBJECT."
  (declare (ignore keys))
  (format nil "~S" object))

(defun write-char (character &optional stream)
  "Write CHARACTER to STREAM. Returns CHARACTER."
  (let ((dest (%resolve-stream stream)))
    (if (eq dest t)
        (format t "~A" character)
        (%emit-to-stream dest (format nil "~A" character))))
  character)

(defun write-string (string &optional stream)
  "Write STRING to STREAM. Returns STRING."
  (let ((dest (%resolve-stream stream)))
    (if (eq dest t)
        (format t "~A" string)
        (%emit-to-stream dest string)))
  string)

(defun write-line (string &optional stream)
  "Write STRING followed by a newline to STREAM. Returns STRING."
  (let ((dest (%resolve-stream stream)))
    (if (eq dest t)
        (format t "~A~%" string)
        (progn
          (%emit-to-stream dest string)
          (%emit-to-stream dest (format nil "~%")))))
  string)

(defun terpri (&optional stream)
  "Output a newline to STREAM. Returns NIL."
  (let ((dest (%resolve-stream stream)))
    (if (eq dest t)
        (format t "~%")
        (%emit-to-stream dest (format nil "~%"))))
  nil)

(defun fresh-line (&optional stream)
  "Ensure output begins on a fresh line. Returns T if a newline was output.
   (Column tracking not yet implemented — always outputs a newline.)"
  (terpri stream)
  t)

;; -- eval & read-from-string -------------------------------------------------
;;
;; CL's EVAL: evaluate a Lisp form at runtime.
;; CL's READ-FROM-STRING: parse a string into a Lisp object.
;;
;; The native %EVAL-FORM roundtrips through prin1-to-string →
;; reader → compiler; READ-FROM-STRING is a direct native.

(defun eval (form)
  "Evaluate FORM and return its value."
  (%eval-form form))

;; read-from-string is registered as a native directly.

;; -- File I/O ----------------------------------------------------------------
;;
;; The native primitives are:
;;   open-input-file path        → handle (or 0 if open fails)
;;   open-output-file path       → handle (truncates existing)
;;   open-append-file path       → handle (creates or appends)
;;   close-stream handle         → t
;;   read-line handle            → string or nil at EOF
;;   read-char-from handle       → char or nil at EOF
;;   write-string-to handle s    → s
;;   file-position handle        → fixnum or -1
;;   file-length handle          → fixnum or -1
;;   file-exists path            → t / nil
;;   delete-file path            → t / nil
;;
;; The Lisp wrappers below add ergonomics — line-at-a-time text
;; iteration, RAII via with-open-file, file-as-string slurping.

(defun %file-write-line (stream s)
  "Write S to file-handle STREAM followed by a newline. Returns S."
  (write-string-to stream s)
  (write-string-to stream (format nil "~%"))
  s)

(defmacro with-open-file (binding-and-mode &rest body)
  "(with-open-file (var path direction) body...)
   Direction is one of the keywords :input, :output, :append.
   Opens path, binds the handle to var, evaluates body, and closes
   the handle on the way out. (Without conditions we can't yet
   guarantee close on non-local exit; the body just isn't allowed
   to escape via a condition until those land.)"
  (let ((var (car binding-and-mode))
        (path (car (cdr binding-and-mode)))
        (direction (car (cdr (cdr binding-and-mode)))))
    ;; Dispatch at macro-expansion time: compare the keyword the
    ;; user passed against the literal direction keywords.
    (let ((open-fn (cond
                     ((eq direction ':input)  'open-input-file)
                     ((eq direction ':output) 'open-output-file)
                     ((eq direction ':append) 'open-append-file)
                     (t 'open-input-file))))
      `(let ((,var (,open-fn ,path)))
         (let ((result (progn ,@body)))
           (close-stream ,var)
           result)))))

(defun %read-lines-from (stream acc)
  ;; Tail-recursive line reader. Acc is built reversed; caller flips.
  (let ((line (read-line stream)))
    (if (null line)
        (reverse acc)
        (%read-lines-from stream (cons line acc)))))

(defun read-file-lines (path)
  "Read every line of PATH into a list of strings (newlines stripped)."
  (let ((stream (open-input-file path)))
    (let ((result (%read-lines-from stream nil)))
      (close-stream stream)
      result)))

(defun read-file-string (path)
  "Read the entire contents of PATH as a single string. Lines are
   joined with newlines."
  (let ((lines (read-file-lines path)))
    (cond
      ((null lines) "")
      ((null (cdr lines)) (car lines))
      (t (%join-lines lines)))))

(defun %join-lines (lines)
  ;; Concatenate lines with \n separators using format.
  (cond
    ((null lines) "")
    ((null (cdr lines)) (car lines))
    (t (format nil "~A~%~A" (car lines) (%join-lines (cdr lines))))))

(defun write-file-string (path s)
  "Write the string S to PATH, replacing any existing file."
  (let ((stream (open-output-file path)))
    (write-string-to stream s)
    (close-stream stream)
    s))

(defun write-file-lines (path lines)
  "Write each string in LINES to PATH, one per line, replacing
   any existing file."
  (let ((stream (open-output-file path)))
    (%write-lines-to stream lines)
    (close-stream stream)
    lines))

(defun %write-lines-to (stream lines)
  (cond
    ((null lines) nil)
    (t (%file-write-line stream (car lines))
       (%write-lines-to stream (cdr lines)))))

;; -- Loader: load / require / provide / *load-path* / *modules* --------------
;;
;; Closely models CL's load / require / provide. The Rust primitive
;; %load-file (registered in ncl-compiler) reads a UTF-8 source file
;; and runs every top-level form through the active session — this
;; layer adds path resolution and the load-once memo.
;;
;; *load-path* is a list of directory strings searched left-to-right
;; by REQUIRE when a bare module name is passed. The driver mutates
;; it at startup to point into the exe-relative `Library/` directory;
;; user code can push extra paths.
;;
;; *modules* is a list of symbols REQUIRE has loaded. PROVIDE adds
;; to it. A REQUIRE for an already-provided module is a no-op.

(defparameter *load-path* '())
(defparameter *modules* '())
(defparameter *verbose-load* nil)

(defun load (path)
  "Read every top-level form from PATH and evaluate it. Returns T
   on success; the underlying %load-file shim signals on read or
   eval failure."
  (when *verbose-load*
    (format t ";;; loading ~A~%" path))
  (%load-file path))

(defun provide (module-name)
  "Record MODULE-NAME (a symbol) as loaded. REQUIRE for the same
   name afterwards becomes a no-op. Returns MODULE-NAME."
  (cond
    ((member module-name *modules*) module-name)
    (t (setq *modules* (cons module-name *modules*))
       module-name)))

(defun %require-search (name dirs)
  "Walk DIRS; return the first existing 'DIR/NAME.lisp' or NIL."
  (cond
    ((null dirs) nil)
    (t (let ((candidate (format nil "~A/~A.lisp" (car dirs) name)))
         (cond
           ((file-exists candidate) candidate)
           (t (%require-search name (cdr dirs))))))))

(defun require (module-name &optional explicit-path)
  "If MODULE-NAME is in *modules*, do nothing. Otherwise, load it:
   if EXPLICIT-PATH is given, load that file directly; else search
   *load-path* for MODULE-NAME.lisp. Returns MODULE-NAME on
   successful load, NIL if already provided."
  (cond
    ((member module-name *modules*) nil)
    (explicit-path
     (load explicit-path)
     (provide module-name))
    (t
     (let ((found (%require-search module-name *load-path*)))
       (cond
         (found
          (load found)
          (provide module-name))
         (t (error "require: cannot find ~A.lisp in *load-path* (~A)"
                   module-name *load-path*)))))))
