;;;; gui-repl.lisp — a Lisp REPL inside an iGui text window.
;;;;
;;;; Rewritten on top of the text-view primitives. The text window
;;;; owns the grid + cursor + scrollback (such as it is); we just
;;;; track the in-progress input string so we can detect when a form
;;;; is balanced and ready to evaluate, and for backspace handling.
;;;;
;;;; All interaction goes through (eval-string ...) — the same
;;;; pipeline the stdin REPL uses. Defuns, defmacros, defparameter
;;;; etc. all persist across submissions in the live Session.

(defparameter +repl-fg+      (rgb 220 220 220))
(defparameter +repl-bg+      (rgb 18 22 28))
(defparameter +repl-prompt+  (rgb 130 170 230))
(defparameter +repl-result+  (rgb 130 200 130))
(defparameter +repl-error+   (rgb 230 130 130))

(defparameter *repl-id*    nil)
(defparameter *repl-input* "")

(defun repl-prompt-string ()
  (if (parse-complete? *repl-input*) "> " ".. "))

(defun repl-write-prompt ()
  (text-set-pen *repl-id* +repl-prompt+ +repl-bg+)
  (text-write *repl-id* (repl-prompt-string))
  (text-set-pen *repl-id* +repl-fg+ +repl-bg+))

(defun repl-write-result (text)
  (text-set-pen *repl-id* +repl-result+ +repl-bg+)
  (text-write *repl-id* text)
  (text-newline *repl-id*)
  (text-set-pen *repl-id* +repl-fg+ +repl-bg+))

(defun repl-write-error (text)
  (text-set-pen *repl-id* +repl-error+ +repl-bg+)
  (text-write *repl-id* text)
  (text-newline *repl-id*)
  (text-set-pen *repl-id* +repl-fg+ +repl-bg+))

(defun repl-banner ()
  (text-set-pen *repl-id* +repl-prompt+ +repl-bg+)
  (text-write *repl-id* "NewCormanLisp REPL")
  (text-newline *repl-id*)
  (text-set-pen *repl-id* +repl-fg+ +repl-bg+))

(defun paren-depth (s)
  "Count unmatched `(` minus `)` in S, treating `\"…\"` strings as
   opaque. Newlines are ignored — the whole string is one stream."
  (let ((n (length s))
        (depth 0)
        (in-string nil)
        (i 0))
    (loop
      (cond
        ((>= i n) (return depth))
        (t (let ((c (char s i)))
             (cond
               ((eq c #\") (setq in-string (not in-string)))
               (in-string nil)
               ((eq c #\() (setq depth (+ depth 1)))
               ((eq c #\)) (setq depth (- depth 1)))
               (t nil))
             (setq i (+ i 1))))))))

(defun current-line-indent ()
  "Indent the next continuation line should start with: two spaces
   per unmatched `(` in the whole input."
  (let ((depth (paren-depth *repl-input*)))
    (cond
      ((<= depth 0) "")
      (t (make-spaces (* depth 2))))))

(defun make-spaces (n)
  (cond
    ((<= n 0) "")
    (t (string-concat " " (make-spaces (- n 1))))))

(defun repl-handle-enter ()
  (cond
    ((parse-complete? *repl-input*)
     ;; Drop down a line so the result lands below the input.
     (text-newline *repl-id*)
     (let ((src *repl-input*))
       (setq *repl-input* "")
       (let ((result (handler-case (eval-string src)
                       (error (c) (format nil "** ~A" c)))))
         (cond
           ((and (>= (length result) 3)
                 (eq (char result 0) #\*)
                 (eq (char result 1) #\*)
                 (eq (char result 2) #\Space))
            (repl-write-error result))
           (t (repl-write-result result)))))
     (repl-write-prompt))
    (t
     ;; Incomplete — newline + continuation prompt + auto-indent.
     (let ((indent (current-line-indent)))
       (text-newline *repl-id*)
       (text-set-pen *repl-id* +repl-prompt+ +repl-bg+)
       (text-write *repl-id* ".. ")
       (text-set-pen *repl-id* +repl-fg+ +repl-bg+)
       (text-write *repl-id* indent)
       (setq *repl-input* (format nil "~A~%~A" *repl-input* indent))))))

(defun repl-handle-backspace ()
  ;; Drop the last char from the buffer and ask the text view to
  ;; erase one cell to the left. The 0x08 codepoint is interpreted
  ;; on the GUI side as cursor-back + blank-current-cell (no-op at
  ;; col 0). Newlines in the buffer don't have a visible cell to
  ;; erase, so for those we just trim the buffer and move on.
  (when (> (length *repl-input*) 0)
    (let ((last (char *repl-input* (- (length *repl-input*) 1))))
      (setq *repl-input* (string-without-last *repl-input*))
      (cond
        ((eq last #\Newline) nil)
        (t (text-write-char *repl-id* #\Backspace))))))

(defun repl-handle-char (c cp)
  (cond
    ((= cp 13) (repl-handle-enter))      ; Return
    ((= cp 8)  (repl-handle-backspace))  ; Backspace
    ((< cp 32) nil)                       ; other control codes
    (t
     (setq *repl-input* (string-append-char *repl-input* c))
     (text-write-char *repl-id* c))))

(defun run-gui-repl ()
  (igui-start)
  (setq *repl-input* "")
  (setq *repl-id* (open-text-window "ncl REPL"))
  (cond
    ((null *repl-id*)
     (format t "** open-text-window failed — is the iGui frame up?~%")
     :failed)
    (t
     (text-set-pen *repl-id* +repl-fg+ +repl-bg+)
     (text-clear *repl-id*)
     (repl-banner)
     (repl-write-prompt)
     (event-loop-for *repl-id*
       (:frame-close (return :done))
       (:close       (return :done))
       (:char        (repl-handle-char (getf ev :char) (getf ev :codepoint)))))))
