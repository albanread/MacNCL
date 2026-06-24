;;;; clos-tour.lisp — end-to-end tour of the Closette CLOS port.
;;;;
;;;; Exercises every major feature added across stages A–I of
;;;; the port:
;;;;
;;;;   * defclass with single + multiple inheritance
;;;;   * make-instance with initargs / initforms / default-initargs
;;;;   * slot-value, slot-boundp, slot-makunbound, with-slots
;;;;   * :reader / :writer / :accessor auto-generated methods
;;;;   * defgeneric + defmethod with class specializers
;;;;   * call-next-method + next-method-p
;;;;   * :before / :after / :around qualifiers
;;;;   * EQL specializers
;;;;   * print-object specialisation
;;;;   * describe-object
;;;;   * clos-typep + class-precedence-list + subclassp
;;;;
;;;; Usage:
;;;;   ncl --load Lisp/demos/clos-tour.lisp
;;;;
;;;; Or paste-by-section in the REPL.

(format t "~%========================================~%")
(format t "  CLOS tour — Closette port end-to-end~%")
(format t "========================================~%~%")

;; ── 1. Single inheritance + accessors ─────────────────────────────────────
(format t "--- 1. Single inheritance + accessors ---~%")

(defclass animal ()
  ((name :initarg :name :accessor name)
   (sound :initform "..." :accessor sound)))

(defclass dog (animal)
  ((sound :initform "Woof")))

(defclass puppy (dog)
  ((sound :initform "Yip")))

(defparameter d (make-instance 'dog :name "Rex"))
(defparameter pup (make-instance 'puppy :name "Buddy"))

(format t "  (name d) => ~A~%" (name d))
(format t "  (sound d) => ~A~%" (sound d))
(format t "  (name pup) => ~A~%" (name pup))
(format t "  (sound pup) => ~A~%" (sound pup))

(setf (name d) "Max")
(format t "  after rename: (name d) => ~A~%" (name d))

;; ── 2. Multiple inheritance + CPL ─────────────────────────────────────────
(format t "~%--- 2. Multiple inheritance + class-precedence-list ---~%")

(defclass swimmer () ())
(defclass runner () ())
(defclass triathlete (swimmer runner) ())

(format t "  CPL of triathlete: ~A~%"
        (mapcar #'class-name
                (class-precedence-list (find-class 'triathlete))))
(format t "  (clos-typep t-instance 'swimmer) => ~A~%"
        (clos-typep (make-instance 'triathlete) 'swimmer))
(format t "  (clos-typep t-instance 'runner)  => ~A~%"
        (clos-typep (make-instance 'triathlete) 'runner))

;; ── 3. Generic functions + specialised methods ────────────────────────────
(format t "~%--- 3. Generic functions + specialised methods ---~%")

(defgeneric speak (animal))

(defmethod speak ((a animal))
  (format nil "~A says ~A" (name a) (sound a)))

(defmethod speak ((a dog))
  ;; call-next-method to get the animal-level rendering, then
  ;; decorate it.
  (format nil "[bark] ~A" (call-next-method)))

(defmethod speak ((a puppy))
  ;; nested call-next-method walks puppy → dog → animal.
  (format nil "[yip!] ~A" (call-next-method)))

(format t "  (speak d)   => ~A~%" (speak d))
(format t "  (speak pup) => ~A~%" (speak pup))

;; ── 4. Multi-arg dispatch + specificity ──────────────────────────────────
(format t "~%--- 4. Multi-arg dispatch + specificity ---~%")

(defgeneric encounter (a b))
(defmethod encounter ((a t)      (b t))      "two strangers")
(defmethod encounter ((a animal) (b t))      "an animal sees a thing")
(defmethod encounter ((a t)      (b animal)) "a thing sees an animal")
(defmethod encounter ((a animal) (b animal)) "two animals meet")
(defmethod encounter ((a dog)    (b dog))    "tail-wagging")

(format t "  (encounter d pup) => ~A~%" (encounter d pup))
(format t "  (encounter d 42)  => ~A~%" (encounter d 42))
(format t "  (encounter 1 2)   => ~A~%" (encounter 1 2))

;; ── 5. :before / :after / :around qualifiers ─────────────────────────────
(format t "~%--- 5. :before / :after / :around qualifiers ---~%")

(defgeneric feed (animal))

(defmethod feed :before ((a animal))
  (format t "    [pre]   sniff the food~%"))

(defmethod feed ((a animal))
  (format t "    [prim]  eat~%")
  'fed)

(defmethod feed :after ((a animal))
  (format t "    [post]  lick lips~%"))

(defmethod feed :around ((a puppy))
  (format t "    [around] tip the bowl gently~%")
  (let ((r (call-next-method)))
    (format t "    [around] burp~%")
    r))

(format t "  feeding pup:~%")
(format t "  result => ~A~%" (feed pup))

;; ── 6. EQL specializers ──────────────────────────────────────────────────
(format t "~%--- 6. EQL specializers ---~%")

(defgeneric greet (whom))
(defmethod greet ((whom t))             (format nil "Hello, ~A" whom))
(defmethod greet ((whom (eql 'world)))  "Hello, world!")
(defmethod greet ((whom (eql 42)))      "Hello, the answer.")

(format t "  (greet 'world) => ~A~%" (greet 'world))
(format t "  (greet 42)     => ~A~%" (greet 42))
(format t "  (greet 'foo)   => ~A~%" (greet 'foo))
(format t "  (greet 7)      => ~A~%" (greet 7))

;; Recursive generic with EQL base case.
(defgeneric fact (n))
(defmethod fact ((n integer))   (* n (fact (- n 1))))
(defmethod fact ((n (eql 0)))   1)
(format t "  (fact 6) via (eql 0) base => ~A~%" (fact 6))

;; ── 7. print-object specialisation ───────────────────────────────────────
(format t "~%--- 7. print-object specialisation ---~%")

(defmethod print-object ((a animal) stream)
  (format stream "<~A:~A>" (class-name (class-of a)) (name a)))

(format t "  print-object on d: ")
(print-object d t)
(format t "~%  print-object on pup: ")
(print-object pup t)
(format t "~%")

;; ── 8. describe-object ───────────────────────────────────────────────────
(format t "~%--- 8. describe-object ---~%")
(describe-object d)

;; ── 9. with-slots ────────────────────────────────────────────────────────
(format t "~%--- 9. with-slots ---~%")
(format t "  combined: ~A~%"
        (with-slots (name sound) d
          (format nil "~A goes ~A" name sound)))

;; ── 10. slot-boundp / slot-makunbound ────────────────────────────────────
(format t "~%--- 10. slot-boundp / slot-makunbound ---~%")

(defclass box () ((contents :accessor contents)))   ; no initform
(defparameter b (make-instance 'box))
(format t "  fresh box: contents bound? => ~A~%" (slot-boundp b 'contents))
(setf (contents b) 'cookie)
(format t "  after setf: contents bound? => ~A   value: ~A~%"
        (slot-boundp b 'contents) (contents b))
(slot-makunbound b 'contents)
(format t "  after makunbound: bound? => ~A~%" (slot-boundp b 'contents))

(format t "~%========================================~%")
(format t "  Tour complete.~%")
(format t "========================================~%")

nil
