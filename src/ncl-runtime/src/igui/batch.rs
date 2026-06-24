//! Per-pane render state and the typed surface command enum.
//!
//! Phase 3b semantics:
//! - The CP-side batch builder is a thread-local `Vec<SurfaceCmd>` — CP
//!   calls `BeginBatch(childId)` / `Emit*` / `SubmitBatch` and the
//!   submit step hands off ownership to the per-pane "current" slot.
//! - The per-pane current batch is the latest fully-built batch for
//!   that child. WM_PAINT renders from it. Submitting a new batch
//!   replaces the previous one (newer sequence wins) and posts a
//!   `WM_PAINT` to the child via `InvalidateRect`.

#![cfg(windows)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::InvalidateRect;

use super::registry;

// The drawing IR (Rgba, Rect, Point, MarkMode, PathCmd, LineCap,
// LineJoin, StrokeStyle, FontStyle, FontStretch, TextAlign,
// TextTrimming, TextRun, SurfaceCmd, PaneBatch) now lives in the
// platform-neutral `crate::igui_paint` module so the macOS Core
// Graphics renderer shares the exact same command vocabulary. Re-export
// it here so every existing `super::batch::SurfaceCmd` reference in the
// Windows code paths keeps resolving unchanged. See
// docs/PORTING_DESIGN.md §4.5.
pub use crate::igui_paint::*;

static SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn next_sequence() -> u64 {
    SEQUENCE.fetch_add(1, Ordering::Relaxed)
}

// ─── Per-pane "current display batch" registry ────────────────────────

static PANE_STATES: Mutex<Option<HashMap<i64, Arc<PaneBatch>>>> = Mutex::new(None);

/// Hand `batch` to the GUI thread for child `child_id`. Replaces any
/// previously-submitted batch for the same child. Triggers a redraw by
/// invalidating the **render host** HWND (the borderless inner child
/// that owns the swap chain and WM_PAINT loop).
pub fn submit(batch: PaneBatch) -> bool {
    let child_id = batch.child_id;
    let arc = Arc::new(batch);
    {
        let mut guard = PANE_STATES.lock().expect("PANE_STATES poisoned");
        let map = guard.get_or_insert_with(HashMap::new);
        map.insert(child_id, arc);
    }
    if let Some(hwnd) = registry::render_hwnd_of(child_id) {
        let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
        true
    } else {
        false
    }
}

pub fn snapshot(child_id: i64) -> Option<Arc<PaneBatch>> {
    let guard = PANE_STATES.lock().expect("PANE_STATES poisoned");
    guard.as_ref().and_then(|m| m.get(&child_id).cloned())
}

/// Replace child `child_id`'s display batch with a single `Blit` of
/// `pixels` (w×h BGRA32) at the origin, and trigger a repaint. The
/// canvas `present` step calls this once per frame; `pixels` is a
/// fresh per-frame snapshot owned by the resulting batch.
pub fn present_pixels(child_id: i64, w: u32, h: u32, pixels: Arc<Vec<u32>>) -> bool {
    submit(PaneBatch {
        child_id,
        sequence: next_sequence(),
        flags: 0,
        cmds: vec![SurfaceCmd::Blit {
            x: 0.0,
            y: 0.0,
            w,
            h,
            pixels,
        }],
    })
}

#[allow(dead_code)] // used when child windows close
pub fn forget(child_id: i64) {
    let mut guard = PANE_STATES.lock().expect("PANE_STATES poisoned");
    if let Some(map) = guard.as_mut() {
        map.remove(&child_id);
    }
}

// ─── CP-thread batch builder ─────────────────────────────────────────

thread_local! {
    static CURRENT: RefCell<Option<PaneBatch>> = const { RefCell::new(None) };
}

pub fn begin(child_id: i64) {
    CURRENT.with(|slot| {
        *slot.borrow_mut() = Some(PaneBatch {
            child_id,
            sequence: next_sequence(),
            flags: 0,
            cmds: Vec::new(),
        });
    });
}

pub fn push(cmd: SurfaceCmd) -> bool {
    CURRENT.with(|slot| {
        if let Some(batch) = slot.borrow_mut().as_mut() {
            batch.cmds.push(cmd);
            true
        } else {
            false
        }
    })
}

pub fn finish() -> Option<PaneBatch> {
    CURRENT.with(|slot| slot.borrow_mut().take())
}

/// Restore an earlier in-progress batch into the thread-local
/// CURRENT slot. Used by `measure-text` so that calling it from
/// inside a `with-batch` doesn't clobber the user's draw work in
/// progress: take_current()-do-measure-restore-current(saved).
pub fn restore_current(saved: Option<PaneBatch>) {
    CURRENT.with(|slot| *slot.borrow_mut() = saved);
}

// ─── Phase 5: path builder ──────────────────────────────────────────

thread_local! {
    /// In-progress path commands accumulated by `path_*` calls until
    /// the matching `path_finish_*` call wraps them in a
    /// `SurfaceCmd::DrawPath` and pushes it onto the active batch.
    static PATH: RefCell<Vec<PathCmd>> = const { RefCell::new(Vec::new()) };
}

pub fn path_begin() {
    PATH.with(|c| c.borrow_mut().clear());
}

pub fn path_push(cmd: PathCmd) {
    PATH.with(|c| c.borrow_mut().push(cmd));
}

/// Take the current path command stream and emit a DrawPath into the
/// active batch with the given fill / stroke options.
pub fn path_finish(
    fill: Option<Rgba>,
    stroke: Option<(StrokeStyle, Rgba)>,
) -> bool {
    let commands = PATH.with(|c| std::mem::take(&mut *c.borrow_mut()));
    if commands.is_empty() {
        return false;
    }
    push(SurfaceCmd::DrawPath {
        commands,
        fill,
        stroke,
    })
}

#[allow(dead_code)] // used by the GUI thread when a child window closes
pub(crate) fn invalidate(hwnd: HWND) {
    let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
}
