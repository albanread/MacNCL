;;;; Lisp/Library/streams.lisp — Tier 1.A
;;;;
;;;; CLOS-based stream classes layered on top of the native file-
;;;; handle streams provided by core. The native handles (returned
;;;; by open-input-file etc.) are integer fixnums; the new stream
;;;; objects are CLOS instances. Generic dispatchers route to
;;;; whichever is in hand.
;;;;
;;;; What this gives us:
;;;;
;;;;   * (make-string-output-stream)           → returns a stream object
;;;;   * (get-output-stream-string stream)     → flushes its buffer to a string
;;;;   * (make-string-input-stream "src")      → returns a stream object
;;;;   * (with-output-to-string (s) body...)   → body's stream writes are
;;;;                                              collected into a string
;;;;   * (with-input-from-string (s "src") …)  → body reads chars/lines from src
;;;;   * (format stream "fmt" args...)         → format now accepts streams
;;;;   * stream-write-char / -write-string / -read-char / -read-line
;;;;
;;;; The native file shims (write-string-to / read-line / read-char-from
;;;; / close-stream) are untouched — they still work as before for
;;;; integer handles. The wrappers below dispatch on type.

;; (We have a single global namespace; no in-package form needed yet.)

;; ── Class hierarchy ────────────────────────────────────────────────────────

(defclass stream () ()
  (:documentation "Abstract stream root class."))

(defclass string-output-stream (stream)
  ;; buffer is a LIST of strings written in reverse order (most-
  ;; recent first). get-output-stream-string reverses + joins. This
  ;; gives O(1) append, vs string-concat which would be quadratic.
  ((buffer :initform nil :accessor %sos-buffer))
  (:documentation
   "A stream that accumulates writes into an internal string buffer.
    (get-output-stream-string s) returns the accumulated string
    and clears the buffer."))

(defclass string-input-stream (stream)
  ((source :initarg :source :reader %sis-source)
   (pos    :initform 0 :accessor %sis-pos))
  (:documentation
   "A stream that reads from a fixed string. POS tracks how far
    we've consumed."))

;; ── Constructors / extractors ─────────────────────────────────────────────

(defun make-string-output-stream ()
  (make-instance 'string-output-stream))

(defun get-output-stream-string (stream)
  "Pop the accumulated buffer as a string and reset the stream."
  (let ((segments (reverse (%sos-buffer stream))))
    (setf (%sos-buffer stream) nil)
    (%join-strings segments)))

(defun %join-strings (segments)
  "Concatenate a list of strings via format ~A. format is a native
   function; cheaper than recursive string-concat."
  (cond
    ((null segments) "")
    ((null (cdr segments)) (car segments))
    (t (format nil "~A~A" (car segments)
               (%join-strings (cdr segments))))))

(defun make-string-input-stream (source)
  (make-instance 'string-input-stream :source source))

;; ── Predicates ────────────────────────────────────────────────────────────

(defun streamp (x)
  "T iff X is anything we'll accept as a stream — a native file
   handle (integer) or a CLOS stream instance."
  (cond
    ((integerp x) t)
    ((and (clos-instance-p x)
          (subclassp (class-of x) (find-class 'stream)))
     t)
    (t nil)))

(defun output-stream-p (x)
  "T iff X is a stream we can write to."
  (cond
    ((integerp x) t)                                       ; trust the user
    ((and (clos-instance-p x)
          (subclassp (class-of x) (find-class 'string-output-stream)))
     t)
    (t nil)))

(defun input-stream-p (x)
  (cond
    ((integerp x) t)
    ((and (clos-instance-p x)
          (subclassp (class-of x) (find-class 'string-input-stream)))
     t)
    (t nil)))

;; ── Output flushing ───────────────────────────────────────────────────────
;; NCL's standard output and string-output streams are not user-buffered:
;; FORMAT / PRINC / TERPRI write through immediately, and string streams
;; accumulate in place. So FORCE-OUTPUT / FINISH-OUTPUT have nothing to
;; drain and are no-ops that return NIL, per the CL contract (it is always
;; legal for these to be no-ops on an unbuffered stream). They exist so
;; portable code that calls them runs unchanged. CLEAR-OUTPUT likewise has
;; no buffered output to abort.
(defun force-output (&optional stream)
  (declare (ignore stream))
  nil)

(defun finish-output (&optional stream)
  (declare (ignore stream))
  nil)

(defun clear-output (&optional stream)
  (declare (ignore stream))
  nil)

;; ── Write dispatchers ─────────────────────────────────────────────────────

(defun stream-write-string (stream s)
  "Write the string S to STREAM. Returns S."
  (cond
    ((integerp stream)
     (write-string-to stream s)
     s)
    ((and (clos-instance-p stream)
          (eq (class-of stream) (find-class 'string-output-stream)))
     (setf (%sos-buffer stream) (cons s (%sos-buffer stream)))
     s)
    (t (error "stream-write-string: not an output stream: ~A" stream))))

(defun stream-write-char (stream char)
  "Write CHAR to STREAM. CHAR can be a Lisp character; we promote
   to a one-element string via the existing string-append-char
   helper."
  (let ((piece (string-append-char "" char)))
    (stream-write-string stream piece)
    char))

(defun stream-terpri (stream)
  "Write a newline to STREAM. Named after CL's terpri."
  (stream-write-string stream (format nil "~%"))
  nil)

;; ── Read dispatchers ──────────────────────────────────────────────────────

(defun stream-read-char (stream)
  "Read one character. Returns NIL at EOF."
  (cond
    ((integerp stream) (read-char-from stream))
    ((and (clos-instance-p stream)
          (eq (class-of stream) (find-class 'string-input-stream)))
     (%sis-read-char stream))
    (t (error "stream-read-char: not an input stream: ~A" stream))))

(defun %sis-read-char (stream)
  (let ((src (%sis-source stream))
        (pos (%sis-pos stream)))
    (cond
      ((>= pos (length src)) nil)
      (t (let ((c (svref src pos)))   ; works on strings — svref of a string
                                       ; gets the char by index
           (setf (%sis-pos stream) (+ pos 1))
           c)))))

(defun stream-read-line (stream)
  "Read until newline-or-EOF. Returns the line (without newline)
   or NIL at EOF."
  (cond
    ((integerp stream) (read-line stream))
    ((and (clos-instance-p stream)
          (eq (class-of stream) (find-class 'string-input-stream)))
     (%sis-read-line stream))
    (t (error "stream-read-line: not an input stream: ~A" stream))))

(defun %sis-read-line (stream)
  (let ((src (%sis-source stream))
        (start (%sis-pos stream)))
    (cond
      ((>= start (length src)) nil)
      (t
       (let ((end start) (limit (length src)))
         (loop
           (cond
             ((>= end limit) (return nil))
             ((eql (svref src end) #\newline) (return nil))
             (t (setq end (+ end 1)))))
         (let ((line (substring src start end)))
           (setf (%sis-pos stream)
                 (cond ((< end limit) (+ end 1))   ; skip the newline
                       (t end)))
           line))))))

;; ── Macros ────────────────────────────────────────────────────────────────

(defmacro with-output-to-string (var-list &rest body)
  "(with-output-to-string (var) body...) — bind VAR to a fresh
   string-output-stream, run BODY, return the accumulated string.
   The body may freely format/write to VAR."
  (let ((var (car var-list)))
    `(let ((,var (make-string-output-stream)))
       ,@body
       (get-output-stream-string ,var))))

(defmacro with-input-from-string (var-and-source &rest body)
  "(with-input-from-string (var source) body...) — bind VAR to
   a fresh string-input-stream over SOURCE, run BODY, return
   the last form's value."
  (let ((var (car var-and-source))
        (src (cadr var-and-source)))
    `(let ((,var (make-string-input-stream ,src)))
       ,@body)))

;; ── Reform format to accept streams ──────────────────────────────────────
;;
;; The native FORMAT (registered as `format` by the compiler) only
;; understands T (stdout) and NIL (return string). We need it to
;; also accept stream objects. Solution: rename the native to
;; %native-format and redefine FORMAT as a Lisp wrapper that
;; dispatches.
;;
;; All existing call sites in core / clos / library code already
;; resolve `format` through the symbol's function cell, so the
;; redefinition takes effect immediately for everything.

(defparameter %native-format (symbol-function 'format))

(defun format (dest control &rest args)
  "CL's format. DEST is one of:
     T            — write to *standard-output* (currently = stdout)
     NIL          — return the formatted string
     stream       — write to a stream (file handle or CLOS stream)
     string       — append to the string with fill pointer (NYI)"
  (cond
    ((or (eq dest 't) (null dest))
     (apply %native-format dest control args))
    ((streamp dest)
     ;; Format to a string, then push through the stream's
     ;; write-string dispatcher.
     (let ((s (apply %native-format nil control args)))
       (stream-write-string dest s)
       nil))
    (t (error "format: dest must be T, NIL, or a stream, got ~A" dest))))

(provide 'streams)
nil
