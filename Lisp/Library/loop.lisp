;;;; Lisp/Library/loop.lisp
;;;;
;;;; Extended LOOP macro. CL's iconic iteration form.
;;;;
;;;; This file REDEFINES the `loop` macro to dispatch:
;;;;
;;;;   (loop body...)                — simple loop (existing behaviour)
;;;;   (loop for x in '(1 2 3) ...)  — extended LOOP
;;;;
;;;; The dispatch is based on whether the first form is a recognised
;;;; loop keyword. The simple form remains the primitive (a thin
;;;; wrapper around %native-loop) and is what core.lisp's other
;;;; macros (`dotimes`, manual loops in CLOS, etc.) expand to.
;;;;
;;;; Subset implemented:
;;;;
;;;;   Variable / iteration:
;;;;     (for VAR in LIST)
;;;;     (for VAR on LIST)
;;;;     (for VAR from N [to|below|downto|above M] [by STEP])
;;;;     (for VAR = EXPR [then EXPR])
;;;;     (for VAR across VECTOR)   ; via length+svref
;;;;     (repeat N)
;;;;     (as ...)                  ; synonym for `for`
;;;;
;;;;   Outer bindings:
;;;;     (with VAR [= INIT])
;;;;
;;;;   Termination:
;;;;     (while EXPR)
;;;;     (until EXPR)
;;;;     for-clause range bounds (to, below, downto, above)
;;;;     (repeat N)
;;;;
;;;;   Accumulation (no `into` form — assume single accumulator):
;;;;     (collect EXPR)        ; nconc-style; result is the list
;;;;     (sum EXPR)            ; running total
;;;;     (count EXPR)          ; count of true values
;;;;     (minimize EXPR)
;;;;     (maximize EXPR)
;;;;     (append EXPR)         ; like collect but append items
;;;;
;;;;   Control:
;;;;     (initially FORM ...)  ; run once before loop
;;;;     (finally FORM ...)    ; run once after natural completion
;;;;     (do FORM ...)         ; side effects, no accumulation
;;;;     (when EXPR CLAUSE)    ; conditional single sub-clause
;;;;     (unless EXPR CLAUSE)
;;;;     (named NAME)          ; name the implicit block for return-from
;;;;     (return EXPR)         ; exit with EXPR
;;;;
;;;; What's NOT in this slice:
;;;;
;;;;   * `into VAR` for accumulators (single-accumulator only).
;;;;   * `for VAR being the hash-keys/values of HT`.
;;;;   * Destructuring `for (A B) in PAIRS`.
;;;;   * `else` branches on `when/unless` (single clause only).
;;;;   * Multiple sequential `for` clauses are treated as parallel
;;;;     stepping; loop ends when ANY iterator exhausts.
;;;;
;;;; These all land in a future pass — the most common idioms are
;;;; covered.

;; ── Recogniser ────────────────────────────────────────────────────────────

(defun %loop-keyword-p (sym)
  "T iff SYM is a LOOP keyword (the head of an extended-loop clause)."
  (and (symbolp sym)
       (member sym
               '(for as with while until repeat named
                 initially finally do doing return
                 collect collecting append appending
                 sum summing count counting
                 minimize minimizing maximize maximizing
                 when unless if))))

;; ── Tokeniser-as-cursor ───────────────────────────────────────────────────
;;
;; The cursor is a one-cell holder: (car cur) is the REMAINING list of
;; tokens. Eat updates (car cur) to its cdr and returns the old head.
;; This shape lets callers share the cursor by reference (passing the
;; one-cell) and observe each others' advances.

(defun %make-cursor (lst) (cons lst nil))   ; (REMAINING . _)
(defun %cur-peek (cur)  (car (car cur)))
(defun %cur-empty? (cur) (null (car cur)))

(defun %cur-eat! (cur)
  "Pop the head off the cursor and return it."
  (let ((lst (car cur)))
    (cond
      ((null lst) nil)
      (t (let ((head (car lst)))
           (setf (car cur) (cdr lst))
           head)))))

(defun %cur-eat-keyword? (cur kw)
  "If the cursor's head is KW (a symbol), advance and return T."
  (cond
    ((eq (%cur-peek cur) kw) (%cur-eat! cur) t)
    (t nil)))

;; ── Plan structure ────────────────────────────────────────────────────────
;;
;; The plan accumulates as we parse. Each list field is built in
;; REVERSE (cons-prepend — O(1) per clause; the old append-per-add
;; recopied the whole field each time, O(n²) across a clause-heavy
;; loop). Readers restore source order with `reverse` at the two
;; boundaries: %loop-emit / %loop-result-form / the into-reversal
;; walker at emit time, and %parse-when-unless's added-forms capture
;; mid-parse.

(defstruct loop-plan
  (name nil)            ; named block tag, or NIL
  (with-bindings nil)   ; ((var . init-form) ...)
  (iter-bindings nil)   ; ((var . init-form) ...) for for-clause vars
  (iter-tests nil)      ; pre-body tests — for/while/until BEFORE first do
  (post-body-tests nil) ; post-body tests — while/until AFTER do clauses.
                        ; CL `(loop do EXPR until COND)` runs EXPR first,
                        ; then tests COND — Corman's quicksort relies on
                        ; this. The split tracks which side of the body a
                        ; test landed in.
  (iter-steps nil)      ; forms — run after body to advance iterators
  (initially nil)       ; forms — run before main loop
  (body nil)            ; forms — main per-iteration body
  (body-seen nil)       ; flipped to T once a do/doing clause was parsed;
                        ; subsequent while/until tests are routed to
                        ; post-body-tests.
  (finally nil)         ; forms — run after natural completion
  (accumulators nil)    ; ((kind var . extras) ...) for the result computation
  )

(defun %plan-add-with (plan var init)
  (setf (loop-plan-with-bindings plan)
        (cons (cons var init) (loop-plan-with-bindings plan))))

(defun %plan-add-iter-binding (plan var init)
  (setf (loop-plan-iter-bindings plan)
        (cons (cons var init) (loop-plan-iter-bindings plan))))

(defun %plan-add-test (plan form)
  (cond
    ((loop-plan-body-seen plan)
     (setf (loop-plan-post-body-tests plan)
           (cons form (loop-plan-post-body-tests plan))))
    (t
     (setf (loop-plan-iter-tests plan)
           (cons form (loop-plan-iter-tests plan))))))

(defun %plan-add-step (plan form)
  (setf (loop-plan-iter-steps plan)
        (cons form (loop-plan-iter-steps plan))))

(defun %plan-add-body (plan form)
  (setf (loop-plan-body plan)
        (cons form (loop-plan-body plan)))
  (setf (loop-plan-body-seen plan) t))

(defun %plan-add-initially (plan form)
  (setf (loop-plan-initially plan)
        (cons form (loop-plan-initially plan))))

(defun %plan-add-finally (plan form)
  (setf (loop-plan-finally plan)
        (cons form (loop-plan-finally plan))))

(defun %plan-add-accumulator (plan kind var extras)
  (setf (loop-plan-accumulators plan)
        (cons (cons kind (cons var extras))
              (loop-plan-accumulators plan))))

;; ── Clause parsers ────────────────────────────────────────────────────────

(defun %loop-pattern-bindings (pattern source)
  "Given a loop iteration variable PATTERN — a symbol, NIL, or a
   possibly-dotted list of symbols — and an accessor form SOURCE that
   yields the value to destructure, return a list of (VAR ACCESSOR-FORM)
   pairs binding each symbol in PATTERN to the matching part of SOURCE.
   NIL in a pattern position ignores that slot (no binding). This is the
   `(for (a b) in pairs)` destructuring used by LOOP."
  (cond
    ((null pattern) nil)
    ((symbolp pattern) (list (list pattern source)))
    ((consp pattern)
     (append (%loop-pattern-bindings (car pattern) (list 'car source))
             (%loop-pattern-bindings (cdr pattern) (list 'cdr source))))
    (t (error "loop: bad destructuring pattern ~A" pattern))))

(defun %parse-for (plan cur)
  "Parse one or more parallel for-bindings joined by `and`:
     (for i from 1 to 3 and j in list ...)
   All bindings step together; the loop ends when ANY iterator
   terminates (NCL treats sequential for-clauses as parallel too)."
  (%parse-one-for plan cur)
  (when (%cur-eat-keyword? cur 'and)
    (%parse-for plan cur)))

(defun %parse-one-for (plan cur)
  "(for VAR <spec>). spec is one of:
     in LIST                          → walk list
     on LIST                          → walk cons cells
     = EXPR [then EXPR]               → bind, optionally step
     from N [to|below|downto|above M] [by STEP]
     across VECTOR                    → walk vector by index

   `of-type TYPE` may appear between VAR and the spec keyword:
     (loop for a of-type integer in '(1 2 3) collect a)
   The type declaration is accepted but ignored — NCL's compiler
   doesn't yet act on CL type declarations, and accepting+ignoring
   matches what the test corpus assumes."
  (let ((var (%cur-eat! cur)))
    ;; Skip optional `of-type TYPE`. Token-eater eats both; the
    ;; declared type is discarded.
    (when (%cur-eat-keyword? cur 'of-type)
      (%cur-eat! cur))
    (cond
      ;; for VAR in LIST  (VAR may be a destructuring pattern)
      ((%cur-eat-keyword? cur 'in)
       (let* ((list-form (%cur-eat! cur))
              ;; optional `by STEP-FN` — advance with (funcall fn tail)
              ;; instead of cdr (e.g. `by #'cddr` to step two at a time)
              (step-fn (when (%cur-eat-keyword? cur 'by) (%cur-eat! cur)))
              (tail-var (gensym "TAIL-")))
         (%plan-add-iter-binding plan tail-var list-form)
         (%plan-add-test plan `(null ,tail-var))
         (%plan-add-step plan
           `(setq ,tail-var ,(if step-fn
                                 `(funcall ,step-fn ,tail-var)
                                 `(cdr ,tail-var))))
         ;; Bind the element — a plain symbol, or each var of a
         ;; destructuring pattern — to (car tail-var), recomputed each step.
         (dolist (b (%loop-pattern-bindings var `(car ,tail-var)))
           (%plan-add-iter-binding plan (car b) (cadr b))
           (%plan-add-step plan `(setq ,(car b) ,(cadr b))))))
      ;; for VAR on LIST  (VAR may be a destructuring pattern; it
      ;; destructures the successive tails)
      ((%cur-eat-keyword? cur 'on)
       (let* ((list-form (%cur-eat! cur))
              (step-fn (when (%cur-eat-keyword? cur 'by) (%cur-eat! cur))))
         (cond
           ((symbolp var)
            (%plan-add-iter-binding plan var list-form)
            (%plan-add-test plan `(null ,var))
            (%plan-add-step plan
              `(setq ,var ,(if step-fn `(funcall ,step-fn ,var) `(cdr ,var)))))
           (t
            (let ((tail-var (gensym "TAIL-")))
              (%plan-add-iter-binding plan tail-var list-form)
              (%plan-add-test plan `(null ,tail-var))
              (%plan-add-step plan
                `(setq ,tail-var ,(if step-fn
                                      `(funcall ,step-fn ,tail-var)
                                      `(cdr ,tail-var))))
              (dolist (b (%loop-pattern-bindings var tail-var))
                (%plan-add-iter-binding plan (car b) (cadr b))
                (%plan-add-step plan `(setq ,(car b) ,(cadr b)))))))))
      ;; for VAR across VECTOR
      ((%cur-eat-keyword? cur 'across)
       (let* ((vec-form (%cur-eat! cur))
              (vec-var (gensym "VEC-"))
              (i-var (gensym "I-"))
              (n-var (gensym "N-")))
         (%plan-add-iter-binding plan vec-var vec-form)
         (%plan-add-iter-binding plan i-var 0)
         (%plan-add-iter-binding plan n-var `(length ,vec-var))
         (%plan-add-iter-binding plan var `(svref ,vec-var 0))
         (%plan-add-test plan `(>= ,i-var ,n-var))
         (%plan-add-step plan `(setq ,i-var (+ ,i-var 1)))
         (%plan-add-step plan `(when (< ,i-var ,n-var)
                                 (setq ,var (svref ,vec-var ,i-var))))))
      ;; for VAR being [the|each] {hash-key[s]|hash-value[s]} {of|in} HT
      ((%cur-eat-keyword? cur 'being)
       (%parse-being-clause plan cur var))
      ;; for VAR = EXPR [then EXPR]   (VAR may be a destructuring pattern)
      ((%cur-eat-keyword? cur '=)
       (let* ((init (%cur-eat! cur))
              (step (if (%cur-eat-keyword? cur 'then) (%cur-eat! cur) init)))
         (if (symbolp var)
             (progn
               (%plan-add-iter-binding plan var init)
               (%plan-add-step plan `(setq ,var ,step)))
             ;; destructuring: iterate a hidden value, destructure each step
             (let ((item (gensym "ITEM-")))
               (%plan-add-iter-binding plan item init)
               (%plan-add-step plan `(setq ,item ,step))
               (dolist (b (%loop-pattern-bindings var item))
                 (%plan-add-iter-binding plan (car b) (cadr b))
                 (%plan-add-step plan `(setq ,(car b) ,(cadr b))))))))
      ;; for VAR [from|upfrom|downfrom N] [to|upto|below|downto|above M]
      ;; [by STEP] — the numeric stepping clause. CL lets these
      ;; sub-keywords appear in any order (e.g. `for i by s from x to
      ;; 10`, or a bare `for i below n` with no `from`), so route the
      ;; whole tail to %parse-from-clause, which consumes them
      ;; order-independently and defaults the start to 0.
      ((let ((k (%cur-peek cur)))
         (and (symbolp k)
              (member k '(from upfrom downfrom to upto below downto above by))))
       (%parse-from-clause plan cur var))
      (t (error "loop: unknown for-spec after ~A" var)))))

(defun %parse-being-clause (plan cur var)
  "(for VAR being [the|each] {hash-key[s]|hash-value[s]} {of|in} HT
        [using ({hash-value|hash-key} OTHER-VAR)]).
   Implemented by snapshotting the table's (key . value) pairs with
   MAPHASH once, then iterating that list. Package iteration
   (being the symbols of …) is not supported — NCL has no packages."
  (or (%cur-eat-keyword? cur 'the) (%cur-eat-keyword? cur 'each))
  (let ((kind (%cur-eat! cur)))
    (or (%cur-eat-keyword? cur 'of) (%cur-eat-keyword? cur 'in))
    (let ((ht-form (%cur-eat! cur))
          (tail-var (gensym "HTTAIL-"))
          (acc-g (gensym "HTACC-"))
          (k-g (gensym "HTK-"))
          (v-g (gensym "HTV-"))
          (using-kind nil)
          (using-var nil))
      (when (%cur-eat-keyword? cur 'using)
        (let ((spec (%cur-eat! cur)))      ; (hash-value v) / (hash-key k)
          (setq using-kind (car spec))
          (setq using-var (cadr spec))))
      ;; Snapshot the pairs: maphash builds a list of (key . value).
      (%plan-add-iter-binding plan tail-var
        `(let ((,acc-g nil))
           (maphash (lambda (,k-g ,v-g)
                      (setq ,acc-g (cons (cons ,k-g ,v-g) ,acc-g)))
                    ,ht-form)
           ,acc-g))
      (%plan-add-test plan `(null ,tail-var))
      (%plan-add-step plan `(setq ,tail-var (cdr ,tail-var)))
      ;; The iteration variable takes the key (default) or value.
      (let ((var-acc (if (member kind '(hash-value hash-values))
                         `(cdr (car ,tail-var))
                         `(car (car ,tail-var)))))
        (%plan-add-iter-binding plan var var-acc)
        (%plan-add-step plan `(setq ,var ,var-acc)))
      ;; Optional paired `using` variable takes the other half.
      (when using-var
        (let ((u-acc (if (member using-kind '(hash-value hash-values))
                         `(cdr (car ,tail-var))
                         `(car (car ,tail-var)))))
          (%plan-add-iter-binding plan using-var u-acc)
          (%plan-add-step plan `(setq ,using-var ,u-acc)))))))

(defun %parse-from-clause (plan cur var)
  "Parse a numeric stepping for-clause's range:
     [from|upfrom|downfrom N] [to|upto|below|downto|above M] [by STEP]
   All sub-keywords are accepted in any order — CL allows e.g.
   `for i by s from x to 10` as well as `for i from x to 10 by s`.
   With no `from`, the start defaults to 0 (so `for i below n` ≡
   `for i from 0 below n`). `upto`=`to`, `upfrom`=`from`; `downfrom`
   sets the start and steps downward.

   The from/to/by value forms are each evaluated exactly once, in the
   order they appear in the source (CL 6.1.2.1.1): a `from`/`upfrom`/
   `downfrom` binds VAR at the point it appears, and `to`/`by` capture
   non-trivial forms into iter-bindings as they are read, so e.g.
   `by (incf x) from x` increments x before reading it for the start."
  (let ((cmp nil) (limit nil) (step 1) (direction 1) (bound-start nil))
    (flet ((cap (form)
             ;; Capture a non-trivial form into a fresh iter-binding so
             ;; it's evaluated once, here, in source order; literals and
             ;; symbols pass through untouched.
             (if (consp form)
                 (let ((g (gensym "LOOPV-")))
                   (%plan-add-iter-binding plan g form)
                   g)
                 form)))
      (loop
        (cond
          ((%cur-eat-keyword? cur 'from)
           (%plan-add-iter-binding plan var (%cur-eat! cur)) (setq bound-start t))
          ((%cur-eat-keyword? cur 'upfrom)
           (%plan-add-iter-binding plan var (%cur-eat! cur)) (setq bound-start t))
          ((%cur-eat-keyword? cur 'downfrom)
           (%plan-add-iter-binding plan var (%cur-eat! cur))
           (setq bound-start t) (setq direction -1))
          ((%cur-eat-keyword? cur 'to)     (setq cmp '<=) (setq limit (cap (%cur-eat! cur))))
          ((%cur-eat-keyword? cur 'upto)   (setq cmp '<=) (setq limit (cap (%cur-eat! cur))))
          ((%cur-eat-keyword? cur 'below)  (setq cmp '<)  (setq limit (cap (%cur-eat! cur))))
          ((%cur-eat-keyword? cur 'downto)
           (setq cmp '>=) (setq direction -1) (setq limit (cap (%cur-eat! cur))))
          ((%cur-eat-keyword? cur 'above)
           (setq cmp '>)  (setq direction -1) (setq limit (cap (%cur-eat! cur))))
          ((%cur-eat-keyword? cur 'by)     (setq step (cap (%cur-eat! cur))))
          (t (return)))))
    ;; No `from` seen → start at 0 (bound after any captured forms; the
    ;; literal 0 has no evaluation-order concern).
    (unless bound-start
      (%plan-add-iter-binding plan var 0))
    (cond
      (cmp
       (%plan-add-test plan `(not (,cmp ,var ,limit))))
      ;; No bound → infinite range; user must use while/until/return.
      )
    (cond
      ((eql direction 1)  (%plan-add-step plan `(setq ,var (+ ,var ,step))))
      (t                  (%plan-add-step plan `(setq ,var (- ,var ,step)))))))

(defun %parse-with (plan cur)
  "(with VAR [= INIT] [and VAR2 [= INIT2]] ...) — one or more parallel
   outer bindings joined by `and`."
  (%parse-one-with plan cur)
  (when (%cur-eat-keyword? cur 'and)
    (%parse-with plan cur)))

(defun %parse-one-with (plan cur)
  "(with VAR [of-type TYPE] [= INIT]).  VAR may be a destructuring
   pattern; all with-bindings land in one let*, so the hidden item is
   bound before the pattern vars that reference it."
  (let ((var (%cur-eat! cur)))
    (when (%cur-eat-keyword? cur 'of-type) (%cur-eat! cur))  ; skip type
    (let ((init (if (%cur-eat-keyword? cur '=) (%cur-eat! cur) nil)))
      (if (symbolp var)
          (%plan-add-with plan var init)
          (let ((item (gensym "WITH-")))
            (%plan-add-with plan item init)
            (dolist (b (%loop-pattern-bindings var item))
              (%plan-add-with plan (car b) (cadr b))))))))

(defun %parse-while (plan cur)
  (let ((expr (%cur-eat! cur)))
    (%plan-add-test plan `(not ,expr))))

(defun %parse-until (plan cur)
  (let ((expr (%cur-eat! cur)))
    (%plan-add-test plan expr)))

(defun %parse-repeat (plan cur)
  (let ((expr (%cur-eat! cur))
        (counter (gensym "REPEAT-")))
    (%plan-add-iter-binding plan counter expr)
    (%plan-add-test plan `(<= ,counter 0))
    (%plan-add-step plan `(setq ,counter (- ,counter 1)))))

;; Optional `into VAR` tail on an accumulation clause. Returns the
;; user's accumulator variable if present (and NIL otherwise), having
;; eaten the `into` keyword and the variable name.
(defun %parse-into? (cur)
  (when (%cur-eat-keyword? cur 'into)
    (%cur-eat! cur)))

(defun %parse-collect (plan cur)
  (let* ((expr (%cur-eat! cur))
         (into (%parse-into? cur))
         (acc (or into (gensym "COLLECT-"))))
    (%plan-add-with plan acc nil)
    (%plan-add-body plan `(setq ,acc (cons ,expr ,acc)))
    (%plan-add-accumulator plan 'collect acc into)))

(defun %parse-append (plan cur)
  (let* ((expr (%cur-eat! cur))
         (into (%parse-into? cur))
         (acc (or into (gensym "APPEND-"))))
    (%plan-add-with plan acc nil)
    (%plan-add-body plan `(setq ,acc (append* ,acc ,expr)))
    (%plan-add-accumulator plan 'append acc into)))

(defun %parse-sum (plan cur)
  (let* ((expr (%cur-eat! cur))
         (into (%parse-into? cur))
         (acc (or into (gensym "SUM-"))))
    (%plan-add-with plan acc 0)
    (%plan-add-body plan `(setq ,acc (+ ,acc ,expr)))
    (%plan-add-accumulator plan 'sum acc into)))

(defun %parse-count (plan cur)
  (let* ((expr (%cur-eat! cur))
         (into (%parse-into? cur))
         (acc (or into (gensym "COUNT-"))))
    (%plan-add-with plan acc 0)
    (%plan-add-body plan `(when ,expr (setq ,acc (+ ,acc 1))))
    (%plan-add-accumulator plan 'count acc into)))

(defun %parse-minimize (plan cur)
  (let* ((expr (%cur-eat! cur))
         (into (%parse-into? cur))
         (acc (or into (gensym "MIN-")))
         (val (gensym "V-")))
    (%plan-add-with plan acc nil)
    (%plan-add-body plan
                    `(let ((,val ,expr))
                       (when (or (null ,acc) (< ,val ,acc))
                         (setq ,acc ,val))))
    (%plan-add-accumulator plan 'min acc into)))

(defun %parse-maximize (plan cur)
  (let* ((expr (%cur-eat! cur))
         (into (%parse-into? cur))
         (acc (or into (gensym "MAX-")))
         (val (gensym "V-")))
    (%plan-add-with plan acc nil)
    (%plan-add-body plan
                    `(let ((,val ,expr))
                       (when (or (null ,acc) (> ,val ,acc))
                         (setq ,acc ,val))))
    (%plan-add-accumulator plan 'max acc into)))

;; (do FORM ...) — run forms each iteration, no accumulation.
;; Reads forms until the next clause keyword or end of body.
(defun %parse-do (plan cur)
  (loop
    (cond
      ((%cur-empty? cur) (return nil))
      ((%loop-keyword-p (%cur-peek cur)) (return nil))
      (t (%plan-add-body plan (%cur-eat! cur))))))

(defun %parse-initially (plan cur)
  (loop
    (cond
      ((%cur-empty? cur) (return nil))
      ((%loop-keyword-p (%cur-peek cur)) (return nil))
      (t (%plan-add-initially plan (%cur-eat! cur))))))

(defun %parse-finally (plan cur)
  (loop
    (cond
      ((%cur-empty? cur) (return nil))
      ((%loop-keyword-p (%cur-peek cur)) (return nil))
      (t (%plan-add-finally plan (%cur-eat! cur))))))

;; (return EXPR) — exit with EXPR. Returns from the implicit block.
(defun %parse-return (plan cur)
  (let ((expr (%cur-eat! cur))
        (name (or (loop-plan-name plan) 'nil)))
    (%plan-add-body plan `(return-from ,name ,expr))))

;; (when EXPR CLAUSE) / (unless EXPR CLAUSE) — single sub-clause.
;; We bury the sub-clause's effect inside an (if ...) in the body.
;; Implementation: save the body list (a shared tail, since adders
;; only cons onto the front), parse the sub-clause, then peel the
;; new front cells off as the additions and wrap them.
(defun %parse-when-unless (plan cur negate)
  (let* ((expr (%cur-eat! cur))
         (saved-tail (loop-plan-body plan)))
    (%parse-one-clause plan cur)
    ;; body is reverse-accumulated: everything in front of
    ;; SAVED-TAIL (compared by EQ) was added by the sub-clause.
    ;; Pushing front-to-back restores the additions' source order.
    (let ((added nil)
          (p (loop-plan-body plan)))
      (loop
        (when (eq p saved-tail) (return nil))
        (setq added (cons (car p) added))
        (setq p (cdr p)))
      ;; Trim plan body back to its pre-sub-clause state.
      (setf (loop-plan-body plan) saved-tail)
      ;; Wrap the additions in an if.
      (let ((wrapped
             (cond
               ((null added) nil)
               (negate `(unless ,expr ,@added))
               (t      `(when ,expr ,@added)))))
        (when wrapped
          (%plan-add-body plan wrapped))))))

;; Helper used by when/unless to parse exactly one sub-clause.
(defun %parse-one-clause (plan cur)
  (let ((head (%cur-eat! cur)))
    (cond
      ((eq head 'do)         (%plan-add-body plan (%cur-eat! cur)))
      ((eq head 'collect)    (%parse-collect plan cur))
      ((eq head 'collecting) (%parse-collect plan cur))
      ((eq head 'append)     (%parse-append plan cur))
      ((eq head 'appending)  (%parse-append plan cur))
      ((eq head 'sum)        (%parse-sum plan cur))
      ((eq head 'summing)    (%parse-sum plan cur))
      ((eq head 'count)      (%parse-count plan cur))
      ((eq head 'counting)   (%parse-count plan cur))
      ((eq head 'minimize)   (%parse-minimize plan cur))
      ((eq head 'minimizing) (%parse-minimize plan cur))
      ((eq head 'maximize)   (%parse-maximize plan cur))
      ((eq head 'maximizing) (%parse-maximize plan cur))
      ((eq head 'return)     (%parse-return plan cur))
      (t (error "loop: unsupported sub-clause head ~A" head)))))

;; ── Top-level parser ──────────────────────────────────────────────────────

(defun %parse-loop (body)
  (let ((plan (make-loop-plan))
        (cur (%make-cursor body)))
    ;; Optional leading (named NAME) clause.
    (when (eq (%cur-peek cur) 'named)
      (%cur-eat! cur)
      (setf (loop-plan-name plan) (%cur-eat! cur)))
    (loop
      (cond
        ((%cur-empty? cur) (return nil))
        (t (let ((head (%cur-eat! cur)))
             (cond
               ((or (eq head 'for) (eq head 'as)) (%parse-for plan cur))
               ((eq head 'with)       (%parse-with plan cur))
               ((eq head 'while)      (%parse-while plan cur))
               ((eq head 'until)      (%parse-until plan cur))
               ((eq head 'repeat)     (%parse-repeat plan cur))
               ((eq head 'do)         (%parse-do plan cur))
               ((eq head 'doing)      (%parse-do plan cur))
               ((eq head 'initially)  (%parse-initially plan cur))
               ((eq head 'finally)    (%parse-finally plan cur))
               ((eq head 'collect)    (%parse-collect plan cur))
               ((eq head 'collecting) (%parse-collect plan cur))
               ((eq head 'append)     (%parse-append plan cur))
               ((eq head 'appending)  (%parse-append plan cur))
               ((eq head 'sum)        (%parse-sum plan cur))
               ((eq head 'summing)    (%parse-sum plan cur))
               ((eq head 'count)      (%parse-count plan cur))
               ((eq head 'counting)   (%parse-count plan cur))
               ((eq head 'minimize)   (%parse-minimize plan cur))
               ((eq head 'minimizing) (%parse-minimize plan cur))
               ((eq head 'maximize)   (%parse-maximize plan cur))
               ((eq head 'maximizing) (%parse-maximize plan cur))
               ((eq head 'when)       (%parse-when-unless plan cur nil))
               ((eq head 'unless)     (%parse-when-unless plan cur t))
               ((eq head 'if)         (%parse-when-unless plan cur nil))
               ((eq head 'return)     (%parse-return plan cur))
               (t (error "loop: unknown clause head ~A" head)))))))
    plan))

;; ── Code generation ───────────────────────────────────────────────────────

(defun %loop-rewrite-return (form name)
  "Replace `(return X)` and `(return)` inside FORM with
   `(return-from NAME X)` / `(return-from NAME nil)`. Stops
   descending into nested (loop ...) and (block ...) forms so
   their own returns target their own blocks."
  (cond
    ((atom form) form)
    ((eq (car form) 'loop) form)
    ((eq (car form) 'block) form)
    ((eq (car form) 'return)
     (cond
       ((null (cdr form)) `(return-from ,name nil))
       (t `(return-from ,name ,(cadr form)))))
    (t (mapcar (lambda (sub) (%loop-rewrite-return sub name)) form))))

(defun %loop-result-form (plan)
  "Pick the final-value form. CL says the value of the last
   accumulator clause; collect's accumulator was built with cons-
   prepend so we reverse it back. Accumulators with an `into` variable
   bind that variable instead and contribute NOTHING to the loop value
   — the user retrieves them via `finally`."
  (let ((anon (remove-if (lambda (a) (cddr a))   ; cddr = the into-var, if any
                         ;; stored newest-first; restore source order
                         ;; so (car (last anon)) is the LAST clause.
                         (reverse (loop-plan-accumulators plan)))))
    (cond
      ((null anon) nil)
      (t
       (let* ((last (car (last anon)))
              (kind (car last))
              (var (cadr last)))
         (cond
           ((eq kind 'collect) `(reverse ,var))
           (t var)))))))

(defun %loop-collect-into-reversals (plan)
  "For each distinct `collect ... into VAR`, emit a form that reverses
   VAR in place after the loop and before `finally`, so finally (and any
   later reader) sees the items in collection order. APPEND-into already
   builds in order, and numeric accumulators need no fixup."
  (let ((seen nil) (forms nil))
    (dolist (a (reverse (loop-plan-accumulators plan)))
      (let ((kind (car a)) (var (cadr a)) (into (cddr a)))
        (when (and into (eq kind 'collect) (not (member var seen)))
          (setq seen (cons var seen))
          (setq forms (cons `(setq ,var (nreverse ,var)) forms)))))
    (reverse forms)))

(defun %loop-emit (plan)
  "Walk PLAN, emit the full expansion. Shape:
     (block NAME
       (let (with-bindings + iter-bindings + accumulator vars)
         (initially-forms ...)
         (loop
           (when (or test1 test2 ...) (return nil))
           body-forms ...
           step-forms ...)
         finally-forms ...
         result-form))"
  (let* ((name (or (loop-plan-name plan) 'nil))
         ;; Plan list fields are reverse-accumulated (see the plan
         ;; structure comment) — restore source order here, once.
         (all-bindings
          (append
           ;; with-bindings come first (user-visible scope).
           (mapcar (lambda (pair) `(,(car pair) ,(cdr pair)))
                   (reverse (loop-plan-with-bindings plan)))
           ;; iter-bindings come second (loop-internal).
           (mapcar (lambda (pair) `(,(car pair) ,(cdr pair)))
                   (reverse (loop-plan-iter-bindings plan)))))
         (tests (reverse (loop-plan-iter-tests plan)))
         (post-tests (reverse (loop-plan-post-body-tests plan)))
         (test-form
          (cond
            ((null tests) 'nil)            ; no automatic termination
            ((null (cdr tests)) (car tests))
            (t `(or ,@tests))))
         (post-test-form
          (cond
            ((null post-tests) 'nil)       ; no post-body termination
            ((null (cdr post-tests)) (car post-tests))
            (t `(or ,@post-tests))))
         ;; Rewrite (return X) → (return-from NAME X) so user
         ;; code's `return` inside body/initially/finally reaches
         ;; the implicit block. The body's accumulator-setq forms
         ;; we added don't contain `return`, but it's harmless to
         ;; walk them.
         (body (mapcar (lambda (f) (%loop-rewrite-return f name))
                       (reverse (loop-plan-body plan))))
         (steps (reverse (loop-plan-iter-steps plan)))
         (initially (mapcar (lambda (f) (%loop-rewrite-return f name))
                            (reverse (loop-plan-initially plan))))
         (finally (mapcar (lambda (f) (%loop-rewrite-return f name))
                          (reverse (loop-plan-finally plan))))
         (result (%loop-result-form plan)))
    ;; The simple `loop`'s `return` doesn't unwind — subsequent
    ;; body forms run after it (see core.lisp's caveat). So we
    ;; structure each iteration as cond: if the test fires, return;
    ;; else run body + steps. The cond branch with `return` is the
    ;; ONLY form in that branch — nothing follows to execute after
    ;; the return-flag is set.
    ;; Iteration shape:
    ;;   pre-body tests first (for / while / until that appeared BEFORE
    ;;   any do clause), then body, then post-body tests (while / until
    ;;   that appeared AFTER do clauses), then steps. Each test bails
    ;;   out of the implicit block. With only pre-tests this collapses
    ;;   to the original shape.
    ;;
    ;; A non-trivial subtlety: when STEPS is empty, the inner
    ;; `(t ,@steps)` clause becomes `(t)`, which NCL's lowerer rejects
    ;; ("cond clause with only a test"). Splice a NIL body in that
    ;; case so the resulting cond is always well-formed.
    (let* ((step-clause (if steps `(t ,@steps) '(t nil)))
           (post-test-cond
            (cond
              ;; No post-test → skip the inner cond entirely; just
              ;; run steps after body.
              ((null post-tests) `(progn ,@steps))
              (t
               `(cond
                  (,post-test-form (return nil))
                  ,step-clause)))))
      `(block ,name
         (let* ,all-bindings
           ,@initially
           (loop
             (cond
               (,test-form (return nil))
               (t ,@body
                  ,post-test-cond)))
           ,@(%loop-collect-into-reversals plan)
           ,@finally
           ,result)))))

;; ── Macro redefinition ────────────────────────────────────────────────────
;;
;; The dispatch: simple-loop usage (first form is a list or
;; non-keyword atom) goes to the old %native-loop path; extended-
;; loop usage goes through the parser + emitter.

(defmacro loop (&rest body)
  (cond
    ((null body)              `(%native-loop (lambda ())))
    ((consp (car body))       `(%native-loop (lambda () ,@body)))
    ((null (car body))        `(%native-loop (lambda () ,@body)))
    ((%loop-keyword-p (car body))
     (%loop-emit (%parse-loop body)))
    (t                        `(%native-loop (lambda () ,@body)))))

(provide 'loop)
nil
