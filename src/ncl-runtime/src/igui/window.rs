//! MDI frame window, MDI client child, message pump, and the
//! cross-thread helpers used by `iGui.OpenChild` / `CloseChild` /
//! `SetTitle`.
//!
//! Window-creation operations issued by the language thread are
//! marshalled to the GUI thread via private `WM_USER` messages and
//! `SendMessageW`, which blocks until the WndProc returns. This
//! preserves the iGui rule that all HWND ownership lives on the GUI
//! thread without forcing a typed RPC between the two.

#![cfg(windows)]

use std::ptr;
use std::sync::OnceLock;
use std::sync::Mutex;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePatternBrush,
    CreateSolidBrush, DeleteDC, DeleteObject, FillRect as GdiFillRect, GetDC, ReleaseDC,
    SelectObject, SetBkMode, SetTextColor, TextOutW,
    BACKGROUND_MODE, FONT_CHARSET, FONT_CLIP_PRECISION, FONT_OUTPUT_PRECISION,
    FONT_QUALITY, HBITMAP, HBRUSH, HDC, HFONT, HGDIOBJ,
    TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, VK_CAPITAL, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, CreateIconIndirect, CreateWindowExW, DefFrameProcW, DefWindowProcW,
    DispatchMessageW, GetClientRect, GetMessageTime, GetMessageW, GetWindowLongPtrW,
    LoadCursorW, PostMessageW, PostQuitMessage, RegisterClassExW, SendMessageW,
    SetWindowLongPtrW, ShowWindow, TranslateAcceleratorW, TranslateMessage,
    CLIENTCREATESTRUCT, CW_USEDEFAULT, GWLP_WNDPROC, HACCEL, ICONINFO, IDC_ARROW,
    MDICREATESTRUCTW, MSG, SW_SHOW, WHEEL_DELTA, WM_CHAR, WM_CLOSE,
    WM_COMMAND, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MDICREATE, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETFOCUS, WM_SETICON, WM_SIZE, WM_SYSCOLORCHANGE,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WM_THEMECHANGED, WM_USER, WNDCLASSEXW, WNDCLASS_STYLES, WS_CHILD,
    WS_CLIPCHILDREN, WS_EX_APPWINDOW, WS_HSCROLL, WS_OVERLAPPEDWINDOW, WS_VISIBLE, WS_VSCROLL,
};

use super::channels::{self, modifier, mouse_op, IGuiEvent};
use super::child::{self, MdiBootstrap, MDI_CHILD_CLASS};
use super::cp_exports::FRAME_HWND;
use super::registry;
use super::renderer;
use super::IGuiError;

const FRAME_CHILD_ID: i64 = 1;
const FRAME_CLASS: PCWSTR = w!("NewCL.iGui.Frame");

// Private messages used to marshal language-thread calls onto the GUI
// thread. lparam is the address of the corresponding *Request struct,
// which the WndProc reads, mutates, and returns 0; the SendMessageW
// caller reads its own request struct on return.
const WM_IGUI_OPEN_CHILD: u32 = WM_USER + 1;
const WM_IGUI_CLOSE_CHILD: u32 = WM_USER + 2;
const WM_IGUI_SET_TITLE: u32 = WM_USER + 3;
const WM_IGUI_SET_MENU: u32 = WM_USER + 4;
const WM_IGUI_MDI_VERB: u32 = WM_USER + 5;
/// Open a built-in text-view MDI child. Like WM_IGUI_OPEN_CHILD but
/// the child class is `text_view`'s, with its own WndProc + grid
/// state. Routed through the frame so the WM_MDICREATE call lands
/// on the GUI thread.
const WM_IGUI_OPEN_TEXT: u32 = WM_USER + 7;
/// Drain the pending text-command queue for a text-view child onto
/// its grid, then invalidate. wparam carries the child_id. Both
/// queue-drain and InvalidateRect run on the GUI thread inside the
/// frame's WndProc — the language thread sees nothing past `child_id`
/// as an opaque token. Posted (not sent) so a tight write loop
/// doesn't block on the GUI thread.
const WM_IGUI_TEXT_FLUSH: u32 = WM_USER + 8;
/// Open a REPL MDI child. Like WM_IGUI_OPEN_TEXT but uses the
/// REPL class (split-pane D2D renderer with input editor).
const WM_IGUI_OPEN_REPL: u32 = WM_USER + 9;
/// Posted (from any thread) when a new crash dump is available.
/// Frame WndProc opens/invalidates the crash_view MDI child.
pub(super) const WM_IGUI_CRASH_FLUSH: u32 = WM_USER + 10;
/// Open a Markdown doc-pane MDI child. Like WM_IGUI_OPEN_TEXT but
/// the child class is `doc_pane`'s, with the docpane (Direct2D
/// Markdown + Mermaid) renderer.
const WM_IGUI_OPEN_DOC: u32 = WM_USER + 11;
/// Invalidate a doc-pane child so WM_PAINT re-reads its Markdown
/// source. wparam carries the child_id. Posted (not sent) so a
/// streaming write loop on the language thread doesn't block on the
/// GUI thread.
const WM_IGUI_DOC_FLUSH: u32 = WM_USER + 12;
/// Open a Help-pane MDI child — a read-only documentation browser
/// over a folder of `.md` files, with sidebar + link navigation.
/// Distinct from WM_IGUI_OPEN_DOC: that one opens a Lisp-scriptable
/// single-document pane; this one carries a `path` (folder or .md
/// file) and never accepts streamed updates.
const WM_IGUI_OPEN_HELP: u32 = WM_USER + 13;
/// Sent from the language thread to a render-host HWND to install
/// or clear a Win32 timer driving `EvTick` events.
/// `wparam` carries the interval in ms (0 = clear), `lparam` is unused.
pub(crate) const WM_IGUI_SET_TIMER: u32 = WM_USER + 6;
/// Win32 timer id used by the redraw-rate ticker. One timer per
/// render host; reusing the same id replaces the previous one.
pub(crate) const TICK_TIMER_ID: usize = 0xA1;

/// Base WM_COMMAND id for auto-assigned Demos menu items.
/// Range 0x4000..=0x4FFF (4096 slots) — well above all other ranges.
const DEMO_CMD_BASE: u16 = 0x4000;
const DEMO_CMD_END:  u16 = 0x4FFF;

/// HWND of the MDI client. Set by `run` after `CreateWindowExW`.
static MDI_CLIENT: Mutex<Option<isize>> = Mutex::new(None);
static GUI_THREAD_ID: OnceLock<u32> = OnceLock::new();

/// Original WNDPROC of the MDICLIENT, saved before we replace it so
/// our subclass can forward unhandled messages correctly.
static MDICLIENT_ORIG_PROC: OnceLock<isize> = OnceLock::new();
/// λ brush handle (raw isize) kept alive for the process lifetime.
static LAMBDA_BRUSH_RAW: OnceLock<isize> = OnceLock::new();

/// Discovered demo files: (menu_id, display_name, file_path).
/// Populated once in `run()` before the menu bar is built.
static DEMO_FILES: OnceLock<Vec<(u16, String, std::path::PathBuf)>> = OnceLock::new();

// ── Lambda background brush ────────────────────────────────────────────────

/// Color helpers: COLORREF = R | (G<<8) | (B<<16).
const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16))
}

/// Build an 80×80 GDI pattern brush: dark-slate navy background with a
/// barely-lighter italic λ (U+03BB) tiled at two diagonal offsets per
/// cell.  The half-brick offset creates a continuous diagonal lattice.
///
/// Called once on the GUI thread immediately after MDICLIENT is created.
/// The returned HBRUSH lives for the process lifetime.
unsafe fn make_lambda_brush() -> HBRUSH {
    const TILE: i32 = 80;

    // Background: deep navy-slate  #1C2834
    const BG: COLORREF = rgb(28, 40, 52);
    // Lambda glyph: ~55 units brighter per channel — subtle but legible
    const FG: COLORREF = rgb(58, 80, 104);

    // All GDI calls are unsafe; group them in one block so Rust 2024's
    // "unsafe in unsafe fn" lint is satisfied without scattering blocks.
    unsafe {
        // Build bitmap on a screen-compatible DC.
        let screen_dc = GetDC(None);
        let mem_dc = CreateCompatibleDC(Some(screen_dc));
        let bmp: HBITMAP = CreateCompatibleBitmap(screen_dc, TILE, TILE);
        let old_bmp: HGDIOBJ = SelectObject(mem_dc, HGDIOBJ(bmp.0));

        // Fill solid background.
        let bg_brush: HBRUSH = CreateSolidBrush(BG);
        let tile_rect = RECT { left: 0, top: 0, right: TILE, bottom: TILE };
        GdiFillRect(mem_dc, &tile_rect, bg_brush);
        DeleteObject(HGDIOBJ(bg_brush.0));

        // Draw λ with a thin italic Segoe UI — the slant echoes the
        // traditional hand-written Greek letter and looks elegant at small
        // sizes.  Two stamps per tile at (8,6) and (48,46) produce a
        // half-brick diagonal repeat when the brush is tiled.
        SetBkMode(mem_dc, BACKGROUND_MODE(TRANSPARENT.0));
        SetTextColor(mem_dc, FG);

        let font: HFONT = CreateFontW(
            28, 0,                        // height (cell height), width (auto)
            0, 0,                         // escapement, orientation
            100,                          // weight: FW_THIN
            1, 0, 0,                      // italic, no underline, no strikeout
            FONT_CHARSET(1),              // DEFAULT_CHARSET
            FONT_OUTPUT_PRECISION(0),     // OUT_DEFAULT_PRECIS
            FONT_CLIP_PRECISION(0),       // CLIP_DEFAULT_PRECIS
            FONT_QUALITY(5),              // CLEARTYPE_QUALITY
            32u32,                        // FF_SWISS (sans-serif)
            w!("Segoe UI"),
        );
        let old_font: HGDIOBJ = SelectObject(mem_dc, HGDIOBJ(font.0));

        // U+03BB λ — one UTF-16 codepoint (BMP, no surrogate needed).
        let lambda: &[u16] = &[0x03BB_u16];
        let _ = TextOutW(mem_dc,  8,  6, lambda); // top-left stamp
        let _ = TextOutW(mem_dc, 48, 46, lambda); // bottom-right stamp (half-brick)

        SelectObject(mem_dc, old_font);
        DeleteObject(HGDIOBJ(font.0));
        SelectObject(mem_dc, old_bmp);

        // Pattern brush tiles the bitmap seamlessly.
        let brush: HBRUSH = CreatePatternBrush(bmp);

        DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(None, screen_dc);

        brush
    }
}

/// Replacement WNDPROC for the MDICLIENT window.  Intercepts WM_ERASEBKGND
/// to paint the λ-tiled background; all other messages are forwarded to
/// the original MDICLIENT WndProc saved in MDICLIENT_ORIG_PROC.
unsafe extern "system" fn mdi_bg_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_ERASEBKGND {
        if let Some(&raw) = LAMBDA_BRUSH_RAW.get() {
            let hdc = HDC(wparam.0 as *mut _);
            let brush = HBRUSH(raw as *mut _);
            let mut rect = RECT::default();
            unsafe { let _ = GetClientRect(hwnd, &mut rect); }
            unsafe { GdiFillRect(hdc, &rect, brush); }
            return LRESULT(1); // background erased — suppress default erase
        }
    }
    // Forward everything else (and WM_ERASEBKGND if brush not ready) to
    // the original MDICLIENT WndProc.
    let orig_raw = MDICLIENT_ORIG_PROC.get().copied().unwrap_or(0);
    if orig_raw != 0 {
        // SAFETY: orig_raw was obtained from GetWindowLongPtrW(GWLP_WNDPROC)
        // immediately before installation and is a valid WNDPROC pointer.
        unsafe {
            let f: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT =
                std::mem::transmute(orig_raw);
            CallWindowProcW(Some(f), hwnd, msg, wparam, lparam)
        }
    } else {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }
}

pub(super) fn mdi_client_hwnd() -> Option<HWND> {
    let raw = MDI_CLIENT.lock().ok()?;
    raw.map(|r| HWND(r as *mut _))
}

/// Post WM_IGUI_CRASH_FLUSH to the frame from any thread.  Called
/// by crash_view::push() after a new dump is appended.
pub(super) fn post_crash_flush() {
    let Some(frame_raw) = super::cp_exports::FRAME_HWND.get() else {
        return;
    };
    let frame = HWND(*frame_raw as *mut _);
    let _ = unsafe {
        PostMessageW(
            Some(frame),
            WM_IGUI_CRASH_FLUSH,
            WPARAM(0),
            LPARAM(0),
        )
    };
}

/// Scan the Lisp/demos/ directory for *.lisp files and return
/// `(menu_id, display_name, path)` triples sorted by name.
/// Search order: `<exe>/demos/`  then `<exe>/../../Lisp/demos/` (dev).
fn discover_demos() -> Vec<(u16, String, std::path::PathBuf)> {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };
    let exe_dir = match exe.parent() {
        Some(d) => d.to_path_buf(),
        None => return Vec::new(),
    };
    let candidates: Vec<std::path::PathBuf> = vec![
        exe_dir.join("demos"),
        exe_dir.ancestors()
            .nth(2)
            .map(|p| p.join("Lisp").join("demos"))
            .unwrap_or_default(),
    ];
    for dir in &candidates {
        if !dir.is_dir() { continue; }
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        let mut files: Vec<std::path::PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("lisp"))
            .collect();
        files.sort();
        if files.is_empty() { continue; }
        return files.into_iter()
            .enumerate()
            .take((DEMO_CMD_END - DEMO_CMD_BASE + 1) as usize)
            .map(|(i, path)| {
                let stem = path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown");
                // "othello-gui" → "Othello Gui"
                let pretty: String = stem.split('-')
                    .map(|w| {
                        let mut ch = w.chars();
                        match ch.next() {
                            Some(f) => f.to_uppercase().collect::<String>() + ch.as_str(),
                            None => String::new(),
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                (DEMO_CMD_BASE + i as u16, pretty, path)
            })
            .collect();
    }
    Vec::new()
}

/// Build a 32×32 HICON containing the λ glyph on the same dark-slate
/// background used by the MDI pattern brush.  Sent to the frame via
/// WM_SETICON so the title bar and taskbar show the λ symbol.
unsafe fn make_lambda_icon() -> windows::Win32::UI::WindowsAndMessaging::HICON {
    use windows::Win32::Graphics::Gdi::CreateBitmap;
    use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, ICONINFO};

    const SZ: i32 = 32;
    const BG: COLORREF = rgb(28, 40, 52);
    // Brighter foreground so the icon is legible at 16×16 and 32×32.
    const FG: COLORREF = rgb(160, 190, 220);

    unsafe {
        let screen_dc = GetDC(None);
        let mem_dc = CreateCompatibleDC(Some(screen_dc));
        let bmp = CreateCompatibleBitmap(screen_dc, SZ, SZ);
        let old_bmp = SelectObject(mem_dc, HGDIOBJ(bmp.0));

        let bg_brush = CreateSolidBrush(BG);
        let rect = RECT { left: 0, top: 0, right: SZ, bottom: SZ };
        GdiFillRect(mem_dc, &rect, bg_brush);
        DeleteObject(HGDIOBJ(bg_brush.0));

        SetBkMode(mem_dc, BACKGROUND_MODE(TRANSPARENT.0));
        SetTextColor(mem_dc, FG);

        let font = CreateFontW(
            22, 0, 0, 0,
            400,            // FW_NORMAL
            1, 0, 0,        // italic, no underline, no strikeout
            FONT_CHARSET(1),
            FONT_OUTPUT_PRECISION(0),
            FONT_CLIP_PRECISION(0),
            FONT_QUALITY(5),
            32u32,          // FF_SWISS
            w!("Segoe UI"),
        );
        let old_font = SelectObject(mem_dc, HGDIOBJ(font.0));
        let lambda: &[u16] = &[0x03BB_u16];
        let _ = TextOutW(mem_dc, 5, 4, lambda);
        SelectObject(mem_dc, old_font);
        DeleteObject(HGDIOBJ(font.0));
        SelectObject(mem_dc, old_bmp);

        // AND mask: all zeros → fully opaque icon.
        let mask_bmp = CreateBitmap(SZ, SZ, 1, 1, None);
        let icon_info = ICONINFO {
            fIcon: windows::core::BOOL(1),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask_bmp,
            hbmColor: bmp,
        };
        let icon = CreateIconIndirect(&icon_info).unwrap_or_default();

        DeleteObject(HGDIOBJ(bmp.0));
        DeleteObject(HGDIOBJ(mask_bmp.0));
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(None, screen_dc);
        icon
    }
}

pub(crate) fn gui_thread_id() -> Option<u32> {
    GUI_THREAD_ID.get().copied()
}

// ── Help / documentation launcher ─────────────────────────────────────────

/// Open NCL's user-facing documentation in a *help-pane* MDI child.
///
/// The help pane is a full read-only docs browser: it scans the
/// chosen folder for `.md` files, builds a sidebar of pages, follows
/// in-page Markdown links, and renders Mermaid diagrams through
/// `docpane`. Distinct from the bare `doc_pane` (which is a single
/// Lisp-scriptable Markdown surface with no chrome).
///
/// Folder search order:
///   1. `<exe_dir>/docs/user/`           — production installation
///   2. `<exe_dir>/docs/`                — fallback if no `user/`
///   3. `<exe_dir>/../../docs/user/`     — dev build (exe under target/<profile>/)
///   4. `<exe_dir>/../../docs/`          — dev fallback
///   5. `CARGO_MANIFEST_DIR/../../docs/user/` — `cargo run` from anywhere
pub(crate) fn open_docs() {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));

    let docs_path: Option<std::path::PathBuf> = exe_dir
        .as_ref()
        .map(|d| d.join("docs").join("user"))
        .filter(|p| p.is_dir())
        .or_else(|| {
            exe_dir.as_ref().map(|d| d.join("docs")).filter(|p| p.is_dir())
        })
        .or_else(|| {
            exe_dir
                .as_ref()
                .and_then(|d| d.ancestors().nth(2))
                .map(|root| root.join("docs").join("user"))
                .filter(|p| p.is_dir())
        })
        .or_else(|| {
            exe_dir
                .as_ref()
                .and_then(|d| d.ancestors().nth(2))
                .map(|root| root.join("docs"))
                .filter(|p| p.is_dir())
        })
        .or_else(|| {
            let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            manifest
                .ancestors()
                .nth(2)
                .map(|r| r.join("docs").join("user"))
                .filter(|p| p.is_dir())
        });

    let Some(path) = docs_path else {
        eprintln!(
            "[docs] docs/ folder not found. Expected `docs/user/` or `docs/` \
             next to ncl.exe."
        );
        return;
    };
    let path_str = match path.to_str() {
        Some(s) => s.to_owned(),
        None => {
            eprintln!("[docs] docs path is not valid UTF-8: {}", path.display());
            return;
        }
    };
    if super::help_pane::open("NCL Documentation", &path_str).is_none() {
        eprintln!("[docs] failed to open help pane");
    }
}

/// Public entry point. Opens the iGui frame, sets up the MDI client,
/// runs the Win32 message pump until `WM_QUIT`, and returns the quit
/// code. If `worker` is provided, it is spawned on a background
/// thread once the frame is up.
pub fn run<F>(worker: Option<F>) -> Result<i32, IGuiError>
where
    F: FnOnce() + Send + 'static,
{
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let _ = GUI_THREAD_ID.set(unsafe { GetCurrentThreadId() });

    let h_instance = unsafe { GetModuleHandleW(None) }
        .map_err(|e| IGuiError::Win32(format!("GetModuleHandleW failed: {e}")))?
        .into();
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }
        .map_err(|e| IGuiError::Win32(format!("LoadCursorW failed: {e}")))?;

    // Frame class.
    let frame_class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(frame_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: h_instance,
        hIcon: Default::default(),
        hCursor: cursor,
        hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(ptr::null_mut()),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: FRAME_CLASS,
        hIconSm: Default::default(),
    };
    if unsafe { RegisterClassExW(&frame_class) } == 0 {
        return Err(IGuiError::Win32("RegisterClassExW (frame) returned 0".into()));
    }
    child::register_classes()?;
    super::crash_view::register_class()?;
    super::doc_pane::register_class()?;
    super::help_pane::register_class()?;

    // Renderer comes up before any window so child WM_NCCREATE can build
    // its swap chain immediately.
    renderer::install()?;

    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_APPWINDOW,
            FRAME_CLASS,
            w!("NCL"),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1024,
            720,
            None,
            None,
            Some(h_instance),
            None,
        )
    }
    .map_err(|e| IGuiError::Win32(format!("CreateWindowExW (frame) failed: {e}")))?;
    let _ = FRAME_HWND.set(hwnd.0 as isize);

    // MDI client occupies the whole frame body for now (no toolbar /
    // status bar yet).
    let mut frame_rect = RECT::default();
    unsafe { GetClientRect(hwnd, &mut frame_rect) }
        .map_err(|e| IGuiError::Win32(format!("GetClientRect (frame) failed: {e}")))?;
    let mut create = CLIENTCREATESTRUCT {
        hWindowMenu: Default::default(),
        idFirstChild: 0xCC00,
    };
    let mdi = unsafe {
        CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
            w!("MDICLIENT"),
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN | WS_HSCROLL | WS_VSCROLL,
            0,
            0,
            frame_rect.right - frame_rect.left,
            frame_rect.bottom - frame_rect.top,
            Some(hwnd),
            None,
            Some(h_instance),
            Some(&mut create as *mut _ as *mut _),
        )
    }
    .map_err(|e| IGuiError::Win32(format!("CreateWindowExW (MDICLIENT) failed: {e}")))?;
    {
        let mut slot = MDI_CLIENT.lock().expect("MDI_CLIENT mutex poisoned");
        *slot = Some(mdi.0 as isize);
    }

    // Install the λ-tiled background.  The brush lives for the process
    // lifetime; no explicit cleanup needed since we exit shortly after
    // the frame is destroyed.
    let lambda_brush = unsafe { make_lambda_brush() };
    let _ = LAMBDA_BRUSH_RAW.set(lambda_brush.0 as isize);
    unsafe {
        // Save the original MDICLIENT WndProc then replace it with ours.
        let orig = GetWindowLongPtrW(mdi, GWLP_WNDPROC);
        let _ = MDICLIENT_ORIG_PROC.set(orig);
        SetWindowLongPtrW(mdi, GWLP_WNDPROC, mdi_bg_proc as *const () as isize);
    }

    channels::install();
    super::system_colors::sample();

    // Discover demo files before building the menu so the Demos menu
    // can list them.  The static owns the Vec for the process lifetime.
    let demo_files = discover_demos();
    let demo_name_ids: Vec<(u16, String)> = demo_files
        .iter()
        .map(|(id, name, _)| (*id, name.clone()))
        .collect();
    let _ = DEMO_FILES.set(demo_files);

    // Default menu bar: File | Edit | Lisp | [Demos] | Tools.
    // `iGui.SetMenu` from a language-thread call will replace this, but
    // `menu::install_for_frame` always re-appends Lisp+Tools so they
    // stay reachable whatever the Lisp menu spec says.
    if let Some(default_menu) = super::tools_menu::build_default_menu_bar(&demo_name_ids) {
        let _ = unsafe {
            windows::Win32::UI::WindowsAndMessaging::SetMenu(hwnd, Some(default_menu))
        };
        let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::DrawMenuBar(hwnd) };
    }

    // Set the λ icon on the frame window (both the 16×16 taskbar icon
    // and the 32×32 Alt+Tab / title-bar icon).
    unsafe {
        let icon = make_lambda_icon();
        if icon.0 as isize != 0 {
            SendMessageW(hwnd, WM_SETICON, Some(WPARAM(0)), Some(LPARAM(icon.0 as isize)));
            SendMessageW(hwnd, WM_SETICON, Some(WPARAM(1)), Some(LPARAM(icon.0 as isize)));
        }
    }

    let _ = unsafe { ShowWindow(hwnd, SW_SHOW) };

    if let Some(worker) = worker {
        std::thread::Builder::new()
            .name("igui-language".into())
            .spawn(worker)
            .map_err(|e| IGuiError::Win32(format!("spawn language thread: {e}")))?;
    }

    // Frame-level accelerator table for the built-in tools:
    // Ctrl+Shift+E opens ledit, Ctrl+Shift+L opens the log view,
    // both regardless of which child has focus.
    let accel: Option<HACCEL> = super::tools_menu::build_accelerator_table();

    let mut msg = MSG::default();
    let exit_code = unsafe {
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 == 0 {
                break msg.wParam.0 as i32;
            }
            if r.0 == -1 {
                break 1;
            }
            // Frame accelerators run before MDI accel and TranslateMessage:
            // they own the highest-priority shortcuts (Ctrl+Shift+E to
            // open ledit) regardless of which child has focus.
            if let Some(h) = accel {
                if TranslateAcceleratorW(hwnd, h, &mut msg) != 0 {
                    continue;
                }
            }
            // MDI requires TranslateMDISysAccel before TranslateMessage
            // for system MDI shortcuts (Ctrl+F4, Ctrl+F6, etc.).
            if windows::Win32::UI::WindowsAndMessaging::TranslateMDISysAccel(mdi, &msg).as_bool() {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    };

    Ok(exit_code)
}

unsafe extern "system" fn frame_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let mdi = mdi_client_hwnd().unwrap_or_default();

    match msg {
        WM_IGUI_OPEN_CHILD => {
            let req_ptr = lparam.0 as *mut OpenChildRequest;
            if !req_ptr.is_null() {
                let req = unsafe { &mut *req_ptr };
                req.out = handle_open_child(req);
            }
            LRESULT(0)
        }
        WM_IGUI_OPEN_TEXT => {
            let req_ptr = lparam.0 as *mut OpenTextRequest;
            if !req_ptr.is_null() {
                let req = unsafe { &mut *req_ptr };
                if let Some(mdi_client) = mdi_client_hwnd() {
                    req.out = super::text_view::create_on_gui_thread(mdi_client, &req.title);
                }
            }
            LRESULT(0)
        }
        WM_IGUI_TEXT_FLUSH => {
            let child_id = wparam.0 as i64;
            super::text_view::flush_on_gui_thread(child_id);
            LRESULT(0)
        }
        WM_IGUI_OPEN_DOC => {
            let req_ptr = lparam.0 as *mut OpenDocRequest;
            if !req_ptr.is_null() {
                let req = unsafe { &mut *req_ptr };
                if let Some(mdi_client) = mdi_client_hwnd() {
                    req.out = super::doc_pane::create_on_gui_thread(mdi_client, &req.title);
                }
            }
            LRESULT(0)
        }
        WM_IGUI_DOC_FLUSH => {
            let child_id = wparam.0 as i64;
            super::doc_pane::flush_on_gui_thread(child_id);
            LRESULT(0)
        }
        WM_IGUI_OPEN_HELP => {
            let req_ptr = lparam.0 as *mut OpenHelpRequest;
            if !req_ptr.is_null() {
                let req = unsafe { &mut *req_ptr };
                if let Some(mdi_client) = mdi_client_hwnd() {
                    req.out = super::help_pane::create_on_gui_thread(
                        mdi_client, &req.title, &req.path,
                    );
                }
            }
            LRESULT(0)
        }
        WM_IGUI_OPEN_REPL => {
            let req_ptr = lparam.0 as *mut OpenReplRequest;
            if !req_ptr.is_null() {
                let req = unsafe { &mut *req_ptr };
                if let Some(mdi_client) = mdi_client_hwnd() {
                    let child_id = registry::allocate_child_id();
                    let title_w: Vec<u16> = req.title.iter().copied().collect();
                    if super::repl_child::open_from_gui_thread(mdi_client, &title_w, child_id) {
                        req.out = Some(child_id);
                    }
                }
            }
            LRESULT(0)
        }
        WM_IGUI_CLOSE_CHILD => {
            let req_ptr = lparam.0 as *mut CloseChildRequest;
            if !req_ptr.is_null() {
                let req = unsafe { &mut *req_ptr };
                if let Some(mdi_child) = registry::mdi_hwnd_of(req.child_id) {
                    if mdi.0 as isize != 0 {
                        child::close_via_mdi(mdi, mdi_child);
                        req.ok = true;
                    }
                }
            }
            LRESULT(0)
        }
        WM_IGUI_SET_TITLE => {
            let req_ptr = lparam.0 as *mut SetTitleRequest;
            if !req_ptr.is_null() {
                let req = unsafe { &*req_ptr };
                if let Some(mdi_child) = registry::mdi_hwnd_of(req.child_id) {
                    child::set_title(mdi_child, &req.title);
                }
            }
            LRESULT(0)
        }
        WM_IGUI_SET_MENU => {
            let req_ptr = lparam.0 as *mut SetMenuRequest;
            if !req_ptr.is_null() {
                let req = unsafe { &mut *req_ptr };
                req.ok = super::menu::install_for_frame(hwnd, mdi, &req.spec);
            }
            LRESULT(0)
        }
        WM_IGUI_CRASH_FLUSH => {
            super::crash_view::flush_on_gui_thread(hwnd);
            LRESULT(0)
        }
        WM_IGUI_MDI_VERB => {
            // wparam high byte = verb tag (avoid having to allocate
            // a request struct).
            let tag = wparam.0 as u8;
            if let Some(verb) = mdi_verb_from_tag(tag) {
                if mdi.0 as isize != 0 {
                    if matches!(verb, super::menu::MdiVerb::CloseAll) {
                        for (_id, mdi_child) in registry::snapshot() {
                            child::close_via_mdi(mdi, mdi_child);
                        }
                    } else {
                        super::menu::dispatch_mdi(mdi, verb);
                    }
                }
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let cmd_id = (wparam.0 & 0xFFFF) as u16;

            // ── File-menu commands ─────────────────────────────────────
            if cmd_id >= super::ledit::FILE_CMD_BASE && cmd_id <= super::ledit::FILE_CMD_END {
                if cmd_id == super::ledit::FILE_CMD_EXIT {
                    unsafe { PostQuitMessage(0) };
                } else if mdi.0 as isize != 0 {
                    super::ledit::do_file_cmd(cmd_id, hwnd, mdi);
                }
                return LRESULT(0);
            }

            // ── Demos menu ─────────────────────────────────────────────
            if cmd_id >= DEMO_CMD_BASE && cmd_id <= DEMO_CMD_END {
                if mdi.0 as isize != 0 {
                    if let Some(demos) = DEMO_FILES.get() {
                        if let Some((_, _, path)) =
                            demos.iter().find(|(id, _, _)| *id == cmd_id)
                        {
                            match std::fs::read_to_string(path) {
                                Ok(text) => {
                                    let path_clone = path.clone();
                                    // Load into ledit so the user can read/edit it.
                                    super::ledit::load_content(hwnd, mdi, text.clone(), Some(path_clone));
                                    // Append the conventional entry-point call and fire
                                    // EvalBuffer so the demo both defines its functions
                                    // AND runs immediately without the user having to
                                    // press F5 again.
                                    let stem = path
                                        .file_stem()
                                        .and_then(|s| s.to_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let run_call = format!(
                                        "(handler-case (run-{stem}) (error (c) (format t \"[demo] ~A~%\" c)))"
                                    );
                                    let source = format!("{}\n{}", text, run_call);
                                    channels::push(IGuiEvent::EvalBuffer { source });
                                }
                                Err(e) => {
                                    eprintln!("[demos] cannot read demo file: {e}");
                                }
                            }
                        }
                    }
                }
                return LRESULT(0);
            }

            // Built-in tools (ledit, log view) are wired before the
            // user menu so they work even if no language-thread spec
            // has been installed.
            if cmd_id == super::ledit::MENU_CMD_ID {
                if mdi.0 as isize != 0 {
                    super::ledit::open(hwnd, mdi);
                }
                return LRESULT(0);
            }
            if cmd_id == super::log_view::MENU_CMD_ID {
                if mdi.0 as isize != 0 {
                    super::log_view::open(hwnd, mdi);
                }
                return LRESULT(0);
            }
            if cmd_id == super::repl_child::MENU_CMD_ID {
                if mdi.0 as isize != 0 {
                    super::repl_child::open(mdi);
                }
                return LRESULT(0);
            }
            if cmd_id == super::crash_view::MENU_CMD_ID {
                if mdi.0 as isize != 0 {
                    super::crash_view::open(hwnd, mdi);
                }
                return LRESULT(0);
            }
            if cmd_id == super::tools_menu::HELP_CMD_DOCS {
                open_docs();
                return LRESULT(0);
            }
            // Edit-menu commands: forward to the active MDI child.
            // ledit's WndProc recognises these IDs in its own
            // WM_COMMAND handler and dispatches to the right method.
            // If no child is active or the active child doesn't
            // care about Edit commands, the message is harmless.
            if cmd_id >= super::ledit::EDIT_CMD_BASE
                && cmd_id <= super::ledit::EDIT_CMD_END
            {
                if mdi.0 as isize != 0 {
                    let active_raw = unsafe {
                        windows::Win32::UI::WindowsAndMessaging::SendMessageW(
                            mdi,
                            windows::Win32::UI::WindowsAndMessaging::WM_MDIGETACTIVE,
                            Some(WPARAM(0)),
                            Some(LPARAM(0)),
                        )
                    };
                    let active = HWND(active_raw.0 as *mut _);
                    if active.0 as isize != 0 {
                        unsafe {
                            windows::Win32::UI::WindowsAndMessaging::SendMessageW(
                                active,
                                WM_COMMAND,
                                Some(wparam),
                                Some(lparam),
                            )
                        };
                    }
                }
                return LRESULT(0);
            }
            // MDI verbs auto-allocated in install_for_frame.
            if let Some(verb) = super::menu::lookup_mdi_verb(cmd_id) {
                if mdi.0 as isize != 0 {
                    if matches!(verb, super::menu::MdiVerb::CloseAll) {
                        for (_id, mdi_child) in registry::snapshot() {
                            child::close_via_mdi(mdi, mdi_child);
                        }
                    } else {
                        super::menu::dispatch_mdi(mdi, verb);
                    }
                }
                return LRESULT(0);
            }
            // Command ids at/above the MDICLIENT idFirstChild (0xCC00)
            // belong to Win32's MDI machinery — above all the restore /
            // minimize / close buttons that a *maximized* MDI child
            // injects into the frame's menu bar (the "small window icons"
            // visible when a child is full within the frame). Those MUST
            // reach DefFrameProcW, or the buttons are dead — returning
            // LRESULT(0) here used to swallow them. Genuine user-menu ids
            // are far below 0xCC00, so they still surface as Menu events.
            if cmd_id >= 0xCC00 {
                return unsafe { DefFrameProcW(hwnd, Some(mdi), msg, wparam, lparam) };
            }
            // User menu items: push EvMenu so the language thread can
            // dispatch on item_id.
            channels::push(IGuiEvent::Menu {
                menu_id: 0,
                item_id: cmd_id as i64,
            });
            LRESULT(0)
        }
        WM_SIZE => {
            // MDI client sizes itself via DefFrameProcW.
            channels::push(IGuiEvent::Resize {
                child_id: FRAME_CHILD_ID,
                width: (lparam.0 & 0xFFFF) as i64,
                height: ((lparam.0 >> 16) & 0xFFFF) as i64,
            });
            unsafe { DefFrameProcW(hwnd, Some(mdi), msg, wparam, lparam) }
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            push_key(FRAME_CHILD_ID, true, wparam, lparam);
            unsafe { DefFrameProcW(hwnd, Some(mdi), msg, wparam, lparam) }
        }
        WM_KEYUP | WM_SYSKEYUP => {
            push_key(FRAME_CHILD_ID, false, wparam, lparam);
            unsafe { DefFrameProcW(hwnd, Some(mdi), msg, wparam, lparam) }
        }
        WM_CHAR => {
            channels::push(IGuiEvent::Char {
                child_id: FRAME_CHILD_ID,
                codepoint: wparam.0 as i64,
                mods: current_modifiers(),
                time_ms: msg_time(),
            });
            unsafe { DefFrameProcW(hwnd, Some(mdi), msg, wparam, lparam) }
        }
        WM_MOUSEMOVE => {
            push_mouse(FRAME_CHILD_ID, mouse_op::MOVE, 0, lparam);
            unsafe { DefFrameProcW(hwnd, Some(mdi), msg, wparam, lparam) }
        }
        WM_LBUTTONDOWN => {
            push_mouse(FRAME_CHILD_ID, mouse_op::LEFT_DOWN, 1, lparam);
            unsafe { DefFrameProcW(hwnd, Some(mdi), msg, wparam, lparam) }
        }
        WM_LBUTTONUP => {
            push_mouse(FRAME_CHILD_ID, mouse_op::LEFT_UP, 1, lparam);
            unsafe { DefFrameProcW(hwnd, Some(mdi), msg, wparam, lparam) }
        }
        WM_RBUTTONDOWN => {
            push_mouse(FRAME_CHILD_ID, mouse_op::RIGHT_DOWN, 2, lparam);
            unsafe { DefFrameProcW(hwnd, Some(mdi), msg, wparam, lparam) }
        }
        WM_RBUTTONUP => {
            push_mouse(FRAME_CHILD_ID, mouse_op::RIGHT_UP, 2, lparam);
            unsafe { DefFrameProcW(hwnd, Some(mdi), msg, wparam, lparam) }
        }
        WM_MBUTTONDOWN => {
            push_mouse(FRAME_CHILD_ID, mouse_op::MIDDLE_DOWN, 3, lparam);
            unsafe { DefFrameProcW(hwnd, Some(mdi), msg, wparam, lparam) }
        }
        WM_MBUTTONUP => {
            push_mouse(FRAME_CHILD_ID, mouse_op::MIDDLE_UP, 3, lparam);
            unsafe { DefFrameProcW(hwnd, Some(mdi), msg, wparam, lparam) }
        }
        WM_MOUSEWHEEL => {
            let raw = ((wparam.0 >> 16) & 0xFFFF) as i16;
            let delta = raw as i64;
            let lines = if WHEEL_DELTA != 0 {
                delta / (WHEEL_DELTA as i64)
            } else {
                0
            };
            channels::push(IGuiEvent::Mouse {
                child_id: FRAME_CHILD_ID,
                x: (lparam.0 & 0xFFFF) as i16 as i64,
                y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i64,
                op: mouse_op::WHEEL,
                button: 0,
                mods: current_modifiers(),
                wheel_delta: delta,
                wheel_lines: lines,
                time_ms: msg_time(),
            });
            unsafe { DefFrameProcW(hwnd, Some(mdi), msg, wparam, lparam) }
        }
        WM_SETFOCUS => {
            channels::push(IGuiEvent::Focus {
                child_id: FRAME_CHILD_ID,
                gained: true,
            });
            unsafe { DefFrameProcW(hwnd, Some(mdi), msg, wparam, lparam) }
        }
        WM_KILLFOCUS => {
            channels::push(IGuiEvent::Focus {
                child_id: FRAME_CHILD_ID,
                gained: false,
            });
            unsafe { DefFrameProcW(hwnd, Some(mdi), msg, wparam, lparam) }
        }
        WM_SYSCOLORCHANGE | WM_THEMECHANGED => {
            super::system_colors::refresh_and_notify();
            unsafe { DefFrameProcW(hwnd, Some(mdi), msg, wparam, lparam) }
        }
        WM_CLOSE => {
            channels::push(IGuiEvent::FrameClose);
            // Close every registered MDI child, then destroy the frame.
            if mdi.0 as isize != 0 {
                for (_id, child_hwnd) in registry::snapshot() {
                    child::close_via_mdi(mdi, child_hwnd);
                }
            }
            let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefFrameProcW(hwnd, Some(mdi), msg, wparam, lparam) },
    }
}

fn handle_open_child(req: &OpenChildRequest) -> Option<i64> {
    let mdi = mdi_client_hwnd()?;
    let child_id = registry::allocate_child_id();
    let bootstrap = Box::into_raw(Box::new(MdiBootstrap { child_id }));
    let h_module = unsafe { GetModuleHandleW(None) }.ok()?;
    let h_owner = windows::Win32::Foundation::HANDLE(h_module.0);

    // Width/height of 0 means "use the Windows default size";
    // otherwise honour what the caller asked for.
    let cx = if req.width  > 0 { req.width  } else { CW_USEDEFAULT };
    let cy = if req.height > 0 { req.height } else { CW_USEDEFAULT };

    // When an explicit size is given, centre the child inside the MDI
    // client area.  Fall back to CW_USEDEFAULT when no size is set.
    let (x, y) = if req.width > 0 && req.height > 0 {
        let mut client_rect = windows::Win32::Foundation::RECT::default();
        let _ = unsafe { GetClientRect(mdi, &mut client_rect) };
        let client_w = client_rect.right  - client_rect.left;
        let client_h = client_rect.bottom - client_rect.top;
        (
            ((client_w - req.width)  / 2).max(0),
            ((client_h - req.height) / 2).max(0),
        )
    } else {
        (CW_USEDEFAULT, CW_USEDEFAULT)
    };

    let mdi_create = MDICREATESTRUCTW {
        szClass: MDI_CHILD_CLASS,
        szTitle: PCWSTR::from_raw(req.title.as_ptr()),
        hOwner: h_owner,
        x,
        y,
        cx,
        cy,
        style: WS_VISIBLE | WS_OVERLAPPEDWINDOW,
        lParam: LPARAM(bootstrap as isize),
    };
    let result = unsafe {
        SendMessageW(
            mdi,
            WM_MDICREATE,
            Some(WPARAM(0)),
            Some(LPARAM(&mdi_create as *const _ as isize)),
        )
    };
    let new_hwnd = HWND(result.0 as *mut _);
    if new_hwnd.0.is_null() {
        // WM_MDICREATE failed; reclaim the bootstrap to avoid leaking.
        let _ = unsafe { Box::from_raw(bootstrap) };
        return None;
    }
    Some(child_id)
}

pub(crate) fn msg_time() -> i64 {
    unsafe { GetMessageTime() as i64 }
}

pub(crate) fn current_modifiers() -> i64 {
    let mut m = 0i64;
    unsafe {
        if (GetKeyState(VK_SHIFT.0 as i32) as i16) < 0 {
            m |= modifier::SHIFT;
        }
        if (GetKeyState(VK_CONTROL.0 as i32) as i16) < 0 {
            m |= modifier::CONTROL;
        }
        if (GetKeyState(VK_MENU.0 as i32) as i16) < 0 {
            m |= modifier::ALT;
        }
        if (GetKeyState(VK_LWIN.0 as i32) as i16) < 0
            || (GetKeyState(VK_RWIN.0 as i32) as i16) < 0
        {
            m |= modifier::WIN;
        }
        if (GetKeyState(VK_CAPITAL.0 as i32) & 1) != 0 {
            m |= modifier::CAPS;
        }
    }
    m
}

pub(crate) fn push_key(child_id: i64, down: bool, wparam: WPARAM, lparam: LPARAM) {
    let scancode = ((lparam.0 >> 16) & 0xFF) as i64;
    let repeat = (lparam.0 & 0xFFFF) as i64;
    channels::push(IGuiEvent::Key {
        child_id,
        vkey: wparam.0 as i64,
        scancode,
        mods: current_modifiers(),
        repeat,
        down,
        time_ms: msg_time(),
    });
}

pub(crate) fn push_mouse(child_id: i64, op: i64, button: i64, lparam: LPARAM) {
    let x = (lparam.0 & 0xFFFF) as i16 as i64;
    let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i64;
    channels::push(IGuiEvent::Mouse {
        child_id,
        x,
        y,
        op,
        button,
        mods: current_modifiers(),
        wheel_delta: 0,
        wheel_lines: 0,
        time_ms: msg_time(),
    });
}

// ─── Cross-thread request structures ─────────────────────────────────

pub(crate) struct OpenChildRequest {
    pub title: Vec<u16>,
    /// Initial pixel size. (0, 0) means "let Windows pick" via
    /// CW_USEDEFAULT (the existing behaviour).
    pub width: i32,
    pub height: i32,
    pub out: Option<i64>,
}

pub(crate) struct OpenTextRequest {
    pub title: Vec<u16>,
    pub out: Option<i64>,
}

pub(crate) struct OpenReplRequest {
    pub title: Vec<u16>,
    pub out: Option<i64>,
}

pub(crate) struct OpenDocRequest {
    pub title: Vec<u16>,
    pub out: Option<i64>,
}

pub(crate) struct OpenHelpRequest {
    pub title: Vec<u16>,
    pub path: String,
    pub out: Option<i64>,
}

pub(crate) struct CloseChildRequest {
    pub child_id: i64,
    pub ok: bool,
}

pub(crate) struct SetTitleRequest {
    pub child_id: i64,
    pub title: Vec<u16>,
}

pub(crate) struct SetMenuRequest {
    pub spec: String,
    pub ok: bool,
}

fn mdi_verb_from_tag(tag: u8) -> Option<super::menu::MdiVerb> {
    use super::menu::MdiVerb;
    match tag {
        1 => Some(MdiVerb::Cascade),
        2 => Some(MdiVerb::TileH),
        3 => Some(MdiVerb::TileV),
        4 => Some(MdiVerb::CloseAll),
        5 => Some(MdiVerb::ArrangeIcons),
        _ => None,
    }
}

fn mdi_verb_to_tag(verb: super::menu::MdiVerb) -> u8 {
    use super::menu::MdiVerb;
    match verb {
        MdiVerb::Cascade => 1,
        MdiVerb::TileH => 2,
        MdiVerb::TileV => 3,
        MdiVerb::CloseAll => 4,
        MdiVerb::ArrangeIcons => 5,
    }
}

/// Called from the language thread. Marshals to the GUI thread via
/// SendMessageW; blocks until the child has been created.
pub fn open_child(title: &str) -> Option<i64> {
    open_child_sized(title, 0, 0)
}

/// Open a child with an explicit initial pixel size. Pass 0 for
/// either dimension to fall back to Windows' CW_USEDEFAULT.
pub fn open_child_sized(title: &str, width: i32, height: i32) -> Option<i64> {
    let frame_raw = *FRAME_HWND.get()?;
    let frame = HWND(frame_raw as *mut _);
    let mut title_w: Vec<u16> = title.encode_utf16().collect();
    title_w.push(0);
    let mut req = OpenChildRequest {
        title: title_w,
        width,
        height,
        out: None,
    };
    unsafe {
        SendMessageW(
            frame,
            WM_IGUI_OPEN_CHILD,
            Some(WPARAM(0)),
            Some(LPARAM(&mut req as *mut _ as isize)),
        )
    };
    req.out
}

/// Called from the language thread. Opens a graphical REPL MDI child
/// (split-pane Direct2D renderer with an input editor). Marshals to
/// the GUI thread via SendMessageW; blocks until the window is live.
pub fn open_repl_child(title: &str) -> Option<i64> {
    let frame_raw = *FRAME_HWND.get()?;
    let frame = HWND(frame_raw as *mut _);
    let mut title_w: Vec<u16> = title.encode_utf16().collect();
    title_w.push(0);
    let mut req = OpenReplRequest {
        title: title_w,
        out: None,
    };
    unsafe {
        SendMessageW(
            frame,
            WM_IGUI_OPEN_REPL,
            Some(WPARAM(0)),
            Some(LPARAM(&mut req as *mut _ as isize)),
        )
    };
    req.out
}

/// Called from the language thread. Same SendMessageW marshalling
/// as `open_child`, but routes to the text-view class on the GUI
/// thread (where state allocation + WM_MDICREATE happen safely).
pub fn open_text_child(title: &str) -> Option<i64> {
    let frame_raw = *FRAME_HWND.get()?;
    let frame = HWND(frame_raw as *mut _);
    let mut title_w: Vec<u16> = title.encode_utf16().collect();
    title_w.push(0);
    let mut req = OpenTextRequest {
        title: title_w,
        out: None,
    };
    unsafe {
        SendMessageW(
            frame,
            WM_IGUI_OPEN_TEXT,
            Some(WPARAM(0)),
            Some(LPARAM(&mut req as *mut _ as isize)),
        )
    };
    req.out
}

pub fn close_child(child_id: i64) -> bool {
    let Some(frame_raw) = FRAME_HWND.get() else {
        return false;
    };
    let frame = HWND(*frame_raw as *mut _);
    let mut req = CloseChildRequest {
        child_id,
        ok: false,
    };
    unsafe {
        SendMessageW(
            frame,
            WM_IGUI_CLOSE_CHILD,
            Some(WPARAM(0)),
            Some(LPARAM(&mut req as *mut _ as isize)),
        )
    };
    req.ok
}

/// Marshal `spec` to the GUI thread, where it's parsed and installed
/// as the frame's menu bar. Returns true on success.
pub fn set_menu(spec: &str) -> bool {
    let Some(frame_raw) = FRAME_HWND.get() else {
        return false;
    };
    let frame = HWND(*frame_raw as *mut _);
    let mut req = SetMenuRequest {
        spec: spec.to_owned(),
        ok: false,
    };
    unsafe {
        SendMessageW(
            frame,
            WM_IGUI_SET_MENU,
            Some(WPARAM(0)),
            Some(LPARAM(&mut req as *mut _ as isize)),
        )
    };
    req.ok
}

/// Install or clear the per-child redraw timer. `interval_ms <= 0`
/// clears the timer; otherwise WM_TIMER fires every `interval_ms`
/// milliseconds and the render host pushes an `EvTick` event.
pub fn set_redraw_rate(child_id: i64, interval_ms: i64) -> bool {
    let Some(render_hwnd) = registry::render_hwnd_of(child_id) else {
        return false;
    };
    let interval = if interval_ms <= 0 { 0 } else { interval_ms as usize };
    unsafe {
        SendMessageW(
            render_hwnd,
            WM_IGUI_SET_TIMER,
            Some(WPARAM(interval)),
            Some(LPARAM(0)),
        )
    };
    true
}

/// Post a "drain the text-view command queue and repaint" message
/// at the frame. Frame WndProc dispatches to text_view's flush
/// handler on the GUI thread, which applies queued commands to the
/// child's grid and then InvalidateRects the child window. The
/// language thread never touches a child HWND. Posted (not sent)
/// so a tight write loop doesn't block on the GUI thread.
pub(crate) fn post_text_flush(child_id: i64) {
    let Some(frame_raw) = FRAME_HWND.get() else {
        return;
    };
    let frame = HWND(*frame_raw as *mut _);
    let _ = unsafe {
        PostMessageW(
            Some(frame),
            WM_IGUI_TEXT_FLUSH,
            WPARAM(child_id as usize),
            LPARAM(0),
        )
    };
}

/// Open a read-only Help-pane MDI child via the GUI thread. `path`
/// is a docs folder or a `.md` file; the help-pane scans the folder
/// for sibling docs, builds a sidebar, and follows in-page links.
/// Used by Help → Documentation; can also be opened from Lisp.
pub fn open_help_child(title: &str, path: &str) -> Option<i64> {
    let frame_raw = *FRAME_HWND.get()?;
    let frame = HWND(frame_raw as *mut _);
    let mut title_w: Vec<u16> = title.encode_utf16().collect();
    title_w.push(0);
    let mut req = OpenHelpRequest {
        title: title_w,
        path: path.to_owned(),
        out: None,
    };
    unsafe {
        SendMessageW(
            frame,
            WM_IGUI_OPEN_HELP,
            Some(WPARAM(0)),
            Some(LPARAM(&mut req as *mut _ as isize)),
        )
    };
    req.out
}

/// Open a Markdown doc-pane MDI child via the GUI thread. Returns the
/// child id Lisp uses with `doc_pane::set_markdown` etc. Blocks until
/// the GUI thread has run `WM_MDICREATE`, mirroring `open_text_child`.
pub fn open_doc_child(title: &str) -> Option<i64> {
    let frame_raw = *FRAME_HWND.get()?;
    let frame = HWND(frame_raw as *mut _);
    let mut title_w: Vec<u16> = title.encode_utf16().collect();
    title_w.push(0);
    let mut req = OpenDocRequest {
        title: title_w,
        out: None,
    };
    unsafe {
        SendMessageW(
            frame,
            WM_IGUI_OPEN_DOC,
            Some(WPARAM(0)),
            Some(LPARAM(&mut req as *mut _ as isize)),
        )
    };
    req.out
}

/// Post a "the doc-pane source changed; repaint" at the frame. The
/// frame dispatches to `doc_pane::flush_on_gui_thread`, which just
/// invalidates the child so WM_PAINT re-reads the source. Posted so
/// a streaming `append_markdown` loop doesn't block.
pub(crate) fn post_doc_flush(child_id: i64) {
    let Some(frame_raw) = FRAME_HWND.get() else {
        return;
    };
    let frame = HWND(*frame_raw as *mut _);
    let _ = unsafe {
        PostMessageW(
            Some(frame),
            WM_IGUI_DOC_FLUSH,
            WPARAM(child_id as usize),
            LPARAM(0),
        )
    };
}

/// Marshal an MDI verb to the GUI thread for execution.
pub fn dispatch_mdi_verb(verb: super::menu::MdiVerb) {
    let Some(frame_raw) = FRAME_HWND.get() else {
        return;
    };
    let frame = HWND(*frame_raw as *mut _);
    let tag = mdi_verb_to_tag(verb) as usize;
    unsafe {
        SendMessageW(
            frame,
            WM_IGUI_MDI_VERB,
            Some(WPARAM(tag)),
            Some(LPARAM(0)),
        )
    };
}

pub fn set_child_title(child_id: i64, title: &str) {
    let Some(frame_raw) = FRAME_HWND.get() else {
        return;
    };
    let frame = HWND(*frame_raw as *mut _);
    let mut title_w: Vec<u16> = title.encode_utf16().collect();
    title_w.push(0);
    let req = SetTitleRequest {
        child_id,
        title: title_w,
    };
    unsafe {
        SendMessageW(
            frame,
            WM_IGUI_SET_TITLE,
            Some(WPARAM(0)),
            Some(LPARAM(&req as *const _ as isize)),
        )
    };
}

