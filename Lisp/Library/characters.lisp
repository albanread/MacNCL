;;;; Lisp/Library/characters.lisp — character functions.
;;;;
;;;; Ported from Roger Corman's Sys/characters.lisp. The Rust-side
;;;; primitives — CHAR-CODE, CODE-CHAR, CHAR-UPCASE, CHAR-DOWNCASE,
;;;; ALPHA-CHAR-P, ALPHANUMERICP, UPPER-CASE-P, LOWER-CASE-P,
;;;; BOTH-CASE-P, GRAPHIC-CHAR-P, DIGIT-CHAR-P, DIGIT-CHAR — are
;;;; installed as native shims (see src/ncl-runtime/src/chars.rs).
;;;; This file wraps them and adds:
;;;;
;;;;   * the variadic comparison chain (char= / char/= / char< /
;;;;     char> / char<= / char>=) — by codepoint;
;;;;   * the case-insensitive variants (char-equal / char-not-equal /
;;;;     char-lessp / char-greaterp / char-not-lessp / char-not-greaterp)
;;;;     — codepoint of CHAR-UPCASE on both sides;
;;;;   * Corman's traditional aliases CHAR-INT / INT-CHAR;
;;;;   * NAME-CHAR / CHAR-NAME — a small lookup over the standard
;;;;     CL character names (#\Space, #\Newline, #\Tab, #\Return,
;;;;     #\Backspace, #\Page, #\Rubout, #\Linefeed, #\Null, #\Escape).
;;;;
;;;; Unicode note: NCL's CHAR is the full 21-bit Unicode scalar
;;;; (U+0000..U+10FFFF), not Corman's 8-bit Latin-1. All predicates
;;;; defer to Rust's `char` methods, which know the proper
;;;; categories — `alpha-char-p` recognises Cyrillic, Arabic, CJK,
;;;; etc. as letters, not just ASCII.
;;;;
;;;; Style note: NCL's DEFUN does not auto-install a `(block NAME ...)`
;;;; around the body (a Common-Lisp convenience some implementations
;;;; provide), and DO is not yet implemented. The variadic comparisons
;;;; below are therefore expressed through recursion and existing
;;;; higher-order helpers (EVERY, MAPCAR) rather than via RETURN-FROM /
;;;; DO loops. Same observable behaviour; smaller blast radius for
;;;; the JIT.

;; ── Corman aliases for the coercions ────────────────────────────────────

(defun char-int (ch) (char-code ch))
(defun int-char (n) (code-char n))

;; ── Internal helpers ───────────────────────────────────────────────────
;;
;; %chain-by walks CHARS in pairs and returns T iff every
;; consecutive pair (KEY a, KEY b) satisfies PRED. Empty or
;; single-element CHARS is vacuously T (matching CL spec).

(defun %chain-by (pred chars key)
  "Internal: T iff (PRED (KEY x) (KEY y)) holds for every adjacent
   pair (x, y) in CHARS."
  (cond
    ((null chars) t)
    ((null (cdr chars)) t)
    ((funcall pred
              (funcall key (car chars))
              (funcall key (cadr chars)))
     (%chain-by pred (cdr chars) key))
    (t nil)))

(defun %all-equal-by (chars key)
  "Internal: T iff every char in CHARS maps to the same value under
   KEY. Empty / single-element CHARS is vacuously T."
  (cond
    ((null chars) t)
    ((null (cdr chars)) t)
    (t (let ((k (funcall key (car chars))))
         (every (lambda (c) (= k (funcall key c))) (cdr chars))))))

(defun %pairwise-distinct-by (chars key)
  "Internal: T iff no two chars in CHARS map to the same value
   under KEY. Walks the upper-triangular pair set recursively."
  (cond
    ((null chars) t)
    ((null (cdr chars)) t)
    (t (let ((k (funcall key (car chars))))
         (and (every (lambda (c) (/= k (funcall key c))) (cdr chars))
              (%pairwise-distinct-by (cdr chars) key))))))

(defun %upcase-code (c) (char-code (char-upcase c)))

;; ── Strict-equality / inequality (by codepoint) ─────────────────────────

(defun char= (&rest chars)
  "T iff every char in CHARS has the same codepoint. (char=) with
   no args is T per CL spec."
  (%all-equal-by chars #'char-code))

(defun char/= (&rest chars)
  "T iff no two chars in CHARS have the same codepoint. CL says
   `char/=' is 'all distinct', not 'consecutive distinct': we
   walk every (i, j) pair with i < j."
  (%pairwise-distinct-by chars #'char-code))

;; ── Strict-ordering chain (codepoint) ──────────────────────────────────

(defun char<  (&rest chars) (%chain-by #'<  chars #'char-code))
(defun char>  (&rest chars) (%chain-by #'>  chars #'char-code))
(defun char<= (&rest chars) (%chain-by #'<= chars #'char-code))
(defun char>= (&rest chars) (%chain-by #'>= chars #'char-code))

;; ── Case-insensitive variants ──────────────────────────────────────────
;;
;; The -EQUAL / -LESSP / -GREATERP family compares by codepoint
;; of CHAR-UPCASE on both sides — exactly Corman's contract.

(defun char-equal     (&rest chars) (%all-equal-by chars #'%upcase-code))
(defun char-not-equal (&rest chars) (%pairwise-distinct-by chars #'%upcase-code))

(defun char-lessp        (&rest chars) (%chain-by #'<  chars #'%upcase-code))
(defun char-greaterp     (&rest chars) (%chain-by #'>  chars #'%upcase-code))
(defun char-not-greaterp (&rest chars) (%chain-by #'<= chars #'%upcase-code))
(defun char-not-lessp    (&rest chars) (%chain-by #'>= chars #'%upcase-code))

;; ── Character names ────────────────────────────────────────────────────
;;
;; CL spec only mandates a handful of standard names. We ship the
;; same set Corman does, with case-insensitive lookup.

(defparameter *named-characters*
  '(("Null"      .   0)
    ("Backspace" .   8)
    ("Tab"       .   9)
    ("Newline"   .  10)
    ("Linefeed"  .  10)
    ("Page"      .  12)
    ("Return"    .  13)
    ("Escape"    .  27)
    ("Space"     .  32)
    ("Rubout"    . 127)))

(defun %find-name-for-code (code pairs)
  (cond
    ((null pairs) nil)
    ((= (cdr (car pairs)) code) (car (car pairs)))
    (t (%find-name-for-code code (cdr pairs)))))

(defun char-name (ch)
  "Return CH's standard name as a string (e.g. \"Space\"), or NIL
   if CH has no standard name."
  (unless (characterp ch)
    (error "char-name: not a character: ~S" ch))
  (%find-name-for-code (char-code ch) *named-characters*))

(defun %find-char-for-name (s pairs)
  (cond
    ((null pairs) nil)
    ((string-equal s (car (car pairs))) (code-char (cdr (car pairs))))
    (t (%find-char-for-name s (cdr pairs)))))

(defun name-char (name)
  "Return the character whose name is NAME (a string or symbol),
   or NIL if none. Lookup is case-insensitive: \"space\", \"Space\",
   and \"SPACE\" all find #\\Space."
  (cond
    ((characterp name) name)
    ((stringp name) (%find-char-for-name name *named-characters*))
    ((symbolp name)
     (%find-char-for-name (symbol-name name) *named-characters*))
    (t (error "name-char: not a string/symbol/character: ~S" name))))

;; ── Direct-call CHAR coercion ──────────────────────────────────────────

(defun character (x)
  "Coerce X to a character. Accepts a character, a one-character
   string, or a symbol whose name is one character long."
  (cond
    ((characterp x) x)
    ((and (stringp x) (= (length x) 1)) (char x 0))
    ((symbolp x)
     (let ((n (symbol-name x)))
       (if (= (length n) 1)
           (char n 0)
           (error "character: ~S cannot be coerced to a character" x))))
    (t (error "character: ~S cannot be coerced to a character" x))))

;; ── string-equal ────────────────────────────────────────────────────────
;;
;; Case-insensitive string equality. Logically a character/string
;; helper; we use it for NAME-CHAR's case-insensitive lookup and
;; expose it under its CL name. Recursive on character indices —
;; no LOOP or DO required.

(defun %string-equal-from (a b i n)
  (cond
    ((>= i n) t)
    ((char-equal (char a i) (char b i))
     (%string-equal-from a b (+ i 1) n))
    (t nil)))

(defun string-equal (a b)
  "Case-insensitive string equality: T iff A and B have the same
   length and every pair of characters is CHAR-EQUAL."
  (and (= (length a) (length b))
       (%string-equal-from a b 0 (length a))))

;; ── equalp ──────────────────────────────────────────────────────────────
;;
;; CL's most permissive built-in equality:
;;
;;   * numbers: value compare, ignores type — `(equalp 1 1.0) => T`,
;;     `(equalp 2 2/1) => T`.
;;   * characters: CHAR-EQUAL (case-insensitive).
;;   * strings: same length + every pair CHAR-EQUAL (case-insensitive).
;;   * conses: recursive on car and cdr.
;;   * vectors: same length + every element EQUALP.
;;   * everything else: falls through to EQ.
;;
;; Per the spec, EQUALP on hash tables and structures requires
;; deeper structural inspection; we don't have a hash-table walker
;; yet and structs are EQ-only for now. Both fall through to EQ,
;; which matches user-visible expectations for the ANSI hyperspec
;; examples (which only exercise number/char/string/cons/vector).
;;
;; Lives at the end of characters.lisp so STRING-EQUAL and CHAR-EQUAL
;; are already defined. The function is dependency-free past those
;; two; modules loaded after characters.lisp (lists, places, numbers,
;; xp, describe, …) can use it freely.

(defun %vector-equalp-from (a b i n)
  (cond
    ((>= i n) t)
    ((equalp (svref a i) (svref b i))
     (%vector-equalp-from a b (+ i 1) n))
    (t nil)))

(defun equalp (a b)
  "Common-Lisp EQUALP. T iff A and B are equal under the loosest
   built-in test: numbers compared by VALUE across types, characters
   and strings case-insensitively, and conses/vectors recursively."
  (cond
    ((eq a b) t)
    ((and (numberp a) (numberp b)) (= a b))
    ((and (characterp a) (characterp b)) (char-equal a b))
    ;; Strings are vectors at the heap level, so the string check
    ;; comes first to dispatch on case-insensitive compare.
    ((and (stringp a) (stringp b)) (string-equal a b))
    ((and (consp a) (consp b))
     (and (equalp (car a) (car b))
          (equalp (cdr a) (cdr b))))
    ((and (vectorp a) (vectorp b))
     (and (= (length a) (length b))
          (%vector-equalp-from a b 0 (length a))))
    (t nil)))

(provide 'characters)
nil
