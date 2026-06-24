;;;; Lisp/Library/win32.lisp
;;;;
;;;; Idiomatic surface for the Win32 FFI. Phase 4 of
;;;; docs/WINDOWS_FFI.md. Auto-required by init.lisp when
;;;; (windows-enabled-p).
;;;;
;;;; The metadata pack (loaded at --windows startup by the driver)
;;;; holds dll/args/ret signatures for ~14K Win32 functions. This
;;;; file exposes two idioms on top of it:
;;;;
;;;;   (win32 NAME args...)            cold path. One lookup +
;;;;                                   one FFI call per invocation.
;;;;                                   Good for one-shot calls.
;;;;
;;;;   (defwin32 LISP-NAME WIN32-NAME) declare a binding at
;;;;                                   macroexpand time. Bakes the
;;;;                                   signature into a regular
;;;;                                   defun; runtime cost == hand-
;;;;                                   written binding. Good for
;;;;                                   hot paths / library code.
;;;;
;;;; Both drop down to the same %ffi-call kernel. defwin32 also
;;;; respects :route :ui by wrapping the call in (on-ui-thread ...)
;;;; if the metadata says so.

(provide 'win32)

;;; -- Cold path: one-shot dynamic dispatch --

(defun win32 (name &rest args)
  "Dispatch a Win32 call by name. NAME is a string (Win32 names are
   case-sensitive — use the exact spelling from the SDK headers).
   Looks up NAME in the metadata pack, marshals ARGS per its
   signature, and invokes. Errors if NAME isn't in the pack."
  (apply #'%win32-call name args))

;;; -- Hot path: macro-time binding --

(defun %defwin32-params (n)
  "Generate N gensym'd parameter symbols for a (defwin32 …)
   expansion. NCL's (return …) inside (when …) inside (loop …)
   doesn't synchronously break, so we use an explicit block +
   return-from."
  (let ((acc nil) (i 0))
    (block param-loop
      (loop
        (cond ((>= i n) (return-from param-loop (nreverse acc)))
              (t (push (gensym "A") acc)
                 (setq i (+ i 1))))))))

(defmacro defwin32 (lisp-name win32-name)
  "Declare a Lisp binding for the Win32 function named WIN32-NAME,
   accessible as LISP-NAME. Reads metadata at macroexpand time and
   bakes the signature into a regular defun. Runtime cost is
   identical to a hand-written %ffi-call binding.

   Errors at macro time if the metadata pack isn't loaded or
   WIN32-NAME isn't in it."
  (let ((meta (%win32-lookup win32-name)))
    (unless meta
      (error "defwin32: ~A not in metadata pack (--windows not set, or unknown name)"
             win32-name))
    (let* ((dll      (getf meta :dll))
           (arg-tags (getf meta :args))
           (ret-tag  (getf meta :ret))
           (route    (getf meta :route))
           (params   (%defwin32-params (length arg-tags))))
      (if (eq route :ui)
          `(defun ,lisp-name ,params
             (on-ui-thread
               (%ffi-call ,dll ,win32-name ',arg-tags ,ret-tag ,@params)))
          `(defun ,lisp-name ,params
             (%ffi-call ,dll ,win32-name ',arg-tags ,ret-tag ,@params))))))

nil
