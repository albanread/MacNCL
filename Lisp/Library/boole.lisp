;;;; boole.lisp — CL BOOLE function and bitwise operation constants
;;;;
;;;; Port of cormanlisp/Sys/boole.lisp (Frank A. Adrian, 2000).
;;;; The seven derived bitwise ops (logandc1, logandc2, logorc1, logorc2,
;;;; lognor, logeqv, lognand) are absent from NCL's Rust core, so we define
;;;; them here as pure-Lisp wrappers around logand / logior / logxor / lognot.
;;;;
;;;; NOTE: NCL lacks #. (read-time eval), so the case arms use literal
;;;; integers 0–15 rather than #.boole-and etc.

;;; ── Derived bitwise operations ──────────────────────────────────────────────

(defun logandc1 (i1 i2)
  "Bitwise AND of (complement i1) and i2."
  (logand (lognot i1) i2))

(defun logandc2 (i1 i2)
  "Bitwise AND of i1 and (complement i2)."
  (logand i1 (lognot i2)))

(defun logorc1 (i1 i2)
  "Bitwise OR of (complement i1) and i2."
  (logior (lognot i1) i2))

(defun logorc2 (i1 i2)
  "Bitwise OR of i1 and (complement i2)."
  (logior i1 (lognot i2)))

(defun lognor (i1 i2)
  "Bitwise complement of (logior i1 i2)."
  (lognot (logior i1 i2)))

(defun logeqv (i1 i2)
  "Bitwise equivalence (complement of XOR)."
  (lognot (logxor i1 i2)))

(defun lognand (i1 i2)
  "Bitwise complement of (logand i1 i2)."
  (lognot (logand i1 i2)))

;;; ── BOOLE constants (CL standard, CLHS 12.1.3) ──────────────────────────────

(defconstant boole-clr   0)   ; always 0
(defconstant boole-and   1)   ; logand
(defconstant boole-andc1 2)   ; logandc1
(defconstant boole-2     3)   ; i2
(defconstant boole-andc2 4)   ; logandc2
(defconstant boole-1     5)   ; i1
(defconstant boole-xor   6)   ; logxor
(defconstant boole-ior   7)   ; logior
(defconstant boole-nor   8)   ; lognor
(defconstant boole-eqv   9)   ; logeqv
(defconstant boole-c1    10)  ; lognot i1
(defconstant boole-orc1  11)  ; logorc1
(defconstant boole-c2    12)  ; lognot i2
(defconstant boole-orc2  13)  ; logorc2
(defconstant boole-nand  14)  ; lognand
(defconstant boole-set   15)  ; always -1

;;; ── BOOLE function ──────────────────────────────────────────────────────────

(defun boole (op i1 i2)
  "Perform the bitwise operation indicated by OP on integers I1 and I2.
OP must be one of the boole-* constants (0–15)."
  (unless (integerp i1) (error "boole: ~S is not an integer" i1))
  (unless (integerp i2) (error "boole: ~S is not an integer" i2))
  (case op
    (0  0)                   ; boole-clr
    (1  (logand  i1 i2))     ; boole-and
    (2  (logandc1 i1 i2))    ; boole-andc1
    (3  i2)                  ; boole-2
    (4  (logandc2 i1 i2))    ; boole-andc2
    (5  i1)                  ; boole-1
    (6  (logxor  i1 i2))     ; boole-xor
    (7  (logior  i1 i2))     ; boole-ior
    (8  (lognor  i1 i2))     ; boole-nor
    (9  (logeqv  i1 i2))     ; boole-eqv
    (10 (lognot  i1))        ; boole-c1
    (11 (logorc1 i1 i2))     ; boole-orc1
    (12 (lognot  i2))        ; boole-c2
    (13 (logorc2 i1 i2))     ; boole-orc2
    (14 (lognand i1 i2))     ; boole-nand
    (15 -1)                  ; boole-set
    (otherwise
     (error "boole: ~S is not a valid boole operation (must be 0–15)" op))))
