;;;; Lisp/Library/memoize.lisp
;;;;
;;;; Function memoization, after Norvig (PAIP §9) by way of Tim
;;;; Bradshaw's port in Corman's `examples/memoize.lisp`. Same
;;;; mechanism as `trace`: swap a symbol's function cell for a
;;;; wrapper that consults a cache, falling through to the original
;;;; only on a miss.
;;;;
;;;; Usage:
;;;;
;;;;   (require 'memoize)
;;;;   (defun fib (n) (if (<= n 1) n (+ (fib (- n 1)) (fib (- n 2)))))
;;;;   (memoize-function 'fib)
;;;;   (fib 30)         ; instant — cached
;;;;   (clear-memoized-function 'fib)
;;;;   (unmemoize-function 'fib)
;;;;
;;;; Pairs naturally with `(trace fib)`:
;;;;
;;;;   (require 'trace)
;;;;   (require 'memoize)
;;;;   (defun fib (n) (if (<= n 1) n (+ (fib (- n 1)) (fib (- n 2)))))
;;;;   (memoize-function 'fib)
;;;;   (trace fib)
;;;;   (fib 5)
;;;;   ;; First-time args see the recursive descent in the trace;
;;;;   ;; subsequent appearances of the same n return from the cache
;;;;   ;; without recursing.
;;;;
;;;; Storage is a per-function hash-table. The wrapper picks one
;;;; of three paths depending on arity:
;;;;
;;;;   * 0 args  → one cell under :no-args.
;;;;   * 1 arg   → the arg is the key. Fixnums / chars / symbols /
;;;;               T / NIL all hash consistently under NCL's
;;;;               current identity-based `%word-hash`, so this
;;;;               path is genuinely O(1) and is what the common
;;;;               fib / factorial / lookup-table workloads need.
;;;;   * N args  → fall back to an alist under :multi, linear-
;;;;               scanned with `equal`. Correct, O(n), fine for
;;;;               typical caches. Upgrades to a real hash-table
;;;;               path once `%word-hash` gets structural hashing
;;;;               for cons / string keys (abi.rs:1713 TODO).

(defparameter *memoized-functions* nil
  "Alist of (NAME TABLE ORIGINAL). TABLE is the hash-table this
   function's wrapper consults; ORIGINAL is the function value
   the wrapper falls through to on a miss.")

;; ── Internals ────────────────────────────────────────────────────────────

(defun %make-memo-wrapper (name orig table)
  "The closure installed in NAME's function cell. Routes around
   the identity-hash limitation by special-casing arity:

     * 0 args  → cache under :no-args
     * 1 arg   → use the arg as the hash key directly (fixnums,
                 chars, symbols, T, NIL all hash consistently)
     * N args  → fall back to an alist under the :multi key,
                 linear-scanned with `equal`. Correct but O(n)."
  (lambda (&rest args)
    (cond
      ((null args)
       (multiple-value-bind (val found) (gethash :no-args table)
         (cond (found val)
               (t (let ((r (funcall orig)))
                    (setf (gethash :no-args table) r)
                    r)))))
      ((null (cdr args))
       (let ((k (car args)))
         (multiple-value-bind (val found) (gethash k table)
           (cond (found val)
                 (t (let ((r (apply orig args)))
                      (setf (gethash k table) r)
                      r))))))
      (t
       (let* ((alist (gethash :multi table))
              (hit (assoc args alist :test #'equal)))
         (cond (hit (cdr hit))
               (t (let ((r (apply orig args)))
                    (setf (gethash :multi table)
                          (cons (cons args r) alist))
                    r))))))))

(defun %remove-memo-alist (key alist)
  (cond
    ((null alist) nil)
    ((eq (car (car alist)) key) (cdr alist))
    (t (cons (car alist) (%remove-memo-alist key (cdr alist))))))

;; ── Public API ──────────────────────────────────────────────────────────

(defun function-memoized-p (name)
  "T iff NAME is currently memoized."
  (if (assoc name *memoized-functions*) t nil))

(defun memoize-function (name)
  "Install a memoizing wrapper on NAME's function cell. Subsequent
   calls with the same arguments (compared via `equal`) return the
   cached result instead of invoking the original.

   Returns NAME on success; signals an error if NAME has no
   function binding or is already memoized."
  (cond
    ((not (symbolp name))
     (error "memoize-function: not a symbol: ~A" name))
    ((not (fboundp name))
     (error "memoize-function: ~A has no function binding" name))
    ((function-memoized-p name)
     (error "memoize-function: ~A is already memoized" name))
    (t
     (let ((table (make-hash-table :test 'equal))
           (orig (symbol-function name)))
       (setf (symbol-function name)
             (%make-memo-wrapper name orig table))
       (setq *memoized-functions*
             (cons (list name table orig) *memoized-functions*))
       name))))

(defun unmemoize-function (name)
  "Restore NAME's original function. Errors if NAME isn't memoized."
  (let ((hit (assoc name *memoized-functions*)))
    (cond
      ((null hit)
       (error "unmemoize-function: ~A is not memoized" name))
      (t
       (setf (symbol-function name) (car (cdr (cdr hit))))
       (setq *memoized-functions*
             (%remove-memo-alist name *memoized-functions*))
       name))))

(defun unmemoize-functions ()
  "Unmemoize every currently-memoized function."
  (let ((names (mapcar #'car *memoized-functions*)))
    (dolist (n names) (unmemoize-function n))
    names))

(defun clear-memoized-function (name)
  "Empty NAME's cache; the wrapper stays installed."
  (let ((hit (assoc name *memoized-functions*)))
    (cond
      ((null hit)
       (error "clear-memoized-function: ~A is not memoized" name))
      (t
       (clrhash (car (cdr hit)))
       name))))

(defun clear-memoized-functions ()
  "Empty every memoized function's cache."
  (let ((names (mapcar #'car *memoized-functions*)))
    (dolist (n names) (clear-memoized-function n))
    names))

;; ── Convenience macro ──────────────────────────────────────────────────

(defmacro memoize (&rest names)
  "Memoize each NAME. With no NAMES, returns the currently
   memoized list."
  (cond
    ((null names) `(mapcar #'car *memoized-functions*))
    (t `(progn
          ,@(mapcar (lambda (n) `(memoize-function ',n)) names)
          (mapcar #'car *memoized-functions*)))))

(defmacro unmemoize (&rest names)
  "Unmemoize each NAME. With no NAMES, unmemoize everything."
  (cond
    ((null names) `(unmemoize-functions))
    (t `(progn
          ,@(mapcar (lambda (n) `(unmemoize-function ',n)) names)
          (mapcar #'car *memoized-functions*)))))

(provide 'memoize)
nil
