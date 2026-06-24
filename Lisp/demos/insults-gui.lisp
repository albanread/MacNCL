;;;; insults-gui.lisp — iGui port of cormanlisp/examples/insults.lisp.
;;;;
;;;; Roger's Elizabethan insult generator — three columns of period-
;;;; accurate slurs, random one from each — moved into an iGui text
;;;; window. The original was a console y-or-n-p loop. This one runs
;;;; in a window: click (or any key but Q) for the next insult, Q
;;;; or close to quit.
;;;;
;;;; Insults variously attributed in the original to William
;;;; Shakespeare, Matthew A. Lecher, Jerry Maguire of Center Grove
;;;; High School, and "a fellow priest who was the Vicar of a
;;;; neighbouring parish when I worked in the beautiful Waikato
;;;; District."  We keep all three columns verbatim.
;;;;
;;;; Usage:
;;;;   ncl --windows -l Lisp/demos/insults-gui.lisp --eval "(run-insults-gui)"

;; ── Word lists — verbatim from cormanlisp/examples/insults.lisp ────

(defparameter *column1*
  '(artless bawdy beslubbering bootless churlish cockered clouted
    craven currish dankish dissembling droning errant fawning fobbing
    froward frothy gleeking goatish gorbellied fool-born infectious
    jarring loggerheaded lumpish mammering mangled mewling paunchy
    ill-nurtured puking puny qualling rank reeky roguish ruttish
    saucy spleeny spongy rump-fed tottering unmuzzled vain venomed
    tardy-gaited warped wayward weedy yeasty))

(defparameter *column2*
  '(base-court bat-fowling beef-witted beetle-headed boil-brained
    clapper-clawed clay-brained common-kissing crook-pated
    dismal-dreaming dizzy-eyed doghearted dread-bolted earth-vexing
    elf-skinned fat-kidneyed fen-sucked flap-mouthed fly-bitten
    folly-fallen gudgeon full-gorged guts-griping half-faced
    hasty-witted hedge-born hell-hated idle-headed ill-breeding
    maggot-pie knotty-pated milk-livered motley-minded onion-eyed
    plume-plucked pottle-deep pox-marked reeling-ripe rough-hewn
    rude-growing puttock shard-borne sheep-biting spur-galled
    swag-bellied strumpet tickle-brained toad-spotted unchin-snouted
    weather-bitten))

(defparameter *column3*
  '(apple-john baggage barnacle bladder boar-pig bugbear bum-bailey
    canker-blossom clack-dish clotpole coxcomb codpiece death-token
    dewberry flap-dragon flax-wench flirt-gill foot-licker fustilarian
    giglet haggard harpy hedge-pig horn-beast hugger-mugger joithead
    lewdster lout malt-worm mammet measle minnow miscreant moldwarp
    mumble-news nut-hook pigeon-egg pignut pumpion ratsbane scut
    skainsmate varlot vassal whey-face wagtail))

;; ── Insult formatter ──────────────────────────────────────────────

(defun random-elt (list)
  (nth (random (length list)) list))

(defun next-insult ()
  "Build one Shakespearean slur of the form `Thou A B C!`."
  (format nil "Thou ~A ~A ~A!"
          (random-elt *column1*)
          (random-elt *column2*)
          (random-elt *column3*)))

;; ── iGui shell ────────────────────────────────────────────────────

(defparameter +parchment+    (rgb 245 235 200))   ; aged-paper background
(defparameter +scribe-ink+   (rgb 60  35  20))    ; warm brown text
(defparameter +scribe-bold+  (rgb 110 25  25))    ; rubric red

(defparameter *insults-id* nil)
(defparameter *insults-count* 0)

(defun write-insult ()
  (setq *insults-count* (+ *insults-count* 1))
  (text-set-pen *insults-id* +scribe-bold+ +parchment+)
  (text-write   *insults-id* (format nil "~3D. " *insults-count*))
  (text-set-pen *insults-id* +scribe-ink+ +parchment+)
  (text-write   *insults-id* (next-insult))
  (text-newline *insults-id*))

(defun run-insults-gui ()
  (igui-start)
  (setq *insults-count* 0)
  (setq *insults-id* (open-text-window "Insults — Elizabethan"))
  (cond
    ((null *insults-id*)
     (format t "** open-text-window failed (is --windows enabled?)~%")
     :failed)
    (t
     (text-set-pen *insults-id* +scribe-ink+ +parchment+)
     (text-clear   *insults-id*)
     (text-set-pen *insults-id* +scribe-bold+ +parchment+)
     (text-write   *insults-id* "An Elizabethan Insult Generator")
     (text-newline *insults-id*)
     (text-set-pen *insults-id* +scribe-ink+ +parchment+)
     (text-write   *insults-id* "Click or press any key for an insult.  Q to quit.")
     (text-newline *insults-id*)
     (text-newline *insults-id*)
     ;; Open with one so the window isn't empty.
     (write-insult)
     (event-loop-for *insults-id*
       (:frame-close (return :done))
       (:close       (return :done))
       (:mouse       (when (eq (getf ev :op) :left-down)
                       (write-insult)))
       (:char        (let ((ch (getf ev :char)))
                       (cond
                         ((or (eq ch #\q) (eq ch #\Q))
                          (return :done))
                         (t (write-insult)))))))))
