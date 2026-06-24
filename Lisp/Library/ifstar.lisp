;;;; Lisp/Library/ifstar.lisp
;;;;
;;;; Foderaro's `if*` macro, public domain — the more legible
;;;; conditional that ships in Allegro CL (the `:excl` package)
;;;; and got included in Corman as `examples/ifstar.lisp`. Same
;;;; semantics; we drop the `(defpackage :excl ...)` shell since
;;;; NCL is single-namespace. The macro expands to a plain `cond`.
;;;;
;;;; Syntax:
;;;;
;;;;   (if* TEST then  CONSEQUENT-FORMS...
;;;;        elseif TEST then CONSEQUENT-FORMS...
;;;;        else   ALT-FORMS...)
;;;;
;;;;   (if* TEST thenret)
;;;;       ; consequent IS the test value — useful for
;;;;       ; (if* (member x list) thenret) returning the tail
;;;;
;;;; Compared to nested `if` or shaggy `cond`, the keyword
;;;; alignment makes long conditionals readable:
;;;;
;;;;   (if* (eq op :add) then
;;;;          (incf counter)
;;;;          (push val results)
;;;;        elseif (eq op :reset) then
;;;;          (setq counter 0)
;;;;          (setq results nil)
;;;;        else
;;;;          (error "unknown op ~A" op))
;;;;
;;;; Macro walks the args in reverse, runs a tiny state machine,
;;;; builds up a list of `(test . body)` clauses, wraps it in
;;;; `cond`. Same algorithm Foderaro published; only the host
;;;; iteration syntax changes (`do` → our `loop`).

(defparameter *if-keywords*
  '(then thenret else elseif)
  "The four marker symbols that punctuate an if* form. NCL's
   reader case-folds, so `Then` / `THEN` / `then` all read as the
   same symbol; comparing by `eq` here is enough.")

(defun %if*-keyword? (s)
  "Return the canonical keyword symbol if S is one; else NIL.
   Foderaro's original used `symbol-name` + `string-equal`; we
   don't have symbol-name exposed yet but case-folding at read
   time makes plain `eq` equivalent."
  (and (symbolp s)
       (cond
         ((eq s 'then)    'then)
         ((eq s 'thenret) 'thenret)
         ((eq s 'else)    'else)
         ((eq s 'elseif)  'elseif)
         (t nil))))

(defun %if*-finalize (total-col)
  "Build the final form. If any clause has no body (the `thenret`
   case — return the test value if true), rewrite all empty-body
   clauses to `((setq it TEST) it)` and wrap the whole cond in a
   `(let ((it nil)) …)`. Standard CL would handle `(cond (test))`
   directly, but NCL's compiler doesn't yet, so we lift the value
   into an anaphoric variable ourselves."
  (let ((any-empty nil))
    (dolist (clause total-col)
      (when (null (cdr clause))
        (setq any-empty t)))
    (cond
      (any-empty
       (let ((it (gensym "IT-")))
         `(let ((,it nil))
            (cond
              ,@(mapcar
                 (lambda (clause)
                   (cond
                     ((null (cdr clause))
                      ;; `(test)` → `((setq it test) it)`
                      `((setq ,it ,(car clause)) ,it))
                     (t clause)))
                 total-col)))))
      (t `(cond ,@total-col)))))

(defmacro if* (&rest args)
  "Foderaro's `if*`. Walks ARGS to build a cond. See the file
   header for syntax. Errors point at the offending token.

   Implementation note: our `loop`'s `(return)` sets a flag
   that the loop honours *after the current iteration body
   completes*, not via a stack unwind. So we have to gate every
   tail-after-return path with a `cond` — there's no falling
   off the end of a `when` after a return."
  (let ((xx (reverse args))
        (state :init)
        (else-seen nil)
        (total-col nil)
        (col nil))
    (loop
      (cond
        ;; ── Loop exit: we've consumed every arg ──
        ((null xx)
         (cond
           ((eq state :compl) (return (%if*-finalize total-col)))
           (t (error "if*: illegal form ~S" args))))
        ;; ── Otherwise step the state machine on (car xx) ──
        (t
         (let* ((tok (car xx))
                (lookat (%if*-keyword? tok)))
           (cond
             ;; ── :init — haven't started a clause yet ──
             ((eq state :init)
              (cond
                (lookat
                 (cond
                   ((eq lookat 'thenret)
                    (setq col nil)
                    (setq state :then))
                   (t (error "if*: bad keyword ~A" lookat))))
                (t
                 (setq state :col)
                 (setq col nil)
                 (setq col (cons tok col)))))
             ;; ── :col — collecting consequent forms ──
             ((eq state :col)
              (cond
                (lookat
                 (cond
                   ((eq lookat 'else)
                    (when else-seen
                      (error "if*: multiple elses"))
                    (setq else-seen t)
                    (setq state :init)
                    (setq total-col (cons (cons 't col) total-col)))
                   ((eq lookat 'then)
                    (setq state :then))
                   (t (error "if*: bad keyword ~S" lookat))))
                (t
                 (setq col (cons tok col)))))
             ;; ── :then — next token is the test expression ──
             ((eq state :then)
              (cond
                (lookat
                 (error "if*: keyword ~S at the wrong place" tok))
                (t
                 (setq state :compl)
                 (setq total-col (cons (cons tok col) total-col)))))
             ;; ── :compl — clause finished; expect `elseif` ──
             ((eq state :compl)
              (cond
                ((not (eq lookat 'elseif))
                 (error "if*: missing elseif clause")))
              (setq state :init))))
         (setq xx (cdr xx)))))))

(provide 'ifstar)
nil
