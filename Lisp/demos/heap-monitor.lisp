;;;; Lisp/demos/heap-monitor.lisp — tiny live GC heap monitor in iGui.
;;;;
;;;; Three small horizontal bars (young, old, peak) that update
;;;; ~4 Hz against the live `(gc-stats)` plist. Sticker-sized:
;;;; everything fits in ~150×60 of pixel real estate, so it's
;;;; happy in a tiny corner of a larger window.
;;;;
;;;; Updates are driven by iGui's `:tick` events (set-redraw-rate)
;;;; rather than a Lisp worker thread — keeps the demo bullet-
;;;; proof against click/focus events racing the repaint, and
;;;; gives us a single deterministic event loop.
;;;;
;;;; Run:
;;;;   ncl --load Lisp/demos/heap-monitor.lisp --eval "(run-heap-monitor)"
;;;;
;;;; or via:
;;;;   ./tools/Start-Gui.ps1 -Demo heap-monitor

(defparameter +bg+         (rgb 18 22 30))
(defparameter +bar-frame+  (rgb 60 65 75))
(defparameter +bar-empty+  (rgb 35 40 50))
(defparameter +young-col+  (rgb 100 200 110))   ; green
(defparameter +old-col+    (rgb 110 160 230))   ; blue
(defparameter +peak-col+   (rgb 230 180 80))    ; amber
(defparameter +label-col+  (rgb 220 220 220))

;; Sticker-sized layout. Bars stack tight in the top-left, ~120 px
;; wide total with the labels in the gutter.
(defparameter *bar-x*      36)
(defparameter *bar-width*  100)
(defparameter *bar-height* 8)
(defparameter *row-pitch*  12)
(defparameter *row-top*    8)

(defun paint-bar (label row used cap color)
  "One tiny labelled bar at `row` (1-indexed). Integer-only math."
  (let* ((y (+ *row-top* (* row *row-pitch*)))
         (filled (cond
                   ((<= used 0) 0)
                   ((>= used cap) *bar-width*)
                   (t (truncate (* *bar-width* used) cap)))))
    (fill-rect *bar-x* y *bar-width* *bar-height* +bar-empty+)
    (when (> filled 0)
      (fill-rect *bar-x* y filled *bar-height* color))
    (stroke-rect *bar-x* y *bar-width* *bar-height* 1 +bar-frame+)
    (draw-text 4 (+ y *bar-height*) label 9 +label-col+)))

(defun paint-monitor (id)
  (let* ((s (gc-stats))
         (young-used (getf s :young-used))
         (young-cap  (max (getf s :young-cap) 1))
         (old-used   (getf s :old-used))
         (old-cap    (max (getf s :old-cap) 1))
         (peak       (getf s :peak-young-bytes)))
    (with-batch id
      (clear +bg+)
      (paint-bar "yng" 1 young-used young-cap +young-col+)
      (paint-bar "old" 2 old-used   old-cap   +old-col+)
      (paint-bar "pk"  3 peak       young-cap +peak-col+))))

(defun run-heap-monitor ()
  "Open the heap-monitor child, drive repaints off iGui's :tick.
   Close the window to exit."
  (igui-start)
  (let ((id (open-child "heap")))
    (cond
      ((null id)
       (format t "** open-child failed~%")
       :failed)
      (t
       (paint-monitor id)
       ;; 250 ms tick = 4 Hz refresh. iGui coalesces backed-up
       ;; ticks so the language thread only sees one per drain.
       (set-redraw-rate id 250)
       (event-loop-for id
         (:frame-close (return :done))
         (:close       (return :done))
         (:tick        (paint-monitor id))
         (:resize      (paint-monitor id))
         (t            nil))))))   ; ignore mouse/focus/keys etc.
