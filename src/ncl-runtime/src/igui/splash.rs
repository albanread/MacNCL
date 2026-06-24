//! Startup splash — iGui progress window shown while the Library loads.
//!
//! Lifecycle:
//!   1. `begin(total_forms)` — start the iGui frame (if not already up),
//!      open a fixed-size MDI child, draw the initial empty bar.
//!   2. `set_module(name)` — called at the start of each require/load.
//!      Updates the label and forces an immediate redraw.
//!   3. `tick()` — called after each compiled form; redraws every
//!      REDRAW_EVERY ticks so the bar moves smoothly without hammering.
//!   4. `finish()` — draw 100%, pause briefly, close the child.
//!
//! The splash lives inside the normal iGui MDI frame so it inherits the
//! D2D renderer, correct DPI scaling, and the NCL dark theme palette.

#![cfg(windows)]

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use super::batch::{
    self as batch_mod, FontStretch, FontStyle, Point, Rect, Rgba, SurfaceCmd, TextAlign, TextRun,
    TextTrimming,
};
use super::lisp_shims;
use super::window;

// ─── Shared state ────────────────────────────────────────────────────────────

static CHILD_ID:   AtomicI64 = AtomicI64::new(-1);
static FORMS_DONE: AtomicU32 = AtomicU32::new(0);
static FORMS_TOTAL: AtomicU32 = AtomicU32::new(1);
static LAST_DRAWN: AtomicU32 = AtomicU32::new(0);
static ACTIVE:     AtomicBool = AtomicBool::new(false);

static MODULE_NAME: OnceLock<std::sync::Mutex<String>> = OnceLock::new();

fn module_name() -> &'static std::sync::Mutex<String> {
    MODULE_NAME.get_or_init(|| std::sync::Mutex::new(String::new()))
}

// Rebuild+submit a new batch every this many ticks (keeps CPU cost low).
const REDRAW_EVERY: u32 = 6;

// ─── Palette ─────────────────────────────────────────────────────────────────
// NCL dark theme

const BG:         Rgba = Rgba { r: 0.102, g: 0.114, b: 0.137, a: 1.0 }; // #1A1D23
const BAR_TRACK:  Rgba = Rgba { r: 0.173, g: 0.188, b: 0.216, a: 1.0 }; // #2C3037
const BAR_FILL:   Rgba = Rgba { r: 0.302, g: 0.608, b: 0.961, a: 1.0 }; // #4D9BF5
const TEXT_TITLE: Rgba = Rgba { r: 0.878, g: 0.894, b: 0.910, a: 1.0 }; // #E0E4E8
const TEXT_GRAY:  Rgba = Rgba { r: 0.533, g: 0.553, b: 0.588, a: 1.0 }; // #888E96

// ─── Window & drawing geometry ───────────────────────────────────────────────

/// Outer MDI child size (Win32 pixels, includes title bar).
const WIN_W: i32 = 520;
const WIN_H: i32 = 130;

/// Drawing coordinate space (D2D DIPs in the client area below the title bar).
const DRAW_W: f32 = 520.0;
const DRAW_H: f32 = 96.0;   // ≈ WIN_H minus ~34 px title bar

// ─── Public API ──────────────────────────────────────────────────────────────

/// Start the splash.  Call from the Lisp worker thread, before Library loading.
/// `total_forms` should be the estimated number of forms that will be compiled
/// (use the pre-baked sum: core + clos + Library ≈ 805 without xp).
pub fn begin(total_forms: u32) {
    FORMS_TOTAL.store(total_forms.max(1), Ordering::Relaxed);
    FORMS_DONE.store(0, Ordering::Relaxed);
    LAST_DRAWN.store(0, Ordering::Relaxed);

    // Bring up the iGui frame (idempotent; does nothing if already running).
    if !lisp_shims::ensure_igui_started() {
        return; // frame failed to start; continue without splash
    }

    // Open a fixed-size child for the loading bar.
    let id = match window::open_child_sized("NCL — Starting…", WIN_W, WIN_H) {
        Some(id) => id,
        None => return,
    };
    CHILD_ID.store(id, Ordering::Relaxed);
    ACTIVE.store(true, Ordering::Relaxed);

    draw_splash(id, 0, total_forms.max(1), "");
}

/// Update the displayed module name and force an immediate redraw.
/// Call at the start of each `(require …)` / file load.
pub fn set_module(name: &str) {
    if !ACTIVE.load(Ordering::Relaxed) { return; }
    if let Ok(mut m) = module_name().lock() {
        m.clear();
        m.push_str(name);
    }
    let id = CHILD_ID.load(Ordering::Relaxed);
    if id < 0 { return; }
    let done  = FORMS_DONE.load(Ordering::Relaxed);
    let total = FORMS_TOTAL.load(Ordering::Relaxed);
    LAST_DRAWN.store(done, Ordering::Relaxed);
    draw_splash(id, done, total, name);
}

/// Advance by one compiled form.  Redraws every `REDRAW_EVERY` ticks.
#[inline]
pub fn tick() {
    if !ACTIVE.load(Ordering::Relaxed) { return; }
    let done = FORMS_DONE.fetch_add(1, Ordering::Relaxed) + 1;
    let last = LAST_DRAWN.load(Ordering::Relaxed);
    if done.wrapping_sub(last) < REDRAW_EVERY { return; }
    LAST_DRAWN.store(done, Ordering::Relaxed);
    let id    = CHILD_ID.load(Ordering::Relaxed);
    if id < 0 { return; }
    let total = FORMS_TOTAL.load(Ordering::Relaxed);
    let module = module_name().lock().map(|m| m.clone()).unwrap_or_default();
    draw_splash(id, done, total, &module);
}

/// Finalise: draw 100 %, pause briefly, then close the child.
pub fn finish() {
    ACTIVE.store(false, Ordering::Relaxed);
    let id = CHILD_ID.load(Ordering::Relaxed);
    if id < 0 { return; }
    let total = FORMS_TOTAL.load(Ordering::Relaxed);
    draw_splash(id, total, total, "Ready");
    std::thread::sleep(Duration::from_millis(300));
    window::close_child(id);
    CHILD_ID.store(-1, Ordering::Relaxed);
}

// ─── Drawing ─────────────────────────────────────────────────────────────────

fn draw_splash(child_id: i64, done: u32, total: u32, module: &str) {
    let frac = (done as f32 / total as f32).clamp(0.0, 1.0);

    batch_mod::begin(child_id);

    // ── Background ──────────────────────────────────────────────────────
    batch_mod::push(SurfaceCmd::Clear { color: BG });

    // ── Title ───────────────────────────────────────────────────────────
    batch_mod::push(SurfaceCmd::DrawTextRun {
        run: TextRun {
            text:      "NCL".to_string(),
            origin:    Point { x: 20.0, y: 10.0 },
            family:    "Segoe UI".to_string(),
            size:      16.0,
            weight:    600,
            style:     FontStyle::Normal,
            stretch:   FontStretch::Normal,
            locale:    "en-us".to_string(),
            color:     TEXT_TITLE,
            max_width: Some(DRAW_W - 40.0),
            alignment: TextAlign::Leading,
            trimming:  TextTrimming::None,
        },
    });

    // ── Module / status ─────────────────────────────────────────────────
    let status = if module.is_empty() {
        "Loading…".to_string()
    } else {
        format!("Loading:  {module}")
    };
    batch_mod::push(SurfaceCmd::DrawTextRun {
        run: TextRun {
            text:      status,
            origin:    Point { x: 20.0, y: 40.0 },
            family:    "Segoe UI".to_string(),
            size:      11.0,
            weight:    400,
            style:     FontStyle::Normal,
            stretch:   FontStretch::Normal,
            locale:    "en-us".to_string(),
            color:     TEXT_GRAY,
            max_width: Some(DRAW_W - 80.0),
            alignment: TextAlign::Leading,
            trimming:  TextTrimming::EllipsisChar,
        },
    });

    // ── Percentage (right-aligned) ───────────────────────────────────────
    batch_mod::push(SurfaceCmd::DrawTextRun {
        run: TextRun {
            text:      format!("{}%", (frac * 100.0) as u32),
            origin:    Point { x: 0.0, y: 40.0 },
            family:    "Segoe UI".to_string(),
            size:      11.0,
            weight:    400,
            style:     FontStyle::Normal,
            stretch:   FontStretch::Normal,
            locale:    "en-us".to_string(),
            color:     TEXT_GRAY,
            max_width: Some(DRAW_W - 20.0),
            alignment: TextAlign::Trailing,
            trimming:  TextTrimming::None,
        },
    });

    // ── Progress bar track (full-width strip at the bottom) ─────────────
    let bar_y0 = DRAW_H - 10.0;
    let bar_y1 = DRAW_H;
    batch_mod::push(SurfaceCmd::FillRect {
        rect:          Rect { x0: 0.0, y0: bar_y0, x1: DRAW_W, y1: bar_y1 },
        corner_radius: 0.0,
        color:         BAR_TRACK,
    });

    // ── Progress bar fill ───────────────────────────────────────────────
    let fill_x = (DRAW_W * frac).max(0.0);
    if fill_x > 0.5 {
        batch_mod::push(SurfaceCmd::FillRect {
            rect:          Rect { x0: 0.0, y0: bar_y0, x1: fill_x, y1: bar_y1 },
            corner_radius: 0.0,
            color:         BAR_FILL,
        });
    }

    if let Some(pb) = batch_mod::finish() {
        batch_mod::submit(pb);
    }
}
