;;;; prolog.lisp — a small Prolog interpreter, in NCL.
;;;;
;;;; Unification + SLD resolution with backtracking, after Norvig's
;;;; "Paradigms of AI Programming" (chapter 11). Clauses live on symbol
;;;; property lists; variables are symbols whose name starts with ?;
;;;; bare ? is an anonymous (fresh-each-occurrence) variable.
;;;;
;;;; Run:  ncl --lean --load demos/prolog.lisp
;;;;
;;;; It demonstrates list membership, relational append (forwards and
;;;; backwards), and finishes by solving the Zebra puzzle — Einstein's
;;;; riddle — a classic heavy-backtracking benchmark.

;; ── unification ───────────────────────────────────────────────────

(defconstant +fail+ nil)
(defconstant +no-bindings+ '((t . t)))

(defun variable-p (x)
  (and (symbolp x) (not (null x))
       (eql (char (symbol-name x) 0) #\?)))
(defun get-binding (var bindings) (assoc var bindings))
(defun lookup (var bindings) (cdr (get-binding var bindings)))
(defun extend-bindings (var val bindings)
  (cons (cons var val) (if (eq bindings +no-bindings+) nil bindings)))

(defun unify (x y &optional (bindings +no-bindings+))
  (cond ((eq bindings +fail+) +fail+)
        ((eql x y) bindings)
        ((variable-p x) (unify-variable x y bindings))
        ((variable-p y) (unify-variable y x bindings))
        ((and (consp x) (consp y))
         (unify (rest x) (rest y) (unify (first x) (first y) bindings)))
        (t +fail+)))
(defun unify-variable (var x bindings)
  (cond ((get-binding var bindings) (unify (lookup var bindings) x bindings))
        ((and (variable-p x) (get-binding x bindings))
         (unify var (lookup x bindings) bindings))
        (t (extend-bindings var x bindings))))

(defun subst-bindings (bindings x)
  (cond ((eq bindings +fail+) +fail+)
        ((eq bindings +no-bindings+) x)
        ((and (variable-p x) (get-binding x bindings))
         (subst-bindings bindings (lookup x bindings)))
        ((atom x) x)
        (t (cons (subst-bindings bindings (car x))
                 (subst-bindings bindings (cdr x))))))

;; ── variable renaming + anonymous variables ───────────────────────

(defun unique-find-anywhere-if (pred tree &optional found)
  (if (atom tree)
      (if (funcall pred tree) (adjoin tree found) found)
      (unique-find-anywhere-if pred (car tree)
        (unique-find-anywhere-if pred (cdr tree) found))))
(defvar *var-counter* 0)
(defun new-variable (var)
  (intern (format nil "~A_~D" (symbol-name var)
                  (setq *var-counter* (+ *var-counter* 1)))))
(defun rename-variables (x)
  (sublis (mapcar (lambda (v) (cons v (new-variable v)))
                  (unique-find-anywhere-if #'variable-p x))
          x))
(defun replace-?-vars (exp)
  (cond ((eq exp '?) (gensym "?"))
        ((atom exp) exp)
        (t (cons (replace-?-vars (car exp))
                 (replace-?-vars (cdr exp))))))

;; ── clause database (symbol plists) ───────────────────────────────

(defvar *db-predicates* nil)
(defun predicate (relation) (first relation))
(defun clause-head (clause) (first clause))
(defun clause-body (clause) (rest clause))
(defun get-clauses (pred) (get pred 'clauses))
(defun add-clause (clause)
  (let ((pred (predicate (clause-head clause))))
    (pushnew pred *db-predicates*)
    (setf (get pred 'clauses) (append (get-clauses pred) (list clause)))
    pred))
(defmacro <- (&rest clause)
  `(add-clause (replace-?-vars ',clause)))

;; ── the resolution engine ─────────────────────────────────────────

(defun prove-all (goals bindings)
  (cond ((eq bindings +fail+) +fail+)
        ((null goals) bindings)
        (t (prove (first goals) bindings (rest goals)))))
(defun prove (goal bindings other-goals)
  (some (lambda (clause)
          (let ((new (rename-variables clause)))
            (prove-all (append (clause-body new) other-goals)
                       (unify goal (clause-head new) bindings))))
        (get-clauses (predicate goal))))
(defun solve (goal)
  "Return GOAL with the first solution's bindings applied, or 'no."
  (let ((r (prove-all (list (replace-?-vars goal)) +no-bindings+)))
    (if (eq r +fail+) 'no (subst-bindings r goal))))

;; ── facts and rules ───────────────────────────────────────────────

(<- (member ?item (?item . ?rest)))
(<- (member ?item (?x . ?rest)) (member ?item ?rest))
(<- (append nil ?ys ?ys))
(<- (append (?x . ?xs) ?ys (?x . ?zs)) (append ?xs ?ys ?zs))
(<- (iright ?l ?r (?l ?r . ?rest)))
(<- (iright ?l ?r (?x . ?rest)) (iright ?l ?r ?rest))
(<- (nextto ?x ?y ?list) (iright ?x ?y ?list))
(<- (nextto ?x ?y ?list) (iright ?y ?x ?list))
(<- (= ?x ?x))

;; ── the Zebra puzzle — house = (house nationality pet smoke drink color) ──

(<- (zebra ?h ?water-drinker ?zebra-owner)
    (= ?h ((house norwegian ? ? ? ?) ? (house ? ? ? milk ?) ? ?))
    (member (house englishman ? ? ? red) ?h)
    (member (house spaniard dog ? ? ?) ?h)
    (member (house ? ? ? coffee green) ?h)
    (member (house ukrainian ? ? tea ?) ?h)
    (iright (house ? ? ? ? ivory) (house ? ? ? ? green) ?h)
    (member (house ? snails winston ? ?) ?h)
    (member (house ? ? kools ? yellow) ?h)
    (nextto (house ? ? chesterfield ? ?) (house ? fox ? ? ?) ?h)
    (nextto (house ? ? kools ? ?) (house ? horse ? ? ?) ?h)
    (member (house ? ? luckystrike orangejuice ?) ?h)
    (member (house japanese ? parliaments ? ?) ?h)
    (nextto (house norwegian ? ? ? ?) (house ? ? ? ? blue) ?h)
    (member (house ?water-drinker ? ? water ?) ?h)
    (member (house ?zebra-owner zebra ? ? ?) ?h))

;; ── run the demos ─────────────────────────────────────────────────

(format t "~%=== Prolog in NCL ===~%~%")
(format t "member 3 in (1 2 3 4) ? ~A~%"
        (if (eq (solve '(member 3 (1 2 3 4))) 'no) "no" "yes"))
(format t "member 9 in (1 2 3 4) ? ~A~%"
        (if (eq (solve '(member 9 (1 2 3 4))) 'no) "no" "yes"))
(format t "append (a b) (c d) -> ~A~%"
        (fourth (solve '(append (a b) (c d) ?z))))
(format t "split (1 2 3): first solution -> ~A~%"
        (let ((s (solve '(append ?x ?y (1 2 3))))) (list (second s) (third s))))

(format t "~%Solving the Zebra puzzle (heavy backtracking)...~%")
(let ((answer (solve '(zebra ?houses ?water ?zebra))))
  (if (eq answer 'no)
      (format t "  no solution~%")
      (progn
        (format t "  The ~A drinks water.~%" (third answer))
        (format t "  The ~A owns the zebra.~%" (fourth answer)))))
(format t "~%done.~%")
