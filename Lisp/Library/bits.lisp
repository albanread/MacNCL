;;;; Lisp/Library/bits.lisp — CL byte-field operations
;;;;
;;;; Port of cormanlisp/Sys/math.lisp and math2.lisp byte/ldb/dpb sections.
;;;; All functions are pure Lisp over logand / logior / logxor / ash / lognot
;;;; which are already native in NCL's Rust core.
;;;;
;;;; Provides:
;;;;   byte         byte-size    byte-position
;;;;   ldb          (setf ldb)   ldb-test
;;;;   dpb          mask-field   deposit-field

;;; ── Byte specifiers ─────────────────────────────────────────────────────────
;;
;; A byte specifier is a cons (size . position). We don't use defstruct
;; to avoid circularity at load time.

(defun byte (size position)
  "Return a byte specifier for SIZE bits starting at bit POSITION."
  (unless (and (integerp size) (>= size 0))
    (error "byte: size must be a non-negative integer, got ~S" size))
  (unless (and (integerp position) (>= position 0))
    (error "byte: position must be a non-negative integer, got ~S" position))
  (cons size position))

(defun byte-size (bytespec)
  "Return the size (number of bits) of BYTESPEC."
  (car bytespec))

(defun byte-position (bytespec)
  "Return the bit position of BYTESPEC."
  (cdr bytespec))

;;; ── LDB — load byte ─────────────────────────────────────────────────────────

(defun ldb (bytespec integer)
  "Extract the byte specified by BYTESPEC from INTEGER.
Returns a non-negative integer of SIZE bits."
  (let ((mask (- (ash 1 (byte-size bytespec)) 1)))
    (logand (ash integer (- (byte-position bytespec))) mask)))

(defun ldb-test (bytespec integer)
  "Return T if any bit in the byte specified by BYTESPEC is set in INTEGER."
  (not (zerop (ldb bytespec integer))))

;;; ── DPB — deposit byte ──────────────────────────────────────────────────────

(defun dpb (newbyte bytespec integer)
  "Return INTEGER with the byte specified by BYTESPEC replaced by NEWBYTE."
  (let* ((size (byte-size bytespec))
         (pos  (byte-position bytespec))
         (mask (ash (- (ash 1 size) 1) pos)))
    (logior (logand integer (lognot mask))
            (logand (ash newbyte pos) mask))))

;;; ── MASK-FIELD / DEPOSIT-FIELD ───────────────────────────────────────────────

(defun mask-field (bytespec integer)
  "Return INTEGER with all bits outside BYTESPEC zeroed.
Equivalent to (logand integer (dpb -1 bytespec 0))."
  (logand integer (dpb -1 bytespec 0)))

(defun deposit-field (newbyte bytespec integer)
  "Return INTEGER with the BYTESPEC bits replaced by the corresponding bits
of NEWBYTE (unlike DPB, NEWBYTE is not shifted).
Equivalent to (dpb (ldb bytespec newbyte) bytespec integer)."
  (let ((mask (mask-field bytespec -1)))
    (logior (logand newbyte mask)
            (logand integer (lognot mask)))))

(provide 'bits)
nil
