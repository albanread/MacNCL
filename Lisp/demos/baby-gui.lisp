;;;; baby-gui.lisp — iGui port of cormanlisp/examples/baby.lisp.
;;;;
;;;; Roger's baby-talker. The original ran in a console: you typed a
;;;; line, baby responded with a phrase mixing words from its
;;;; vocabulary; the vocabulary grew with what you said (and
;;;; sometimes with what it said back). Quit on "bye" / "stop" /
;;;; "exit" / "quit" / "good bye".
;;;;
;;;; This version moves the conversation into an iGui text window.
;;;; Type a line; the text-window collects each :char event and
;;;; echoes it. Press Enter and baby answers below. Close the
;;;; window (or type one of the quit phrases) to exit.
;;;;
;;;; Usage:
;;;;   ncl --windows -l Lisp/demos/baby-gui.lisp --eval "(run-baby-gui)"

;; ── Vocabulary + generator — verbatim from the corman demo ──────────

(defparameter *vocabulary*
  (copy-tree
    '((boo)
      (goo)
      (gah)
      (waa!)
      (hee-hee))))

(defparameter *feedback-percent* 50)

;; The corman `do-percent` macro used destructured macro params:
;;   (defmacro do-percent ((percent) &rest clauses) ...)
;; — NCL's defmacro doesn't unpack `(percent)` into a lambda-list
;; binding yet. Rewritten with a plain symbol param so the call site
;; below reads `(do-percent 50 ...)` instead of `(do-percent (50) ...)`.
(defmacro do-percent (percent &rest clauses)
  `(when (< (random 100) ,percent)
     ,@clauses))

(defun next-double ()
  (- (random 2.0) 1.0))

(defparameter +e+    (exp 1))
(defparameter +2*e+  (* 2 +e+))

;; Polar Box-Muller. Stash the second normal between calls.
(defparameter *next-normal* nil)

(defun normal-random ()
  (cond
    (*next-normal*
     (let ((v *next-normal*))
       (setq *next-normal* nil)
       v))
    (t
     (let ((v1 0.0) (v2 0.0) (s 1.0))
       (loop
         (setq v1 (* 2 (next-double)))
         (setq v2 (* 2 (next-double)))
         (setq s  (+ (* v1 v1) (* v2 v2)))
         (when (< s 1.0) (return)))
       (let ((m (sqrt (/ (* -2 (log s)) s))))
         (setq *next-normal* (* v2 m))
         (* v1 m))))))

(defun scaled-normal-random (n)
  (* n (/ (+ +e+ (normal-random)) +2*e+)))

(defun normal-random-integer (max-n)
  (let ((n (floor (scaled-normal-random max-n))))
    (cond
      ((< n 0)        0)
      ((>= n max-n)   (- max-n 1))
      (t              n))))

(defun get-random-phrase ()
  "Pick a vocabulary entry, then a random contiguous subseq of it.
   For single-word entries the subseq is the whole thing."
  (let ((phrase (elt *vocabulary* (random (length *vocabulary*)))))
    (cond
      ((> (length phrase) 1)
       (let ((start (random (length phrase)))
             (end   (random (length phrase))))
         (when (> start end)
           (let ((tmp start)) (setq start end) (setq end tmp)))
         (when (= start end)
           (if (> start 0) (setq start (- start 1)) (setq end (+ end 1))))
         (subseq phrase start (+ end 1))))
      (t phrase))))

(defun generate-response ()
  "Stream baby's reply word by word until we've emitted a Gaussian-
   sized run (1..6 words centred near 3). Each word is pulled from
   the current vocabulary phrase; when one runs dry, pick a new one."
  (let ((n        (max 1 (normal-random-integer 7)))
        (out      nil)
        (current  nil))
    (dotimes (_ n)
      (when (null current)
        (setq current (get-random-phrase)))
      (setq out (cons (first current) out))
      (setq current (cdr current)))
    (nreverse out)))

(defun update-vocabulary (input)
  (when input
    (setq *vocabulary* (cons input *vocabulary*))))

(defun respond (input)
  (update-vocabulary input)
  (let ((response (generate-response)))
    (do-percent *feedback-percent*
      (update-vocabulary response))
    response))

;; ── Input → words ───────────────────────────────────────────────────

(defun punctuation-p (ch)
  (find ch ".,;:`!?#-()\"\\"))

(defun strip-punctuation (s)
  "Replace every punctuation char in S with a space — same idea as
   the corman demo's `substitute-if #'punctuation-p`."
  (let ((out (make-string (length s) :initial-element #\space))
        (i 0))
    (loop
      (when (>= i (length s)) (return out))
      (let ((ch (char s i)))
        (setf (char out i) (if (punctuation-p ch) #\space ch))
        (setq i (+ i 1))))))

(defun string-words (s)
  "Split S on whitespace, intern each piece as a symbol, return the
   list. Empty pieces are dropped. The corman demo used
   `read-from-string` over a `(…)` wrapper; we do it by hand because
   our READ accepts a string but the round-trip is finicky for
   single-word inputs."
  (let ((words nil)
        (start 0)
        (n     (length s))
        (i     0))
    (loop
      (cond
        ((>= i n)
         (when (< start i)
           (setq words (cons (intern (string-upcase (subseq s start i)))
                             words)))
         (return (nreverse words)))
        ((or (eq (char s i) #\space) (eq (char s i) #\tab))
         (when (< start i)
           (setq words (cons (intern (string-upcase (subseq s start i)))
                             words)))
         (setq start (+ i 1))
         (setq i (+ i 1)))
        (t (setq i (+ i 1)))))))

(defun parse-input-line (s)
  (string-words (strip-punctuation s)))

(defun quit-phrase-p (words)
  (or (equal words '(good bye))
      (equal words '(good-bye))
      (equal words '(bye))
      (equal words '(quit))
      (equal words '(stop))
      (equal words '(exit))))

;; ── iGui shell ─────────────────────────────────────────────────────

(defparameter +nursery-bg+   (rgb 250 245 240))
(defparameter +baby-ink+     (rgb 200 80  130))
(defparameter +you-ink+      (rgb 60  90  140))
(defparameter +banner+       (rgb 120 90  140))

(defparameter *baby-id*    nil)
(defparameter *baby-input* "")

(defun format-baby (words)
  (let ((s (format nil "~{~A ~}" words)))
    ;; Trim the trailing space the ~{ ~} loop left and add a period
    ;; unless the line already ends with punctuation.
    (let ((n (length s)))
      (when (and (> n 0) (eq (char s (- n 1)) #\space))
        (setq s (subseq s 0 (- n 1))))
      (let ((m (length s)))
        (when (and (> m 0)
                   (not (punctuation-p (char s (- m 1)))))
          (setq s (concatenate 'string s "."))))
      s)))

(defun write-banner ()
  (text-set-pen *baby-id* +banner+ +nursery-bg+)
  (text-write   *baby-id* "Talk to baby.  Type a line and press Enter.")
  (text-newline *baby-id*)
  (text-set-pen *baby-id* +baby-ink+ +nursery-bg+)
  (text-write   *baby-id* "Quit phrases: bye / good bye / stop / exit / quit.")
  (text-newline *baby-id*)
  (text-newline *baby-id*))

(defun write-prompt ()
  (text-set-pen *baby-id* +you-ink+ +nursery-bg+)
  (text-write   *baby-id* "you: "))

(defun write-baby-line (response)
  (text-set-pen *baby-id* +baby-ink+ +nursery-bg+)
  (text-write   *baby-id* "baby: ")
  (text-write   *baby-id* (format-baby response))
  (text-newline *baby-id*))

(defun handle-input-line ()
  "Called when Enter is pressed. Submit the buffered line through
   `respond`, render baby's reply, reset the input buffer.
   Returns T if the user wants to keep talking, NIL on quit."
  (text-newline *baby-id*)
  (let ((words (parse-input-line *baby-input*)))
    (setq *baby-input* "")
    (cond
      ((quit-phrase-p words)
       (text-set-pen *baby-id* +banner+ +nursery-bg+)
       (text-write   *baby-id* "baby: bye")
       (text-newline *baby-id*)
       nil)
      (t
       (write-baby-line (respond words))
       (write-prompt)
       t))))

(defun handle-char (ch)
  (cond
    ((or (eq ch #\Newline) (eq ch #\Return))
     (handle-input-line))
    ((or (eq ch #\Backspace) (eq ch #\Rubout))
     (let ((n (length *baby-input*)))
       (when (> n 0)
         (setq *baby-input* (subseq *baby-input* 0 (- n 1)))
         (text-write-char *baby-id* #\Backspace)))
     t)
    ((null ch)
     ;; Unknown / control char with no character payload; ignore.
     t)
    (t
     (setq *baby-input* (concatenate 'string *baby-input* (string ch)))
     (text-write-char *baby-id* ch)
     t)))

(defun run-baby-gui ()
  (igui-start)
  (setq *baby-input* "")
  (setq *baby-id* (open-text-window "baby — type and press Enter"))
  (cond
    ((null *baby-id*)
     (format t "** open-text-window failed (is --windows enabled?)~%")
     :failed)
    (t
     (text-set-pen *baby-id* +baby-ink+ +nursery-bg+)
     (text-clear   *baby-id*)
     (write-banner)
     (write-prompt)
     (event-loop-for *baby-id*
       (:frame-close (return :done))
       (:close       (return :done))
       (:char        (unless (handle-char (getf ev :char))
                       (return :done)))))))
