;;;; Lisp/Library/win32-buffer.lisp
;;;;
;;;; defstruct-win32 — declare a C-shaped record laid out at fixed
;;;; offsets in a foreign buffer. Phase 5 of docs/WINDOWS_FFI.md.
;;;;
;;;; Why this exists
;;;; ───────────────
;;;; Many Win32 functions take or return pointers to struct types
;;;; (RECT, POINT, MSG, WNDCLASSEXW, PAINTSTRUCT, …). The C ABI
;;;; says "allocate one of these, pass me the pointer, I'll fill in
;;;; fields at known offsets." Lisp needs:
;;;;
;;;;   - a way to allocate a sized chunk of memory (MAKE-FOREIGN-BUFFER)
;;;;   - typed reads/writes at offsets (BUFFER-REF-X / BUFFER-SET-X)
;;;;
;;;; Those are the Rust shims this file builds on. defstruct-win32
;;;; turns
;;;;
;;;;     (defstruct-win32 RECT
;;;;       (left   :i32  0)
;;;;       (top    :i32  4)
;;;;       (right  :i32  8)
;;;;       (bottom :i32 12)
;;;;       :size 16)
;;;;
;;;; into the obvious set of accessors + constructor:
;;;;
;;;;   (defun rect-size () 16)
;;;;   (defun make-rect () (let ((p (make-foreign-buffer 16)))
;;;;                         (buffer-zero p 16)
;;;;                         p))
;;;;   (defun rect-free (p) (free-foreign-buffer p 16))
;;;;   (defun rect-left (p) (buffer-ref-i32 p 0))
;;;;   (defun (setf rect-left) (v p) (buffer-set-i32 p 0 v) v)
;;;;   …
;;;;
;;;; Memory management
;;;; ─────────────────
;;;; Foreign buffers are NOT garbage collected. The user calls
;;;; `(rect-free r)` (or the generic `(free-foreign-buffer p size)`)
;;;; when done. Phase 6 may add `(with-rect r body …)` macro sugar
;;;; for RAII; for now the discipline is manual.

(provide 'win32-buffer)

;;; ── Field-tag → ref/set primitive selection ───────────────────────

(defun %win32-field-getter (tag)
  "Return the symbol of the buffer-ref-X primitive for TAG."
  (case tag
    (:i8     'buffer-ref-i8)
    (:u8     'buffer-ref-u8)
    (:i16    'buffer-ref-i16)
    (:u16    'buffer-ref-u16)
    (:i32    'buffer-ref-i32)
    (:u32    'buffer-ref-u32)
    (:i64    'buffer-ref-i64)
    (:u64    'buffer-ref-u64)
    (:isize  'buffer-ref-i64)    ; pointer-sized on x64
    (:usize  'buffer-ref-u64)
    (:handle 'buffer-ref-ptr)
    (:ptr    'buffer-ref-ptr)
    (:bool   'buffer-ref-i32)    ; Win32 BOOL is i32
    (t (error "defstruct-win32: no getter for field tag ~A" tag))))

(defun %win32-field-setter (tag)
  "Return the symbol of the buffer-set-X primitive for TAG."
  (case tag
    (:i8     'buffer-set-i8)
    (:u8     'buffer-set-u8)
    (:i16    'buffer-set-i16)
    (:u16    'buffer-set-u16)
    (:i32    'buffer-set-i32)
    (:u32    'buffer-set-u32)
    (:i64    'buffer-set-i64)
    (:u64    'buffer-set-u64)
    (:isize  'buffer-set-i64)
    (:usize  'buffer-set-u64)
    (:handle 'buffer-set-ptr)
    (:ptr    'buffer-set-ptr)
    (:bool   'buffer-set-i32)
    (t (error "defstruct-win32: no setter for field tag ~A" tag))))

;;; ── Parse the field list ──────────────────────────────────────────

(defun %win32-parse-fields (spec)
  "Walk SPEC = ((NAME TAG OFFSET) … :size N) and return
   (values FIELDS SIZE) where FIELDS is the parsed list and SIZE
   is the explicit total size.

   Note: NCL's (return …) inside a (when …) inside a (loop …)
   doesn't synchronously exit the loop, so we use an explicit
   (block …) + (return-from …) instead."
  (let ((fields nil) (size nil) (rest spec))
    (block parse-loop
      (loop
        (cond
          ((null rest) (return-from parse-loop nil))
          ((eq (car rest) :size)
           (setq size (cadr rest))
           (setq rest (cddr rest)))
          ((consp (car rest))
           (push (car rest) fields)
           (setq rest (cdr rest)))
          (t (error "defstruct-win32: stray atom in spec: ~A" (car rest))))))
    (values (nreverse fields)
            (or size
                (error "defstruct-win32: :size N is required")))))

;;; ── The macro ─────────────────────────────────────────────────────

(defmacro defstruct-win32 (name &rest fields-and-options)
  "Declare a C-shaped record NAME with fields and explicit :size.

   Each FIELD is (FIELD-NAME TAG OFFSET) where TAG is one of the
   FFI type keywords (:i32 :u32 :handle :ptr …) and OFFSET is the
   byte offset within the struct.

   Generates:
     (NAME-size)               → struct size in bytes
     (make-NAME)               → fresh zeroed foreign buffer
     (free-NAME ptr)           → deallocate (matches make-NAME size)
     (NAME-FIELD ptr)          → field read
     (setf (NAME-FIELD ptr) v) → field write

   Example:
     (defstruct-win32 RECT
       (left   :i32  0)
       (top    :i32  4)
       (right  :i32  8)
       (bottom :i32 12)
       :size 16)

     (let ((r (make-rect)))
       (setf (rect-left r) 10)
       (rect-left r))         → 10"
  (multiple-value-bind (fields size)
      (%win32-parse-fields fields-and-options)
    (let* ((name-str (symbol-name name))
           ;; Build symbols manually with string-concat since we
           ;; can't rely on (concatenate) in early NCL. (NCL only
           ;; has the binary string-concat, not the n-ary
           ;; string-append we'd want — and string-append-char
           ;; works on a string + a single character.)
           (size-name  (intern (string-concat name-str "-SIZE")))
           (make-name  (intern (string-concat "MAKE-" name-str)))
           (free-name  (intern (string-concat "FREE-" name-str))))
      `(progn
         (defun ,size-name () ,size)
         (defun ,make-name ()
           (let ((p (make-foreign-buffer ,size)))
             p))                           ; alloc_zeroed already zeros
         (defun ,free-name (ptr)
           (free-foreign-buffer ptr ,size))
         ,@(let ((getters-setters nil))
             (dolist (field fields (nreverse getters-setters))
               (let* ((fname (car field))
                      (tag   (cadr field))
                      (off   (caddr field))
                      (accessor (intern (string-concat
                                         (string-concat name-str "-")
                                         (symbol-name fname))))
                      (getter-fn (%win32-field-getter tag))
                      (setter-fn (%win32-field-setter tag)))
                 (push `(defun ,accessor (ptr)
                          (,getter-fn ptr ,off))
                       getters-setters)
                 (push `(defun (setf ,accessor) (val ptr)
                          (,setter-fn ptr ,off val)
                          val)
                       getters-setters))))
         ',name))))

;;; ── Starter set: common Win32 structs ─────────────────────────────
;;;
;;; Layouts taken from windowsx.h + WinUser.h on x64.

;; RECT — used by GetClientRect, SetWindowRect, DrawText, paint
;; ops, almost every layout function.
(defstruct-win32 RECT
  (left   :i32  0)
  (top    :i32  4)
  (right  :i32  8)
  (bottom :i32 12)
  :size 16)

;; POINT — mouse coords, GetCursorPos, ClientToScreen, …
(defstruct-win32 POINT
  (x :i32 0)
  (y :i32 4)
  :size 8)

;; SIZE — window/client size returns
(defstruct-win32 SIZE
  (cx :i32 0)
  (cy :i32 4)
  :size 8)

;; MSG — the heart of the Windows message pump.
;; struct MSG { HWND, UINT, WPARAM, LPARAM, DWORD time, POINT pt }
;; x64 layout: HWND@0 (8), message@8 (4) + pad to 16, wParam@16 (8),
;; lParam@24 (8), time@32 (4) + pad, pt.x@40 (4), pt.y@44 (4),
;; total 48 bytes.
(defstruct-win32 MSG
  (hwnd     :handle  0)
  (message  :u32     8)
  (wparam   :u64    16)
  (lparam   :u64    24)
  (time     :u32    32)
  (pt-x     :i32    40)
  (pt-y     :i32    44)
  :size 48)

nil
