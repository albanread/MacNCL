;;;; demos/hellowin.lisp — NCL port of the Petzold "Hello, Windows 95!"
;;;;
;;;; A complete Win32 application written in Lisp using the NCL FFI
;;;; surface (Phases 1–6 of docs/WINDOWS_FFI.md):
;;;;
;;;;   --windows                       — UI thread on thread 0
;;;;   (on-ui-thread …)                — marshal calls to thread 0
;;;;   (win32 "X" …)                   — call any Win32 export by name
;;;;   (defstruct-win32 …)             — WNDCLASSEXW, PAINTSTRUCT layouts
;;;;   (define-win32-callback …)       — Lisp WndProc as a real C fn pointer
;;;;
;;;; Run with:
;;;;
;;;;   ncl --windows -l demos/hellowin.lisp
;;;;
;;;; Constants and the original demo shape come from
;;;; E:/CL/cormanlisp/Sys/win32.lisp + E:/CL/cormanlisp/examples/hellowin.lisp
;;;; (the canonical Petzold port Roger Corman shipped). We borrow the
;;;; values; the structure is rewritten to fit NCL's FFI shape.

;; ── Win32 constants we need ─────────────────────────────────────────────
;; (Taken from Sys/win32.lisp in the Corman tree. Most have a Windows
;; SDK origin going back to Win 3.x; the values are stable.)

(defconstant +wm-create+              1)
(defconstant +wm-destroy+             2)
(defconstant +wm-paint+              15)

(defconstant +ws-overlapped+          0)
(defconstant +ws-caption+    #x00C00000)
(defconstant +ws-sysmenu+    #x00080000)
(defconstant +ws-thickframe+ #x00040000)
(defconstant +ws-minimizebox+ #x00020000)
(defconstant +ws-maximizebox+ #x00010000)
(defconstant +ws-overlappedwindow+
  (logior +ws-overlapped+ +ws-caption+ +ws-sysmenu+ +ws-thickframe+
          +ws-minimizebox+ +ws-maximizebox+))            ; #x00CF0000
(defconstant +ws-visible+    #x10000000)

(defconstant +sw-show+                5)

(defconstant +cs-vredraw+             1)
(defconstant +cs-hredraw+             2)

(defconstant +cw-usedefault+ #x80000000)

(defconstant +color-window+           5)              ; brush index + 1 below
(defconstant +idc-arrow+          32512)              ; standard arrow cursor
(defconstant +idi-application+    32512)              ; default app icon

(defconstant +dt-center+           #x01)
(defconstant +dt-vcenter+          #x04)
(defconstant +dt-singleline+       #x20)

(defconstant +null-handle+            0)

;; ── Foreign struct layouts ──────────────────────────────────────────────
;; WNDCLASSEXW — x64 layout, 80 bytes total.
(defstruct-win32 WNDCLASSEXW
  (cb-size            :u32      0)
  (style              :u32      4)
  (lpfn-wndproc       :ptr      8)
  (cb-cls-extra       :i32     16)
  (cb-wnd-extra       :i32     20)
  (h-instance         :handle  24)
  (h-icon             :handle  32)
  (h-cursor           :handle  40)
  (hbr-background     :handle  48)
  (lpsz-menu-name     :ptr     56)
  (lpsz-class-name    :ptr     64)
  (h-icon-sm          :handle  72)
  :size 80)

;; PAINTSTRUCT — x64 layout, 72 bytes total (with 32-byte reserved tail).
;; We only read hdc; the rest is opaque to user code.
(defstruct-win32 PAINTSTRUCT
  (hdc                :handle   0)
  (f-erase            :i32      8)
  (rc-paint-left      :i32     12)
  (rc-paint-top       :i32     16)
  (rc-paint-right     :i32     20)
  (rc-paint-bottom    :i32     24)
  :size 72)

;; ── Helpers ─────────────────────────────────────────────────────────────

(defun wide-string-buf (s)
  "Allocate a foreign UTF-16 buffer holding S with a trailing NUL,
   return the buffer pointer (a fixnum). Caller is responsible for
   keeping the pointer alive at least as long as Win32 might read
   from it — for class names and window text the class registration
   stores it indefinitely, so for this demo we deliberately leak."
  (let* ((bytes (* 2 (+ (length s) 1)))
         (buf   (make-foreign-buffer bytes)))
    (buffer-write-wstring buf 0 s)
    buf))

;; ── The WndProc ─────────────────────────────────────────────────────────
;; Defined as a Lisp closure → JIT-emitted Win32 trampoline →
;; lpfnWndProc slot of WNDCLASSEXW. Windows calls this back on the
;; UI thread for every message routed to our window.

(define-win32-callback hellowin-wndproc (hwnd msg wparam lparam)
  (declare (ignore wparam))
  (cond
    ((= msg +wm-destroy+)
     (win32 "PostQuitMessage" 0)
     0)
    ((= msg +wm-paint+)
     (let ((ps   (make-paintstruct))
           (rect (make-rect)))
       (let ((hdc (win32 "BeginPaint" hwnd ps)))
         (win32 "GetClientRect" hwnd rect)
         (win32 "DrawTextW"
                hdc
                "Hello, Windows 95! — from NCL"
                -1                     ; cchText: -1 = use NUL terminator
                rect
                (logior +dt-singleline+ +dt-center+ +dt-vcenter+))
         (win32 "EndPaint" hwnd ps))
       (free-paintstruct ps)
       (free-rect rect)
       0))
    (t
     (win32 "DefWindowProcW" hwnd msg wparam lparam))))

;; ── Main entry ──────────────────────────────────────────────────────────

(defun hellowin ()
  "Register the window class, create the window, show it. Returns
   the window's HWND. Control then returns to the caller; thread 0
   keeps pumping messages, so the window stays responsive until the
   user closes it (which posts WM_QUIT and unblocks the pump)."
  (on-ui-thread
    (let* ((class-name-buf (wide-string-buf "NCLHelloWin"))
           (window-name    "Hello — NCL FFI demo")
           (h-instance     (win32 "GetModuleHandleW" 0))
           (h-cursor       (win32 "LoadCursorW" 0 +idc-arrow+))
           (h-icon         (win32 "LoadIconW"   0 +idi-application+))
           (wc             (make-wndclassexw)))
      ;; Fill the WNDCLASSEXW.
      (setf (wndclassexw-cb-size wc)         80)
      (setf (wndclassexw-style wc)
            (logior +cs-hredraw+ +cs-vredraw+))
      (setf (wndclassexw-lpfn-wndproc wc)    (hellowin-wndproc))
      (setf (wndclassexw-cb-cls-extra wc)    0)
      (setf (wndclassexw-cb-wnd-extra wc)    0)
      (setf (wndclassexw-h-instance wc)      h-instance)
      (setf (wndclassexw-h-icon wc)          h-icon)
      (setf (wndclassexw-h-cursor wc)        h-cursor)
      (setf (wndclassexw-hbr-background wc)  (+ +color-window+ 1))
      (setf (wndclassexw-lpsz-menu-name wc)  0)
      (setf (wndclassexw-lpsz-class-name wc) class-name-buf)
      (setf (wndclassexw-h-icon-sm wc)       h-icon)

      (let ((atom (win32 "RegisterClassExW" wc)))
        (when (zerop atom)
          (error "RegisterClassExW failed (last-error ~A)"
                 (win32 "GetLastError"))))

      ;; Create the window. lpClassName argument can be either the
      ;; class atom or the string — we pass the same buffer we
      ;; registered with.
      (let ((hwnd (win32 "CreateWindowExW"
                         0                                ; dwExStyle
                         class-name-buf                   ; lpClassName (ptr to wide str)
                         window-name                      ; lpWindowName (auto-marshalled to wstr)
                         (logior +ws-overlappedwindow+ +ws-visible+)
                         +cw-usedefault+ +cw-usedefault+  ; X, Y
                         480 280                          ; nWidth, nHeight
                         +null-handle+                    ; hWndParent
                         +null-handle+                    ; hMenu
                         h-instance                       ; hInstance
                         0)))                             ; lpParam
        (when (zerop hwnd)
          (error "CreateWindowExW failed (last-error ~A)"
                 (win32 "GetLastError")))
        (win32 "ShowWindow" hwnd +sw-show+)
        (win32 "UpdateWindow" hwnd)
        (format t "hellowin: window created, hwnd=~A~%" hwnd)
        hwnd))))

;; Run it. The window is created on the UI thread, which keeps
;; pumping its message loop. The worker (this thread) then blocks
;; on a Sleep loop polling for the window to close.

(format t "Starting hellowin — close the window to exit.~%")
(force-output)

(let ((hwnd (hellowin)))
  ;; Wait for the window to be destroyed. IsWindow returns 0 once
  ;; the HWND has been removed from Windows' internal table (which
  ;; happens after WM_DESTROY is fully processed). The 50ms sleep
  ;; keeps the polling cheap.
  (loop
    (when (zerop (win32 "IsWindow" hwnd)) (return nil))
    (win32 "Sleep" 50)))

(format t "hellowin: window closed, exiting.~%")
nil
