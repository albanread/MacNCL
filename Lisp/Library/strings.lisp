;;;; Lisp/Library/strings.lisp
;;;;
;;;; Full CL string library layered on NCL's primitive string type.
;;;; Ported and adapted from Corman Lisp Sys/strings.lisp.
;;;;
;;;; Provides:
;;;;   Coercion       — string
;;;;   Case           — string-upcase string-downcase string-capitalize
;;;;                    (override bootstrap versions in xp.lisp; add :start/:end)
;;;;   Case-sensitive — string= string/= string< string> string<= string>=
;;;;   Case-insensitive — string-equal (full, overrides characters.lisp's two-arg
;;;;                    version), string-not-equal, string-lessp, string-greaterp,
;;;;                    string-not-greaterp, string-not-lessp
;;;;   Trim           — string-left-trim, string-right-trim, string-trim
;;;;   Construction   — make-string
;;;;   File probe     — probe-file  (wraps the FILE-EXISTS shim)
;;;;   Splitting      — split-string  (bonus, not in ANSI CL but ubiquitous)
;;;;
;;;; Dependencies loaded before us: sequences (find, char=), characters
;;;; (char-lessp/char-greaterp/char-equal), places (incf/decf).
;;;;
;;;; Design note — no VALUES: NCL does not propagate multiple return values
;;;; through recursive tail calls (the secondary-values slot is cleared on
;;;; return).  We therefore use two parallel recursive helpers per predicate
;;;; family: one that returns the ORDER keyword (:equal/:less/:greater) and
;;;; one that returns the MISMATCH INDEX (an integer or NIL).  Each public
;;;; comparison function calls only what it needs.
;;;;
;;;; Mismatch-index semantics per CL spec: the index is into the ORIGINAL
;;;; string (not relative to :start1).  When the "true" branch exits because
;;;; x is a proper prefix of y the returned index is e1 (end of x segment).

;; ── Coercion ────────────────────────────────────────────────────────────────

(defun string (x)
  "Coerce X to a string. Accepts a string (identity), a symbol (its
   name), or a character (a one-character string). Signals on anything else."
  (cond
    ((stringp x) x)
    ((symbolp x) (symbol-name x))
    ((characterp x) (string-append-char "" x))
    (t (error "string: ~S is not a string designator" x))))

;; ── Internal comparison engine ───────────────────────────────────────────────
;;
;; Four mutually-recursive helpers: -order-cs, -mismatch-cs (case-sensitive)
;; and -order-ci, -mismatch-ci (case-insensitive).  Recursion depth equals the
;; shorter segment length; for typical string lengths this is safe.

;; Caseful — order

(defun %string-order-cs (x y i e1 j e2)
  "Return :equal, :less, or :greater for x[i..e1) vs y[j..e2) (case-sensitive)."
  (cond
    ((= i e1) (if (= j e2) :equal :less))
    ((= j e2) :greater)
    ((char< (char x i) (char y j)) :less)
    ((char> (char x i) (char y j)) :greater)
    (t (%string-order-cs x y (+ i 1) e1 (+ j 1) e2))))

;; Caseful — mismatch index

(defun %string-mismatch-cs (x y i e1 j e2)
  "Index in x of first case-sensitive mismatch; e1 if x is a proper prefix
   of y; NIL if the segments are equal."
  (cond
    ((= i e1) (if (= j e2) nil i))
    ((= j e2) i)
    ((char= (char x i) (char y j))
     (%string-mismatch-cs x y (+ i 1) e1 (+ j 1) e2))
    (t i)))

;; Caseless — order

(defun %string-order-ci (x y i e1 j e2)
  "Return :equal, :less, or :greater for x[i..e1) vs y[j..e2) (case-insensitive)."
  (cond
    ((= i e1) (if (= j e2) :equal :less))
    ((= j e2) :greater)
    ((char-lessp    (char x i) (char y j)) :less)
    ((char-greaterp (char x i) (char y j)) :greater)
    (t (%string-order-ci x y (+ i 1) e1 (+ j 1) e2))))

;; Caseless — mismatch index

(defun %string-mismatch-ci (x y i e1 j e2)
  "Index in x of first case-insensitive mismatch; e1 if x is a proper prefix
   of y; NIL if the segments are case-insensitively equal."
  (cond
    ((= i e1) (if (= j e2) nil i))
    ((= j e2) i)
    ((char-equal (char x i) (char y j))
     (%string-mismatch-ci x y (+ i 1) e1 (+ j 1) e2))
    (t i)))

;; ── Case-sensitive comparisons ───────────────────────────────────────────────

(defun string= (x y &key (start1 0) end1 (start2 0) end2)
  "T if the specified substrings of X and Y are identical (case-sensitive)."
  (let* ((a (string x)) (b (string y))
         (e1 (or end1 (length a))) (e2 (or end2 (length b))))
    (eq (%string-order-cs a b start1 e1 start2 e2) :equal)))

(defun string/= (x y &key (start1 0) end1 (start2 0) end2)
  "Mismatch index if substrings differ (case-sensitive), NIL if equal."
  (let* ((a (string x)) (b (string y))
         (e1 (or end1 (length a))) (e2 (or end2 (length b))))
    (unless (eq (%string-order-cs a b start1 e1 start2 e2) :equal)
      (%string-mismatch-cs a b start1 e1 start2 e2))))

(defun string< (x y &key (start1 0) end1 (start2 0) end2)
  "Mismatch index (in x) if x < y, NIL otherwise."
  (let* ((a (string x)) (b (string y))
         (e1 (or end1 (length a))) (e2 (or end2 (length b))))
    (when (eq (%string-order-cs a b start1 e1 start2 e2) :less)
      (%string-mismatch-cs a b start1 e1 start2 e2))))

(defun string> (x y &key (start1 0) end1 (start2 0) end2)
  "Mismatch index (in x) if x > y, NIL otherwise."
  (let* ((a (string x)) (b (string y))
         (e1 (or end1 (length a))) (e2 (or end2 (length b))))
    (when (eq (%string-order-cs a b start1 e1 start2 e2) :greater)
      (%string-mismatch-cs a b start1 e1 start2 e2))))

(defun string<= (x y &key (start1 0) end1 (start2 0) end2)
  "Mismatch index (or end of x) if x <= y, NIL if x > y."
  (let* ((a (string x)) (b (string y))
         (e1 (or end1 (length a))) (e2 (or end2 (length b))))
    (unless (eq (%string-order-cs a b start1 e1 start2 e2) :greater)
      (or (%string-mismatch-cs a b start1 e1 start2 e2) e1))))

(defun string>= (x y &key (start1 0) end1 (start2 0) end2)
  "Mismatch index (or end of x) if x >= y, NIL if x < y."
  (let* ((a (string x)) (b (string y))
         (e1 (or end1 (length a))) (e2 (or end2 (length b))))
    (unless (eq (%string-order-cs a b start1 e1 start2 e2) :less)
      (or (%string-mismatch-cs a b start1 e1 start2 e2) e1))))

;; ── Case-insensitive comparisons ─────────────────────────────────────────────
;;
;; These override the two-arg STRING-EQUAL defined in characters.lisp,
;; adding the full :start1/:end1/:start2/:end2 keyword interface.

(defun string-equal (x y &key (start1 0) end1 (start2 0) end2)
  "Case-insensitive equality. T if substrings match ignoring case."
  (let* ((a (string x)) (b (string y))
         (e1 (or end1 (length a))) (e2 (or end2 (length b))))
    (eq (%string-order-ci a b start1 e1 start2 e2) :equal)))

(defun string-not-equal (x y &key (start1 0) end1 (start2 0) end2)
  "Case-insensitive: mismatch index if substrings differ, NIL if equal."
  (let* ((a (string x)) (b (string y))
         (e1 (or end1 (length a))) (e2 (or end2 (length b))))
    (unless (eq (%string-order-ci a b start1 e1 start2 e2) :equal)
      (%string-mismatch-ci a b start1 e1 start2 e2))))

(defun string-lessp (x y &key (start1 0) end1 (start2 0) end2)
  "Case-insensitive: mismatch index if x < y, NIL otherwise."
  (let* ((a (string x)) (b (string y))
         (e1 (or end1 (length a))) (e2 (or end2 (length b))))
    (when (eq (%string-order-ci a b start1 e1 start2 e2) :less)
      (%string-mismatch-ci a b start1 e1 start2 e2))))

(defun string-greaterp (x y &key (start1 0) end1 (start2 0) end2)
  "Case-insensitive: mismatch index if x > y, NIL otherwise."
  (let* ((a (string x)) (b (string y))
         (e1 (or end1 (length a))) (e2 (or end2 (length b))))
    (when (eq (%string-order-ci a b start1 e1 start2 e2) :greater)
      (%string-mismatch-ci a b start1 e1 start2 e2))))

(defun string-not-greaterp (x y &key (start1 0) end1 (start2 0) end2)
  "Case-insensitive: mismatch index (or end of x) if x <= y, NIL if x > y."
  (let* ((a (string x)) (b (string y))
         (e1 (or end1 (length a))) (e2 (or end2 (length b))))
    (unless (eq (%string-order-ci a b start1 e1 start2 e2) :greater)
      (or (%string-mismatch-ci a b start1 e1 start2 e2) e1))))

(defun string-not-lessp (x y &key (start1 0) end1 (start2 0) end2)
  "Case-insensitive: mismatch index (or end of x) if x >= y, NIL if x < y."
  (let* ((a (string x)) (b (string y))
         (e1 (or end1 (length a))) (e2 (or end2 (length b))))
    (unless (eq (%string-order-ci a b start1 e1 start2 e2) :less)
      (or (%string-mismatch-ci a b start1 e1 start2 e2) e1))))

;; ── Case conversion (full CL versions with :start/:end) ──────────────────────
;;
;; Override the simpler string-upcase / string-downcase in xp.lisp with the
;; full ANSI interface.  Call sites without keyword args are unaffected.

;; All three allocate the result once with MAKE-STRING and fill it
;; in place with (SETF (CHAR ...)). The previous versions grew the
;; result one STRING-APPEND-CHAR at a time — a fresh copy of the
;; whole accumulated string PER CHARACTER, i.e. O(n²) work and n
;; heap allocations for an n-character input.

(defun string-upcase (s &key (start 0) end)
  "Return a copy of S with characters in [START, END) upcased."
  (let* ((a (string s)) (n (length a)) (e (or end n))
         (out (make-string n)) (i 0))
    (loop
      (when (>= i n) (return out))
      (setf (char out i)
            (if (and (>= i start) (< i e))
                (char-upcase (char a i))
                (char a i)))
      (setq i (+ i 1)))))

(defun string-downcase (s &key (start 0) end)
  "Return a copy of S with characters in [START, END) downcased."
  (let* ((a (string s)) (n (length a)) (e (or end n))
         (out (make-string n)) (i 0))
    (loop
      (when (>= i n) (return out))
      (setf (char out i)
            (if (and (>= i start) (< i e))
                (char-downcase (char a i))
                (char a i)))
      (setq i (+ i 1)))))

(defun string-capitalize (s &key (start 0) end)
  "Return a copy of S where, within [START, END), the first character after
   a non-alphanumeric character (or at the start) is upcased and the rest
   downcased. Characters outside the range are copied verbatim."
  (let* ((a (string s)) (n (length a)) (e (or end n))
         (out (make-string n)) (i 0) (new-word t))
    (loop
      (when (>= i n) (return out))
      (let ((c (char a i)))
        (cond
          ((or (< i start) (>= i e))
           (setf (char out i) c))
          ((alphanumericp c)
           (setf (char out i)
                 (if new-word
                     (char-upcase   c)
                     (char-downcase c)))
           (setq new-word nil))
          (t
           (setf (char out i) c)
           (setq new-word t))))
      (setq i (+ i 1)))))

;; ── Trim ─────────────────────────────────────────────────────────────────────

(defun string-left-trim (char-bag s)
  "Remove leading characters that appear in CHAR-BAG from S."
  (let* ((a (string s)) (n (length a)) (i 0))
    (loop
      (when (or (>= i n) (not (find (char a i) char-bag)))
        (return (subseq a i n)))
      (setq i (+ i 1)))))

(defun string-right-trim (char-bag s)
  "Remove trailing characters that appear in CHAR-BAG from S."
  (let* ((a (string s)) (n (length a)) (i (- n 1)))
    (loop
      (when (or (< i 0) (not (find (char a i) char-bag)))
        (return (subseq a 0 (+ i 1))))
      (setq i (- i 1)))))

(defun string-trim (char-bag s)
  "Remove both leading and trailing characters in CHAR-BAG from S."
  (string-left-trim char-bag (string-right-trim char-bag s)))

;; ── Construction ─────────────────────────────────────────────────────────────

(defun make-string (size &key (initial-element #\Space))
  "Return a fresh string of SIZE characters, each INITIAL-ELEMENT."
  ;; One-pass native allocator. The previous string-append-char loop
  ;; copied the whole accumulated string per character — O(n²) — and
  ;; quietly made every make-string + (setf (char ...)) builder
  ;; quadratic with it.
  (%make-string-fill size initial-element))

;; ── File probe ───────────────────────────────────────────────────────────────

(defun probe-file (path)
  "Return PATH if the file exists, NIL otherwise.
   CL spec says return a truename pathname; we return the path string."
  (when (file-exists path) path))

;; ── Splitting (bonus) ─────────────────────────────────────────────────────────

(defun split-string (string &optional (separator #\Space))
  "Split STRING on each SEPARATOR character. Returns a list of substrings,
   omitting empty tokens from adjacent separators. SEPARATOR may be a
   character or a one-character string."
  (let* ((s   (string string))
         (sep (if (characterp separator)
                  separator
                  (char (string separator) 0)))
         (n   (length s))
         (out nil)
         (start 0)
         (i 0))
    (loop
      (cond
        ((>= i n)
         (when (< start n)
           (setq out (cons (subseq s start n) out)))
         (return (nreverse out)))
        ((char= (char s i) sep)
         (when (> i start)
           (setq out (cons (subseq s start i) out)))
         (setq start (+ i 1))
         (setq i (+ i 1)))
        (t
         (setq i (+ i 1)))))))

(provide 'strings)
nil
