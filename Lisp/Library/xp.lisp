;;;; Lisp/Library/xp.lisp — XP Pretty Printer (NCL port)
;;;;
;;;; Port of Richard C. Waters' XP Pretty Printer (MIT, November 1991)
;;;; as integrated in Corman Lisp by Roger Corman (2003-2008).
;;;; NCL adaptation by the NewCormanLisp project.
;;;;
;;;; Key adaptations from Corman Lisp's Sys/xp.lisp:
;;;;   * Flat namespace (no make-package / in-package)
;;;;   * Generic vectors for buffer/prefix/suffix arrays (aref, not char)
;;;;   * No #. read-time eval — all constants inlined
;;;;   * No :conc-name nil in defstruct — explicit alias functions
;;;;   * No macrolet — pprint-logical-block+ uses tree-walk substitution
;;;;   * No prog/go — compile-format and ~{...~} use block/loop
;;;;   * (compile nil pred) → (eval pred) for predicate compilation
;;;;   * NCL stream API: stream-write-string, stream-write-char, stream-terpri

(provide 'xp)

;;; ========================================================================
;;; Part 1: NCL Compatibility Layer
;;; ========================================================================
;;; Capture native operations before we redefine them, define a handful of
;;; CL utility functions that NCL hasn't grown yet, and bind the *print-*
;;; specials.

;; -- Native function captures -----------------------------------------------

(defparameter %xp-native-format        (symbol-function 'format))
(defparameter %xp-native-write-string  (symbol-function 'stream-write-string))
(defparameter %xp-native-write-char    (symbol-function 'stream-write-char))
(defparameter %xp-native-terpri        (symbol-function 'stream-terpri))
(defparameter %xp-native-streamp       (symbol-function 'streamp))

;; ── catch / throw shims ────────────────────────────────────────────
;;
;; NCL doesn't have CL's CATCH/THROW. We layer them on top of the
;; condition system: throwing signals a %xp-throw with tag+value;
;; catch wraps the body in a handler that re-signals if the tag
;; doesn't match.

(defclass %xp-throw (condition)
  ((tag :initarg :tag :initform nil)
   (value :initarg :value :initform nil)))

(defmacro catch (tag &rest body)
  (let ((cv (gensym "C"))
        (tg (gensym "TAG")))
    `(let ((,tg ,tag))
       (handler-case (progn ,@body)
         (%xp-throw (,cv)
           (if (eq (slot-value ,cv 'tag) ,tg)
               (slot-value ,cv 'value)
               (error ,cv)))))))

(defun throw (tag value)
  (error (make-instance '%xp-throw :tag tag :value value)))

;; -- Print control variables ------------------------------------------------

;; *standard-output* — defaulted to T (the native format dest meaning
;; "write to stdout"). XP's redefined format / print / princ / etc.
;; reference this when called as e.g. (format t …) or with a missing
;; stream arg; without a definition here those forms fail with
;; "unbound variable" at runtime. NCL doesn't have stream objects for
;; stdin/stdout — the native format's dest=T path handles output
;; directly, so T-as-sentinel works correctly everywhere downstream.
(defvar *standard-output* t)
(defvar *error-output* t)
(defvar *terminal-io* t)

(defvar *print-escape* t)
(defvar *print-base* 10)
(defvar *print-radix* nil)
(defvar *print-circle* nil)
(defvar *print-pretty* nil)
(defvar *print-level* nil)
(defvar *print-length* nil)
(defvar *print-case* :upcase)
(defvar *print-gensym* t)
(defvar *print-array* t)
(defvar *print-readably* nil)
(defvar *print-shared* nil)
(defvar *print-right-margin* nil)
(defvar *print-miser-width* 40)
(defvar *print-lines* nil)
(defvar *default-right-margin* 70)
(defvar *print-pprint-dispatch* t)        ; set at end of file
(defvar *last-abbreviated-printing*
  (lambda (&optional stream) (declare (ignore stream)) nil))

(defvar *ipd* nil)                        ; initial print dispatch table
(defvar *current-level* 0)
(defvar *current-length* 0)
(defvar *abbreviation-happened* nil)
(defvar *result* nil)
(defvar *locating-circularities* nil)
(defvar *parents* nil)
(defvar *circularity-hash-table* nil)
(defvar *free-circularity-hash-tables* nil)
(defvar *free-xps* nil)

;; -- Missing CL utilities ---------------------------------------------------

(defmacro proclaim (decl) (declare (ignore decl)) nil)

(defmacro multiple-value-setq (vars form)
  "(multiple-value-setq (v1 v2 ...) form) — set each Vi to the
   corresponding return value of FORM. Returns the primary value."
  (let ((mvl (gensym "MVL")))
    `(let ((,mvl (multiple-value-list ,form)))
       ,@(let ((result nil) (i 0))
           (dolist (v vars)
             (push `(setq ,v (nth ,i ,mvl)) result)
             (setq i (+ i 1)))
           (nreverse result))
       (car ,mvl))))

(defun nreconc (x y) (nconc (nreverse x) y))

(defmacro pushnew (item place &rest keys)
  (declare (ignore keys))
  `(unless (member ,item ,place) (setq ,place (cons ,item ,place))))

(defun hash-table-p (x)
  "True iff X looks like an NCL hash-table (a vector whose
   slot-0 is one of the test symbols eq / eql / equal)."
  (and (vectorp x)
       (not (stringp x))
       (>= (length x) 3)
       (let ((test (svref x 0)))
         (or (eq test 'eq) (eq test 'eql) (eq test 'equal)))))

(defun string-upcase (s)
  (let ((out "") (i 0) (n (length s)))
    (loop
      (cond ((>= i n) (return out))
            (t (setq out (string-append-char out (char-upcase (char s i))))
               (setq i (+ i 1)))))))

(defun string-downcase (s)
  (let ((out "") (i 0) (n (length s)))
    (loop
      (cond ((>= i n) (return out))
            (t (setq out (string-append-char out (char-downcase (char s i))))
               (setq i (+ i 1)))))))

(defun parse-integer (string &rest opts)
  (let* ((start (or (getf opts :start) 0))
         (end-arg (getf opts :end))
         (radix (or (getf opts :radix) 10))
         (n (length string))
         (real-end (or end-arg n))
         (i start)
         (neg nil)
         (acc 0)
         (saw-digit nil))
    (block pi-loop
      (loop
        (cond ((>= i real-end)
               (return-from pi-loop
                 (if saw-digit (if neg (- acc) acc) nil)))
              ((and (= i start) (char= (char string i) #\+))
               (setq i (+ i 1)))
              ((and (= i start) (char= (char string i) #\-))
               (setq neg t)
               (setq i (+ i 1)))
              (t
               (let* ((c (char string i))
                      (d (digit-char-p c radix)))
                 (cond (d
                        (setq acc (+ (* acc radix) d))
                        (setq saw-digit t)
                        (setq i (+ i 1)))
                       (t (return-from pi-loop
                            (if saw-digit
                                (if neg (- acc) acc)
                                nil)))))))))))

;; Position with :start / :end / :from-end support. NCL's library
;; position only takes :test/:key — XP needs the windowed scan and
;; the reverse-direction scan, so we provide our own.

(defun %xp-position (item seq &key (test #'eql) (start 0) end from-end)
  (let* ((n (length seq))
         (real-end (or end n)))
    (cond
      (from-end
       (let ((i (- real-end 1)))
         (block %xp-pos-back
           (loop
             (cond ((< i start) (return-from %xp-pos-back nil))
                   ((funcall test item (elt seq i)) (return-from %xp-pos-back i))
                   (t (setq i (- i 1))))))))
      (t
       (let ((i start))
         (block %xp-pos-fwd
           (loop
             (cond ((>= i real-end) (return-from %xp-pos-fwd nil))
                   ((funcall test item (elt seq i)) (return-from %xp-pos-fwd i))
                   (t (setq i (+ i 1)))))))))))

(defun %xp-position-if (pred seq &key (start 0) end from-end)
  (let* ((n (length seq))
         (real-end (or end n)))
    (cond
      (from-end
       (let ((i (- real-end 1)))
         (block %xp-pos-if-back
           (loop
             (cond ((< i start) (return-from %xp-pos-if-back nil))
                   ((funcall pred (elt seq i)) (return-from %xp-pos-if-back i))
                   (t (setq i (- i 1))))))))
      (t
       (let ((i start))
         (block %xp-pos-if-fwd
           (loop
             (cond ((>= i real-end) (return-from %xp-pos-if-fwd nil))
                   ((funcall pred (elt seq i)) (return-from %xp-pos-if-fwd i))
                   (t (setq i (+ i 1)))))))))))

(defun %xp-position-if-not (pred seq &key (start 0) end from-end)
  (%xp-position-if (lambda (x) (not (funcall pred x)))
                   seq :start start :end end :from-end from-end))

;; Convert a subrange of a generic character vector into a string.
;; NCL buffers are general vectors of characters, but the stream API
;; needs an actual string.

(defun %xp-chars->string (vec start end)
  (with-output-to-string (s)
    (let ((i start))
      (loop
        (cond ((>= i end) (return nil))
              (t (stream-write-char s (aref vec i))
                 (setq i (+ i 1))))))))

;; -- Type predicates and platform shims -------------------------------------

;; NCL has no `keywordp`. Define it as "symbol whose printed name
;; starts with `:`" — matches NCL's reader convention for keywords.
(defun keywordp (x)
  (and (symbolp x)
       (not (null x))
       (let ((n (symbol-name x)))
         (and (> (length n) 0)
              (char= (char n 0) #\:)))))

(defun simple-atom-p (x) (and (atom x) (not (symbolp x))))
(defun otherp (x) (not (or (consp x) (symbolp x))))
(defun commonp (x) (declare (ignore x)) t)
(defun bit-vector-p (x) (declare (ignore x)) nil)
(defun simple-vector-p (x) (and (vectorp x) (not (stringp x))))
(defun simple-string-p (x) (stringp x))
(defun simple-bit-vector-p (x) (declare (ignore x)) nil)
(defun packagep (x) (declare (ignore x)) nil)
(defun compiled-function-p (x) (functionp x))

(defparameter %xp-structure-printers (make-hash-table :test 'eq))

(defun structure-printer-for (sym)
  (gethash sym %xp-structure-printers))

(defun (setf structure-printer-for) (val sym)
  (setf (gethash sym %xp-structure-printers) val))

(defun structure-type-p (x)
  "True iff X names a structure type known to XP. Lookup goes through
   our %xp-structure-printers map (NCL doesn't have symbol-plist /
   GET / PUTPROP, so we use a hash table instead)."
  (and (symbolp x) (structure-printer-for x)))

(defun output-width    (&optional (s nil)) (declare (ignore s)) nil)
(defun output-position (&optional (s nil)) (declare (ignore s)) nil)

;;; ========================================================================
;;; Part 2: Circularity Hash Tables
;;; ========================================================================

(defun get-circularity-hash-table ()
  (let ((tbl (pop *free-circularity-hash-tables*)))
    (if tbl tbl (make-hash-table :test 'eq))))

(defun free-circularity-hash-table (tbl)
  (clrhash tbl)
  (pushnew tbl *free-circularity-hash-tables*))

;;; ========================================================================
;;; Part 3: Dispatch Tables
;;; ========================================================================

;; The defstruct in NCL doesn't support :conc-name nil. We define the
;; struct with default naming then provide accessor aliases that match
;; the names XP's body code expects.

(defstruct pprint-dispatch
  (conses-with-cars nil)
  (structures nil)
  (others nil))

(defun %make-empty-dispatch ()
  (let ((tbl (make-pprint-dispatch)))
    (setf (pprint-dispatch-conses-with-cars tbl) (make-hash-table :test 'eq))
    (setf (pprint-dispatch-structures      tbl) (make-hash-table :test 'eq))
    (setf (pprint-dispatch-others          tbl) nil)
    tbl))

(defun conses-with-cars (d) (pprint-dispatch-conses-with-cars d))
(defun (setf conses-with-cars) (v d) (setf (pprint-dispatch-conses-with-cars d) v))
(defun structures (d) (pprint-dispatch-structures d))
(defun (setf structures) (v d) (setf (pprint-dispatch-structures d) v))
(defun others (d) (pprint-dispatch-others d))
(defun (setf others) (v d) (setf (pprint-dispatch-others d) v))

(defstruct entry test fn full-spec)

(defun copy-entry (e)
  (make-entry :test     (entry-test e)
              :fn       (entry-fn e)
              :full-spec (entry-full-spec e)))

(defun test (e) (entry-test e))
(defun (setf test) (v e) (setf (entry-test e) v))
(defun fn   (e) (entry-fn e))
(defun (setf fn) (v e) (setf (entry-fn e) v))
(defun full-spec (e) (entry-full-spec e))
(defun (setf full-spec) (v e) (setf (entry-full-spec e) v))

(defun copy-pprint-dispatch (&optional (table *print-pprint-dispatch*))
  (when (null table) (setq table *ipd*))
  (let* ((new (%make-empty-dispatch))
         (new-cc (conses-with-cars new))
         (new-st (structures new)))
    (maphash (lambda (k v) (setf (gethash k new-cc) (copy-entry v)))
             (conses-with-cars table))
    (maphash (lambda (k v) (setf (gethash k new-st) (copy-entry v)))
             (structures table))
    (setf (others new) (copy-list (others table)))
    new))

(defun priority-> (x y)
  (if (consp x)
      (if (consp y) (> (car x) (car y)) nil)
      (if (consp y) t (> x y))))

(defun adjust-counts (table priority delta)
  (maphash (lambda (k v)
             (declare (ignore k))
             (when (priority-> priority (car (full-spec v)))
               (setf (test v) (+ (test v) delta))))
           (conses-with-cars table))
  (maphash (lambda (k v)
             (declare (ignore k))
             (when (priority-> priority (car (full-spec v)))
               (setf (test v) (+ (test v) delta))))
           (structures table)))

(defun set-pprint-dispatch (type-specifier function
                            &optional (priority 0) (table *print-pprint-dispatch*))
  (when (or (not (numberp priority)) (complexp priority))
    (error "invalid PRIORITY argument ~A to SET-PPRINT-DISPATCH" priority))
  (set-pprint-dispatch+ type-specifier function priority table))

(defun set-pprint-dispatch+ (type-specifier function priority table)
  (let* ((category (specifier-category type-specifier))
         (pred (cond ((not (eq category 'other)) nil)
                     (t (compile-specifier-pred type-specifier))))
         (e (if function
                (make-entry :test pred :fn function
                            :full-spec (list priority type-specifier)))))
    (case category
      (cons-with-car
       (cond ((null e)
              (remhash (car (cdr (cadr type-specifier))) (conses-with-cars table)))
             (t
              (setf (test e)
                    (count-if (lambda (other)
                                (priority-> (car (full-spec other)) priority))
                              (others table)))
              (setf (gethash (car (cdr (cadr type-specifier))) (conses-with-cars table)) e))))
      (structure-type
       (cond ((null e)
              (remhash type-specifier (structures table)))
             (t
              (setf (test e)
                    (count-if (lambda (other)
                                (priority-> (car (full-spec other)) priority))
                              (others table)))
              (setf (gethash type-specifier (structures table)) e))))
      (t ; other
       (let ((old (car (member type-specifier (others table) :test #'equal
                               :key (lambda (e2) (cadr (full-spec e2)))))))
         (when old
           (setf (others table) (remove old (others table)))
           (adjust-counts table (car (full-spec old)) -1)))
       (when e
         (let ((lst (cons nil (others table))))
           (do ((l lst (cdr l)))
               ((null (cdr l)) (setf (cdr l) (list e)))
             (when (priority-> priority (car (full-spec (cadr l))))
               (setf (cdr l) (cons e (cdr l)))
               (return nil)))
           (setf (others table) (cdr lst)))
         (adjust-counts table priority 1)))))
  nil)

(defun fits (obj e) (funcall (test e) obj))

(defun get-printer (object table)
  (let ((e (if (consp object)
               (gethash (car object) (conses-with-cars table))
               (gethash (type-of object) (structures table)))))
    (cond ((not e)
           (setq e (find object (others table) :test #'fits)))
          (t
           (do ((i (test e) (- i 1))
                (l (others table) (cdr l)))
               ((zerop i))
             (when (fits object (car l))
               (setq e (car l))
               (return nil)))))
    (when e (fn e))))

(defun pprint-dispatch (object &optional (table *print-pprint-dispatch*))
  (when (null table) (setq table *ipd*))
  (let ((p (get-printer object table)))
    (values (or p #'non-pretty-print) (not (null p)))))

(defun specifier-category (spec)
  (cond ((and (consp spec)
              (eq (car spec) 'cons)
              (consp (cdr spec))
              (null (cddr spec))
              (consp (cadr spec))
              (eq (car (cadr spec)) 'member)
              (consp (cdr (cadr spec)))
              (null (cdr (cdr (cadr spec)))))
         'cons-with-car)
        ((and (symbolp spec) (structure-type-p spec)) 'structure-type)
        (t 'other)))

(defvar *preds-for-specs*
  '((t always-true) (cons consp) (simple-atom simple-atom-p) (other otherp)
    (null null) (symbol symbolp) (atom atom) (cons consp)
    (list listp) (number numberp) (integer integerp)
    (rational rationalp) (float floatp) (complex complexp)
    (character characterp) (string stringp) (bit-vector bit-vector-p)
    (vector vectorp) (simple-vector simple-vector-p)
    (simple-string simple-string-p) (simple-bit-vector simple-bit-vector-p)
    (array arrayp) (package packagep) (function functionp)
    (compiled-function compiled-function-p) (common commonp)))

(defun always-true (x) (declare (ignore x)) t)

(defun specifier-fn (spec) `(lambda (x) ,(convert-body spec)))

;; Direct interpreter for the type-spec → predicate-function mapping.
;; NCL doesn't have `eval`, so we hand-roll closures for the spec forms
;; that XP actually feeds us (atoms, satisfies, member, and / or / not,
;; cons-with-cdr) instead of constructing a lambda and eval'ing it.

(defun compile-specifier-pred (spec)
  (cond
    ((atom spec)
     (let ((p (cadr (assoc spec *preds-for-specs*))))
       (cond (p (symbol-function p))
             (t (lambda (x) (declare (ignore x)) nil)))))
    ((eq (car spec) 'satisfies)
     (symbol-function (cadr spec)))
    ((eq (car spec) 'member)
     (let ((items (copy-list (cdr spec))))
       (lambda (x) (member x items))))
    ((eq (car spec) 'and)
     (let ((preds (mapcar #'compile-specifier-pred (cdr spec))))
       (lambda (x)
         (block and-loop
           (dolist (p preds t)
             (unless (funcall p x) (return-from and-loop nil)))))))
    ((eq (car spec) 'or)
     (let ((preds (mapcar #'compile-specifier-pred (cdr spec))))
       (lambda (x)
         (block or-loop
           (dolist (p preds nil)
             (when (funcall p x) (return-from or-loop t)))))))
    ((eq (car spec) 'not)
     (let ((p (compile-specifier-pred (cadr spec))))
       (lambda (x) (not (funcall p x)))))
    (t (lambda (x) (declare (ignore x)) nil))))

(defun convert-body (spec)
  (cond ((atom spec)
         (let ((p (cadr (assoc spec *preds-for-specs*))))
           (if p `(,p x) `(typep x ',spec))))
        ((member (car spec) '(and or not))
         (cons (car spec) (mapcar #'convert-body (cdr spec))))
        ((eq (car spec) 'member)
         `(member x ',(copy-list (cdr spec))))
        ((eq (car spec) 'cons)
         `(and (consp x)
               ,@(if (cdr spec) `((let ((x (car x))) ,(convert-body (cadr spec)))))
               ,@(if (cddr spec) `((let ((x (cdr x))) ,(convert-body (caddr spec)))))))
        ((eq (car spec) 'satisfies)
         `(funcall (function ,(cadr spec)) x))
        (t `(typep x ',(copy-tree spec)))))

;;; ========================================================================
;;; Part 4: XP Structure
;;; ========================================================================
;;;
;;; All XP buffer constants inlined:
;;;   queue-entry-size = 7      block-stack-entry-size = 1
;;;   prefix-stack-entry-size = 5
;;;   queue-min-size = 525      (75 × 7)
;;;   block-stack-min-size = 35
;;;   prefix-stack-min-size = 150  (30 × 5)
;;;   buffer-min-size = prefix-min-size = suffix-min-size = 256

(defstruct xp-structure
  base-stream linel line-limit line-no
  char-mode char-mode-counter depth-in-blocks
  block-stack block-stack-ptr
  buffer charpos buffer-ptr buffer-offset
  queue qleft qright
  prefix
  prefix-stack prefix-stack-ptr
  suffix)

;; Initialiser for fresh xp-structure with all the slot arrays
;; pre-allocated. (NCL's make-array initial values can't be referenced
;; by defstruct default expressions because the slot defaults are
;; quoted as data.)

(defun %make-xp-structure ()
  (let ((xp (make-xp-structure)))
    (setf (xp-structure-block-stack  xp) (make-array 35))
    (setf (xp-structure-buffer       xp) (make-array 256 :initial-element #\space))
    (setf (xp-structure-queue        xp) (make-array 525))
    (setf (xp-structure-prefix       xp) (make-array 256 :initial-element #\space))
    (setf (xp-structure-prefix-stack xp) (make-array 150))
    (setf (xp-structure-suffix       xp) (make-array 256 :initial-element #\space))
    xp))

;; Slot accessor / setter aliases — XP's code expects bare slot names.

(defun base-stream (x) (xp-structure-base-stream x))
(defun (setf base-stream) (v x) (setf (xp-structure-base-stream x) v))
(defun linel (x) (xp-structure-linel x))
(defun (setf linel) (v x) (setf (xp-structure-linel x) v))
(defun line-limit (x) (xp-structure-line-limit x))
(defun (setf line-limit) (v x) (setf (xp-structure-line-limit x) v))
(defun line-no (x) (xp-structure-line-no x))
(defun (setf line-no) (v x) (setf (xp-structure-line-no x) v))
(defun char-mode (x) (xp-structure-char-mode x))
(defun (setf char-mode) (v x) (setf (xp-structure-char-mode x) v))
(defun char-mode-counter (x) (xp-structure-char-mode-counter x))
(defun (setf char-mode-counter) (v x) (setf (xp-structure-char-mode-counter x) v))
(defun depth-in-blocks (x) (xp-structure-depth-in-blocks x))
(defun (setf depth-in-blocks) (v x) (setf (xp-structure-depth-in-blocks x) v))
(defun block-stack (x) (xp-structure-block-stack x))
(defun (setf block-stack) (v x) (setf (xp-structure-block-stack x) v))
(defun block-stack-ptr (x) (xp-structure-block-stack-ptr x))
(defun (setf block-stack-ptr) (v x) (setf (xp-structure-block-stack-ptr x) v))
(defun buffer (x) (xp-structure-buffer x))
(defun (setf buffer) (v x) (setf (xp-structure-buffer x) v))
(defun charpos (x) (xp-structure-charpos x))
(defun (setf charpos) (v x) (setf (xp-structure-charpos x) v))
(defun buffer-ptr (x) (xp-structure-buffer-ptr x))
(defun (setf buffer-ptr) (v x) (setf (xp-structure-buffer-ptr x) v))
(defun buffer-offset (x) (xp-structure-buffer-offset x))
(defun (setf buffer-offset) (v x) (setf (xp-structure-buffer-offset x) v))
(defun queue (x) (xp-structure-queue x))
(defun (setf queue) (v x) (setf (xp-structure-queue x) v))
(defun qleft (x) (xp-structure-qleft x))
(defun (setf qleft) (v x) (setf (xp-structure-qleft x) v))
(defun qright (x) (xp-structure-qright x))
(defun (setf qright) (v x) (setf (xp-structure-qright x) v))
(defun prefix (x) (xp-structure-prefix x))
(defun (setf prefix) (v x) (setf (xp-structure-prefix x) v))
(defun prefix-stack (x) (xp-structure-prefix-stack x))
(defun (setf prefix-stack) (v x) (setf (xp-structure-prefix-stack x) v))
(defun prefix-stack-ptr (x) (xp-structure-prefix-stack-ptr x))
(defun (setf prefix-stack-ptr) (v x) (setf (xp-structure-prefix-stack-ptr x) v))
(defun suffix (x) (xp-structure-suffix x))
(defun (setf suffix) (v x) (setf (xp-structure-suffix x) v))
(defun (setf suffix) (v x) (setf (xp-structure-suffix x) v))

;; Position macros — total position, line position, buffer position.

(defmacro LP<-BP (xp &optional (ptr nil))
  (if (null ptr) (setq ptr `(buffer-ptr ,xp)))
  `(+ ,ptr (charpos ,xp)))
(defmacro TP<-BP (xp) `(+ (buffer-ptr ,xp) (buffer-offset ,xp)))
(defmacro BP<-LP (xp ptr) `(- ,ptr (charpos ,xp)))
(defmacro BP<-TP (xp ptr) `(- ,ptr (buffer-offset ,xp)))
(defmacro LP<-TP (xp ptr) `(LP<-BP ,xp (BP<-TP ,xp ,ptr)))

;; check-size: grow the named slot's array if PTR walks past the end.
;; Each branch hard-codes its min-size and entry-size (formerly read
;; from XP symbols at macro-expansion via the package system).

(defmacro check-size (xp vect ptr)
  (let ((min-size (case vect
                    (buffer 256) (prefix 256) (suffix 256)
                    (block-stack 35) (prefix-stack 150) (queue 525)))
        (entry-size (case vect
                      (buffer 1) (prefix 1) (suffix 1)
                      (block-stack 1) (prefix-stack 5) (queue 7))))
    `(when (and (> ,ptr ,(- min-size entry-size))
                (> ,ptr (- (length (,vect ,xp)) ,entry-size)))
       (let* ((old (,vect ,xp))
              (new (make-array (+ ,ptr ,(if (= entry-size 1) 50
                                            (* 10 entry-size))))))
         (replace new old)
         (setf (,vect ,xp) new)))))

;; Block stack — one slot per entry: section-start.

(defmacro section-start (xp) `(aref (block-stack ,xp) (block-stack-ptr ,xp)))

(defun push-block-stack (xp)
  (setf (block-stack-ptr xp) (+ (block-stack-ptr xp) 1))
  (check-size xp block-stack (block-stack-ptr xp)))

(defun pop-block-stack (xp)
  (setf (block-stack-ptr xp) (- (block-stack-ptr xp) 1)))

;; Prefix stack — five slots per entry.

(defmacro prefix-ptr (xp)
  `(aref (prefix-stack ,xp) (prefix-stack-ptr ,xp)))
(defmacro suffix-ptr (xp)
  `(aref (prefix-stack ,xp) (+ (prefix-stack-ptr ,xp) 1)))
(defmacro non-blank-prefix-ptr (xp)
  `(aref (prefix-stack ,xp) (+ (prefix-stack-ptr ,xp) 2)))
(defmacro initial-prefix-ptr (xp)
  `(aref (prefix-stack ,xp) (+ (prefix-stack-ptr ,xp) 3)))
(defmacro section-start-line (xp)
  `(aref (prefix-stack ,xp) (+ (prefix-stack-ptr ,xp) 4)))

(defun push-prefix-stack (xp)
  (let ((old-prefix 0) (old-suffix 0) (old-non-blank 0))
    (when (not (minusp (prefix-stack-ptr xp)))
      (setq old-prefix    (prefix-ptr xp))
      (setq old-suffix    (suffix-ptr xp))
      (setq old-non-blank (non-blank-prefix-ptr xp)))
    (setf (prefix-stack-ptr xp) (+ (prefix-stack-ptr xp) 5))
    (check-size xp prefix-stack (prefix-stack-ptr xp))
    (setf (prefix-ptr xp) old-prefix)
    (setf (suffix-ptr xp) old-suffix)
    (setf (non-blank-prefix-ptr xp) old-non-blank)))

(defun pop-prefix-stack (xp)
  (setf (prefix-stack-ptr xp) (- (prefix-stack-ptr xp) 5)))

;; Queue entry — seven slots: type, kind, pos, depth, end, offset, arg.

(defmacro Qtype   (xp index) `(aref (queue ,xp) ,index))
(defmacro Qkind   (xp index) `(aref (queue ,xp) (+ ,index 1)))
(defmacro Qpos    (xp index) `(aref (queue ,xp) (+ ,index 2)))
(defmacro Qdepth  (xp index) `(aref (queue ,xp) (+ ,index 3)))
(defmacro Qend    (xp index) `(aref (queue ,xp) (+ ,index 4)))
(defmacro Qoffset (xp index) `(aref (queue ,xp) (+ ,index 5)))
(defmacro Qarg    (xp index) `(aref (queue ,xp) (+ ,index 6)))
(defmacro Qnext   (index) `(+ ,index 7))

(defun enqueue (xp type kind &optional arg)
  (setf (qright xp) (+ (qright xp) 7))
  (when (> (qright xp) 518)   ; queue-min-size - queue-entry-size
    (replace (queue xp) (queue xp) :start2 (qleft xp) :end2 (qright xp))
    (setf (qright xp) (- (qright xp) (qleft xp)))
    (setf (qleft xp) 0))
  (check-size xp queue (qright xp))
  (setf (Qtype   xp (qright xp)) type)
  (setf (Qkind   xp (qright xp)) kind)
  (setf (Qpos    xp (qright xp)) (TP<-BP xp))
  (setf (Qdepth  xp (qright xp)) (depth-in-blocks xp))
  (setf (Qend    xp (qright xp)) nil)
  (setf (Qoffset xp (qright xp)) nil)
  (setf (Qarg    xp (qright xp)) arg))

;; Free-pool for recycling XP stream objects.

(defun get-pretty-print-stream (stream)
  (let ((xp (pop *free-xps*)))
    (initialize-xp (if xp xp (%make-xp-structure)) stream)))

(defun free-pretty-print-stream (xp)
  (setf (base-stream xp) nil)
  (pushnew xp *free-xps*))

(defun initialize-xp (xp stream)
  (setf (base-stream xp) stream)
  (setf (linel xp) (max 0 (or *print-right-margin*
                              (output-width stream)
                              *default-right-margin*)))
  (setf (line-limit xp) *print-lines*)
  (setf (line-no xp) 1)
  (setf (char-mode xp) nil)
  (setf (char-mode-counter xp) 0)
  (setf (depth-in-blocks xp) 0)
  (setf (block-stack-ptr xp) 0)
  (setf (charpos xp) (or (output-position stream) 0))
  (setf (section-start xp) 0)
  (setf (buffer-ptr xp) 0)
  (setf (buffer-offset xp) (charpos xp))
  (setf (qleft xp) 0)
  (setf (qright xp) -7)        ; = - queue-entry-size
  (setf (prefix-stack-ptr xp) -5)  ; = - prefix-stack-entry-size
  xp)

;;; ========================================================================
;;; Part 5: Char Modes
;;; ========================================================================

(defun push-char-mode (xp new-mode)
  (when (zerop (char-mode-counter xp))
    (setf (char-mode xp) new-mode))
  (setf (char-mode-counter xp) (+ (char-mode-counter xp) 1)))

(defun pop-char-mode (xp)
  (setf (char-mode-counter xp) (- (char-mode-counter xp) 1))
  (when (zerop (char-mode-counter xp))
    (setf (char-mode xp) nil)))

(defun handle-char-mode (xp char)
  (case (char-mode xp)
    (:cap0 (cond ((not (alphanumericp char)) char)
                 (t (setf (char-mode xp) :down) (char-upcase char))))
    (:cap1 (cond ((not (alphanumericp char)) char)
                 (t (setf (char-mode xp) :capw) (char-upcase char))))
    (:capw (cond ((alphanumericp char) (char-downcase char))
                 (t (setf (char-mode xp) :cap1) char)))
    (:up   (char-upcase char))
    (t     (char-downcase char))))   ; :down (and the default)

;;; ========================================================================
;;; Part 6: Character output
;;; ========================================================================

(defun write-char+ (char xp)
  (if (eql char #\newline)
      (pprint-newline+ :unconditional xp)
      (write-char++ char xp)))

(defun write-string+ (string xp start end)
  (let ((sub-end nil) (next-newline nil) (s start))
    (block ws+
      (loop
        (setq next-newline
              (%xp-position #\newline string :test #'char= :start s :end end))
        (setq sub-end (if next-newline next-newline end))
        (write-string++ string xp s sub-end)
        (when (null next-newline) (return-from ws+ nil))
        (pprint-newline+ :unconditional xp)
        (setq s (+ sub-end 1))))))

(defun write-char++ (char xp)
  (when (> (buffer-ptr xp) (linel xp))
    (force-some-output xp))
  (let ((new-end (+ (buffer-ptr xp) 1))
        (c char))
    (check-size xp buffer new-end)
    (when (char-mode xp) (setq c (handle-char-mode xp c)))
    (setf (aref (buffer xp) (buffer-ptr xp)) c)
    (setf (buffer-ptr xp) new-end)))

(defun force-some-output (xp)
  (attempt-to-output xp nil nil)
  (when (> (buffer-ptr xp) (linel xp))
    (attempt-to-output xp t t)))

(defun write-string++ (string xp start end)
  (when (> (buffer-ptr xp) (linel xp))
    (force-some-output xp))
  (write-string+++ string xp start end))

(defun write-string+++ (string xp start end)
  (let ((new-end (+ (buffer-ptr xp) (- end start))))
    (check-size xp buffer new-end)
    (let ((buf (buffer xp))
          (i (buffer-ptr xp))
          (j start))
      (loop
        (cond ((= j end) (return nil))
              (t (let ((c (char string j)))
                   (when (char-mode xp) (setq c (handle-char-mode xp c)))
                   (setf (aref buf i) c)
                   (setq i (+ i 1))
                   (setq j (+ j 1)))))))
    (setf (buffer-ptr xp) new-end)))

(defun pprint-tab+ (kind colnum colinc xp)
  (let ((indented? nil) (relative? nil))
    (case kind
      (:section (setq indented? t))
      (:line-relative (setq relative? t))
      (:section-relative (setq indented? t) (setq relative? t)))
    (let* ((current (if (not indented?)
                        (LP<-BP xp)
                        (- (TP<-BP xp) (section-start xp))))
           (new (cond ((zerop colinc)
                       (if relative? (+ current colnum) (max colnum current)))
                      (relative?
                       (* colinc (floor (+ current colnum colinc -1) colinc)))
                      ((> colnum current) colnum)
                      (t (+ colnum
                            (* colinc
                               (floor (+ current (- colnum) colinc) colinc))))))
           (length (- new current)))
      (when (plusp length)
        (when (char-mode xp) (handle-char-mode xp #\space))
        (let ((end (+ (buffer-ptr xp) length)))
          (check-size xp buffer end)
          (fill (buffer xp) #\space :start (buffer-ptr xp) :end end)
          (setf (buffer-ptr xp) end))))))

(defun pprint-newline+ (kind xp)
  (enqueue xp :newline kind)
  (do ((ptr (qleft xp) (Qnext ptr)))
      ((not (< ptr (qright xp))))
    (when (and (null (Qend xp ptr))
               (not (> (depth-in-blocks xp) (Qdepth xp ptr)))
               (member (Qtype xp ptr) '(:newline :start-block)))
      (setf (Qend xp ptr) (- (qright xp) ptr))))
  (setf (section-start xp) (TP<-BP xp))
  (when (and (member kind '(:fresh :unconditional)) (char-mode xp))
    (handle-char-mode xp #\newline))
  (when (member kind '(:fresh :unconditional :mandatory))
    (attempt-to-output xp t nil)))

(defun start-block (xp prefix-string on-each-line? suffix-string)
  (let ((pstr prefix-string))
    (when pstr
      (write-string++ pstr xp 0 (length pstr)))
    (when (and (char-mode xp) on-each-line?)
      (setq pstr
            (%xp-chars->string (buffer xp)
                               (- (buffer-ptr xp) (length pstr))
                               (buffer-ptr xp))))
    (push-block-stack xp)
    (enqueue xp :start-block nil
             (if on-each-line? (cons suffix-string pstr) suffix-string))
    (setf (depth-in-blocks xp) (+ (depth-in-blocks xp) 1))
    (setf (section-start xp) (TP<-BP xp))))

(defun end-block (xp suffix)
  (unless (eq *abbreviation-happened* '*print-lines*)
    (when suffix (write-string+ suffix xp 0 (length suffix)))
    (setf (depth-in-blocks xp) (- (depth-in-blocks xp) 1))
    (enqueue xp :end-block nil suffix)
    (block end-block-find
      (do ((ptr (qleft xp) (Qnext ptr)))
          ((not (< ptr (qright xp))))
        (when (and (= (depth-in-blocks xp) (Qdepth xp ptr))
                   (eq (Qtype xp ptr) :start-block)
                   (null (Qoffset xp ptr)))
          (setf (Qoffset xp ptr) (- (qright xp) ptr))
          (return-from end-block-find nil))))
    (pop-block-stack xp)))

(defun pprint-indent+ (kind n xp)
  (enqueue xp :ind kind n))

;; --- attempt-to-output -----------------------------------------------------

(defmacro maybe-too-large (xp qentry)
  `(let ((limit (linel ,xp)))
     (when (eql (line-limit ,xp) (line-no ,xp))
       (setq limit (- limit 2))
       (when (not (minusp (prefix-stack-ptr ,xp)))
         (setq limit (- limit (suffix-ptr ,xp)))))
     (cond ((Qend ,xp ,qentry)
            (> (LP<-TP ,xp (Qpos ,xp (+ ,qentry (Qend ,xp ,qentry)))) limit))
           ((or force-newlines? (> (LP<-BP ,xp) limit)) t)
           (t (return-from atts-loop nil)))))

(defmacro misering? (xp)
  `(and *print-miser-width*
        (<= (- (linel ,xp) (initial-prefix-ptr ,xp)) *print-miser-width*)))

(defun attempt-to-output (xp force-newlines? flush-out?)
  (block atts-loop
    (do () ((> (qleft xp) (qright xp))
            (setf (qleft xp) 0)
            (setf (qright xp) -7))   ; -queue-entry-size
      (case (Qtype xp (qleft xp))
        (:ind
         (unless (misering? xp)
           (set-indentation-prefix xp
             (case (Qkind xp (qleft xp))
               (:block (+ (initial-prefix-ptr xp) (Qarg xp (qleft xp))))
               (t (+ (LP<-TP xp (Qpos xp (qleft xp))) (Qarg xp (qleft xp)))))))
         (setf (qleft xp) (Qnext (qleft xp))))
        (:start-block
         (cond ((maybe-too-large xp (qleft xp))
                (push-prefix-stack xp)
                (setf (initial-prefix-ptr xp) (prefix-ptr xp))
                (set-indentation-prefix xp (LP<-TP xp (Qpos xp (qleft xp))))
                (let ((arg (Qarg xp (qleft xp))))
                  (when (consp arg) (set-prefix xp (cdr arg)))
                  (setf (initial-prefix-ptr xp) (prefix-ptr xp))
                  (cond ((not (listp arg)) (set-suffix xp arg))
                        ((car arg) (set-suffix xp (car arg)))))
                (setf (section-start-line xp) (line-no xp)))
               (t (setf (qleft xp) (+ (qleft xp) (Qoffset xp (qleft xp))))))
         (setf (qleft xp) (Qnext (qleft xp))))
        (:end-block
         (pop-prefix-stack xp)
         (setf (qleft xp) (Qnext (qleft xp))))
        (t  ; :newline
         (when (case (Qkind xp (qleft xp))
                 (:fresh (not (zerop (LP<-BP xp))))
                 (:miser (misering? xp))
                 (:fill (or (misering? xp)
                            (> (line-no xp) (section-start-line xp))
                            (maybe-too-large xp (qleft xp))))
                 (t t))
           (output-line xp (qleft xp))
           (setup-for-next-line xp (qleft xp)))
         (setf (qleft xp) (Qnext (qleft xp)))))))
  (when flush-out? (flush xp)))

(defun flush (xp)
  (unless *locating-circularities*
    (funcall %xp-native-write-string
             (base-stream xp)
             (%xp-chars->string (buffer xp) 0 (buffer-ptr xp))))
  (setf (buffer-offset xp) (+ (buffer-offset xp) (buffer-ptr xp)))
  (setf (charpos xp) (+ (charpos xp) (buffer-ptr xp)))
  (setf (buffer-ptr xp) 0))

(defun output-line (xp qentry)
  (let* ((out-point (BP<-TP xp (Qpos xp qentry)))
         (last-non-blank (%xp-position-if-not (lambda (c) (char= c #\space))
                                              (buffer xp)
                                              :end out-point :from-end t))
         (end (cond ((member (Qkind xp qentry) '(:fresh :unconditional)) out-point)
                    (last-non-blank (+ last-non-blank 1))
                    (t 0)))
         (line-limit-exit (and (line-limit xp)
                               (not (> (line-limit xp) (line-no xp))))))
    (when line-limit-exit
      (setf (buffer-ptr xp) end)
      (write-string+++ " .." xp 0 3)
      (reverse-string-in-place (suffix xp) 0 (suffix-ptr xp))
      ;; Need to convert suffix vector to string for write-string+++
      (let ((suf-str (%xp-chars->string (suffix xp) 0 (suffix-ptr xp))))
        (write-string+++ suf-str xp 0 (length suf-str)))
      (setf (qleft xp) (Qnext (qright xp)))
      (setq *abbreviation-happened* '*print-lines*)
      (throw 'line-limit-abbreviation-exit t))
    (setf (line-no xp) (+ (line-no xp) 1))
    (unless *locating-circularities*
      (funcall %xp-native-write-string
               (base-stream xp)
               (%xp-chars->string (buffer xp) 0 end))
      (funcall %xp-native-terpri (base-stream xp)))))

(defun setup-for-next-line (xp qentry)
  (let* ((out-point (BP<-TP xp (Qpos xp qentry)))
         (prefix-end (cond ((member (Qkind xp qentry) '(:unconditional :fresh))
                            (non-blank-prefix-ptr xp))
                           (t (prefix-ptr xp))))
         (change (- prefix-end out-point)))
    (setf (charpos xp) 0)
    (when (plusp change)
      (check-size xp buffer (+ (buffer-ptr xp) change)))
    (replace (buffer xp) (buffer xp) :start1 prefix-end
             :start2 out-point :end2 (buffer-ptr xp))
    (replace (buffer xp) (prefix xp) :end2 prefix-end)
    (setf (buffer-ptr xp) (+ (buffer-ptr xp) change))
    (setf (buffer-offset xp) (- (buffer-offset xp) change))
    (when (not (member (Qkind xp qentry) '(:unconditional :fresh)))
      (setf (section-start-line xp) (line-no xp)))))

(defun set-indentation-prefix (xp new-position)
  (let ((new-ind (max (non-blank-prefix-ptr xp) new-position)))
    (setf (prefix-ptr xp) (initial-prefix-ptr xp))
    (check-size xp prefix new-ind)
    (when (> new-ind (prefix-ptr xp))
      (fill (prefix xp) #\space :start (prefix-ptr xp) :end new-ind))
    (setf (prefix-ptr xp) new-ind)))

(defun set-prefix (xp prefix-string)
  (replace (prefix xp) prefix-string
           :start1 (- (prefix-ptr xp) (length prefix-string)))
  (setf (non-blank-prefix-ptr xp) (prefix-ptr xp)))

(defun set-suffix (xp suffix-string)
  (let* ((end (length suffix-string))
         (new-end (+ (suffix-ptr xp) end)))
    (check-size xp suffix new-end)
    (let ((suf (suffix xp)))
      (do ((i (- new-end 1) (- i 1))
           (j 0 (+ j 1)))
          ((= j end))
        (setf (aref suf i) (char suffix-string j))))
    (setf (suffix-ptr xp) new-end)))

(defun reverse-string-in-place (vec start end)
  (do ((i start (+ i 1))
       (j (- end 1) (- j 1)))
      ((not (< i j)) vec)
    (let ((c (aref vec i)))
      (setf (aref vec i) (aref vec j))
      (setf (aref vec j) c))))

;;; ========================================================================
;;; Part 7: Public Interface — write, print, prin1, princ, pprint
;;; ========================================================================

(defun decode-stream-arg (stream)
  (cond ((eq stream t) *standard-output*)
        ((null stream) *standard-output*)
        (t stream)))

(defun xp-structure-p-safe (x)
  (and (vectorp x) (>= (length x) 2)
       (eq (svref x 0) 'xp-structure)))

(defun write (object &rest pairs)
  "(write object &key stream escape ...) — pretty-print OBJECT."
  (let* ((stream (or (getf pairs :stream) *standard-output*))
         (escape    (if (member :escape pairs)    (getf pairs :escape)    *print-escape*))
         (radix     (if (member :radix pairs)     (getf pairs :radix)     *print-radix*))
         (base      (if (member :base pairs)      (getf pairs :base)      *print-base*))
         (circle    (if (member :circle pairs)    (getf pairs :circle)    *print-circle*))
         (pretty    (if (member :pretty pairs)    (getf pairs :pretty)    *print-pretty*))
         (level     (if (member :level pairs)     (getf pairs :level)     *print-level*))
         (length    (if (member :length pairs)    (getf pairs :length)    *print-length*))
         (case      (if (member :case pairs)      (getf pairs :case)      *print-case*))
         (gensym    (if (member :gensym pairs)    (getf pairs :gensym)    *print-gensym*))
         (array     (if (member :array pairs)     (getf pairs :array)     *print-array*))
         (pprint-dispatch
                    (if (member :pprint-dispatch pairs)
                        (getf pairs :pprint-dispatch) *print-pprint-dispatch*))
         (right-margin
                    (if (member :right-margin pairs)
                        (getf pairs :right-margin) *print-right-margin*))
         (lines     (if (member :lines pairs)     (getf pairs :lines)     *print-lines*))
         (miser-width (if (member :miser-width pairs)
                          (getf pairs :miser-width) *print-miser-width*))
         (readably  (if (member :readably pairs)  (getf pairs :readably)  *print-readably*)))
    (setq stream (decode-stream-arg stream))
    (let ((*print-pprint-dispatch* pprint-dispatch)
          (*print-right-margin* right-margin)
          (*print-lines* lines)
          (*print-miser-width* miser-width)
          (*print-readably* readably))
      (cond ((or (xp-structure-p-safe stream) pretty)
             (let ((*print-escape* escape) (*print-radix* radix)
                   (*print-base* base) (*print-circle* circle)
                   (*print-pretty* pretty) (*print-level* level)
                   (*print-length* length) (*print-case* case)
                   (*print-gensym* gensym) (*print-array* array))
               (basic-write object stream)))
            (t
             (let ((*print-escape* escape))
               (non-pretty-print object stream))))))
  object)

(defun basic-write (object stream)
  (cond ((xp-structure-p-safe stream) (write+ object stream))
        (*print-pretty*
         (maybe-initiate-xp-printing
          (lambda (s o) (write+ o s)) stream object))
        (t (non-pretty-print object stream))))

(defun maybe-initiate-xp-printing (fn stream &rest args)
  (cond ((xp-structure-p-safe stream)
         (apply fn stream args))
        (t
         (let ((*abbreviation-happened* nil)
               (*locating-circularities* (if *print-circle* 0 nil))
               (*circularity-hash-table*
                 (if *print-circle* (get-circularity-hash-table) nil))
               (*parents* (when (not *print-shared*) (list nil)))
               (*result* nil))
           (xp-print fn (decode-stream-arg stream) args)
           (when *circularity-hash-table*
             (free-circularity-hash-table *circularity-hash-table*))
           *result*))))

(defun xp-print (fn stream args)
  (setq *result* (do-xp-printing fn stream args))
  (when *locating-circularities*
    (setq *locating-circularities* nil)
    (setq *abbreviation-happened* nil)
    (setq *parents* nil)
    (setq *result* (do-xp-printing fn stream args))))

(defun do-xp-printing (fn stream args)
  (let ((xp (get-pretty-print-stream stream))
        (*current-level* 0)
        (result nil))
    (catch 'line-limit-abbreviation-exit
      (start-block xp nil nil nil)
      (setq result (apply fn xp args))
      (end-block xp nil))
    (when (and *locating-circularities*
               (zerop *locating-circularities*)
               (= (line-no xp) 1)
               (zerop (buffer-offset xp)))
      (setq *locating-circularities* nil))
    (when (catch 'line-limit-abbreviation-exit
            (attempt-to-output xp nil t) nil)
      (attempt-to-output xp t t))
    (free-pretty-print-stream xp)
    result))

(defun write+ (object xp)
  (let ((*parents* *parents*)
        (obj object))
    (unless (and *circularity-hash-table*
                 (eq (circularity-process xp obj nil) :subsequent))
      (when (and *circularity-hash-table* (consp obj))
        (setq obj (cons (car obj) (cdr obj))))
      (let ((printer (if *print-pretty*
                         (get-printer obj *print-pprint-dispatch*)
                         nil))
            (type nil))
        (cond (printer (funcall printer xp obj))
              ((maybe-print-fast xp obj) nil)
              ((and *print-pretty*
                    (progn (setq type (type-of obj)) (symbolp type))
                    (progn (setq printer (structure-printer-for type))
                           (and printer (not (eq printer :none)))))
               (funcall printer xp obj))
              ((and *print-pretty* *print-array* (arrayp obj)
                    (not (stringp obj))
                    (not (bit-vector-p obj))
                    (not (structure-type-p (type-of obj))))
               (pretty-array xp obj))
              (t (let ((s (with-output-to-string (str)
                            (non-pretty-print obj str))))
                   (write-string+ s xp 0 (length s)))))))))

(defun non-pretty-print (object s)
  (if *print-escape*
      (funcall %xp-native-format s "~S" object)
      (funcall %xp-native-format s "~A" object)))

;; --- Circularity ----------------------------------------------------------

(defun circularity-process (xp object interior-cdr?)
  (unless (or (numberp object)
              (characterp object)
              (and (symbolp object)
                   (or (null *print-gensym*) (symbol-package object))))
    (let ((id (gethash object *circularity-hash-table*)))
      (cond
        (*locating-circularities*
         (cond ((null id)
                (when *parents* (push object *parents*))
                (setf (gethash object *circularity-hash-table*) 0)
                nil)
               ((zerop id)
                (cond ((or (null *parents*) (member object *parents*))
                       (setq *locating-circularities*
                             (+ *locating-circularities* 1))
                       (setf (gethash object *circularity-hash-table*)
                             *locating-circularities*)
                       :subsequent)
                      (t nil)))
               (t :subsequent)))
        (t
         (cond ((or (null id) (zerop id)) nil)
               ((plusp id)
                (cond (interior-cdr?
                       (setq *current-level* (- *current-level* 1))
                       (write-string++ ". #" xp 0 3))
                      (t (write-char++ #\# xp)))
                (print-fixnum xp id)
                (write-char++ #\= xp)
                (setf (gethash object *circularity-hash-table*) (- id))
                :first)
               (t (if interior-cdr?
                      (write-string++ ". #" xp 0 3)
                      (write-char++ #\# xp))
                  (print-fixnum xp (- id))
                  (write-char++ #\# xp)
                  :subsequent)))))))

;; --- Fast path for common atoms -------------------------------------------

(defun maybe-print-fast (xp object)
  (let ((obj object))
    (cond ((stringp obj)
           (cond ((null *print-escape*)
                  (write-string+ obj xp 0 (length obj)) t)
                 ((every (lambda (c) (not (or (char= c #\") (char= c #\\))))
                         obj)
                  (write-char++ #\" xp)
                  (write-string+ obj xp 0 (length obj))
                  (write-char++ #\" xp) t)))
          ((integerp obj)
           (when (and (null *print-radix*) (= *print-base* 10))
             (when (minusp obj)
               (write-char++ #\- xp)
               (setq obj (- obj)))
             (print-fixnum xp obj) t))
          ((symbolp obj)
           (let ((s (symbol-name obj))
                 (is-key (keywordp obj))
                 (mode (case *print-case*
                         (:downcase :down)
                         (:capitalize :cap1)
                         (t nil))))
             (when (or is-key (no-escapes-needed s))
               (when (and is-key *print-escape*)
                 (write-char++ #\: xp))
               (when mode (push-char-mode xp mode))
               (write-string++ s xp 0 (length s))
               (when mode (pop-char-mode xp))
               t))))))

(defun print-fixnum (xp fixnum)
  (multiple-value-bind (digits d) (truncate fixnum 10)
    (unless (zerop digits)
      (print-fixnum xp digits))
    (write-char++ (code-char (+ 48 d)) xp)))

(defun no-escapes-needed (s)
  (let ((n (length s)))
    (and (not (zerop n))
         (let ((c (char s 0)))
           (or (and (alpha-char-p c) (upper-case-p c)) (find c "*<>")))
         (block ne-check
           (do ((i 1 (+ i 1))) ((= i n) (return-from ne-check t))
             (let ((c (char s i)))
               (when (not (or (digit-char-p c)
                              (and (alpha-char-p c) (upper-case-p c))
                              (find c "*+<>-")))
                 (return-from ne-check nil))))))))

;;; ========================================================================
;;; Part 8: Stream operations — print, prin1, princ, pprint, format, ...
;;; ========================================================================

(defun print (object &optional (stream *standard-output*))
  (setq stream (decode-stream-arg stream))
  (terpri stream)
  (let ((*print-escape* t))
    (basic-write object stream))
  (write-char #\space stream)
  object)

(defun prin1 (object &optional (stream *standard-output*))
  (setq stream (decode-stream-arg stream))
  (let ((*print-escape* t))
    (basic-write object stream))
  object)

(defun princ (object &optional (stream *standard-output*))
  (setq stream (decode-stream-arg stream))
  (let ((*print-escape* nil))
    (basic-write object stream))
  object)

(defun pprint (object &optional (stream *standard-output*))
  (setq stream (decode-stream-arg stream))
  (terpri stream)
  (let ((*print-escape* t) (*print-pretty* t))
    (basic-write object stream))
  (values))

(defun write-to-string (object &rest pairs)
  (with-output-to-string (s)
    (apply #'write object :stream s pairs)))

(defun prin1-to-string (object)
  (with-output-to-string (s)
    (let ((*print-escape* t))
      (basic-write object s))))

(defun princ-to-string (object)
  (with-output-to-string (s)
    (let ((*print-escape* nil))
      (basic-write object s))))

(defvar *format-string-cache* t)

(defun format (stream string-or-fn &rest args)
  (let ((strm stream) (ctl string-or-fn))
    (cond ((stringp strm)
           (funcall %xp-native-format strm "~A"
                    (with-output-to-string (s)
                      (apply #'format s ctl args)))
           nil)
          ((null strm)
           (with-output-to-string (s)
             (apply #'format s ctl args)))
          (t
           (when (eq strm t) (setq strm *standard-output*))
           (when (stringp ctl)
             (setq ctl (process-format-string ctl nil)))
           (cond ((not (stringp ctl))
                  (apply ctl strm args))
                 ((xp-structure-p-safe strm)
                  (apply #'using-format strm ctl args))
                 (t (apply %xp-native-format strm ctl args)))
           nil))))

(defun process-format-string (string force-fn?)
  (cond ((not (stringp string)) string)
        ((not *format-string-cache*)
         (maybe-compile-format-string string force-fn?))
        (t (when (not (hash-table-p *format-string-cache*))
             (setq *format-string-cache* (make-hash-table :test 'eq)))
           (let ((v (gethash string *format-string-cache*)))
             (when (or (not v) (and force-fn? (stringp v)))
               (setq v (maybe-compile-format-string string force-fn?))
               (setf (gethash string *format-string-cache*) v))
             v))))

(defun write-char (char &optional (stream *standard-output*))
  (setq stream (decode-stream-arg stream))
  (if (xp-structure-p-safe stream)
      (write-char+ char stream)
      (funcall %xp-native-write-char stream char))
  char)

(defun write-string (string &optional (stream *standard-output*) &rest pairs)
  (let ((start (or (getf pairs :start) 0))
        (end   (or (getf pairs :end) (length string))))
    (setq stream (decode-stream-arg stream))
    (cond ((xp-structure-p-safe stream)
           (write-string+ string stream start end))
          (t (funcall %xp-native-write-string stream
                      (if (and (zerop start) (= end (length string)))
                          string
                          (subseq string start end))))))
  string)

(defun write-line (string &optional (stream *standard-output*) &rest pairs)
  (let ((start (or (getf pairs :start) 0))
        (end   (or (getf pairs :end) (length string))))
    (setq stream (decode-stream-arg stream))
    (cond ((xp-structure-p-safe stream)
           (write-string+ string stream start end)
           (pprint-newline+ :unconditional stream))
          (t (funcall %xp-native-write-string stream
                      (if (and (zerop start) (= end (length string)))
                          string
                          (subseq string start end)))
             (funcall %xp-native-terpri stream))))
  string)

(defun terpri (&optional (stream *standard-output*))
  (setq stream (decode-stream-arg stream))
  (if (xp-structure-p-safe stream)
      (pprint-newline+ :unconditional stream)
      (funcall %xp-native-terpri stream))
  nil)

(defun fresh-line (&optional (stream *standard-output*))
  (setq stream (decode-stream-arg stream))
  (cond ((xp-structure-p-safe stream)
         (attempt-to-output stream t t)
         (when (not (zerop (LP<-BP stream)))
           (pprint-newline+ :fresh stream)
           t))
        (t (funcall %xp-native-terpri stream) t)))

(defun finish-output (&optional (stream *standard-output*))
  (setq stream (decode-stream-arg stream))
  (when (xp-structure-p-safe stream)
    (attempt-to-output stream t t))
  nil)

(defun force-output (&optional (stream *standard-output*))
  (setq stream (decode-stream-arg stream))
  (when (xp-structure-p-safe stream)
    (attempt-to-output stream t t))
  nil)

(defun clear-output (&optional (stream *standard-output*))
  (setq stream (decode-stream-arg stream))
  (when (xp-structure-p-safe stream)
    (let ((*locating-circularities* 0))
      (attempt-to-output stream t t)))
  nil)

(defun streamp (x)
  (or (xp-structure-p-safe x)
      (funcall %xp-native-streamp x)))

;;; ========================================================================
;;; Part 9: safe-assoc utility
;;; ========================================================================
;;; (NCL's defstruct is kept as-is — XP doesn't override it. Users who
;;; want a per-struct print-function can call set-pprint-dispatch+
;;; manually.)

(defun safe-assoc (item lst)
  (do ((l lst (cdr l))) ((not (consp l)) nil)
    (when (and (consp (car l)) (eq (caar l) item))
      (return (car l)))))

;;; ========================================================================
;;; Part 10: pprint-logical-block and helpers
;;; ========================================================================

;; macrolet isn't available — we walk the body at macro-expansion time
;; and substitute pprint-pop / pprint-exit-if-list-exhausted with their
;; non-local-exiting equivalents.

(defun %xp-pblock-subst (form list-sym stream-sym)
  (cond ((atom form) form)
        ((and (eq (car form) 'pprint-pop) (null (cdr form)))
         `(pprint-pop+ ,list-sym ,stream-sym))
        ((and (eq (car form) 'pprint-exit-if-list-exhausted) (null (cdr form)))
         `(when (null ,list-sym) (return-from logical-block nil)))
        ((member (car form) '(pprint-logical-block pprint-logical-block+))
         form)
        (t (cons (%xp-pblock-subst (car form) list-sym stream-sym)
                 (%xp-pblock-subst (cdr form) list-sym stream-sym)))))

(defmacro pprint-logical-block (args &rest body)
  "(pprint-logical-block (stream list &key prefix per-line-prefix suffix) body)"
  (let* ((stream-symbol (car args))
         (list-arg (cadr args))
         (opts (cddr args))
         (prefix (getf opts :prefix nil))
         (per-line-prefix (getf opts :per-line-prefix nil))
         (suffix (getf opts :suffix "")))
    (when (or (null stream-symbol) (eq stream-symbol t))
      (setq stream-symbol '*standard-output*))
    (unless (symbolp stream-symbol)
      (setq stream-symbol '*standard-output*))
    `(maybe-initiate-xp-printing
      (lambda (,stream-symbol)
        (let ((+l ,list-arg)
              (+p ,(or prefix per-line-prefix ""))
              (+s ,suffix))
          (pprint-logical-block+
            (,stream-symbol +l +p +s ,(not (null per-line-prefix)) t nil)
            ,@body nil)))
      (decode-stream-arg ,stream-symbol))))

(defmacro pprint-logical-block+ (spec &rest body)
  "(pprint-logical-block+ (var args prefix suffix per-line? circle-check? atsign?) body)"
  (let* ((var          (nth 0 spec))
         (args         (nth 1 spec))
         (prefix       (nth 2 spec))
         (suffix       (nth 3 spec))
         (per-line?    (nth 4 spec))
         (circle-check? (nth 5 spec))
         (atsign?      (nth 6 spec))
         (cc (if (and circle-check? atsign?) 'not-first-p circle-check?))
         (expanded (mapcar (lambda (f) (%xp-pblock-subst f args var))
                           body)))
    `(let ((*current-level* (+ *current-level* 1))
           (*current-length* -1)
           (*parents* *parents*)
           ,@(if (and circle-check? atsign?)
                 `((not-first-p (plusp *current-length*)))))
       (unless (check-block-abbreviation ,var ,args ,cc)
         (block logical-block
           (start-block ,var ,prefix ,per-line? ,suffix)
           (progn ,@expanded)
           (end-block ,var ,suffix))))))

(defun pprint-newline (kind &optional (stream *standard-output*))
  (setq stream (decode-stream-arg stream))
  (when (not (member kind '(:linear :miser :fill :mandatory)))
    (error "Invalid KIND argument ~A to PPRINT-NEWLINE" kind))
  (when (xp-structure-p-safe stream)
    (pprint-newline+ kind stream))
  nil)

(defun pprint-indent (relative-to n &optional (stream *standard-output*))
  (setq stream (decode-stream-arg stream))
  (when (not (member relative-to '(:block :current)))
    (error "Invalid KIND argument ~A to PPRINT-INDENT" relative-to))
  (when (xp-structure-p-safe stream)
    (pprint-indent+ relative-to n stream))
  nil)

(defun pprint-tab (kind colnum colinc &optional (stream *standard-output*))
  (setq stream (decode-stream-arg stream))
  (when (not (member kind '(:line :section :line-relative :section-relative)))
    (error "Invalid KIND argument ~A to PPRINT-TAB" kind))
  (when (xp-structure-p-safe stream)
    (pprint-tab+ kind colnum colinc stream))
  nil)

(defun check-block-abbreviation (xp args circle-check?)
  (cond ((not (listp args)) (write+ args xp) t)
        ((and *print-level* (> *current-level* *print-level*))
         (write-char++ #\# xp)
         (setq *abbreviation-happened* t)
         t)
        ((and *circularity-hash-table* circle-check?
              (eq (circularity-process xp args nil) :subsequent)) t)
        (t nil)))

;;; ========================================================================
;;; Part 11: Compiled FORMAT
;;; ========================================================================

(proclaim '(special *string* *used-args* *used-outer-args* *used-initial*
                    *get-arg-carefully* *inner-end* *outer-end* *at-top*))

(defvar *string* nil)
(defvar *used-args* nil)
(defvar *used-outer-args* nil)
(defvar *used-initial* nil)
(defvar *get-arg-carefully* nil)
(defvar *inner-end* nil)
(defvar *outer-end* nil)
(defvar *at-top* nil)
(defvar *default-package* "USER")
(defvar *testing-errors* nil)

(defvar *fn-table* (make-hash-table))

(defmacro def-format-handler (char args &rest body)
  (let ((name (intern (string-append-char "FORMAT-" (char-upcase char)))))
    `(progn
       (defun ,name ,args ,@body)
       (setf (gethash (char-upcase ,char) *fn-table*) (function ,name))
       (setf (gethash (char-downcase ,char) *fn-table*) (function ,name)))))

(defun initial () (setq *used-initial* t) 'init)
(defun args () (setq *used-args* t) 'args)
(defun outer-args () (setq *used-outer-args* t) 'outer-args)

(defmacro bind-initial (&rest code)
  `(let* ((*used-initial* nil)
          (body (progn ,@code)))
     (if *used-initial* (make-binding 'init (args) body) body)))

(defmacro bind-args (doit? val &rest code)
  (cond ((eq doit? t)
         `(let* ((val ,val)
                 (*used-args* nil)
                 (body (progn ,@code)))
            (if *used-args*
                (make-binding 'args val body)
                (cons val body))))
        (t
         `(flet ((codefn () ,@code))
            (if (not ,doit?) (codefn)
                (let* ((val ,val)
                       (*used-args* nil)
                       (body (codefn)))
                  (if *used-args*
                      (make-binding 'args val body)
                      (cons val body))))))))

(defmacro bind-outer-args (&rest code)
  `(let* ((*used-outer-args* nil)
          (body (progn ,@code)))
     (if *used-outer-args* (make-binding 'outer-args (args) body) body)))

(defmacro maybe-bind (doit? var val &rest code)
  `(let ((body (progn ,@code)))
     (if ,doit? (make-binding ,var ,val body) body)))

(defun make-binding (var value body)
  `((let ((,var ,value)) ,@body)))

(defun num-args () `(length ,(args)))

(defun get-arg ()
  (if *get-arg-carefully*
      (if *at-top* `(pprint-pop+top ,(args) xp) `(pprint-pop+ ,(args) xp))
      `(pop ,(args))))

(defmacro pprint-pop+ (args xp)
  `(if (pprint-pop-check+ ,args ,xp)
       (return-from logical-block nil)
       (pop ,args)))

(defun pprint-pop-check+ (args xp)
  (setq *current-length* (+ *current-length* 1))
  (cond ((not (listp args))
         (write-string++ ". " xp 0 2)
         (write+ args xp)
         t)
        ((and *print-length*
              (not (< *current-length* *print-length*)))
         (write-string++ "..." xp 0 3)
         (setq *abbreviation-happened* t)
         t)
        ((and *circularity-hash-table*
              (not (zerop *current-length*)))
         (case (circularity-process xp args t)
           (:first (write+ (cons (car args) (cdr args)) xp) t)
           (:subsequent t)
           (t nil)))))

(defmacro pprint-pop+top (args xp)
  `(if (pprint-pop-check+top ,args ,xp)
       (return-from logical-block nil)
       (pop ,args)))

(defun pprint-pop-check+top (args xp)
  (setq *current-length* (+ *current-length* 1))
  (cond ((not (listp args))
         (write-string++ ". " xp 0 2)
         (write+ args xp)
         t)
        ((and *print-length*
              (not (< *current-length* *print-length*)))
         (write-string++ "..." xp 0 3)
         (setq *abbreviation-happened* t)
         t)))

(defun literal (start end)
  (let ((sub-end nil) (next-newline nil) (result nil) (st start))
    (block lit-loop
      (loop
        (setq next-newline
              (%xp-position #\newline *string* :start st :end end))
        (setq sub-end (if next-newline next-newline end))
        (when (< st sub-end)
          (push (if (= st (- sub-end 1))
                    `(write-char++ ,(aref *string* st) xp)
                    `(write-string++ ,(subseq *string* st sub-end) xp
                                     0 ,(- sub-end st)))
                result))
        (when (null next-newline) (return-from lit-loop nil))
        (push '(pprint-newline+ :unconditional xp) result)
        (setq st (+ sub-end 1))))
    (if (null (cdr result)) (car result) (cons 'progn (nreverse result)))))

(defmacro formatter (string)
  `(function
    (lambda (s &rest args)
      (formatter-in-package ,string "USER"))))

(defmacro formatter-in-package (string reader-package)
  (formatter-fn string reader-package))

(defun formatter-fn (string-arg pkg)
  (let ((*string* string-arg)
        (*default-package* pkg))
    (or (catch :format-compilation-error
          `(apply (function maybe-initiate-xp-printing)
                  (function
                   (lambda (xp &rest args)
                     ,@(bind-initial
                        `((block top
                            ,@(let ((*get-arg-carefully* nil)
                                    (*at-top* t)
                                    (*inner-end* 'top)
                                    (*outer-end* 'top))
                                (compile-format 0 (length *string*))))))
                     (if ,(args) (copy-list ,(args)))))
                  s args))
        `(apply ,%xp-native-format s ,*string* args))))

(defun maybe-compile-format-string (string force-fn?)
  ;; Runtime format strings: NCL has no eval, so we can't compile XP
  ;; directives dynamically. Use the string verbatim — the format
  ;; dispatcher will route it through the native format engine.
  (declare (ignore force-fn?))
  string)

(defun err (id msg i)
  (when *testing-errors* (throw :testing-errors (list id i)))
  (funcall %xp-native-format t "XP: cannot compile format string ~%~A~%~S~%"
           msg *string*)
  (throw :format-compilation-error nil))

(defun position-in (set start)
  (%xp-position-if (lambda (c) (find c set)) *string* :start start))

(defun position-not-in (set start)
  (%xp-position-if-not (lambda (c) (find c set)) *string* :start start))

(defun next-directive1 (start end)
  (let ((i (%xp-position #\~ *string* :start start :end end)) (j nil))
    (when i
      (setq j (params-end (+ i 1)))
      (when (char= (aref *string* j) #\/)
        (setq j (%xp-position #\/ *string* :start (+ j 1) :end end))
        (when (null j)
          (err 3 "Matching / missing"
               (%xp-position #\/ *string* :start start)))))
    (values i j)))

(defun params-end (start)
  (let ((j start) (end (length *string*)))
    (block params-end-loop
      (loop
        (setq j (position-not-in "+-0123456789,Vv#:@" j))
        (when (null j) (err 1 "missing directive" (- start 1)))
        (when (not (eq (aref *string* j) #\')) (return-from params-end-loop j))
        (setq j (+ j 1))
        (when (= j end) (err 2 "No character after '" (- j 1)))
        (setq j (+ j 1))))))

(defun directive-start (end)
  (let ((e end))
    (block dir-start
      (loop
        (setq e (%xp-position #\~ *string* :end e :from-end t))
        (when (or (zerop e) (not (eq (aref *string* (- e 1)) #\')))
          (return-from dir-start e))
        (setq e (- e 1))))))

(defun next-directive (start end)
  (let ((i nil) (j nil) (ii nil) (k nil) (count 0) (c nil) (close nil)
        (pairs '((#\( . #\)) (#\[ . #\]) (#\< . #\>) (#\{ . #\}))))
    (multiple-value-setq (i j) (next-directive1 start end))
    (when i
      (setq c (aref *string* j))
      (setq close (cdr (assoc c pairs)))
      (when close
        (setq k j)
        (setq count 0)
        (block nd-loop
          (loop
            (multiple-value-setq (ii k) (next-directive1 k end))
            (when (null ii) (err 4 "No matching close directive" j))
            (when (eql (aref *string* k) c) (setq count (+ count 1)))
            (when (eql (aref *string* k) close)
              (setq count (- count 1))
              (when (minusp count) (setq j k) (return-from nd-loop nil)))))))
    (values c i j)))

(defun chunk-up (start end)
  (let ((positions (list start)) (spot start))
    (block ch-loop
      (loop
        (multiple-value-bind (c i j) (next-directive spot end)
          (declare (ignore i))
          (when (null c)
            (return-from ch-loop (nreverse (cons end positions))))
          (when (eql c #\;) (push (+ j 1) positions))
          (setq spot j))))))

(defun fancy-directives-p (str)
  (let ((*string* str))
    (let ((i nil) (j 0) (end (length *string*)) (c nil))
      (block fd-loop
        (loop
          (multiple-value-setq (i j) (next-directive1 j end))
          (when (not i) (return-from fd-loop nil))
          (setq c (aref *string* j))
          (when (or (find c "_Ii/Ww") (and (find c ">Tt") (colonp j)))
            (return-from fd-loop t)))))))

(defun num-args-in-args (start &optional (errp nil))
  (let ((n 0) (i (- start 1)) (c nil))
    (block na-loop
      (loop
        (setq i (position-not-in "+-0123456789," (+ i 1)))
        (setq c (aref *string* i))
        (cond ((or (char= c #\V) (char= c #\v)) (setq n (+ n 1)))
              ((char= c #\#)
               (when errp
                 (err 21 "# not allowed in ~~<...~~> by (formatter \"...\")" start))
               (return-from na-loop nil))
              ((char= c #\') (setq i (+ i 1)))
              (t (return-from na-loop n)))))))

;; --- compile-format: no prog/go (uses block + loop) -----------------------

(defun compile-format (start end)
  (let ((result nil) (st start))
    (block cf-exit
      (loop
        (multiple-value-bind (c i j) (next-directive st end)
          (let ((jj j))
            (when (if (null c) (< st end) (< st i))
              (push (literal st (if i i end)) result))
            (when (null c) (return-from cf-exit (nreverse result)))
            (cond
              ((char= c #\newline)
               (let ((colon nil) (atsign-val nil))
                 (multiple-value-setq (colon atsign-val)
                   (parse-params (+ i 1) nil :nocolonatsign t))
                 (when atsign-val
                   (push '(pprint-newline+ :unconditional xp) result))
                 (setq jj (+ jj 1))
                 (when (not colon)
                   (setq jj (%xp-position-if-not
                             (lambda (ch)
                               (or (char= ch #\tab) (char= ch #\space)))
                             *string* :start jj :end end))
                   (when (null jj) (setq jj end)))
                 (setq st jj)))
              (t
               (let ((fn (gethash c *fn-table*)))
                 (when (null fn) (err 5 "Unknown format directive" jj))
                 (setq jj (+ jj 1))
                 (push (funcall fn (+ i 1) jj) result)
                 (setq st jj))))))))))

(defun parse-params (start defaults &rest opts)
  (let ((max-arg (or (getf opts :max) (length defaults)))
        (nocolon (getf opts :nocolon nil))
        (noatsign (getf opts :noatsign nil))
        (nocolonatsign (getf opts :nocolonatsign nil))
        (colon nil) (atsign nil) (params nil)
        (i start) (j nil) (c nil))
    (block pp-loop1
      (loop
        (setq c (aref *string* i))
        (cond ((or (char= c #\V) (char= c #\v))
               (push (get-arg) params) (setq i (+ i 1)))
              ((char= c #\#)
               (push (num-args) params) (setq i (+ i 1)))
              ((char= c #\')
               (setq i (+ i 1))
               (push (aref *string* i) params)
               (setq i (+ i 1)))
              ((char= c #\,)
               (push nil params))
              (t
               (setq j (position-not-in "+-0123456789" i))
               (when (= i j) (return-from pp-loop1 nil))
               (push (parse-integer *string* :start i :end j :radix 10) params)
               (setq i j)))
        (if (char= (aref *string* i) #\,)
            (setq i (+ i 1))
            (return-from pp-loop1 nil))))
    (setq params (nreverse params))
    (do ((ps params (cdr ps))
         (ds defaults (cdr ds))
         (nps nil))
        ((null ds) (setq params (nreconc nps ps)))
      (push (cond ((or (null ps) (null (car ps))) (car ds))
                  ((not (consp (car ps))) (car ps))
                  (t `(cond (,(car ps)) (t ,(car ds)))))
            nps))
    (when (and max-arg (< max-arg (length params)))
      (err 6 "Too many parameters" i))
    (block pp-loop2
      (loop
        (setq c (aref *string* i))
        (cond ((char= c #\:)
               (when colon (err 7 "Two colons specified" i))
               (setq colon t))
              ((char= c #\@)
               (when atsign (err 8 "Two atsigns specified" i))
               (setq atsign t))
              (t (return-from pp-loop2 nil)))
        (setq i (+ i 1))))
    (when (and colon nocolon) (err 9 "Colon not permitted" i))
    (when (and atsign noatsign) (err 10 "Atsign not permitted" i))
    (when (and colon atsign nocolonatsign)
      (err 11 "Colon and atsign together not permitted" i))
    (values colon atsign params)))

(defun colonp (j)
  (or (eql (aref *string* (- j 1)) #\:)
      (and (eql (aref *string* (- j 1)) #\@)
           (eql (aref *string* (- j 2)) #\:))))

(defun atsignp (j)
  (or (eql (aref *string* (- j 1)) #\@)
      (and (eql (aref *string* (- j 1)) #\:)
           (eql (aref *string* (- j 2)) #\@))))

;;; ========================================================================
;;; Part 12: Format handlers
;;; ========================================================================

(def-format-handler #\/ (start end)
  (multiple-value-bind (colon atsign params) (parse-params start nil :max nil)
    (let* ((wh-name-start (+ (params-end start) 1))
           (colon-pos (%xp-position #\: *string*
                                    :start wh-name-start :end (- end 1)))
           (name-start (cond ((null colon-pos) wh-name-start)
                             ((and (< colon-pos (- end 1))
                                   (char= #\: (aref *string* (+ colon-pos 1))))
                              (+ colon-pos 2))
                             (t (+ colon-pos 1))))
           (fn (intern (string-upcase
                        (subseq *string* name-start (- end 1))))))
      (if (not (find-if #'consp params))
          `(funcall (symbol-function ',fn) xp ,(get-arg) ,colon ,atsign ,@params)
          (let ((vars (mapcar (lambda (a) (declare (ignore a)) (gensym "P"))
                              params)))
            `(let ,(mapcar #'list vars params)
               (funcall (symbol-function ',fn) xp ,(get-arg)
                        ,colon ,atsign ,@vars)))))))

(def-format-handler #\A (start end)
  (if (not (= end (+ start 1))) (simple-directive start end)
      `(let ((*print-escape* nil))
         (write+ ,(get-arg) xp))))

(def-format-handler #\S (start end)
  (if (not (= end (+ start 1))) (simple-directive start end)
      `(let ((*print-escape* t))
         (write+ ,(get-arg) xp))))

(def-format-handler #\D (start end) (simple-directive start end))
(def-format-handler #\B (start end) (simple-directive start end))
(def-format-handler #\O (start end) (simple-directive start end))
(def-format-handler #\X (start end) (simple-directive start end))
(def-format-handler #\R (start end) (simple-directive start end))
(def-format-handler #\C (start end) (simple-directive start end))
(def-format-handler #\F (start end) (simple-directive start end))
(def-format-handler #\E (start end) (simple-directive start end))
(def-format-handler #\G (start end) (simple-directive start end))
(def-format-handler #\$ (start end) (simple-directive start end))

(defun simple-directive (start end)
  (let ((n (num-args-in-args start)))
    (cond (n `(using-format xp ,(subseq *string* (- start 1) end)
                            ,@(copy-tree (make-list (+ n 1)
                                                    :initial-element (get-arg)))))
          (t (multiple-value-bind (colon atsign params)
                 (parse-params start nil :max 8)
               (let* ((nparams (length params))
                      (arg-str (subseq "v,v,v,v,v,v,v,v" 0
                                       (max 0 (- (* 2 nparams) 1))))
                      (str (string-concat
                            (string-concat "~" arg-str)
                            (string-concat (if colon ":" "")
                                           (string-concat
                                            (if atsign "@" "")
                                            (subseq *string* (- end 1) end))))))
                 `(using-format xp ,str ,@params ,(get-arg))))))))

(defun using-format (xp string &rest args)
  (let ((result (apply %xp-native-format nil string args)))
    (write-string+ result xp 0 (length result))))

(defun make-list (n &rest opts)
  (let ((elt (getf opts :initial-element nil))
        (result nil)
        (i 0))
    (loop
      (cond ((>= i n) (return result))
            (t (push elt result) (setq i (+ i 1)))))))

;; --- P, %, &, |, ~ --------------------------------------------------------

(def-format-handler #\P (start end) (declare (ignore end))
  (multiple-value-bind (colon atsign) (parse-params start nil)
    (let ((arg (if colon `(car (backup-in-list 1 ,(initial) ,(args))) (get-arg))))
      (if atsign
          `(if (not (eql ,arg 1)) (write-string++ "ies" xp 0 3) (write-char++ #\y xp))
          `(if (not (eql ,arg 1)) (write-char++ #\s xp))))))

(def-format-handler #\% (start end) (declare (ignore end))
  (multiple-newlines start :unconditional))

(def-format-handler #\& (start end) (declare (ignore end))
  (multiple-newlines start :fresh))

(defun multiple-newlines (start kind)
  (multiple-value-bind (colon atsign params)
      (parse-params start '(1) :nocolon t :noatsign t)
    (declare (ignore colon atsign))
    (if (eql (car params) 1)
        `(pprint-newline+ ,kind xp)
        `(multiple-newlines1 xp ,kind ,(car params)))))

(defun multiple-newlines1 (xp kind num)
  (let ((k kind))
    (do ((n num (- n 1))) ((not (plusp n)))
      (pprint-newline+ k xp)
      (setq k :unconditional))))

(def-format-handler #\| (start end) (declare (ignore end))
  (multiple-chars start (code-char 12)))   ; form-feed for ~|

(def-format-handler #\~ (start end) (declare (ignore end))
  (multiple-chars start #\~))

(defun multiple-chars (start char)
  (multiple-value-bind (colon atsign params)
      (parse-params start '(1) :nocolon t :noatsign t)
    (declare (ignore colon atsign))
    (if (eql (car params) 1)
        `(write-char++ ,char xp)
        `(multiple-chars1 xp ,(car params) ,char))))

(defun multiple-chars1 (xp num char)
  (do ((n num (- n 1))) ((not (plusp n)))
    (write-char++ char xp)))

;; --- T, *, ?, ^ -----------------------------------------------------------

(def-format-handler #\T (start end) (declare (ignore end))
  (multiple-value-bind (colon atsign params) (parse-params start '(1 1))
    `(pprint-tab+ ,(if colon (if atsign :section-relative :section)
                       (if atsign :line-relative :line))
                  ,(pop params) ,(pop params) xp)))

(def-format-handler #\* (start end) (declare (ignore end))
  (cond ((atsignp (params-end start))
         (multiple-value-bind (colon atsign params)
             (parse-params start '(0) :nocolon t)
           (declare (ignore colon atsign))
           `(setq args (backup-to ,(car params) ,(initial) ,(args)))))
        (t
         (multiple-value-bind (colon atsign params)
             (parse-params start '(1))
           (declare (ignore atsign))
           `(setq args
                  ,(if colon
                       `(backup-in-list ,(car params) ,(initial) ,(args))
                       `(nthcdr ,(car params) ,(args))))))))

(defun backup-in-list (num list some-tail)
  (backup-to (- (tail-pos list some-tail) num) list some-tail))

(defun backup-to (num list some-tail)
  (if (not *circularity-hash-table*) (nthcdr num list)
      (multiple-value-bind (pos share) (tail-pos list some-tail)
        (declare (ignore pos))
        (cond ((not (< num share)) (nthcdr num list))
              (t (do ((l (nthcdr num list) (cdr l))
                      (n (- share num) (- n 1))
                      (r nil (cons (car l) r)))
                     ((zerop n) (nreconc r l))))))))

(defun tail-pos (list some-tail)
  (block tp-outer
    (do ((n 0 (+ n 1))
         (l list (cdr l)))
        (nil)
      (do ((m n (- m 1))
           (st some-tail (cdr st)))
          (nil)
        (when (minusp m) (return nil))
        (when (eq st l) (return-from tp-outer (values m n)))))))

(def-format-handler #\? (start end) (declare (ignore end))
  (multiple-value-bind (colon atsign) (parse-params start nil :nocolon t)
    (declare (ignore colon))
    (if (not atsign)
        `(apply (function format) xp ,(get-arg) ,(get-arg))
        `(let ((fnp (process-format-string ,(get-arg) t)))
           (setq args (apply fnp xp ,(args)))))))

(def-format-handler #\^ (start end) (declare (ignore end))
  (multiple-value-bind (colon atsign params)
      (parse-params start nil :max 3 :noatsign t)
    (declare (ignore atsign))
    `(if ,(cond ((null params)
                 `(null ,(if colon `(cdr ,(outer-args)) (args))))
                (t `(do-complex-^-test ,@params)))
         (return-from ,(if colon *outer-end* *inner-end*) nil))))

(defun do-complex-^-test (a1 &optional (a2 nil) (a3 nil))
  (cond (a3 (and (<= a1 a2) (<= a2 a3)))
        (a2 (= a1 a2))
        (t (= 0 a1))))

;; --- delimited pairs: [ ( { < etc. ---------------------------------------

(def-format-handler #\[ (start end)
  (multiple-value-bind (colon atsign params)
      (parse-params start nil :max 1 :nocolonatsign t)
    (let* ((st (+ (params-end start) 1))
           (chunks (chunk-up st end))
           (innards (do ((ns chunks (cdr ns))
                         (ms (cdr chunks) (cdr ms))
                         (result nil))
                        ((null ms) (nreverse result))
                      (push (compile-format (car ns) (directive-start (car ms)))
                            result))))
      (cond (colon
             (when (not (= (length innards) 2))
               (err 13 "Wrong number of clauses in ~~:[...~~]" (- st 1)))
             `(cond ((null ,(get-arg)) ,@(car innards))
                    (t ,@(cadr innards))))
            (atsign
             (when (not (= (length innards) 1))
               (err 14 "Too many clauses in ~~@[...~~]" (- st 1)))
             `(cond ((car args) ,@(car innards)) (t ,(get-arg))))
            (t
             (let* ((jbox (list -1))
                    (len (- (length chunks) 2))
                    (else? (colonp (- (nth len chunks) 1))))
               `(case ,(if params (car params) (get-arg))
                  ,@(mapcar (lambda (unit)
                              (let ((jv (+ (car jbox) 1)))
                                (setf (car jbox) jv)
                                `(,(if (and else? (= jv len)) t jv) ,@unit)))
                            innards))))))))

(def-format-handler #\( (start end)
  (multiple-value-bind (colon atsign) (parse-params start nil)
    (let ((st (+ (params-end start) 1))
          (en (directive-start end)))
      `(progn (push-char-mode xp ,(cond ((and colon atsign) :up)
                                        (colon :cap1)
                                        (atsign :cap0)
                                        (t :down)))
              ,@(compile-format st en)
              (pop-char-mode xp)))))

(def-format-handler #\; (start end) (declare (ignore start))
  (err 15 "~~; appears out of context" (- end 1)))
(def-format-handler #\] (start end) (declare (ignore start))
  (err 16 "Unmatched closing directive" (- end 1)))
(def-format-handler #\) (start end) (declare (ignore start))
  (err 17 "Unmatched closing directive" (- end 1)))
(def-format-handler #\> (start end) (declare (ignore start))
  (err 18 "Unmatched closing directive" (- end 1)))
(def-format-handler #\} (start end) (declare (ignore start))
  (err 19 "Unmatched closing directive" (- end 1)))

;; ~{ — iteration. Original used prog/go; we use (block %iter (loop ...)).

(def-format-handler #\{ (start end)
  (multiple-value-bind (colon atsign params)
      (parse-params start '(-1) :max 1)
    (let* ((force-once (colonp (- end 1)))
           (n (car params))
           (bounded (not (eql n -1)))
           (st (+ (params-end start) 1))
           (en (directive-start end)))
      (car
       (maybe-bind bounded 'N n
         (maybe-bind (not (> en st)) 'fn
                     `(process-format-string ,(get-arg) t)
           (bind-args (not atsign) (get-arg)
             (let ((inner-body
                     (bind-outer-args
                       (bind-args colon (get-arg)
                         (bind-initial
                           (let ((*get-arg-carefully*
                                   (and *get-arg-carefully* atsign))
                                 (*at-top* (and *at-top* atsign))
                                 (*outer-end* nil)
                                 (*inner-end* nil))
                             (cond ((not colon)
                                    (if (not (> en st))
                                        `((setq args (apply fn xp ,(args))))
                                        (compile-format st en)))
                                   (t
                                    (let ((*inner-end* 'inner))
                                      `((block inner
                                          ,@(if (not (> en st))
                                                `((setq args (apply fn xp ,(args))))
                                                (compile-format st en)))))))))))))
               (cond
                 (force-once
                  `((block %iter
                      (let ((%first-p t))
                        (loop
                          (cond (%first-p (setq %first-p nil))
                                (t (when (null ,(args))
                                     (return-from %iter nil))))
                          ,@(if bounded
                                `((when (= N 0) (return-from %iter nil))
                                  (setq N (- N 1))))
                          ,@inner-body)))))
                 (t
                  `((block %iter
                      (loop
                        (when (null ,(args)) (return-from %iter nil))
                        ,@(if bounded
                              `((when (= N 0) (return-from %iter nil))
                                (setq N (- N 1))))
                        ,@inner-body)))))))))))))

;; ~< : either justification (~< … ~>) or logical-block (~< … ~:>)

(def-format-handler #\< (start end)
  (if (colonp (- end 1))
      (handle-logical-block start end)
      (handle-standard-< start end)))

(defun handle-standard-< (start end)
  (let ((n (num-args-in-directive start end)))
    `(using-format xp ,(subseq *string* (- start 1) end)
                   ,@(copy-tree (make-list n :initial-element (get-arg))))))

(defun num-args-in-directive (start end)
  (let ((n 0) (c nil) (i nil) (j nil))
    (setq n (+ n (or (num-args-in-args start t) 0)))
    (multiple-value-setq (j i) (next-directive1 start end))
    (block na-dir-loop
      (loop
        (multiple-value-setq (c i j) (next-directive j end))
        (when (null c) (return-from na-dir-loop n))
        (cond ((eql c #\;)
               (when (colonp j)
                 (err 22 "~~:; not supported in ~~<...~~> by formatter" j)))
              ((find c "*[^<_IiWw{Tt")
               (err 23 "~~<...~~> too complicated to be supported by formatter" j))
              ((eql c #\()
               (setq n (+ n (num-args-in-directive (+ i 1) j))))
              ((find c "%&|~")
               (setq n (+ n (or (num-args-in-args (+ i 1) t) 0))))
              ((eql c #\?)
               (when (atsignp j)
                 (err 23 "~~<...~~> too complicated to be supported by formatter" j))
               (setq n (+ n 2)))
              ((find c "AaSsDdBbOoXxRrCcFfEeGg$Pp")
               (setq n (+ n (+ 1 (or (num-args-in-args (+ i 1) t) 0))))))))))

;; --- _, I, W : pretty-printing directives ---------------------------------

(def-format-handler #\_ (start end) (declare (ignore end))
  (multiple-value-bind (colon atsign) (parse-params start nil)
    `(pprint-newline+ ,(cond ((and colon atsign) :mandatory)
                             (colon :fill)
                             (atsign :miser)
                             (t :linear)) xp)))

(def-format-handler #\I (start end) (declare (ignore end))
  (multiple-value-bind (colon atsign params)
      (parse-params start '(0) :noatsign t)
    (declare (ignore atsign))
    `(pprint-indent+ ,(if colon :current :block) ,(car params) xp)))

(def-format-handler #\W (start end) (declare (ignore end))
  (multiple-value-bind (colon atsign) (parse-params start nil)
    (cond ((not (or colon atsign))
           `(write+ ,(get-arg) xp))
          (t `(let (,@(if colon '((*print-pretty* t)))
                    ,@(if atsign '((*print-level* nil) (*print-length* nil))))
                (write+ ,(get-arg) xp))))))

;; --- handle-logical-block --------------------------------------------------

(defun handle-logical-block (start end)
  (multiple-value-bind (colon atsign) (parse-params start nil)
    (let* ((st (+ (params-end start) 1))
           (chunks (chunk-up st end))
           (on-each-line?
             (and (cddr chunks) (atsignp (- (cadr chunks) 1))))
           (prefix
             (cond ((cddr chunks)
                    (pop chunks)
                    (subseq *string* st (directive-start (car chunks))))
                   (colon "(")))
           (suffix
             (cond ((cddr chunks)
                    (subseq *string* (cadr chunks)
                            (directive-start (caddr chunks))))
                   (colon ")"))))
      (when (cdddr chunks)
        (err 24 "Too many subclauses in ~~<...~~:>" (- st 1)))
      (when (and prefix (or (find #\~ prefix) (find #\newline prefix)))
        (err 25 "Prefix in ~~<...~~:> must be literal" st))
      (when (and suffix (or (find #\~ suffix) (find #\newline suffix)))
        (err 26 "Suffix in ~~<...~~:> must be literal" (cadr chunks)))
      (car (bind-args t (if atsign `(prog1 ,(args) (setq ,(args) nil))
                            (get-arg))
             (bind-initial
              `((pprint-logical-block+ (xp ,(args) ,prefix ,suffix ,on-each-line?
                                           ,(not (and *at-top* atsign)) ,atsign)
                  ,@(fill-transform (atsignp (- end 1))
                      (let ((*get-arg-carefully* t)
                            (*at-top* (and *at-top* atsign))
                            (*inner-end* 'logical-block)
                            (*outer-end* 'logical-block))
                        (compile-format (car chunks)
                                        (directive-start (cadr chunks)))))))))))))

(defun fill-transform (doit? body)
  (if (not doit?) body
      (mapcan (lambda (form)
                (cond ((eq (car form) 'write-string++)
                       (fill-transform-literal (cadr form)))
                      ((eq (car form) 'write-char++)
                       (fill-transform-char (cadr form)))
                      (t (list form))))
              body)))

(defun fill-transform-char (char)
  (if (or (char= char #\space) (char= char #\tab))
      (list `(write-char++ ,char xp) '(pprint-newline+ :fill xp))
      `((write-char++ ,char xp))))

(defun fill-transform-literal (string)
  (let ((index 0) (result nil) (end nil))
    (block ftl-loop
      (loop
        (setq end nil)
        (let ((white (%xp-position-if
                       (lambda (c) (or (char= c #\space) (char= c #\tab)))
                       string :start index)))
          (when white
            (setq end (%xp-position-if-not
                       (lambda (c) (or (char= c #\space) (char= c #\tab)))
                       string :start (+ white 1))))
          (when (null end) (setq end (length string)))
          (push `(write-string++ ,(subseq string index end) xp
                                 0 ,(- end index))
                result)
          (when white (push '(pprint-newline+ :fill xp) result))
          (when (null white)
            (return-from ftl-loop (nreverse result)))
          (setq index end))))))

;;; ========================================================================
;;; Part 13: Pretty-Printing Helpers (pretty-array, fn-call, etc.)
;;; ========================================================================

(defun pretty-array (xp array)
  (cond ((vectorp array) (pretty-vector xp array))
        (t (write-string++ "#<ARRAY>" xp 0 8))))

(defun pretty-vector (xp v)
  (pprint-logical-block (xp nil :prefix "#(" :suffix ")")
    (let ((end (length v)) (i 0))
      (when (plusp end)
        (block pv-loop
          (loop (pprint-pop)
                (write+ (aref v i) xp)
                (setq i (+ i 1))
                (when (= i end) (return-from pv-loop nil))
                (write-char++ #\space xp)
                (pprint-newline+ :fill xp)))))))

;; pprint-linear / pprint-fill / pprint-tabular bypass pprint-logical-block
;; and inline the start/end-block dance. NCL has no unwind-protect, so an
;; early exit from inside pprint-logical-block (via the macroexpanded
;; return-from logical-block that pprint-pop+ emits on circular / length
;; abbreviation) would skip the closing suffix. The explicit per-element
;; loop here always reaches end-block on the normal-exit path.

(defun %xp-pp-emit (xp lst colon? newline-kind tab-fn)
  (let ((cursor lst))
    (start-block xp (if colon? "(" "") nil (if colon? ")" ""))
    (when (consp cursor)
      (write+ (car cursor) xp)
      (setq cursor (cdr cursor))
      (block %xp-pp-loop
        (loop
          (when (null cursor) (return-from %xp-pp-loop nil))
          (unless (consp cursor)
            (write-string++ " . " xp 0 3)
            (write+ cursor xp)
            (return-from %xp-pp-loop nil))
          (write-char++ #\space xp)
          (when tab-fn (funcall tab-fn xp))
          (pprint-newline+ newline-kind xp)
          (write+ (car cursor) xp)
          (setq cursor (cdr cursor)))))
    (end-block xp (if colon? ")" ""))))

(defun %xp-pp-dispatch (s lst colon? newline-kind tab-fn)
  (let ((dest (decode-stream-arg s)))
    (cond ((xp-structure-p-safe dest)
           (%xp-pp-emit dest lst colon? newline-kind tab-fn))
          (t (maybe-initiate-xp-printing
              (lambda (xp) (%xp-pp-emit xp lst colon? newline-kind tab-fn))
              dest))))
  nil)

(defun pprint-linear (s list &optional (colon? t) atsign?)
  (declare (ignore atsign?))
  (%xp-pp-dispatch s list colon? :linear nil))

(defun pprint-fill (s list &optional (colon? t) atsign?)
  (declare (ignore atsign?))
  (%xp-pp-dispatch s list colon? :fill nil))

(defun pprint-tabular (s list &optional (colon? t) atsign? (tabsize nil))
  (declare (ignore atsign?))
  (let ((ts (or tabsize 16)))
    (%xp-pp-dispatch s list colon? :fill
                     (lambda (xp)
                       (pprint-tab+ :section-relative 0 ts xp)))))

(defun fn-call (xp list)
  (funcall (formatter "~:<~W~^ ~:I~@_~@{~W~^ ~_~}~:>") xp list))

(defun alternative-fn-call (xp list)
  (if (> (length (symbol-name (car list))) 12)
      (funcall (formatter "~:<~1I~@{~W~^ ~_~}~:>") xp list)
      (funcall (formatter "~:<~W~^ ~:I~@_~@{~W~^ ~_~}~:>") xp list)))

(defun bind-list (xp list &rest args)
  (declare (ignore args))
  (if (do ((i 50 (- i 1))
           (ls list (cdr ls)))
          ((null ls) t)
        (when (or (not (consp ls)) (not (symbolp (car ls))) (minusp i))
          (return nil)))
      (pprint-fill xp list)
      (funcall (formatter "~:<~@{~:/pprint-fill/~^ ~_~}~:>") xp list)))

(defun block-like (xp list &rest args)
  (declare (ignore args))
  (funcall (formatter "~:<~1I~^~W~^ ~@_~W~^~@{ ~_~W~^~}~:>") xp list))

(defun defun-like (xp list &rest args)
  (declare (ignore args))
  (funcall
   (formatter "~:<~1I~W~^ ~@_~W~^ ~@_~:/pprint-fill/~^~@{ ~_~W~^~}~:>")
   xp list))

(defun print-fancy-fn-call (xp list template)
  (let ((i 0) (in-first-section t) (lst list) (tmpl template))
    (pprint-logical-block+ (xp lst "(" ")" nil t nil)
      (write+ (pprint-pop) xp)
      (pprint-indent+ :current 1 xp)
      (block pfc-loop
        (loop
          (pprint-exit-if-list-exhausted)
          (write-char++ #\space xp)
          (when (eq i (car tmpl))
            (pprint-indent+ :block (cadr tmpl) xp)
            (setq tmpl (cddr tmpl))
            (setq in-first-section nil))
          (pprint-newline (cond ((and (zerop i) in-first-section) :miser)
                                (in-first-section :fill)
                                (t :linear))
                          xp)
          (write+ (pprint-pop) xp)
          (setq i (+ i 1)))))))

(defun maybelab (xp item &rest args)
  (declare (ignore args)
           (special need-newline indentation))
  (when need-newline (pprint-newline+ :mandatory xp))
  (cond ((and item (symbolp item))
         (write+ item xp)
         (setq need-newline nil))
        (t (pprint-tab+ :section indentation 0 xp)
           (write+ item xp)
           (setq need-newline t))))

(defun function-call-p (x)
  (and (consp x) (symbolp (car x)) (fboundp (car x))))

;;; ========================================================================
;;; Part 14: Default *PRINT-PPRINT-DISPATCH* entries
;;; ========================================================================

(defun let-print (xp obj)
  (funcall (formatter "~:<~1I~W~^ ~@_~/bind-list/~^~@{ ~_~W~^~}~:>") xp obj))

(defun cond-print (xp obj)
  (funcall (formatter "~:<~W~^ ~:I~@_~@{~:/pprint-linear/~^ ~_~}~:>") xp obj))

(defun dmm-print (xp list)
  (print-fancy-fn-call xp list '(3 1)))

(defun defsetf-print (xp list)
  (print-fancy-fn-call xp list '(3 1)))

(defun do-print (xp obj)
  (funcall
   (formatter
    "~:<~W~^ ~:I~@_~/bind-list/~^ ~_~:/pprint-linear/ ~1I~^~@{ ~_~W~^~}~:>")
   xp obj))

(defun flet-print (xp obj)
  (funcall
   (formatter "~:<~1I~W~^ ~@_~:<~@{~/block-like/~^ ~_~}~:>~^~@{ ~_~W~^~}~:>")
   xp obj))

(defun function-print (xp list)
  (if (and (consp (cdr list)) (null (cddr list)))
      (funcall (formatter "#'~W") xp (cadr list))
      (fn-call xp list)))

(defun mvb-print (xp list)
  (print-fancy-fn-call xp list '(1 3 2 1)))

(defun prog-print (xp list)
  (let ((need-newline t)
        (indentation (+ 1 (length (symbol-name (car list))))))
    (declare (special need-newline indentation))
    (funcall (formatter "~:<~W~^ ~:/pprint-fill/~^ ~@{~/maybelab/~^ ~}~:>")
             xp list)))

(defun setq-print (xp obj)
  (funcall (formatter "~:<~W~^ ~:I~@_~@{~W~^ ~:_~W~^ ~_~}~:>") xp obj))

(defun quote-print (xp list)
  (if (and (consp (cdr list)) (null (cddr list)))
      (funcall (formatter "'~W") xp (cadr list))
      (pprint-fill xp list)))

(defun tagbody-print (xp list)
  (let ((need-newline (and (consp (cdr list))
                           (symbolp (cadr list)) (cadr list)))
        (indentation (+ 1 (length (symbol-name (car list))))))
    (declare (special need-newline indentation))
    (funcall (formatter "~:<~W~^ ~@{~/maybelab/~^ ~}~:>") xp list)))

(defun up-print (xp list)
  (print-fancy-fn-call xp list '(0 3 1 1)))

;;; ========================================================================
;;; Part 15: Initial Dispatch Table
;;; ========================================================================

(setq *ipd* (%make-empty-dispatch))

(set-pprint-dispatch+ '(satisfies function-call-p) #'fn-call '(-5) *ipd*)
(set-pprint-dispatch+ 'cons #'pprint-fill '(-10) *ipd*)

;; Common Lisp special forms and macros.
(set-pprint-dispatch+ '(cons (member defstruct))   #'block-like '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member block))       #'block-like '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member case))        #'block-like '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member catch))       #'block-like '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member ccase))       #'block-like '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member cond))        #'cond-print '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member ctypecase))   #'block-like '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member defconstant)) #'defun-like '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member defmacro))    #'defun-like '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member define-modify-macro)) #'dmm-print '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member defparameter)) #'defun-like '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member defsetf))     #'defsetf-print '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member deftype))     #'defun-like '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member defun))       #'defun-like '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member defvar))      #'defun-like '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member do))          #'do-print   '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member do*))         #'do-print   '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member dolist))      #'block-like '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member dotimes))     #'block-like '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member ecase))       #'block-like '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member etypecase))   #'block-like '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member eval-when))   #'block-like '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member flet))        #'flet-print '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member function))    #'function-print '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member labels))      #'flet-print '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member lambda))      #'block-like '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member let))         #'let-print  '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member let*))        #'let-print  '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member locally))     #'block-like '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member macrolet))    #'flet-print '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member multiple-value-bind)) #'mvb-print '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member multiple-value-setq)) #'block-like '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member prog))        #'prog-print '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member prog*))       #'prog-print '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member progv))       #'defun-like '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member psetf))       #'setq-print '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member psetq))       #'setq-print '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member quote))       #'quote-print '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member return-from)) #'block-like '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member setf))        #'setq-print '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member setq))        #'setq-print '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member tagbody))     #'tagbody-print '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member throw))       #'block-like '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member typecase))    #'block-like '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member unless))      #'block-like '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member unwind-protect)) #'up-print '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member when))        #'block-like '(0) *ipd*)
(set-pprint-dispatch+ '(cons (member with-output-to-string)) #'block-like '(0) *ipd*)

;; Initialise *print-pprint-dispatch* to a fresh copy of *IPD*.
(when (eq *print-pprint-dispatch* t)
  (setq *print-pprint-dispatch* (copy-pprint-dispatch nil)))

nil
