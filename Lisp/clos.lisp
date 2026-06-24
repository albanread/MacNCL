;;;; clos.lisp — CLOS port from Corman Lisp's Closette.
;;;;
;;;; Original: Closette 1.0 (Xerox, 1991), as elaborated in
;;;; Roger Corman's Corman Lisp Sys/clos.lisp. Heavy editing by
;;;; RGC/LC over decades — we're porting the latest shape.
;;;;
;;;; Staging:
;;;;   Stage A — utilities + std-instance representation (this file
;;;;             at first; later stages append).
;;;;   Stage B — class metaobjects + bootstrap.
;;;;   Stage C — defclass + finalize-inheritance.
;;;;   Stage D — make-instance + slot-value.
;;;;   Stage E — defgeneric + dispatch.
;;;;   Stage F — defmethod.
;;;;   Stage G — call-next-method, before/after/around.
;;;;   Stage H — EQL specializers.
;;;;
;;;; Closette uses Corman-specific primitives (uref, alloc-clos-
;;;; instance, with-synchronization). We adapt:
;;;;   - CLOS instances are 4-element Vectors with a marker.
;;;;   - Synchronisation is dropped; we're single-threaded.
;;;;   - The (setf NAME) function-naming convention becomes
;;;;     %SETF-NAME (matches our chunk-6 generic-setf fallback).

;; ─── Stage A: utilities + std-instance representation ───────────────────────

;; -- CLOS instance layout ---------------------------------------------------
;;
;; A CLOS instance is a 4-cell Vector:
;;   slot 0: '%CLOS-INSTANCE marker — distinguishes CLOS objects
;;           from ordinary defstruct vectors and bare vectors.
;;   slot 1: the class (itself a CLOS instance — meta-circular).
;;   slot 2: the slot-storage vector (one cell per effective slot).
;;   slot 3: the class-signature snapshot, for class-redefinition
;;           detection. NIL until set.
;;
;; Closette uses `uref` + offset constants to read these. We use
;; `svref` with named offsets.

(defparameter %clos-instance-marker '%clos-instance)
(defparameter %clos-instance-class-offset 1)
(defparameter %clos-instance-slots-offset 2)
(defparameter %clos-instance-signature-offset 3)
(defparameter %clos-instance-cell-count 4)

(defun clos-instance-p (x)
  "True iff X is a CLOS instance (a 4-vector tagged with our
   marker in slot 0). False for ordinary vectors, defstructs, or
   anything else — defstructs use their own type symbol in slot
   0, never the %CLOS-INSTANCE marker."
  (and (vectorp x)
       (= (length x) %clos-instance-cell-count)
       (eq (svref x 0) %clos-instance-marker)))

(defun alloc-clos-instance ()
  "Allocate an unfilled CLOS instance. Slot 0 is set to the
   marker; the other cells stay NIL until allocate-std-instance
   fills them in."
  (let ((v (make-array %clos-instance-cell-count :initial-element nil)))
    (setf (svref v 0) %clos-instance-marker)
    v))

;; Direct accessors — used internally where we know the arg is a
;; CLOS instance and don't want the type check overhead. Closette
;; uses `clos-instance-class` etc. for the same purpose.

(defun clos-instance-class (x) (svref x %clos-instance-class-offset))
(defun clos-instance-slots (x) (svref x %clos-instance-slots-offset))

;; std-instance-class / -slots / -signature are the public
;; accessors with type checks. Closette wraps each with a
;; clos-instance-p check; we do the same.

(defun std-instance-class (x)
  (cond
    ((clos-instance-p x) (clos-instance-class x))
    (t (error "Not a CLOS instance: ~A" x))))

(defun %setf-std-instance-class (val x)
  (cond
    ((clos-instance-p x)
     (setf (svref x %clos-instance-class-offset) val))
    (t (error "Not a CLOS instance: ~A" x))))

(defun std-instance-slots (x)
  (cond
    ((clos-instance-p x) (clos-instance-slots x))
    (t (error "Not a CLOS instance: ~A" x))))

(defun %setf-std-instance-slots (val x)
  (cond
    ((clos-instance-p x)
     (setf (svref x %clos-instance-slots-offset) val))
    (t (error "Not a CLOS instance: ~A" x))))

(defun std-instance-signature (x)
  (cond
    ((clos-instance-p x)
     (svref x %clos-instance-signature-offset))
    (t (error "Not a CLOS instance: ~A" x))))

(defun %setf-std-instance-signature (val x)
  (cond
    ((clos-instance-p x)
     (setf (svref x %clos-instance-signature-offset) val))
    (t (error "Not a CLOS instance: ~A" x))))

;; -- Allocation -------------------------------------------------------------

(defun allocate-std-instance (class slots)
  "Build a CLOS instance bound to CLASS, with SLOTS as its
   per-instance slot vector. Used during make-instance and when
   bootstrap manually constructs the meta-class tower."
  (let ((x (alloc-clos-instance)))
    (setf (svref x %clos-instance-class-offset) class)
    (setf (svref x %clos-instance-slots-offset) slots)
    x))

;; A unique sentinel for "this slot has never been written".
;; Anything `eql`-equal to this is treated as unbound.
(defparameter secret-unbound-value (list "slot unbound"))

(defun allocate-slot-storage (size initial-value)
  "Make-array wrapper. CLOS instances all use simple-vectors for
   their slot storage — one cell per effective slot."
  (make-array size :initial-element initial-value))

;; -- General-purpose utilities ----------------------------------------------

(defmacro push-on-end (value location)
  "Append VALUE to the end of the list at LOCATION. Closette
   uses this heavily for accumulators where order must be
   preserved (e.g. method ordering)."
  `(setf ,location (nconc ,location (list ,value))))

(defun %setf-getf* (new-value plist key)
  "(setf (getf* plist key) new-value) — like (setf getf) but
   destructively modifies PLIST in place when KEY is already
   there, else appends. PLIST must be non-nil. Closette uses
   this for canonicalising initargs.

   Internal note: we copy PLIST into a let-local before the
   walk + appends because the compiler doesn't yet box mutated
   parameters. The destructive nconc reaches the caller via
   PLIST's existing cons-cell tail, so the semantics still
   match CL even though we never reassign the param itself."
  (let ((p plist))
    (block body
      (let ((x p))
        (loop
          (cond
            ((null x) (return nil))
            ((eq (car x) key)
             (setf (car (cdr x)) new-value)
             (return-from body new-value))
            (t (setq x (cdr (cdr x)))))))
      (setq p (nconc p (list key)))
      (setq p (nconc p (list new-value)))
      new-value)))

(defun mapappend (fn &rest args)
  "Like mapcar, but the per-call results are appended together.
   Standard Closette workhorse for flattening nested results."
  (cond
    ((some (lambda (x) (null x)) args) nil)
    (t (append (apply fn (mapcar #'car args))
               (apply #'mapappend fn (mapcar #'cdr args))))))

(defun mapplist (fn x)
  "mapcar over a property list, calling FN with each (key value)
   pair in turn."
  (cond
    ((null x) nil)
    (t (cons (funcall fn (car x) (cadr x))
             (mapplist fn (cddr x))))))

;; -- Method table -----------------------------------------------------------
;;
;; Each generic function carries a method-table. The table holds
;; the registered methods plus a single-entry cache of the most
;; recently looked-up (types → method) pair — Closette's hot-path
;; optimisation. Stage E's dispatch logic reads this; for now we
;; just install the data structure and its CRUD helpers.
;;
;; Closette wraps every access in `with-synchronization` over a
;; per-table critical section. We're single-threaded and skip
;; that — restore when concurrent dispatch becomes a concern.

(defstruct method-table
  (method-list nil)
  (cached-method nil)
  (cached-method-types nil)
  (eql-specializers nil))

(defun clear-method-table (table)
  (setf (method-table-method-list table) nil)
  (setf (method-table-cached-method table) nil)
  (setf (method-table-cached-method-types table) nil)
  table)

(defun add-method-table-method (table types method)
  "Push (TYPES, METHOD) onto the table and prime the cache
   with this newest entry."
  (setf (method-table-method-list table)
        (cons types (cons method (method-table-method-list table))))
  (setf (method-table-cached-method table) method)
  (setf (method-table-cached-method-types table) types)
  table)

(defun lists-match (list1 list2)
  "Element-wise EQ comparison of two equal-length lists.
   Returns T iff every pair matches; NIL on first mismatch or
   length difference."
  (let ((x list1) (y list2))
    (loop
      (cond
        ((null x) (return (null y)))
        ((null y) (return nil))
        ((not (eq (car x) (car y))) (return nil))
        (t (setq x (cdr x)) (setq y (cdr y)))))))

(defun find-method-table-method (table eqls-classes)
  "Walk TABLE's method-list (laid out as types1 method1 types2
   method2 …) and return the first method whose types match
   EQLS-CLASSES, or NIL."
  (let ((p (method-table-method-list table)))
    (loop
      (cond
        ((null p) (return nil))
        ((lists-match (car p) eqls-classes) (return (cadr p)))
        (t (setq p (cdr (cdr p))))))))

;; -- EQL specializer cache --------------------------------------------------
;;
;; Used by Stage H. Maps an eql-target object → a synthetic class
;; object representing the (eql obj) specialiser. One global table.

(defparameter *clos-singleton-specializers*
  (make-hash-table :test 'eql))

;; -- Predicates -------------------------------------------------------------
;;
;; Stage A defines the signatures; the predicates start as
;; constant-NIL because no CLOS classes exist yet. Stage B
;; promotes them once the meta-class tower is wired and
;; STANDARD-CLASS / STANDARD-GENERIC-FUNCTION become real
;; class objects.

(defun standard-class-p (x)
  (declare (ignore x))
  nil)

(defun standard-generic-function-p (x)
  (declare (ignore x))
  nil)

;; ─── Stage B: class metaobjects + bootstrap ─────────────────────────────────
;;
;; The chicken-and-egg: defclass needs the metaclass to exist, but
;; the metaclass is itself a class. Closette resolves this with a
;; manual construction sequence at the end of the file (its "10
;; easy steps"). We follow the same shape — the data (the-defclass-
;; standard-class) is declared up front, then the bootstrap section
;; at the bottom of this stage hand-builds standard-class and T.

;; -- find-class registry ----------------------------------------------------

(defparameter *clos-class-table* (make-hash-table :test 'eq))

(defun find-class (name &optional (errorp t))
  "Return the class object registered under NAME. Signals an
   error if no such class is registered and ERRORP is true;
   returns NIL otherwise."
  (let ((c (gethash name *clos-class-table*)))
    (cond
      (c c)
      (errorp (error "find-class: no class named ~A" name))
      (t nil))))

(defun %setf-find-class (new-value name &optional errorp)
  (declare (ignore errorp))
  (setf (gethash name *clos-class-table*) new-value)
  new-value)

(defun forget-all-classes ()
  (clrhash *clos-class-table*)
  nil)

(defparameter *clos-generic-function-table* (make-hash-table :test 'eq))

(defun forget-all-generic-functions ()
  (clrhash *clos-generic-function-table*)
  nil)

;; -- Slot definitions (plist representation) --------------------------------
;;
;; Closette represents slot definitions as plists with :name,
;; :initargs, :initform, :initfunction, :readers, :writers,
;; :allocation, optionally :documentation and :shared-slot. The
;; accessors are getf/setf-getf*. See clos.lisp:747.

(defun make-direct-slot-definition (&key name (initargs nil) (initform nil)
                                          (initfunction nil) (readers nil)
                                          (writers nil) (allocation :instance))
  (let ((slot (list ':name name
                    ':initargs initargs
                    ':initform initform
                    ':initfunction initfunction
                    ':readers readers
                    ':writers writers
                    ':allocation allocation)))
    (when (eq allocation :class)
      (setf slot (nconc slot (list ':shared-slot (list secret-unbound-value)))))
    slot))

(defun make-effective-slot-definition (&key name (initargs nil) (initform nil)
                                            (initfunction nil) (allocation :instance))
  (list ':name name
        ':initargs initargs
        ':initform initform
        ':initfunction initfunction
        ':allocation allocation))

(defun slot-definition-name (slot) (getf slot ':name))
(defun slot-definition-initargs (slot) (getf slot ':initargs))
(defun slot-definition-initform (slot) (getf slot ':initform))
(defun slot-definition-initfunction (slot) (getf slot ':initfunction))
(defun slot-definition-readers (slot) (getf slot ':readers))
(defun slot-definition-writers (slot) (getf slot ':writers))
(defun slot-definition-allocation (slot) (getf slot ':allocation))
(defun slot-definition-documentation (slot) (getf slot ':documentation))
(defun slot-definition-shared-slot (slot) (getf slot ':shared-slot))

(defun instance-slot-p (slot)
  (eq (slot-definition-allocation slot) ':instance))

;; -- Standard-class slot positions (the meta-circular shortcut) -------------
;;
;; standard-class's effective-slots layout — the index of each
;; slot in the slot-storage vector. Closette hard-codes these in
;; slot-location; we mirror the choice. See the-defclass-standard-
;; class below for the source of truth, but ORDER MATTERS for the
;; bootstrap so we list them once explicitly.

(defparameter *standard-class-slot-names*
  '(direct-slots effective-slots
    shared-slot-definitions shared-slots
    direct-default-initargs effective-default-initargs
    direct-superclasses class-precedence-list direct-methods
    direct-subclasses
    name documentation))

;; the-class-standard-class is filled in by the bootstrap below.
;; the-slots-of-standard-class is the list of effective-slot-
;; definitions for standard-class.
(defparameter the-slots-of-standard-class nil)
(defparameter the-class-standard-class nil)

;; SLOT ORDER MATTERS — must match what compute-slots will
;; produce later when defclass standard-class re-creates the
;; class. compute-slots walks CPL = [SC, CL, SP, FRC, MO, T]
;; in order, gathering each class's direct-slots:
;;   SC: direct-slots, effective-slots, shared-slot-definitions,
;;       shared-slots, direct-default-initargs,
;;       effective-default-initargs   (6)
;;   CL: (none)
;;   SP: direct-superclasses, class-precedence-list,
;;       direct-methods                (3)
;;   FRC: direct-subclasses             (1)
;;   MO: name, documentation            (2)
;; If the bootstrap-skeleton order doesn't match this, T's slot
;; vector (filled in step 5 with the skeleton order) is
;; misaligned after step 8 swaps T's class to the new
;; standard-class — leading to e.g. T's slot 0 holding 'T (the
;; class-name) being interpreted as direct-slots, which crashes
;; the next mapappend.
(defparameter the-defclass-standard-class
  '(defclass standard-class (class)
     ((direct-slots)
      (effective-slots)
      (shared-slot-definitions :initform ())
      (shared-slots :initform ())
      (direct-default-initargs :initform () :initarg :direct-default-initargs)
      (effective-default-initargs :initform ())
      (direct-superclasses :initarg :direct-superclasses)
      (class-precedence-list)
      (direct-methods :initform ())
      (direct-subclasses :initform ())
      (name :initarg :name)
      (documentation :initform () :initarg :documentation))))

;; -- Slot access ------------------------------------------------------------
;;
;; slot-location walks the class's effective-slots list looking
;; for the named slot, returning its position-among-instance-slots
;; (shared slots are counted separately). Closette short-circuits
;; the lookup of 'effective-slots in standard-class because that
;; query would recurse forever otherwise.

(defun slot-location (class slot-name)
  "Return the index of SLOT-NAME within CLASS's instance-slot
   storage, or NIL if it's a shared slot or absent. Special case:
   the lookup of 'effective-slots in standard-class returns 1 by
   construction — without this short-circuit, finding any slot in
   standard-class would recurse infinitely. (Position 1 is determined
   by the slot order produced by compute-slots walking standard-class's
   CPL; see the-defclass-standard-class.)"
  (cond
    ((and (eq slot-name 'effective-slots)
          (eq class the-class-standard-class))
     1)
    (t
     (let ((slots (class-effective-slots class))
           (pos 0)
           (result nil))
       (loop
         (cond
           ((null slots) (return nil))
           (t (let ((s (car slots)))
                (cond
                  ((eq (slot-definition-name s) slot-name)
                   (setq result pos)
                   (setq slots nil))
                  (t (when (instance-slot-p s)
                       (setq pos (+ pos 1)))
                     (setq slots (cdr slots))))))))
       result))))

(defun shared-slot-location (class slot-name)
  (let ((slots (class-shared-slot-definitions class))
        (pos 0)
        (result nil))
    (loop
      (cond
        ((null slots) (return nil))
        (t (let ((s (car slots)))
             (cond
               ((eq (slot-definition-name s) slot-name)
                (setq result pos)
                (setq slots nil))
               (t (setq pos (+ pos 1))
                  (setq slots (cdr slots))))))))
    result))

;; slot-contents is just svref. Closette uses a uref-with-offset
;; trick for inline access; we don't have that and svref is fine.
(defun slot-contents (slots location) (svref slots location))
(defun %setf-slot-contents (new-value slots location)
  (setf (svref slots location) new-value)
  new-value)

(defun std-slot-value (instance slot-name)
  "Read SLOT-NAME from INSTANCE via slot-location. Errors if
   the slot doesn't exist or is unbound."
  (let ((class (class-of instance)))
    (let ((location (slot-location class slot-name))
          (val nil))
      (cond
        (location
         (setq val (slot-contents (std-instance-slots instance) location)))
        (t
         (let ((sloc (shared-slot-location class slot-name)))
           (cond
             (sloc
              (setq val (car (slot-contents (class-shared-slots class) sloc))))
             (t (error "The slot ~A is missing from the class ~A."
                       slot-name class))))))
      (when (eq secret-unbound-value val)
        (error "The slot ~A is unbound in the object ~A." slot-name instance))
      val)))

(defun %setf-std-slot-value (new-value instance slot-name)
  (let ((class (class-of instance)))
    (let ((location (slot-location class slot-name)))
      (cond
        (location
         (setf (slot-contents (std-instance-slots instance) location)
               new-value))
        (t
         (let ((sloc (shared-slot-location class slot-name)))
           (cond
             (sloc
              (setf (car (slot-contents (class-shared-slots class) sloc))
                    new-value))
             (t (error "The slot ~A is missing from the class ~A."
                       slot-name class))))))))
  new-value)

;; slot-value / (setf slot-value) — Stage D adds the full
;; standard-class-p dispatch path and slot-value-using-class.
;; For Stage B we just route to std-slot-value.

(defun slot-value (object slot-name)
  (std-slot-value object slot-name))

(defun %setf-slot-value (new-value object slot-name)
  (%setf-std-slot-value new-value object slot-name))

;; -- Class metaobject accessors --------------------------------------------
;;
;; Closette implements these as plain defuns calling slot-value.
;; They become generic functions in stage E (which dispatches via
;; standard-class-p to the std-slot-value fast path); for now they
;; just call slot-value directly.

(defun class-name (class) (slot-value class 'name))
(defun %setf-class-name (new-value class)
  (setf (slot-value class 'name) new-value))

(defun class-documentation (class) (slot-value class 'documentation))
(defun %setf-class-documentation (new-value class)
  (setf (slot-value class 'documentation) new-value))

(defun class-direct-superclasses (class)
  (slot-value class 'direct-superclasses))
(defun %setf-class-direct-superclasses (new-value class)
  (setf (slot-value class 'direct-superclasses) new-value))

(defun class-direct-slots (class) (slot-value class 'direct-slots))
(defun %setf-class-direct-slots (new-value class)
  (setf (slot-value class 'direct-slots) new-value))

(defun class-precedence-list (class)
  (slot-value class 'class-precedence-list))
(defun %setf-class-precedence-list (new-value class)
  (setf (slot-value class 'class-precedence-list) new-value))

(defun class-effective-slots (class) (slot-value class 'effective-slots))
(defun %setf-class-effective-slots (new-value class)
  (setf (slot-value class 'effective-slots) new-value))

(defun class-direct-subclasses (class)
  (slot-value class 'direct-subclasses))
(defun %setf-class-direct-subclasses (new-value class)
  (setf (slot-value class 'direct-subclasses) new-value))

(defun class-direct-methods (class) (slot-value class 'direct-methods))
(defun %setf-class-direct-methods (new-value class)
  (setf (slot-value class 'direct-methods) new-value))

(defun class-shared-slots (class) (slot-value class 'shared-slots))
(defun %setf-class-shared-slots (new-value class)
  (setf (slot-value class 'shared-slots) new-value))

(defun class-shared-slot-definitions (class)
  (slot-value class 'shared-slot-definitions))
(defun %setf-class-shared-slot-definitions (new-value class)
  (setf (slot-value class 'shared-slot-definitions) new-value))

;; -- subclassp / sub-specializer-p -----------------------------------------

(defun subclassp (c1 c2)
  (not (null (find c2 (class-precedence-list c1)))))

(defun sub-specializer-p (c1 c2 c-arg)
  (let ((cpl (class-precedence-list c-arg)))
    (not (null (find c2 (cdr (member c1 cpl)))))))

;; -- class-of + built-in-class-of ------------------------------------------
;;
;; Initial built-in-class-of returns NIL for built-in types until
;; Stage C runs the full bootstrap that defclasses INTEGER /
;; SYMBOL / etc. After that, all built-ins resolve.

(defun class-of (x)
  (cond
    ((clos-instance-p x) (std-instance-class x))
    (t (built-in-class-of x))))

(defun built-in-class-of (x)
  "Slow but straightforward typecase. Stage C extends the
   class table to cover all the built-in types this checks for."
  (typecase x
    (null      (find-class 'null nil))
    (symbol    (find-class 'symbol nil))
    (integer   (find-class 'integer nil))
    (cons      (find-class 'cons nil))
    (character (find-class 'character nil))
    (string    (find-class 'string nil))
    (vector    (find-class 'vector nil))
    (function  (find-class 'function nil))
    (t         (find-class 't nil))))

;; -- Standard-instance allocation (used during bootstrap and later) ---------

(defun std-allocate-instance (class)
  (allocate-std-instance
    class
    (allocate-slot-storage
      (length (class-effective-slots class))
      secret-unbound-value)))

;; ─── Bootstrap steps 1-5: skeleton standard-class + T ───────────────────────
;;
;; Each defclass slot in the-defclass-standard-class becomes an
;; effective-slot-definition (just :name + :allocation :instance
;; for now; initforms get plumbed in stage C). Then we manually
;; allocate standard-class and patch up the circular class-of
;; link — once that's done class-effective-slots etc. all work
;; on standard-class itself.

(forget-all-classes)
(forget-all-generic-functions)

;; Step 1: build effective-slot-definitions for standard-class's slots.
(setq the-slots-of-standard-class
      (mapcar (lambda (slotd)
                (make-effective-slot-definition
                  :name (car slotd)
                  :initargs (let ((a (getf (cdr slotd) ':initarg)))
                              (cond (a (list a))
                                    (t nil)))
                  :initform (getf (cdr slotd) ':initform)
                  :allocation ':instance))
              (nth 3 the-defclass-standard-class)))

;; Step 2: hand-allocate standard-class with placeholder class link.
(setq the-class-standard-class
      (allocate-std-instance
        'tba
        (make-array (length the-slots-of-standard-class)
                    :initial-element secret-unbound-value)))
;; Step 3: install the circular class-of link.
(setf (std-instance-class the-class-standard-class)
      the-class-standard-class)
;; (now slot-value on standard-class works — slot-location's
;; effective-slots short-circuit kicks in)

;; Step 4: fill in standard-class's class-effective-slots so that
;; lookups for OTHER slot names also work.
(setf (class-effective-slots the-class-standard-class)
      the-slots-of-standard-class)
;; Step 5: hand-build the class T. T has no superclasses, no
;; methods, no slots — it's the root of the type hierarchy.
(setf (gethash 't *clos-class-table*)
      (let ((class (std-allocate-instance the-class-standard-class)))
        (setf (class-name class) 't)
        (setf (class-documentation class) nil)
        (setf (class-direct-subclasses class) nil)
        (setf (class-direct-superclasses class) nil)
        (setf (class-direct-methods class) nil)
        (setf (class-direct-slots class) nil)
        (setf (class-precedence-list class) (list class))
        (setf (class-effective-slots class) nil)
        (setf (class-shared-slot-definitions class) nil)
        (setf (class-shared-slots class) nil)
        class))

;; ─── Stage C: defclass + finalize-inheritance + bootstrap rest ─────────────
;;
;; Defclass becomes a macro that calls ensure-class, which
;; allocates a fresh class instance, fills in its slots, then
;; finalizes inheritance (computes CPL + effective slots).
;;
;; Closette uses generic-function-based reader/writer methods
;; (add-reader-method / add-writer-method) when slots have
;; :reader / :writer / :accessor options. Those need Stage E's
;; generic-function machinery, so we accept the option syntax
;; but DROP the auto-generated accessors here. Stage F or G can
;; revisit.

;; -- canonicalisation helpers ----------------------------------------------

(defun canonicalize-direct-slot (spec)
  "Translate a slot-spec from defclass surface syntax into a
   list-form that, when EVALuated, produces a property list
   suitable for make-direct-slot-definition."
  (cond
    ((symbolp spec) `(list :name ',spec))
    (t
     (let ((name (car spec))
           (initfunction nil)
           (initform nil)
           (initargs nil)
           (readers nil)
           (writers nil)
           (other nil))
       (let ((olist (cdr spec)))
         (loop
           (cond
             ((null olist) (return nil))
             (t (case (car olist)
                  (:initform
                   (setq initfunction `(function (lambda () ,(cadr olist))))
                   (setq initform `',(cadr olist)))
                  (:initarg
                   (setq initargs (append initargs (list (cadr olist)))))
                  (:reader
                   (setq readers (append readers (list (cadr olist)))))
                  (:writer
                   (setq writers (append writers (list (cadr olist)))))
                  (:accessor
                   (setq readers (append readers (list (cadr olist))))
                   (setq writers
                         (append writers (list `(setf ,(cadr olist))))))
                  (otherwise
                   (setq other
                         (append other
                                 (list `',(car olist) `',(cadr olist))))))
                (setq olist (cdr (cdr olist)))))))
       `(list
         :name ',name
         ,@(when initfunction
             `(:initform ,initform :initfunction ,initfunction))
         ,@(when initargs `(:initargs ',initargs))
         ,@(when readers `(:readers ',readers))
         ,@(when writers `(:writers ',writers))
         ,@other)))))

(defun canonicalize-direct-slots (slot-definitions)
  `(list ,@(mapcar #'canonicalize-direct-slot slot-definitions)))

(defun canonicalize-direct-superclass (class-name)
  `(find-class ',class-name))

(defun canonicalize-direct-superclasses (direct-superclasses)
  `(list ,@(mapcar #'canonicalize-direct-superclass direct-superclasses)))

(defun canonicalize-defclass-option (option)
  (case (car option)
    (:metaclass
     (list :metaclass `(find-class ',(cadr option))))
    (:documentation
     (list :documentation `',(cadr option)))
    (:default-initargs
     (list :direct-default-initargs
           `(list ,@(mapplist
                     (lambda (key value)
                       `(list ',key ',value (function (lambda () ,value))))
                     (cdr option)))))
    (otherwise
     (list `',(car option) `',(cdr option)))))

(defun canonicalize-defclass-options (options)
  (mapappend #'canonicalize-defclass-option options))

;; -- defclass macro --------------------------------------------------------

(defmacro defclass (name direct-superclasses slot-definitions &rest options)
  ;; Spec says defclass returns the class object. Our printer doesn't
  ;; cycle-detect yet so returning the class crashes the REPL when its
  ;; metaclass back-link is followed. Return the class NAME instead —
  ;; defstruct does the same. Stage I (printer polish) lifts this.
  `(progn
     (ensure-class ',name
                   :direct-superclasses
                   ,(canonicalize-direct-superclasses direct-superclasses)
                   :direct-slots
                   ,(canonicalize-direct-slots slot-definitions)
                   ,@(canonicalize-defclass-options options))
     ',name))

;; -- ensure-class ----------------------------------------------------------
;;
;; Closette's ensure-class accepts a :metaclass option and
;; switches between make-instance-standard-class and the
;; generic-function make-instance based on it. We only support
;; the standard-class case at this stage.

(defun ensure-class (name &rest all-keys
                     &key (metaclass the-class-standard-class)
                     &allow-other-keys)
  (declare (ignore metaclass))
  (let ((class (apply #'make-instance-standard-class
                      the-class-standard-class
                      :name name
                      all-keys)))
    (setf (find-class name) class)
    class))

(defun %setf-find-class (new-value name &rest rest)
  (declare (ignore rest))
  (setf (gethash name *clos-class-table*) new-value)
  new-value)

;; -- make-instance-standard-class ------------------------------------------
;;
;; Builds a class instance directly, bypassing the generic-
;; function dispatch path (which doesn't exist yet). Closette's
;; std-after-initialization-for-classes also wires up reader /
;; writer methods if the slot specs request them — we skip that
;; for now (stage E adds it back).

(defun make-instance-standard-class (metaclass &key name direct-superclasses
                                                    direct-slots
                                               &allow-other-keys)
  (declare (ignore metaclass))
  (let ((class (std-allocate-instance the-class-standard-class)))
    (setf (class-name class) name)
    (setf (class-documentation class) nil)
    (setf (class-direct-subclasses class) nil)
    (setf (class-direct-methods class) nil)
    (setf (slot-value class 'direct-default-initargs) nil)
    (setf (slot-value class 'effective-default-initargs) nil)
    (std-after-initialization-for-classes
     class
     :direct-superclasses direct-superclasses
     :direct-slots direct-slots)
    (std-finalize-inheritance class)
    class))

(defun std-after-initialization-for-classes (class &key direct-superclasses
                                                        direct-slots
                                                   &allow-other-keys)
  ;; Update class hierarchy.
  (let ((supers (or direct-superclasses
                    (list (find-class 'standard-object nil)))))
    (when (some (lambda (x) (null x)) supers)
      (setq supers (list (find-class 't))))
    (setf (class-direct-superclasses class) supers)
    (dolist (super supers)
      (let ((subs (class-direct-subclasses super)))
        (unless (member class subs)
          (setf (class-direct-subclasses super) (cons class subs))))))
  (let ((slots (mapcar (lambda (props)
                         (apply #'make-direct-slot-definition props))
                       direct-slots)))
    (setf (class-direct-slots class) slots))
  nil)

;; -- finalize-inheritance --------------------------------------------------

(defun finalize-inheritance (class) (std-finalize-inheritance class))

(defun std-finalize-inheritance (class)
  (setf (class-precedence-list class) (compute-class-precedence-list class))
  (let ((class-slots (compute-slots class)))
    (setf (class-effective-slots class)
          (remove-if-not #'instance-slot-p class-slots))
    (setf (class-shared-slot-definitions class)
          (remove-if #'instance-slot-p class-slots))
    (setf (class-shared-slots class) nil))
  nil)

;; -- Class precedence list (CPL) ------------------------------------------

(defun collect-superclasses* (class)
  (labels ((walk (seen supers)
             (let ((todo (set-difference supers seen)))
               (cond
                 ((null todo) supers)
                 (t (let ((c (car todo)))
                      (walk (cons c seen)
                            (union (class-direct-superclasses c) supers))))))))
    (walk nil (list class))))

(defun local-precedence-ordering (class)
  ;; Closette's version uses (mapcar #'list left right) to pair
  ;; the parent chain. Our mapcar is single-list; we walk the
  ;; supers list manually keeping the previous element as state.
  (let ((supers (class-direct-superclasses class))
        (prev class)
        (result nil))
    (dolist (super supers)
      (setq result (cons (list prev super) result))
      (setq prev super))
    (nreverse result)))

(defun std-tie-breaker-rule (minimal-elements cpl-so-far)
  (block tb
    (dolist (cpl-c (reverse cpl-so-far))
      (let* ((supers (class-direct-superclasses cpl-c))
             (common (intersection minimal-elements supers)))
        (when common
          (return-from tb (car common)))))))

(defun topological-sort (elements constraints tie-breaker)
  (block ts
    (let ((rem-c constraints)
          (rem-e elements)
          (result nil))
      (loop
        (let ((minimal
                (remove-if (lambda (c)
                             (member c rem-c :key #'cadr))
                           rem-e)))
          (cond
            ((null minimal)
             (cond
               ((null rem-e) (return-from ts result))
               (t (error "Inconsistent precedence graph."))))
            (t
             (let ((choice (cond
                             ((null (cdr minimal)) (car minimal))
                             (t (funcall tie-breaker minimal result)))))
               (setq result (append result (list choice)))
               (setq rem-e (remove choice rem-e))
               (setq rem-c (remove choice rem-c
                                   :test (lambda (x pair)
                                           (member x pair))))))))))))

(defun compute-class-precedence-list (class)
  (std-compute-class-precedence-list class))

(defun std-compute-class-precedence-list (class)
  (let ((classes (collect-superclasses* class)))
    (topological-sort classes
                      (remove-duplicates
                       (mapappend #'local-precedence-ordering classes))
                      #'std-tie-breaker-rule)))

;; -- Slot inheritance (compute-slots) -------------------------------------

(defun find-if-not (pred lst &key (key #'identity))
  "Like find-if but for the negated predicate. Closette uses it
   to find the first slot with a non-nil initfunction."
  (find-if (complement pred) lst :key key))

(defun compute-slots (class) (std-compute-slots class))

(defun std-compute-slots (class)
  (let* ((all-slots (mapappend #'class-direct-slots
                               (class-precedence-list class)))
         (all-names (remove-duplicates
                     (mapcar #'slot-definition-name all-slots))))
    (mapcar (lambda (name)
              (compute-effective-slot-definition
               class
               (remove-if-not (lambda (s)
                                (eq name (slot-definition-name s)))
                              all-slots)))
            all-names)))

(defun compute-effective-slot-definition (class direct-slots)
  (std-compute-effective-slot-definition class direct-slots))

(defun std-compute-effective-slot-definition (class direct-slots)
  (declare (ignore class))
  (let* ((initer (find-if-not (lambda (x) (null x)) direct-slots
                              :key #'slot-definition-initfunction))
         (first-slot (car direct-slots))
         (alloc (slot-definition-allocation first-slot)))
    (make-effective-slot-definition
     :name (slot-definition-name first-slot)
     :initform (when initer (slot-definition-initform initer))
     :initfunction (when initer (slot-definition-initfunction initer))
     :initargs (remove-duplicates
                (mapappend #'slot-definition-initargs direct-slots))
     :allocation alloc)))

;; ─── Bootstrap steps 6-9: defclass standard-object → all built-ins ─────────
;;
;; Closette steps 6-9 use defclass directly. The first one
;; (standard-object) has T as a parent — and T is already in
;; the table from stage B step 5. After that, the meta-classes
;; chain up through metaobject / forward-referenced-class /
;; specializer / class / standard-class. Step 8 then re-points
;; every existing class's class-of link to the freshly-defclassed
;; standard-class. Step 9 defclasses the built-in types so
;; built-in-class-of finds them.

;; Step 6: superclass tower for standard-class.
(defclass standard-object (t) ())
(defclass metaobject ()
  ((name :initarg :name)
   (documentation :initform nil :initarg :documentation)))
(defclass forward-referenced-class (metaobject)
  ((direct-subclasses :initform nil)))
(defclass specializer (forward-referenced-class)
  ((direct-superclasses :initarg :direct-superclasses)
   class-precedence-list
   (direct-methods :initform nil)))
(defclass class (specializer) ())

;; Step 7: define the full standard-class via defclass, then
;; re-point the global the-class-standard-class to the new
;; class object (defclass returns the NAME, not the class).
(defclass standard-class (class)
  (direct-slots
   effective-slots
   (shared-slot-definitions :initform nil)
   (shared-slots :initform nil)
   (direct-default-initargs
    :initform nil :initarg :direct-default-initargs)
   (effective-default-initargs :initform nil)))
(setq the-class-standard-class (find-class 'standard-class))

;; Step 8: every previously-allocated class instance still points
;; at the SKELETON standard-class from stage B; rewrite each one
;; to reference the new standard-class.
(dolist (n '(t standard-object metaobject forward-referenced-class
             specializer class standard-class))
  (let ((c (find-class n nil)))
    (when c
      (setf (std-instance-class c) the-class-standard-class))))

;; Step 9: defclass the built-in types so built-in-class-of can
;; resolve them. Order matters where there's inheritance.
(defclass symbol (t) ())
(defclass sequence (t) ())
(defclass array (t) ())
(defclass number (t) ())
(defclass character (t) ())
(defclass function (t) ())
(defclass package (t) ())
(defclass pathname (t) ())
(defclass readtable (t) ())
(defclass stream (t) ())
(defclass list (sequence) ())
(defclass null (symbol list) ())
(defclass cons (list) ())
(defclass vector (array sequence) ())
(defclass string (vector) ())
(defclass integer (number) ())
(defclass float (number) ())
(defclass hash-table (t) ())

;; Promote the predicates now that real classes exist. Each
;; checks "is x a CLOS instance whose class IS standard-class"
;; — which catches instances of standard-class itself and any
;; user-defined subclass.
(defun standard-class-p (x)
  (and (clos-instance-p x)
       (eq (class-of x) the-class-standard-class)))

;; standard-generic-function-p is promoted in stage E once
;; standard-generic-function exists.

;; Return a printable sentinel so Session::eval's last-value
;; format_word doesn't try to render a circular class instance
;; (printer cycle handling lands in stage I).

;; ─── Stage D: make-instance + slot-value polish ────────────────────────────
;;
;; Closette splits instance creation across allocate-instance,
;; make-instance, initialize-instance and shared-initialize, all
;; eventually as generic functions. Stage D ships them as plain
;; functions so they're usable now; Stage I (or later) re-defines
;; the user-facing ones as generic-function-method tuples once
;; defmethod exists.
;;
;; What we provide:
;;   * slot-boundp / slot-makunbound / slot-exists-p
;;   * std-allocate-instance + allocate-instance dispatch
;;   * make-instance / initialize-instance / shared-initialize
;;     (plain-function flavour)
;;   * default-initargs handling on the class side
;;   * with-slots (read + write via slot-value)
;;   * with-accessors (calls user-named accessor functions)

;; -- slot-boundp / -makunbound / -exists-p ---------------------------------

(defun std-slot-boundp (instance slot-name)
  (let ((class (class-of instance)))
    (let ((location (slot-location class slot-name)))
      (cond
        (location
         (not (eq secret-unbound-value
                  (slot-contents (std-instance-slots instance) location))))
        (t
         (let ((sloc (shared-slot-location class slot-name)))
           (cond
             (sloc
              (not (eq secret-unbound-value
                       (car (slot-contents (class-shared-slots class) sloc)))))
             (t (error "The slot ~A is missing from the class ~A."
                       slot-name class)))))))))

(defun slot-boundp (object slot-name)
  (std-slot-boundp object slot-name))

(defun std-slot-makunbound (instance slot-name)
  (let ((class (class-of instance)))
    (let ((location (slot-location class slot-name)))
      (cond
        (location
         (setf (slot-contents (std-instance-slots instance) location)
               secret-unbound-value))
        (t
         (let ((sloc (shared-slot-location class slot-name)))
           (cond
             (sloc
              (setf (car (slot-contents (class-shared-slots class) sloc))
                    secret-unbound-value))
             (t (error "The slot ~A is missing from the class ~A."
                       slot-name class))))))))
  instance)

(defun slot-makunbound (object slot-name)
  (std-slot-makunbound object slot-name))

(defun std-slot-exists-p (instance slot-name)
  (not (null (find slot-name (class-effective-slots (class-of instance))
                   :key #'slot-definition-name))))

(defun slot-exists-p (object slot-name)
  (std-slot-exists-p object slot-name))

;; -- allocate-instance -----------------------------------------------------
;;
;; Closette's generic version dispatches on metaclass. We have no
;; defgeneric yet so allocate-instance is a plain function that
;; picks the standard path. Stage E or later upgrades this to a gf.

(defun allocate-instance (class)
  (std-allocate-instance class))

;; -- default-initargs ------------------------------------------------------

(defun class-default-initargs (class)
  (slot-value class 'effective-default-initargs))

(defun %setf-class-default-initargs (new-value class)
  (setf (slot-value class 'effective-default-initargs) new-value))

(defun class-direct-default-initargs (class)
  (slot-value class 'direct-default-initargs))

(defun %setf-class-direct-default-initargs (new-value class)
  (setf (slot-value class 'direct-default-initargs) new-value))

(defun compute-default-initargs (class)
  "Walk the CPL once, collecting each class's direct-default-
   initargs in turn. Earlier (more specific) wins on duplicates."
  (let ((result nil))
    (dolist (super (class-precedence-list class))
      (dolist (entry (class-direct-default-initargs super))
        (let ((key (car entry)))
          (unless (member key result :key #'car)
            (setq result (append result (list entry)))))))
    result))

;; -- shared-initialize -----------------------------------------------------
;;
;; Walks every effective slot of the instance's class. For each
;; slot, checks whether any of its initargs appears in the
;; supplied initargs plist; if so, uses that value. Otherwise, if
;; the slot is in the SLOT-NAMES set (or SLOT-NAMES is T) and not
;; already bound, evaluates the slot's initfunction.
;;
;; SLOT-NAMES = T   →   initialise every unbound slot from initform
;; SLOT-NAMES = ()  →   skip initforms entirely
;; otherwise         →   only initialise slots whose name appears

(defun %lookup-initarg (initargs slot-initargs)
  "Return (cons VALUE T) if any of SLOT-INITARGS is present as a
   key in INITARGS (a plist), else NIL. cons-form so callers can
   distinguish a found NIL value from absent."
  (let ((result nil))
    (block done
      (dolist (key slot-initargs)
        (let ((tail (member key initargs)))
          (when tail
            (setq result (cons (cadr tail) t))
            (return-from done nil)))))
    result))

;; SHARED-INITIALIZE, INITIALIZE-INSTANCE, and REINITIALIZE-INSTANCE
;; are defined as plain DEFUNs at this point in the file because
;; DEFGENERIC isn't introduced until later. Stage F (after defmethod
;; is wired) re-installs them as proper generic functions with the
;; existing body as the primary method on (T ...), so user code can
;; layer :BEFORE / :AFTER methods on top without losing the primary
;; behaviour. See "Stage F: standard-GF promotion" below.

(defun shared-initialize (instance slot-names &rest all-keys)
  (dolist (slot (class-effective-slots (class-of instance)))
    (let* ((slot-name (slot-definition-name slot))
           (slot-initargs (slot-definition-initargs slot))
           (found (%lookup-initarg all-keys slot-initargs)))
      (cond
        (found
         (setf (slot-value instance slot-name) (car found)))
        (t
         (when (and (not (slot-boundp instance slot-name))
                    (not (null (slot-definition-initfunction slot)))
                    (or (eq slot-names 't)
                        (member slot-name slot-names)))
           (setf (slot-value instance slot-name)
                 (funcall (slot-definition-initfunction slot))))))))
  instance)

(defun initialize-instance (instance &rest all-keys)
  (apply #'shared-initialize instance 't all-keys))

(defun reinitialize-instance (instance &rest all-keys)
  (apply #'shared-initialize instance nil all-keys))

;; -- merge-default-initargs ------------------------------------------------

(defun %merge-default-initargs (class initargs)
  "Add each of CLASS's effective-default-initargs to INITARGS
   only when no entry with the same key is already present.
   Default initforms are funcalled lazily (their initfunction is
   the third element of each (key value initfn) triple)."
  (let ((result initargs))
    (dolist (entry (class-default-initargs class))
      (let ((key (car entry)))
        (unless (member key result)
          (let ((initfn (caddr entry)))
            (setq result (append result
                                 (list key (funcall initfn))))))))
    result))

;; -- make-instance ---------------------------------------------------------
;;
;; Plain-function form. Accepts either a class object or a class
;; name (symbol). Effective-default-initargs are merged BEFORE
;; passing the keys to initialize-instance — that's what the
;; AMOP says, and Closette's late-stage make-instance gf does it
;; too via shared-initialize's :default-initargs handling.

(defun make-instance (class-or-name &rest initargs)
  (let ((class (cond
                 ((symbolp class-or-name) (find-class class-or-name))
                 (t class-or-name))))
    (let ((merged (%merge-default-initargs class initargs)))
      (let ((instance (allocate-instance class)))
        (apply #'initialize-instance instance merged)
        instance))))

;; -- with-slots ------------------------------------------------------------
;;
;; Vassili Bykov's classic shape: each binding name becomes a
;; symbol-macro (here: a let-binding for the read form, plus a
;; setf-method indirection). We don't have define-symbol-macro
;; yet, so simulate by expanding (with-slots (a b) instance . body)
;; into (let ((a (slot-value instance 'a)) (b (slot-value instance 'b)))
;;        body...)
;; — read-only. A future stage can promote to symbol-macros.

(defmacro with-slots (slot-bindings instance-form &rest body)
  (let ((inst-var (gensym "INSTANCE-")))
    `(let ((,inst-var ,instance-form))
       (let ,(mapcar
              (lambda (b)
                (cond
                  ((symbolp b)
                   `(,b (slot-value ,inst-var ',b)))
                  (t
                   `(,(car b) (slot-value ,inst-var ',(cadr b))))))
              slot-bindings)
         ,@body))))

(defmacro with-accessors (accessor-bindings instance-form &rest body)
  "Read-only flavour — bind each name to the call of its accessor
   function on INSTANCE-FORM."
  (let ((inst-var (gensym "INSTANCE-")))
    `(let ((,inst-var ,instance-form))
       (let ,(mapcar
              (lambda (b)
                `(,(car b) (,(cadr b) ,inst-var)))
              accessor-bindings)
         ,@body))))

;; ─── Stage E: defgeneric + dispatch skeleton ───────────────────────────────
;;
;; Generic functions are CLOS instances of standard-generic-function
;; that carry a method list, a method-class, and a *discriminating
;; function* — the actual Lisp function that gets stored in the
;; symbol's function cell so plain `(gf x y)` calls dispatch
;; through it.
;;
;; This stage builds the scaffolding but doesn't yet install any
;; primary methods. defmethod (stage F) registers methods;
;; finalize-generic-function recomputes the discriminating function
;; whenever the method set changes.
;;
;; The dispatch path looks like:
;;
;;   (my-gf a b)
;;     → (symbol-function 'my-gf) === gf's discriminating function
;;     → (lambda (&rest args)
;;          (let* ((classes (mapcar #'class-of (required-portion args)))
;;                 (emf (or (find-method-table-method table classes)
;;                          (slow-method-lookup ...))))
;;            (funcall emf args)))
;;
;; The classes-to-emf-table is the per-gf cache; on a miss
;; slow-method-lookup runs compute-applicable-methods-using-classes
;; + std-compute-effective-method-function, then primes the cache.

;; -- analyze-lambda-list ---------------------------------------------------
;;
;; Tedious but boring. Reused by defgeneric and defmethod to extract
;; required-names / required-args / specializers / rest-var / keys /
;; optionals / aux / allow-other-keys from a (possibly specialized)
;; lambda list.

(defun %make-keyword (sym)
  ;; intern in :keyword. Our intern shim accepts (symbol-name pkg-name).
  (intern (symbol-name sym) "KEYWORD"))

(defun %get-keyword-from-arg (arg)
  (cond
    ((listp arg)
     (cond
       ((listp (car arg)) (caar arg))
       (t (%make-keyword (car arg)))))
    (t (%make-keyword arg))))

(defun analyze-lambda-list (lambda-list)
  (let ((keys nil)
        (key-args nil)
        (required-names nil)
        (required-args nil)
        (specializers nil)
        (rest-var nil)
        (optionals nil)
        (auxs nil)
        (allow-other-keys nil)
        (state ':parsing-required))
    (dolist (arg lambda-list)
      (cond
        ((eq arg '&optional) (setq state ':parsing-optional))
        ((eq arg '&rest)     (setq state ':parsing-rest))
        ((eq arg '&key)      (setq state ':parsing-key))
        ((eq arg '&allow-other-keys) (setq allow-other-keys 't))
        ((eq arg '&aux)      (setq state ':parsing-aux))
        (t
         (case state
           (:parsing-required
            (push-on-end arg required-args)
            (cond
              ((listp arg)
               (push-on-end (car arg) required-names)
               (push-on-end (cadr arg) specializers))
              (t
               (push-on-end arg required-names)
               (push-on-end 't specializers))))
           (:parsing-optional (push-on-end arg optionals))
           (:parsing-rest     (setq rest-var arg))
           (:parsing-key
            (push-on-end (%get-keyword-from-arg arg) keys)
            (push-on-end arg key-args))
           (:parsing-aux      (push-on-end arg auxs))))))
    (list :required-names required-names
          :required-args required-args
          :specializers specializers
          :rest-var rest-var
          :keywords keys
          :key-args key-args
          :auxiliary-args auxs
          :optional-args optionals
          :allow-other-keys allow-other-keys)))

;; -- Generic-function metaclass --------------------------------------------

(defclass generic-function (metaobject) ())

(defclass standard-generic-function (generic-function)
  ((lambda-list :initarg :lambda-list)
   (required-args :initarg :required-args)
   (methods :initform nil)
   (method-class :initarg :method-class)
   (discriminating-function)
   (classes-to-emf-table)
   (method-combination :initarg :method-combination :initform 'standard)
   (method-combination-order :initform ':most-specific-first)))

(defparameter the-class-gf          (find-class 'generic-function))
(defparameter the-class-standard-gf (find-class 'standard-generic-function))

;; -- Generic-function slot accessors ---------------------------------------

(defun generic-function-name (gf) (slot-value gf 'name))
(defun %setf-generic-function-name (new-value gf)
  (setf (slot-value gf 'name) new-value))

(defun generic-function-lambda-list (gf) (slot-value gf 'lambda-list))
(defun %setf-generic-function-lambda-list (new-value gf)
  (setf (slot-value gf 'lambda-list) new-value)
  (setf (slot-value gf 'required-args)
        (getf (analyze-lambda-list new-value) ':required-args))
  new-value)

(defun generic-function-required-args (gf)
  (slot-value gf 'required-args))
(defun %setf-generic-function-required-args (new-value gf)
  (setf (slot-value gf 'required-args) new-value))

(defun generic-function-methods (gf) (slot-value gf 'methods))
(defun %setf-generic-function-methods (new-value gf)
  (setf (slot-value gf 'methods) new-value))

(defun generic-function-method-class (gf) (slot-value gf 'method-class))
(defun %setf-generic-function-method-class (new-value gf)
  (setf (slot-value gf 'method-class) new-value))

(defun generic-function-discriminating-function (gf)
  (slot-value gf 'discriminating-function))
(defun %setf-generic-function-discriminating-function (new-value gf)
  (setf (slot-value gf 'discriminating-function) new-value))

(defun classes-to-emf-table (gf) (slot-value gf 'classes-to-emf-table))
(defun %setf-classes-to-emf-table (new-value gf)
  (setf (slot-value gf 'classes-to-emf-table) new-value))

(defun num-required-args (gf)
  (length (generic-function-required-args gf)))

;; -- standard-generic-function predicate (promoted) ------------------------

(defun standard-generic-function-p (x)
  (and (clos-instance-p x)
       (eq (class-of x) the-class-standard-gf)))

;; -- find-generic-function ------------------------------------------------

(defun find-generic-function (name &optional (errorp t))
  (let ((gf (gethash name *clos-generic-function-table*)))
    (cond
      (gf gf)
      (errorp (error "No generic function named ~A." name))
      (t nil))))

(defun %setf-find-generic-function (gf name)
  (setf (gethash name *clos-generic-function-table*) gf)
  gf)

;; -- make-instance-standard-generic-function -------------------------------

(defun make-instance-standard-generic-function (gf-class &key name documentation
                                                              lambda-list
                                                              method-class
                                                              (method-combination 'standard)
                                                              (method-combination-order ':most-specific-first)
                                                         &allow-other-keys)
  (declare (ignore gf-class))
  (let ((gf (std-allocate-instance the-class-standard-gf)))
    (setf (generic-function-name gf) name)
    (setf (slot-value gf 'documentation) documentation)
    (setf (generic-function-lambda-list gf) lambda-list)
    (setf (generic-function-methods gf) nil)
    (setf (generic-function-method-class gf) method-class)
    (setf (classes-to-emf-table gf) (make-method-table))
    (setf (slot-value gf 'method-combination) method-combination)
    (setf (slot-value gf 'method-combination-order) method-combination-order)
    (finalize-generic-function gf)
    gf))

;; -- ensure-generic-function -----------------------------------------------

(defparameter %ensure-gf-no-method-class (list 'no-method-class))

(defun ensure-generic-function (name &rest all-keys
                                &key
                                (generic-function-class the-class-standard-gf)
                                (method-class %ensure-gf-no-method-class)
                                lambda-list
                                &allow-other-keys)
  (declare (ignore generic-function-class))
  (let ((existing (find-generic-function name nil)))
    (cond
      (existing existing)
      (t
       (let* ((mc (cond ((eq method-class %ensure-gf-no-method-class) nil)
                        (t method-class)))
              (req (getf (analyze-lambda-list lambda-list) ':required-args))
              (gf (apply #'make-instance-standard-generic-function
                         the-class-standard-gf
                         :name name
                         :method-class mc
                         :required-args req
                         all-keys)))
         (setf (find-generic-function name) gf)
         ;; Install discriminating function as the symbol's
         ;; function so `(name arg ...)` dispatches through it.
         (when (symbolp name)
           (setf (symbol-function name)
                 (generic-function-discriminating-function gf)))
         gf)))))

;; -- finalize-generic-function --------------------------------------------
;;
;; Rebuilds the discriminating function whenever the method set
;; changes. Also clears the per-gf classes-to-emf cache. The
;; new discriminating function picks up the latest method list
;; via closure-over `gf`.

(defun finalize-generic-function (gf)
  (setf (generic-function-discriminating-function gf)
        (std-compute-discriminating-function gf))
  (clear-method-table (classes-to-emf-table gf))
  (when (symbolp (generic-function-name gf))
    (setf (symbol-function (generic-function-name gf))
          (generic-function-discriminating-function gf)))
  nil)

(defun std-compute-discriminating-function (gf)
  ;; Stage E version: no method dispatch yet. The lambda just
  ;; errors with "no applicable methods" — Stage F replaces this
  ;; with one that actually consults the method list.
  (lambda (&rest args)
    (declare (ignore args))
    (error "No applicable method for generic function ~A."
           (generic-function-name gf))))

;; -- defgeneric macro ------------------------------------------------------
;;
;; Surface syntax:
;;   (defgeneric foo (a b) [options...])
;;
;; Options include :method-class, :documentation, :method
;; (each :method becomes a defmethod — stage F handles them).
;; For Stage E we just accept and discard :method options.

(defun canonicalize-defgeneric-option (option)
  (case (car option)
    (:generic-function-class
     (list ':generic-function-class `(find-class ',(cadr option))))
    (:method-class
     (list ':method-class `(find-class ',(cadr option))))
    (:method-combination
     (list ':method-combination `',(cadr option)))
    (:documentation
     (list ':documentation `',(cadr option)))
    (otherwise
     (list `',(car option) `',(cadr option)))))

(defun canonicalize-defgeneric-options (options)
  (mapappend #'canonicalize-defgeneric-option options))

(defmacro defgeneric (function-name lambda-list &rest options)
  (let ((non-method-options
         (remove-if (lambda (o) (eq (car o) ':method)) options))
        (method-forms
         (mapcar (lambda (o) `(defmethod ,function-name ,@(cdr o)))
                 (remove-if-not (lambda (o) (eq (car o) ':method)) options))))
    `(progn
       (ensure-generic-function ',function-name
                                :lambda-list ',lambda-list
                                ,@(canonicalize-defgeneric-options
                                   non-method-options))
       ,@method-forms
       ',function-name)))

;; ─── Stage F: defmethod + method classes ───────────────────────────────────
;;
;; Methods are CLOS instances of standard-method. Each method
;; carries a compiled "method function" of shape
;;   (lambda (args next-emfun) ...)
;; that takes the GF's full argument list and the next method's
;; effective-method-function. Stage F handles primary-only
;; dispatch; Stage G layers in :before / :after / :around.

;; -- Method classes --------------------------------------------------------

(defclass method (metaobject) ())

(defclass standard-method (method)
  ((qualifiers   :initarg :qualifiers)
   (lambda-list  :initarg :lambda-list)
   (specializers :initarg :specializers)
   (body         :initarg :body)
   (generic-function :initform nil)
   (function)))

(defparameter the-class-method          (find-class 'method))
(defparameter the-class-standard-method (find-class 'standard-method))

;; -- Method accessors ------------------------------------------------------

(defun method-qualifiers   (m) (slot-value m 'qualifiers))
(defun %setf-method-qualifiers   (v m) (setf (slot-value m 'qualifiers) v))
(defun method-lambda-list  (m) (slot-value m 'lambda-list))
(defun %setf-method-lambda-list  (v m) (setf (slot-value m 'lambda-list) v))
(defun method-specializers (m) (slot-value m 'specializers))
(defun %setf-method-specializers (v m) (setf (slot-value m 'specializers) v))
(defun method-body         (m) (slot-value m 'body))
(defun %setf-method-body         (v m) (setf (slot-value m 'body) v))
(defun method-generic-function (m) (slot-value m 'generic-function))
(defun %setf-method-generic-function (v m)
  (setf (slot-value m 'generic-function) v))
(defun method-function     (m) (slot-value m 'function))
(defun %setf-method-function     (v m) (setf (slot-value m 'function) v))

;; -- defmethod parsing -----------------------------------------------------

(defun extract-lambda-list (specialized-lambda-list)
  (let* ((plist (analyze-lambda-list specialized-lambda-list))
         (requireds (getf plist ':required-names))
         (rv        (getf plist ':rest-var))
         (ks        (getf plist ':key-args))
         (aok       (getf plist ':allow-other-keys))
         (opts      (getf plist ':optional-args))
         (auxs      (getf plist ':auxiliary-args))
         (result requireds))
    (when opts (setq result (append result (cons '&optional opts))))
    (when rv   (setq result (append result (list '&rest rv))))
    (when (or ks aok) (setq result (append result (cons '&key ks))))
    (when aok  (setq result (append result (list '&allow-other-keys))))
    (when auxs (setq result (append result (cons '&aux auxs))))
    result))

(defun extract-specializers (specialized-lambda-list)
  (getf (analyze-lambda-list specialized-lambda-list) ':specializers))

(defun parse-defmethod (args)
  "ARGS = (fn-spec [qualifier...] specialized-lambda-list . body)
   Returns a list (fn-spec qualifiers lambda-list specializers body)."
  (let ((fn-spec (car args))
        (rest (cdr args))
        (qualifiers nil)
        (slist nil)
        (body nil))
    ;; Collect qualifiers (non-list atoms before the lambda list).
    (loop
      (cond
        ((null rest) (return nil))
        ((and (atom (car rest)) (not (null (car rest))))
         (setq qualifiers (append qualifiers (list (car rest))))
         (setq rest (cdr rest)))
        (t (return nil))))
    (setq slist (car rest))
    (setq body (cdr rest))
    (list fn-spec
          qualifiers
          (extract-lambda-list slist)
          (extract-specializers slist)
          body)))

;; -- defmethod macro -------------------------------------------------------

(defun canonicalize-specializer (spec)
  ;; EQL specializers land in stage H.
  (cond
    ((and (listp spec) (eq (car spec) 'eql))
     `(intern-eql-specializer ,(cadr spec)))
    (t `(find-class ',spec))))

(defun canonicalize-specializers (specs)
  `(list ,@(mapcar #'canonicalize-specializer specs)))

(defmacro defmethod (&rest args)
  (let* ((parsed (parse-defmethod args))
         (fn-name     (car parsed))
         (qualifiers  (cadr parsed))
         (lambda-list (caddr parsed))
         (specs       (cadddr parsed))
         (body        (car (cddddr parsed))))
    `(ensure-method (find-generic-function ',fn-name nil)
                    :generic-function-name ',fn-name
                    :lambda-list ',lambda-list
                    :qualifiers ',qualifiers
                    :specializers ,(canonicalize-specializers specs)
                    :body ',body)))

;; -- make-instance-standard-method ----------------------------------------

(defun make-instance-standard-method (method-class &key lambda-list qualifiers
                                                        specializers body
                                                   &allow-other-keys)
  (declare (ignore method-class))
  (let ((method (std-allocate-instance the-class-standard-method)))
    (setf (slot-value method 'name) nil)
    (setf (slot-value method 'documentation) nil)
    (setf (method-lambda-list  method) lambda-list)
    (setf (method-qualifiers   method) qualifiers)
    (setf (method-specializers method) specializers)
    (setf (method-body         method) body)
    (setf (method-generic-function method) nil)
    (setf (method-function method) (std-compute-method-function method))
    method))

;; -- ensure-method / add-method / remove-method ---------------------------

(defun ensure-method (gf &rest all-keys
                      &key generic-function-name lambda-list
                      &allow-other-keys)
  (declare (ignore lambda-list))
  ;; If gf doesn't exist yet, create one on the fly using the
  ;; method's lambda-list. This is the path defgeneric-less
  ;; defmethods take.
  (let ((real-gf
         (cond
           ((null gf)
            (ensure-generic-function
             generic-function-name
             :lambda-list (getf all-keys ':lambda-list)))
           (t gf))))
    (let ((method (apply #'make-instance-standard-method
                         the-class-standard-method
                         all-keys)))
      (setf (slot-value method 'name) generic-function-name)
      (add-method real-gf method)
      method)))

(defun add-method (gf method)
  ;; Remove any existing method with the same qualifiers+specializers.
  (let ((old (find-method gf (method-qualifiers method)
                          (method-specializers method) nil)))
    (when old (remove-method gf old)))
  (setf (method-generic-function method) gf)
  (setf (generic-function-methods gf)
        (cons method (generic-function-methods gf)))
  (dolist (spec (method-specializers method))
    (let ((existing (class-direct-methods spec)))
      (unless (member method existing)
        (setf (class-direct-methods spec) (cons method existing)))))
  (finalize-generic-function gf)
  gf)

(defun remove-method (gf method)
  (setf (generic-function-methods gf)
        (remove method (generic-function-methods gf)))
  (setf (method-generic-function method) nil)
  (dolist (spec (method-specializers method))
    (setf (class-direct-methods spec)
          (remove method (class-direct-methods spec))))
  (finalize-generic-function gf)
  gf)

(defun find-method (gf qualifiers specializers &optional (errorp t))
  (let ((m (find-if (lambda (mm)
                      (and (equal qualifiers (method-qualifiers mm))
                           (equal specializers (method-specializers mm))))
                    (generic-function-methods gf))))
    (cond
      ((null m)
       (cond (errorp (error "No such method for ~A." (generic-function-name gf)))
             (t nil)))
      (t m))))

;; -- compute-method-function ----------------------------------------------
;;
;; Stage F: simplest shape. Method body wraps in a block named
;; after the gf so RETURN-FROM works, then APPLY'd to args.
;; next-emfun is bound but unused — Stage G consults it for
;; call-next-method.

(defun std-compute-method-function (method)
  (let* ((lambda-list (method-lambda-list method))
         (body (method-body method))
         (gf-name (slot-value method 'name))
         (blk (cond ((symbolp gf-name) gf-name)
                    (t (gensym "METHOD-")))))
    ;; Always wrap with flet of call-next-method/next-method-p so
    ;; method bodies can use them. The flet bindings are inert if
    ;; unreferenced. Closette gates this on a search-tree of the
    ;; macroexpanded body to skip the flet for bodies that don't
    ;; mention CNM — purely a perf optimisation.
    (compile nil
             `(lambda (args next-emfun)
                (flet ((call-next-method (&rest cnm-args)
                         (cond
                           ((null next-emfun)
                            (error "No next method for generic-function call."))
                           (t (funcall next-emfun
                                       (cond (cnm-args cnm-args)
                                             (t args))))))
                       (next-method-p ()
                         (cond ((null next-emfun) nil)
                               (t t))))
                  (apply (lambda ,lambda-list
                           (block ,blk ,@body))
                         args))))))

;; -- compute-applicable-methods-using-classes -----------------------------

(defun %every-pair-subclassp (classes specs)
  (cond
    ((and (null classes) (null specs)) t)
    ((or (null classes) (null specs)) nil)
    ((subclassp (car classes) (car specs))
     (%every-pair-subclassp (cdr classes) (cdr specs)))
    (t nil)))

(defun compute-applicable-methods-using-classes (gf required-classes)
  (let ((applicable
         (remove-if-not (lambda (method)
                          (%every-pair-subclassp
                           required-classes
                           (method-specializers method)))
                        (generic-function-methods gf))))
    (sort applicable
          (lambda (m1 m2)
            (std-method-more-specific-p gf m1 m2 required-classes)))))

;; -- method-more-specific-p ----------------------------------------------

(defun std-method-more-specific-p (gf m1 m2 required-classes)
  (declare (ignore gf))
  (let ((s1 (method-specializers m1))
        (s2 (method-specializers m2))
        (cs required-classes)
        (result nil)
        (done nil))
    (loop
      (cond
        (done (return result))
        ((or (null s1) (null s2) (null cs)) (return nil))
        (t
         (let ((sp1 (car s1)) (sp2 (car s2)) (cl (car cs)))
           (cond
             ((eq sp1 sp2)
              (setq s1 (cdr s1)) (setq s2 (cdr s2)) (setq cs (cdr cs)))
             (t
              (setq result (sub-specializer-p sp1 sp2 cl))
              (setq done t)))))))))

;; -- effective-method-function --------------------------------------------

(defun primary-method-p (m) (null (method-qualifiers m)))
(defun before-method-p  (m) (equal '(:before) (method-qualifiers m)))
(defun after-method-p   (m) (equal '(:after)  (method-qualifiers m)))
(defun around-method-p  (m) (equal '(:around) (method-qualifiers m)))

(defun compute-primary-emfun (methods)
  (cond
    ((null methods) nil)
    (t (let ((next (compute-primary-emfun (cdr methods))))
         (lambda (args)
           (funcall (method-function (car methods)) args next))))))

(defun std-compute-effective-method-function (gf methods)
  (let ((primaries (remove-if-not #'primary-method-p methods))
        (around    (find-if #'around-method-p methods)))
    (cond
      ((null primaries)
       (error "No primary methods for this generic-function call."))
      (around
       ;; The around method runs first; its body decides whether
       ;; to call-next-method, which delegates to the rest of the
       ;; method chain (befores → primary chain → afters).
       (let ((next-emfun
              (std-compute-effective-method-function
               gf (remove around methods))))
         (lambda (args)
           (funcall (method-function around) args next-emfun))))
      (t
       (let ((primary-chain (compute-primary-emfun (cdr primaries)))
             (most-specific-primary (car primaries))
             (befores (remove-if-not #'before-method-p methods))
             ;; CLOS spec: :after methods run from least- to
             ;; most-specific (reverse of normal precedence).
             (reverse-afters
              (reverse (remove-if-not #'after-method-p methods))))
         (lambda (args)
           (dolist (b befores)
             (funcall (method-function b) args nil))
           (let ((result (funcall (method-function most-specific-primary)
                                  args primary-chain)))
             (dolist (a reverse-afters)
               (funcall (method-function a) args nil))
             result)))))))

;; -- discriminating function (real version) ------------------------------
;;
;; Replaces the Stage E stub. The closure captures `gf` and the
;; classes-to-emf-table; each call computes the required-portion's
;; classes, consults the cache, and falls through to
;; slow-method-lookup on miss. The emf takes the FULL arg list and
;; returns the result.

(defun %first-n (lst n)
  (cond
    ((or (zerop n) (null lst)) nil)
    (t (cons (car lst) (%first-n (cdr lst) (- n 1))))))

(defun slow-method-lookup (gf table classes args)
  (declare (ignore args))
  (let ((methods (compute-applicable-methods-using-classes gf classes)))
    (cond
      ((null methods)
       (error "No applicable method for ~A on classes ~A"
              (generic-function-name gf)
              (mapcar #'class-name classes)))
      (t
       (let ((emf (std-compute-effective-method-function gf methods)))
         (add-method-table-method table classes emf)
         emf)))))

;; Replace the stage-E stub:
(defun std-compute-discriminating-function (gf)
  (lambda (&rest args)
    (let* ((table   (classes-to-emf-table gf))
           (num     (num-required-args gf))
           (req     (%first-n args num))
           (classes (mapcar #'class-of req)))
      (let ((emf (or (find-method-table-method table classes)
                     (slow-method-lookup gf table classes args))))
        (funcall emf args)))))

;; Re-finalize every GF that was created in Stage E with the stub
;; discriminator so existing GFs pick up the new dispatch path.
(maphash (lambda (name gf)
           (declare (ignore name))
           (finalize-generic-function gf))
         *clos-generic-function-table*)

;; ─── Stage H: EQL specializers ─────────────────────────────────────────────
;;
;; Method specializers can be `(eql obj)` instead of a class. Such
;; methods only apply when their argument is EQL to the captured
;; object. EQL specializers are MORE SPECIFIC than any class
;; specializer for the matching object.
;;
;; Closette stores singleton specializers in
;; *clos-singleton-specializers* (declared in stage A). Each EQL
;; spec is a CLOS instance of class EQL-SPECIALIZER, with an
;; `object` slot holding the eql target.

(defclass eql-specializer (specializer)
  ((object :initarg :object)))

(defparameter the-class-eql-specializer (find-class 'eql-specializer))

(defun eql-specializer-p (x)
  (and (clos-instance-p x)
       (eq (class-of x) the-class-eql-specializer)))

(defun eql-specializer-object (s) (slot-value s 'object))

(defun intern-eql-specializer (obj)
  "Find or create the canonical EQL specializer for OBJ. Two
   defmethods that name `(eql 'foo)` get the SAME spec instance,
   which is what the dispatch code keys on."
  (let ((existing (gethash obj *clos-singleton-specializers*)))
    (cond
      (existing existing)
      (t
       (let ((s (std-allocate-instance the-class-eql-specializer)))
         ;; Populate the inherited specializer/metaobject slots so
         ;; class-direct-methods (which add-method writes to)
         ;; doesn't blow up. EQL specs aren't real classes — they
         ;; have no superclasses or CPL beyond themselves.
         (setf (slot-value s 'name) nil)
         (setf (slot-value s 'documentation) nil)
         (setf (slot-value s 'direct-superclasses) nil)
         (setf (slot-value s 'class-precedence-list) (list s))
         (setf (slot-value s 'direct-methods) nil)
         (setf (slot-value s 'direct-subclasses) nil)
         (setf (slot-value s 'object) obj)
         (setf (gethash obj *clos-singleton-specializers*) s)
         s)))))

;; -- EQL-aware dispatch ---------------------------------------------------
;;
;; subclassp / sub-specializer-p / compute-applicable-methods need
;; to be EQL-aware. We patch them in place rather than splitting
;; into "with-classes" and "with-args" variants.

(defun %collect-eql-targets (gf)
  "Walk GF's methods and return every EQL specializer that
   appears. Used by the discriminating function to decide
   whether each argument should match an EQL spec."
  (let ((result nil))
    (dolist (m (generic-function-methods gf))
      (dolist (s (method-specializers m))
        (when (eql-specializer-p s)
          (unless (member s result)
            (setq result (cons s result))))))
    result))

(defun %dispatch-key-for-arg (arg eql-targets)
  ;; If ARG eql's any captured EQL target, the dispatch key is
  ;; that spec; otherwise (class-of arg).
  (let ((spec (find-if (lambda (s) (eql arg (eql-specializer-object s)))
                       eql-targets)))
    (cond (spec spec) (t (class-of arg)))))

(defun %method-applicable-p (method args)
  "Walk METHOD's specializers + ARGS in lockstep. Each pair must
   match: EQL spec ↔ arg eql target; class spec ↔ class-of arg
   subclassp class spec."
  (let ((specs (method-specializers method))
        (a args))
    (block done
      (loop
        (cond
          ((null specs) (return-from done t))
          ((null a) (return-from done nil))
          ((eql-specializer-p (car specs))
           (cond
             ((eql (car a) (eql-specializer-object (car specs)))
              (setq specs (cdr specs)) (setq a (cdr a)))
             (t (return-from done nil))))
          (t
           (cond
             ((subclassp (class-of (car a)) (car specs))
              (setq specs (cdr specs)) (setq a (cdr a)))
             (t (return-from done nil)))))))))

(defun %sub-spec-p (sp1 sp2 arg)
  "True if SP1 is more specific than SP2 for ARG. EQL spec is
   always more specific than class. Otherwise class spec uses
   sub-specializer-p w.r.t. the arg's class-of."
  (cond
    ((eql-specializer-p sp1) t)
    ((eql-specializer-p sp2) nil)
    (t (sub-specializer-p sp1 sp2 (class-of arg)))))

(defun %method-more-specific-p (m1 m2 args)
  (let ((s1 (method-specializers m1))
        (s2 (method-specializers m2))
        (a args))
    (block done
      (loop
        (cond
          ((or (null s1) (null s2) (null a)) (return-from done nil))
          ((eq (car s1) (car s2))
           (setq s1 (cdr s1)) (setq s2 (cdr s2)) (setq a (cdr a)))
          (t (return-from done (%sub-spec-p (car s1) (car s2) (car a)))))))))

(defun compute-applicable-methods (gf args)
  (let ((applicable
         (remove-if-not (lambda (m) (%method-applicable-p m args))
                        (generic-function-methods gf))))
    (sort applicable
          (lambda (m1 m2) (%method-more-specific-p m1 m2 args)))))

;; Re-define the discriminating function to be EQL-aware. The
;; cache key per arg is the EQL spec it matches (if any), else
;; (class-of arg) — that key list is what the per-gf method
;; table is indexed by.

(defun std-compute-discriminating-function (gf)
  (lambda (&rest args)
    (let* ((table       (classes-to-emf-table gf))
           (num         (num-required-args gf))
           (req         (%first-n args num))
           (eql-targets (%collect-eql-targets gf))
           (keys (cond
                   (eql-targets
                    (mapcar (lambda (a) (%dispatch-key-for-arg a eql-targets))
                            req))
                   (t (mapcar #'class-of req)))))
      (let ((cached (find-method-table-method table keys)))
        (cond
          (cached (funcall cached args))
          (t
           (let* ((methods (compute-applicable-methods gf req))
                  (emf (cond
                         ((null methods)
                          (error "No applicable method for ~A on ~A"
                                 (generic-function-name gf) keys))
                         (t
                          (std-compute-effective-method-function gf methods)))))
             (add-method-table-method table keys emf)
             (funcall emf args))))))))

;; Re-finalize every GF so existing ones pick up EQL-aware
;; dispatch.
(maphash (lambda (name gf)
           (declare (ignore name))
           (finalize-generic-function gf))
         *clos-generic-function-table*)

;; ─── Stage I: polish — accessors, print-object, typep ──────────────────────
;;
;; Final stage. Brings the user-facing surface area in line with
;; what most CLOS code expects:
;;
;;   * defclass slot specs with :reader / :writer / :accessor
;;     now actually generate generic-function reader and writer
;;     methods (Stage C accepted the syntax and silently dropped
;;     them).
;;   * print-object becomes a real generic function — defining a
;;     method on it for your class controls how (format nil "~A"
;;     instance) prints.
;;   * typep gets a CLOS-aware front door — (typep obj 'class-name)
;;     tests subclassp on the instance's class.
;;   * describe-object dumps slot values for diagnosis.

;; -- Reader / writer / accessor generation --------------------------------

(defun add-reader-method (class fn-name slot-name)
  (ensure-method
   (ensure-generic-function fn-name :lambda-list '(object))
   :generic-function-name fn-name
   :lambda-list '(object)
   :qualifiers nil
   :specializers (list class)
   :body `((slot-value object ',slot-name))))

(defun %setter-name (fn-name)
  ;; Map an accessor name like FOO into the %SETF-FOO symbol used
  ;; by the generic-setf-fallback. We don't yet have symbol-name,
  ;; so we round-trip the symbol through format ~A which prints it
  ;; as its bare name.
  (intern (format nil "%SETF-~A" fn-name)))

(defun add-writer-method (class fn-name slot-name)
  ;; Writer in Closette is `(setf NAME)` shape. We adapt to the
  ;; generic-setf-fallback convention: a name like FOO becomes
  ;; %SETF-FOO. (setf (foo x) val) lowers to (%setf-foo val x),
  ;; which is then a regular generic-function call.
  (let ((setter-name (cond
                       ((symbolp fn-name) (%setter-name fn-name))
                       ;; If user wrote `(setf foo)` directly,
                       ;; canonicalize to %SETF-FOO too.
                       ((and (listp fn-name) (eq (car fn-name) 'setf))
                        (%setter-name (cadr fn-name)))
                       (t fn-name))))
    (ensure-method
     (ensure-generic-function setter-name
                              :lambda-list '(new-value object))
     :generic-function-name setter-name
     :lambda-list '(new-value object)
     :qualifiers nil
     :specializers (list (find-class 't) class)
     :body `((setf (slot-value object ',slot-name) new-value)))))

(defun %install-direct-slot-accessors (class)
  "Walk CLASS's direct slots; for each slot, register every
   :reader / :writer (the latter possibly via :accessor)."
  (dolist (slot (class-direct-slots class))
    (let ((slot-name (slot-definition-name slot)))
      (dolist (reader (slot-definition-readers slot))
        (add-reader-method class reader slot-name))
      (dolist (writer (slot-definition-writers slot))
        (add-writer-method class writer slot-name)))))

;; Hook into make-instance-standard-class so every defclass
;; auto-generates accessors going forward. Saves the prior
;; definition into a global, redefines through it. (We can't
;; (let ((prev #'...)) (defun ...)) because our compiler bans
;; defun inside let.)
(defparameter %prev-make-instance-standard-class
  (symbol-function 'make-instance-standard-class))

(defun make-instance-standard-class (metaclass &rest all-keys
                                               &key &allow-other-keys)
  (let ((class (apply %prev-make-instance-standard-class
                      metaclass all-keys)))
    (%install-direct-slot-accessors class)
    class))

;; -- print-object generic -------------------------------------------------
;;
;; Default method prints the same thing the runtime printer does
;; (#<CLASSNAME>). User code can specialize per class to customise.
;; We don't yet wire this into the runtime printer — that needs
;; UTF-8 string-back-into-rust plumbing — so for now it's a Lisp-
;; level convenience.

(defgeneric print-object (object stream))

(defmethod print-object ((object t) stream)
  (format stream "#<~A>"
          (cond
            ((clos-instance-p object)
             (class-name (class-of object)))
            (t (class-name (built-in-class-of object))))))

(defun describe-object (object &optional (stream t))
  "Print OBJECT plus, for CLOS instances, every effective slot
   and its current value (or '<unbound>')."
  (cond
    ((clos-instance-p object)
     (let ((class (class-of object)))
       (format stream "~A is an instance of class ~A.~%"
               object (class-name class))
       (format stream "Slots:~%")
       (dolist (slot (class-effective-slots class))
         (let ((name (slot-definition-name slot)))
           (cond
             ((slot-boundp object name)
              (format stream "  ~A: ~A~%" name (slot-value object name)))
             (t
              (format stream "  ~A: <unbound>~%" name)))))))
    (t (format stream "~A is a ~A.~%"
               object (class-name (class-of object)))))
  object)

;; -- CLOS-aware typep -----------------------------------------------------
;;
;; The runtime typep handles primitives. We layer on top: if TYPE
;; is a CLOS class name, defer to subclassp on the object's
;; class. We can't redefine the existing typep (it's a special
;; form / native) so we install a Lisp wrapper named CLOS-TYPEP.
;; User code that wants the CLOS behavior calls CLOS-TYPEP
;; explicitly. (Future stage: have the runtime typep call CLOS-
;; TYPEP for non-primitive type names.)

(defun clos-typep (object type)
  (cond
    ((symbolp type)
     (let ((c (find-class type nil)))
       (cond
         (c (subclassp (class-of object) c))
         (t (typep object type))))) ; fall through for primitives
    (t (typep object type))))

;; -- A few user-friendly shortcuts ---------------------------------------

(defun class-slots (class) (class-effective-slots class))

;; ─── Stage F: standard-GF promotion ────────────────────────────────────
;;
;; Promote the CL standard initializers from plain DEFUNs to proper
;; generic functions, with the existing body installed as the primary
;; method on (T …). Until this runs, a user-side
;; `(defmethod shared-initialize :after ...)` would replace the plain
;; defun with a fresh GF that had ONLY the user's :after method — no
;; primary — and `make-instance` would then loop in method dispatch
;; looking for a primary, blowing the stack (chapter 7 of the corman
;; ANSI hyperspec-examples triggered exactly that).
;;
;; The promotion must run AFTER both DEFGENERIC and DEFMETHOD are
;; defined; placing it at the end of clos.lisp guarantees that
;; ordering without weaving a "Stage F" comment block earlier.

;; Stash the plain-defun bodies under fresh symbols so the promoted
;; GFs can call them by name from inside primary methods. Storing
;; the function value in a defparameter and then referencing it via
;; `,VAR` inside a backquote-built method body fails — Function
;; words don't round-trip through the Value tree the lowerer
;; consumes. Names round-trip cleanly via QUOTE.
(defparameter %primary-shared-initialize
  (symbol-function 'shared-initialize))
(defparameter %primary-initialize-instance
  (symbol-function 'initialize-instance))
(defparameter %primary-reinitialize-instance
  (symbol-function 'reinitialize-instance))

(defun %primary-shared-initialize-call (instance slot-names &rest all-keys)
  (apply %primary-shared-initialize instance slot-names all-keys))
(defun %primary-initialize-instance-call (instance &rest all-keys)
  (apply %primary-initialize-instance instance all-keys))
(defun %primary-reinitialize-instance-call (instance &rest all-keys)
  (apply %primary-reinitialize-instance instance all-keys))

;; Drop the plain defuns so ENSURE-METHOD's GF-or-create path
;; doesn't see a leftover non-GF function value and refuse to
;; install the primary.
(fmakunbound 'shared-initialize)
(fmakunbound 'initialize-instance)
(fmakunbound 'reinitialize-instance)

(defgeneric shared-initialize (instance slot-names &rest all-keys))
(defgeneric initialize-instance (instance &rest all-keys))
(defgeneric reinitialize-instance (instance &rest all-keys))

(defmethod shared-initialize ((instance t) (slot-names t) &rest all-keys)
  (apply #'%primary-shared-initialize-call instance slot-names all-keys))

(defmethod initialize-instance ((instance t) &rest all-keys)
  (apply #'%primary-initialize-instance-call instance all-keys))

(defmethod reinitialize-instance ((instance t) &rest all-keys)
  (apply #'%primary-reinitialize-instance-call instance all-keys))

;; Return a printable sentinel.
nil
