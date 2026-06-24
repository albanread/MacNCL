;;;; adjustable-vector.lisp — fill-pointer / vector-push-extend support.
;;;;
;;;; NCL's `make-array` builds only fixed-size simple-vectors; the
;;;; :fill-pointer and :adjustable keywords are accepted but IGNORED, and
;;;; `vector-push-extend` / `fill-pointer` are undefined. This module
;;;; provides a portable adjustable 1-D vector (amortized O(1) growth, the
;;;; classic double-on-overflow scheme) for code that needs push-extend /
;;;; fill-pointer semantics — e.g. the minesweepers neuroevolution demo,
;;;; whose neural-net layers and GA chromosomes are grown with
;;;; `(make-array 0 :fill-pointer t)` + `vector-push-extend`.
;;;;
;;;; Representation: a defstruct holding a backing simple-vector (the
;;;; physical capacity) and a fill pointer (the logical length, always
;;;; <= capacity). Indexed access goes through `av-ref` / `av-length`,
;;;; which are bounds-checked and signal a *catchable* error — a bare
;;;; `aref` out of bounds aborts the worker thread, so the guard matters.
;;;;
;;;; IMPORTANT: do NOT call bare `aref`/`length`/`svref` on one of these
;;;; boxes — a defstruct is itself vector-backed in NCL, so those would
;;;; read the struct's raw cells, not the logical contents. Use the av-*
;;;; API throughout, and `adjustable-vector-p` (not `vectorp`) to test.

(defstruct adjustable-vector
  storage      ; a simple-vector of physical capacity
  fillptr)     ; logical length (<= capacity)

(defun av-make (&optional (capacity 8) (initial-element 0))
  "An empty adjustable vector (length 0) with backing CAPACITY."
  (make-adjustable-vector
   :storage (make-array (max 1 capacity) :initial-element initial-element)
   :fillptr 0))

(defun av-make-filled (n &optional (initial-element 0))
  "An adjustable vector of logical length N, every element INITIAL-ELEMENT.
   The drop-in for a fixed (make-array N) that must interoperate with av-*."
  (make-adjustable-vector
   :storage (make-array (max 1 n) :initial-element initial-element)
   :fillptr n))

(defun av-length (av) (adjustable-vector-fillptr av))
(defun av-capacity (av) (length (adjustable-vector-storage av)))

(defun av-ref (av i)
  (if (and (>= i 0) (< i (adjustable-vector-fillptr av)))
      (svref (adjustable-vector-storage av) i)
      (error "av-ref: index ~A out of bounds (length ~A)"
             i (adjustable-vector-fillptr av))))

(defun (setf av-ref) (val av i)
  (if (and (>= i 0) (< i (adjustable-vector-fillptr av)))
      (setf (svref (adjustable-vector-storage av) i) val)
      (error "(setf av-ref): index ~A out of bounds (length ~A)"
             i (adjustable-vector-fillptr av))))

(defun av-%grow (av min-capacity)
  "Grow backing storage to at least MIN-CAPACITY (doubling), copying the
   live prefix. Internal."
  (let* ((old (adjustable-vector-storage av))
         (oldcap (length old))
         (newcap (max min-capacity (* 2 oldcap)))
         (new (make-array newcap :initial-element 0)))
    (dotimes (i (adjustable-vector-fillptr av))
      (setf (svref new i) (svref old i)))
    (setf (adjustable-vector-storage av) new)
    av))

(defun av-push-extend (value av)
  "Append VALUE, growing storage as needed; returns its index.
   Argument order mirrors CL's (vector-push-extend value vector)."
  (let ((fp (adjustable-vector-fillptr av)))
    (when (>= fp (length (adjustable-vector-storage av)))
      (av-%grow av (1+ fp)))
    (setf (svref (adjustable-vector-storage av) fp) value)
    (setf (adjustable-vector-fillptr av) (1+ fp))
    fp))

(defun av-push (value av)
  "Append only if there is room (no grow); returns the index, or NIL."
  (let ((fp (adjustable-vector-fillptr av)))
    (when (< fp (length (adjustable-vector-storage av)))
      (setf (svref (adjustable-vector-storage av) fp) value)
      (setf (adjustable-vector-fillptr av) (1+ fp))
      fp)))

(defun av-pop (av)
  "Remove and return the last element."
  (let ((fp (1- (adjustable-vector-fillptr av))))
    (when (< fp 0) (error "av-pop: empty adjustable vector"))
    (setf (adjustable-vector-fillptr av) fp)
    (svref (adjustable-vector-storage av) fp)))

(defun av-set-fill (av n)
  "Set the fill pointer (logical length) to N, growing storage if N
   exceeds capacity. Equivalent to (setf (fill-pointer v) n)."
  (when (> n (length (adjustable-vector-storage av)))
    (av-%grow av n))
  (setf (adjustable-vector-fillptr av) n)
  n)

(defun list->av (list)
  "Build an adjustable vector from LIST (drop-in for (apply #'vector list))."
  (let ((av (av-make (max 1 (length list)))))
    (dolist (x list) (av-push-extend x av))
    av))

(defun av->list (av)
  "The logical contents of AV as a fresh list."
  (let ((out nil))
    (dotimes (i (adjustable-vector-fillptr av))
      (push (svref (adjustable-vector-storage av) i) out))
    (nreverse out)))

(defun av-sort (av predicate &key (key #'identity))
  "Sort AV in place by PREDICATE applied to KEY of each element; returns AV.
   Routes through the list sort — adequate for the small vectors here."
  (let ((sorted (sort (av->list av) predicate :key key))
        (i 0))
    (dolist (x sorted)
      (setf (svref (adjustable-vector-storage av) i) x)
      (setq i (+ i 1)))
    av))

(provide 'adjustable-vector)
