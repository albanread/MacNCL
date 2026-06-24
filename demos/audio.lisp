;;;; audio.lisp — NewAudio integration tour.
;;;;
;;;; A handful of one-liners exercising the AUDIO-* and ABC-* shims
;;;; backed by the sibling NewAudio crate at E:\NewAudio\NewAudio.
;;;; Run with:  ncl --load demos/audio.lisp
;;;;
;;;; (The mixer is Windows-only — non-Windows builds turn each call
;;;; into a no-op return-NIL stub so this file still loads cleanly.)

(format t "~%-- NewAudio tour --~%")
(force-output)

;; Start the mixer. (Every other audio-* call also lazy-starts it,
;; but doing it explicitly makes the order of events obvious.)
(audio-start)

;; ── SFX presets ─────────────────────────────────────────────────────
;; Each AUDIO-<name> call synthesizes a buffer, registers it with the
;; PCM runtime, and returns its SoundId as a fixnum. AUDIO-PLAY plays
;; it back through waveOut.

(defparameter *coin*  (audio-coin  0.4))
(defparameter *jump*  (audio-jump  0.3))
(defparameter *zap*   (audio-zap   0.35))
(defparameter *hit*   (audio-hit   0.15))
(defparameter *click* (audio-click 0.06))

(format t "ids: coin=~A jump=~A zap=~A hit=~A click=~A~%"
        *coin* *jump* *zap* *hit* *click*)
(force-output)

(format t "SFX volley...~%") (force-output)
(audio-play *coin*)  (sleep 0.6)
(audio-play *jump*)  (sleep 0.4)
(audio-play *zap*)   (sleep 0.5)
(audio-play *hit*)   (sleep 0.3)
(audio-play *click*) (sleep 0.2)

;; ── Synthesis ───────────────────────────────────────────────────────
;; AUDIO-TONE renders a sine of FREQ Hz for DUR seconds, returns its
;; SoundId. Build a quick C-major arpeggio.

(defparameter *do-* (audio-tone 261.63 0.25))   ; C4
(defparameter *mi-* (audio-tone 329.63 0.25))   ; E4
(defparameter *so-* (audio-tone 392.00 0.25))   ; G4
(defparameter *do^* (audio-tone 523.25 0.4))    ; C5

(format t "arpeggio...~%") (force-output)
(audio-play *do-*) (sleep 0.27)
(audio-play *mi-*) (sleep 0.27)
(audio-play *so-*) (sleep 0.27)
(audio-play *do^*) (sleep 0.5)

;; ── Spatial: AUDIO-PLAY-VOL takes (id volume pan) ─────────────────
;; volume is 0.0..1.0; pan is -1.0 (full left) .. +1.0 (full right).

(defparameter *ping* (audio-tone 880.0 0.2))
(format t "pan sweep...~%") (force-output)
(audio-play-vol *ping* 0.5 -1.0) (sleep 0.25)
(audio-play-vol *ping* 0.5 -0.5) (sleep 0.25)
(audio-play-vol *ping* 0.5  0.0) (sleep 0.25)
(audio-play-vol *ping* 0.5  0.5) (sleep 0.25)
(audio-play-vol *ping* 0.5  1.0) (sleep 0.4)

;; ── ABC playback ────────────────────────────────────────────────────
;; ABC-PLAY parses an ABC string and sends it to the Windows GM synth
;; via midiOut. The format is a tiny subset of ABC — header fields
;; (X:, T:, M:, L:, K:) plus pitch letters with octave marks and
;; durations. See E:\NewAudio\NewAudio\USER_GUIDE.md for the full
;; grammar.

(format t "twinkle twinkle...~%") (force-output)
(abc-play "X:1
T:Twinkle
M:4/4
L:1/4
K:C
CCGG|AAG2|FFEE|DDC2|FFEE|DDC2|CCGG|AAG2|FFEE|DDC2|")

;; Wait for the tune to play; the MIDI runtime is asynchronous.
(sleep 8.0)

(format t "~%-- done --~%")
(force-output)
