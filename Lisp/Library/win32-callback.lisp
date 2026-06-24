;;;; Lisp/Library/win32-callback.lisp
;;;;
;;;; Win32 callbacks — Lisp closures usable as C function pointers.
;;;; Phase 6 of docs/WINDOWS_FFI.md.
;;;;
;;;; Why this exists
;;;; ───────────────
;;;; Win32 APIs like RegisterClassExW, EnumWindows, SetTimer,
;;;; SetWindowsHookEx accept function pointers. The OS calls those
;;;; functions back at a time IT chooses, with a specific argument
;;;; signature.
;;;;
;;;; We can't pass a Lisp closure directly — Win32 wants a plain C
;;;; function pointer at the ABI level. So this file sits on top of
;;;; the (%make-win32-callback closure arity) shim, which:
;;;;
;;;;   1. Registers the closure with the runtime's callback table
;;;;   2. JIT-emits a tiny trampoline (via LLVM, same machinery as
;;;;      defun bodies) that accepts the Win32-shaped args, packs
;;;;      them into a u64 array, and calls the closure
;;;;   3. Returns the trampoline's raw function-pointer address
;;;;
;;;; The trampoline lives for the process lifetime.
;;;;
;;;; Surface
;;;; ───────
;;;;
;;;;   (define-win32-callback NAME (PARAMS...) BODY...)
;;;;     ;; Defines NAME as a 0-arg function returning the
;;;;     ;; trampoline pointer for a closure of the given shape.
;;;;     ;; Conventionally bound once at top level so the pointer
;;;;     ;; is stable.
;;;;
;;;;   (make-win32-callback (PARAMS...) BODY...)
;;;;     ;; Anonymous version — returns the trampoline pointer
;;;;     ;; directly. Each call creates a new trampoline.
;;;;
;;;; Example: a WNDPROC that handles paint + destroy and defers
;;;; everything else to DefWindowProc:
;;;;
;;;;   (define-win32-callback my-wnd-proc (hwnd msg wparam lparam)
;;;;     (cond
;;;;       ((= msg WM_DESTROY)
;;;;        (win32 "PostQuitMessage" 0)
;;;;        0)
;;;;       (t
;;;;        (win32 "DefWindowProcW" hwnd msg wparam lparam))))
;;;;
;;;;   ;; ... build a WNDCLASSEXW with (my-wnd-proc) as lpfnWndProc ...

(provide 'win32-callback)

(defun make-win32-callback (params body-fn)
  "Construct a Win32 callback from PARAMS (a list of param symbols)
   and BODY-FN (a closure of the matching arity). Returns the
   trampoline's function-pointer address as a fixnum.

   Most user code uses the (define-win32-callback …) macro instead
   of calling this directly; the macro takes care of the closure
   construction."
  (%make-win32-callback body-fn (length params)))

(defmacro define-win32-callback (name params &rest body)
  "Declare NAME as a 0-arg function that returns a Win32-callable
   function pointer for a closure with PARAMS and BODY.

   NCL closures are static-area allocated, so the trampoline's
   underlying closure Word is stable for the process lifetime and
   safe to embed in Win32 data structures.

   Expansion uses a top-level (progn (defparameter …) (defun …))
   because NCL only allows defun at top level — wrapping the defun
   in a (let …) would be rejected by the lowerer.

   Example:
     (define-win32-callback hello-wnd-proc (hwnd msg wparam lparam)
       (cond ((= msg WM_DESTROY) (win32 \"PostQuitMessage\" 0) 0)
             (t (win32 \"DefWindowProcW\" hwnd msg wparam lparam))))

     ;; Call (hello-wnd-proc) once at startup; cache the result."
  (let ((closure-sym (gensym "CB-CLOSURE"))
        (cached-var  (intern (string-concat "%CB-CACHED-"
                                            (symbol-name name)))))
    `(progn
       (defparameter ,cached-var nil)
       (defun ,name ()
         (or ,cached-var
             (let ((,closure-sym (lambda ,params ,@body)))
               (setq ,cached-var
                     (%make-win32-callback ,closure-sym
                                           ,(length params)))
               ,cached-var))))))

nil
