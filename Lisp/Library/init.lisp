;;;; Lisp/Library/init.lisp
;;;;
;;;; Loaded automatically at startup by `ncl` (the driver looks for
;;;; this file next to the executable, or under <repo>/Lisp/Library/
;;;; for dev builds). This file is the place to wire up things that
;;;; should be available in every session — analogous to Corman's
;;;; init.lisp.
;;;;
;;;; The core stdlib (Lisp/core.lisp) and Closette (Lisp/clos.lisp)
;;;; are already baked into the binary and loaded BEFORE this file
;;;; runs. So everything we need — defun, defmacro, defclass, format,
;;;; require, load, *load-path*, *modules* — is already in place.
;;;;
;;;; Out of the box this file does very little. Drop new .lisp files
;;;; next to it and add `(require :module)` lines below to have them
;;;; loaded automatically. Modules are loaded exactly once per
;;;; session (REQUIRE checks *modules*).

;;; ─── Standard Library modules ────────────────────────────────────────────
;;; Tier-1-and-beyond modules layered on top of core+CLOS. Each
;;; lives in its own Library/foo.lisp and is loaded once via
;;; (require ...). Order matters where dependencies exist.

(require 'streams)                       ; string-output-stream, with-output-to-string
(require 'conditions)                    ; define-condition, restart-case, typed handler-case
(require 'loop)                          ; extended LOOP macro
(require 'sequences)                     ; generic position/find/count/etc.
(require 'trees)                         ; subst, sublis, tree-equal, copy-tree, revappend
(require 'characters)                    ; char-code/upcase/case-p, char= family, name-char
(require 'types)                         ; subtypep + type hierarchy helpers
(require 'symbols)                       ; symbol-plist/get/remprop, destructuring-bind, prog1/2
(require 'lists)                         ; mapl/maplist/mapcan/mapcon, pairlis/acons, tailp/ldiff
(require 'places)                        ; (setf first/rest/cddr/...) + defsetf short form
(require 'numbers)                       ; polymorphic floor/ceiling/round/truncate/mod/rem
(require 'boole)                         ; boole-* constants + boole fn + derived logops
(require 'bits)                          ; byte/ldb/dpb/mask-field/deposit-field
(require 'hash-tables)                   ; with-hash-table-iterator, sxhash
;; xp is loaded on demand — see lazy stubs below
(require 'strings)                       ; full string library: trim, cmp, make-string, probe-file
(require 'describe)                      ; (describe obj) — REPL inspection
(require 'advice)                        ; (advise fn args body), (unadvise fn)
(require 'events)                        ; iGui event-loop / with-events-from
(require 'hot-reload)                    ; (start-hot-reload), (check-reloads)

;;; Windows FFI surface (docs/WINDOWS_FFI.md). Only meaningful when
;;; the driver was started with --windows; we still load the
;;; threading shim either way so user code that uses (on-ui-thread …)
;;; gets a clear error if the surface is off, rather than an
;;; "unbound function" mystery. The conditional guards the Win32
;;; binding modules — those are deferred to per-namespace require.
(when (windows-enabled-p)
  (require 'win32-threading)            ; (on-ui-thread …), (post-to-ui-thread …)
  (require 'win32-buffer)               ; foreign buffers + defstruct-win32
  (require 'win32)                      ; (win32 …), (defwin32 …)
  (require 'win32-callback))            ; define-win32-callback for WNDPROC etc.

;;; Example user-side hook: load a personal utilities module if
;;; present. Uncomment and rename to taste.
;; (require 'my-utils)

;;; ─── Lazy-load stubs ─────────────────────────────────────────────────────
;;; xp.lisp is NOT auto-loaded at startup (it contributes ~45% of JIT time).
;;; These thin stubs demand-load it on first call to any pprint entry point.
;;; After (require 'xp) runs, every symbol in this block is redefined by xp
;;; and subsequent calls go directly to the real implementations.

(defun pprint (object &optional stream)
  "Pretty-print OBJECT.  Loads xp on first call."
  (require 'xp)
  (pprint object stream))

(defun pprint-fill (stream list &optional (colon? t) atsign?)
  "Fill-style pretty-print.  Loads xp on first call."
  (require 'xp)
  (pprint-fill stream list colon? atsign?))

(defun pprint-linear (stream list &optional (colon? t) atsign?)
  "Linear-style pretty-print.  Loads xp on first call."
  (require 'xp)
  (pprint-linear stream list colon? atsign?))

(defun pprint-tabular (stream list &optional (colon? t) atsign? tabsize)
  "Tabular-style pretty-print.  Loads xp on first call."
  (require 'xp)
  (pprint-tabular stream list colon? atsign? tabsize))

;;; A user can verify the loader picked this file up by checking
;;; *modules* and *load-path* at the REPL.
(provide 'init)

nil
