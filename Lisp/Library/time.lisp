;;;; Lisp/Library/time.lisp
;;;;
;;;; The `(time form)` macro and the underlying clock primitives.
;;;; Adapted from Corman's `Sys/time.lisp` — the macro shape is
;;;; Roger's, but the backing clock is `(get-internal-real-time)`
;;;; (a monotonic nanosecond counter exposed by the runtime,
;;;; backed by Rust's `std::time::Instant`).
;;;;
;;;; Usage:
;;;;
;;;;   (require 'time)
;;;;   (time (fib 30))
;;;;   ;;   real time: 0.084 s
;;;;   ;;   GC: 0 minor cycles, 0 bytes promoted
;;;;   832040
;;;;
;;;; Also exports:
;;;;
;;;;   `internal-time-units-per-second` — 1_000_000_000 (nanoseconds)
;;;;   `(get-internal-real-time)`       — monotonic ns since process start
;;;;   `(elapsed-seconds start)`        — convenience, returns a real number
;;;;   `(bench thunk &key (repeats 1))` — average over N runs

(defparameter internal-time-units-per-second 1000000000
  "Nanoseconds. The unit of `(get-internal-real-time)`.")

(defun elapsed-seconds (start-tick)
  "Seconds elapsed since START-TICK, as a ratio (or float if you
   coerce). Pair with `(get-internal-real-time)`."
  (/ (- (get-internal-real-time) start-tick)
     internal-time-units-per-second))

;; ── (time form) ─────────────────────────────────────────────────────────

(defmacro time (form)
  "Evaluate FORM, print a one-line timing report to *standard-output*,
   return FORM's values. Reports real (wall-clock) seconds and the
   delta in minor GC cycles + bytes promoted from `(gc-stats)`."
  (let ((t0    (gensym "T0-"))
        (gc0   (gensym "GC0-"))
        (prom0 (gensym "PROM0-"))
        (vals  (gensym "VALS-"))
        (t1    (gensym "T1-"))
        (s1    (gensym "S1-")))
    `(let* ((,t0   (get-internal-real-time))
            (,gc0  (getf (gc-stats) :minor-gcs))
            (,prom0 (getf (gc-stats) :bytes-promoted-total))
            (,vals (multiple-value-list ,form))
            (,t1   (get-internal-real-time))
            (,s1   (gc-stats)))
       (format t "  real time: ~A s~%"
               (%seconds (- ,t1 ,t0)))
       (format t "  GC: ~A minor cycles, ~A bytes promoted~%"
               (- (getf ,s1 :minor-gcs) ,gc0)
               (- (getf ,s1 :bytes-promoted-total) ,prom0))
       (apply #'values ,vals))))

(defun %seconds (nanos)
  "Format a nanosecond duration as a short decimal-seconds string.
   Avoids forcing floats so very small times don't print as 0."
  (cond
    ((>= nanos 1000000000)
     (format nil "~A.~3,'0D"
             (truncate nanos 1000000000)
             (truncate (rem nanos 1000000000) 1000000)))
    ((>= nanos 1000000)
     (format nil "0.~3,'0D"
             (truncate nanos 1000000)))
    ((>= nanos 1000)
     (format nil "0.000~3,'0D"
             (truncate nanos 1000)))
    (t
     (format nil "0.000000~3,'0D" nanos))))

;; ── (bench thunk :repeats N) ────────────────────────────────────────────

(defun bench (thunk &key (repeats 1))
  "Call THUNK no-args REPEATS times, return the average wall-clock
   nanoseconds. Useful for stable micro-benchmark numbers when one
   call is too short to measure (~µs)."
  (let ((start (get-internal-real-time)))
    (dotimes (i repeats) (funcall thunk))
    (truncate (- (get-internal-real-time) start) repeats)))

(provide 'time)
nil
