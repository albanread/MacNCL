;;;; Lisp/Library/describe.lisp — interactive inspection of values.
;;;;
;;;; `(describe obj)` prints a multi-line summary of an object — its
;;;; type, value, structural makeup, and (for symbols) the four
;;;; cells. The output is meant for humans at the REPL, not for
;;;; programs.
;;;;
;;;; Ported from Roger Corman's Sys/describe.lisp (J.P. Massar's
;;;; 2002 rewrite), simplified for what NCL exposes today:
;;;;
;;;;   * Type detection via the existing TYPEP-style predicates
;;;;     and the new TYPE-OF primitive.
;;;;   * Symbol cells via SYMBOL-NAME, SYMBOL-PACKAGE, BOUNDP,
;;;;     SYMBOL-VALUE, FBOUNDP, SYMBOL-FUNCTION.
;;;;   * Integer description includes hex/octal/binary.
;;;;   * Cons / list shows length and elements.
;;;;   * String shows length.
;;;;   * Character shows codepoint and any standard name.
;;;;
;;;; Corman's version uses CLOS-method dispatch via DESCRIBE-OBJECT
;;;; on each type. We use a plain DEFUN with COND because the only
;;;; value of method dispatch here would be user-extensibility, and
;;;; the long-form DEFINE-METHOD-COMBINATION isn't sturdy enough
;;;; today for users to layer on anyway.

;; All printers take a stream so describe can be redirected (the
;; CL `(describe obj &optional stream)` shape). Default is T (the
;; native stdout path); tests pass a string-output-stream to
;; capture.
(defun %desc-line (stream fmt &rest args)
  (apply #'format stream fmt args))

;; ── Per-type describers ──────────────────────────────────────────────

(defun %describe-fixnum (x stream)
  (%desc-line stream "~A~%" 'fixnum)
  (%desc-line stream "    value:    ~A~%" x)
  (%desc-line stream "    hex:      #x~A~%" (write-to-string-hex x))
  (%desc-line stream "    binary:   #b~A~%" (write-to-string-bin x))
  nil)

(defun %describe-bignum (x stream)
  (%desc-line stream "~A~%" 'bignum)
  (%desc-line stream "    value:    ~A~%" x)
  nil)

(defun %describe-ratio (x stream)
  (%desc-line stream "~A~%" 'ratio)
  (%desc-line stream "    value:        ~A~%" x)
  (%desc-line stream "    numerator:    ~A~%" (numerator x))
  (%desc-line stream "    denominator:  ~A~%" (denominator x))
  nil)

(defun %describe-float (x stream)
  (%desc-line stream "~A~%" 'float)
  (%desc-line stream "    value:    ~A~%" x)
  nil)

(defun %describe-char (x stream)
  (%desc-line stream "~A~%" 'character)
  (%desc-line stream "    glyph:    ~A~%" x)
  (%desc-line stream "    code:     ~A~%" (char-code x))
  (let ((name (char-name x)))
    (when name
      (%desc-line stream "    name:     ~A~%" name)))
  nil)

(defun %describe-string (x stream)
  (%desc-line stream "~A~%" 'string)
  (%desc-line stream "    length:   ~A~%" (length x))
  (%desc-line stream "    content:  ~S~%" x)
  nil)

(defun %describe-cons (x stream)
  (%desc-line stream "~A~%" 'cons)
  ;; Walk the spine; report total length if proper, ELSE flag
  ;; the dotted tail. Don't trust LENGTH on improper lists.
  (let ((len (proper-list-length x)))
    (cond
      (len
       (%desc-line stream "    length:   ~A (proper list)~%" len)
       (%desc-line stream "    head:     ~S~%" (car x))
       (when (> len 1)
         (%desc-line stream "    elements: ~S~%" x)))
      (t
       (%desc-line stream "    shape:    improper list / dotted pair~%")
       (%desc-line stream "    car:      ~S~%" (car x))
       (%desc-line stream "    cdr:      ~S~%" (cdr x)))))
  nil)

(defun proper-list-length (lst)
  "Return the length of LST iff it's a proper list (NIL-
   terminated), else NIL. Walks the spine without consing."
  (cond
    ((null lst) 0)
    ((not (consp lst)) nil)
    (t (let ((n (proper-list-length (cdr lst))))
         (if n (+ n 1) nil)))))

(defun %describe-symbol (x stream)
  (%desc-line stream "~A~%" 'symbol)
  (%desc-line stream "    name:     ~A~%" (symbol-name x))
  (let ((p (symbol-package x)))
    (when p (%desc-line stream "    package:  ~A~%" p)))
  (cond
    ((boundp x)
     (%desc-line stream "    value:    ~S~%" (symbol-value x)))
    (t
     (%desc-line stream "    value:    #<unbound>~%")))
  (cond
    ((fboundp x)
     (%desc-line stream "    function: ~A~%" (symbol-function x)))
    (t
     (%desc-line stream "    function: #<unbound>~%")))
  nil)

(defun %describe-function (x stream)
  (%desc-line stream "~A~%" 'function)
  (%desc-line stream "    object:   ~A~%" x)
  nil)

(defun %describe-vector (x stream)
  (%desc-line stream "~A~%" 'simple-vector)
  (%desc-line stream "    length:   ~A~%" (length x))
  (%desc-line stream "    elements: ~S~%" x)
  nil)

;; ── Helpers for integer-radix display ────────────────────────────────
;;
;; CL's WRITE has :base; we approximate via DIGIT-CHAR-driven loops.
;; The digit chars accumulate in a list (head = most significant);
;; COERCE turns the list into a string at the end. Negatives render
;; as "-…" prefixed onto the absolute-value form.

(defun %digit-chars-of (n radix acc)
  (cond
    ((= n 0) acc)
    (t (%digit-chars-of (truncate n radix) radix
                        (cons (digit-char (rem n radix) radix) acc)))))

(defun %write-int-radix (n radix)
  (cond
    ((= n 0) "0")
    ((< n 0)
     ;; Format the absolute-value digits then prepend "-". Using
     ;; `(format nil "-~A" …)` rather than (concatenate 'string …)
     ;; keeps describe's dependency set in core.lisp (no
     ;; Library/sequences.lisp needed).
     (format nil "-~A" (%write-int-radix (- 0 n) radix)))
    (t (coerce (%digit-chars-of n radix nil) 'string))))

(defun write-to-string-hex (n) (%write-int-radix n 16))
(defun write-to-string-bin (n) (%write-int-radix n 2))

;; ── Top-level DESCRIBE ───────────────────────────────────────────────

(defun describe (object &optional (stream t))
  "Print a human-readable description of OBJECT to STREAM (default
   T = native stdout). Returns the original object so DESCRIBE
   can be threaded through pipelines."
  (cond
    ((null object)
     (%desc-line stream "~A~%" 'null)
     (%desc-line stream "    value:    NIL (the empty list)~%"))
    ((eq object t)
     (%desc-line stream "~A~%" 'boolean)
     (%desc-line stream "    value:    T~%"))
    ((characterp object) (%describe-char     object stream))
    ((stringp object)    (%describe-string   object stream))
    ((symbolp object)    (%describe-symbol   object stream))
    ((consp object)      (%describe-cons     object stream))
    ((fixnump object)    (%describe-fixnum   object stream))
    ((bignump object)    (%describe-bignum   object stream))
    ((ratiop object)     (%describe-ratio    object stream))
    ((floatp object)     (%describe-float    object stream))
    ((functionp object)  (%describe-function object stream))
    ((vectorp object)    (%describe-vector   object stream))
    (t
     (%desc-line stream "~A~%" (type-of object))
     (%desc-line stream "    value:    ~S~%" object)))
  object)

(provide 'describe)
nil
