//! Host-owned canvas buffers for fast pixel-direct rendering from Lisp.
//!
//! A canvas is a `w`×`h` BGRA32 framebuffer (one `u32` per pixel, written
//! as `0xAARRGGBB` — little-endian B, G, R, A bytes) owned by the host and
//! associated with a render-host child window by `child_id`. Lisp obtains
//! the buffer's base address from `canvas-open` and writes pixels into it
//! directly through the foreign-buffer pokes (`buffer-set-u32`) — no
//! per-pixel boundary crossing, no allocation, no marshalling.
//! `canvas-present` snapshots the buffer and emits exactly one `Blit`
//! command for the frame (see `batch::present_pixels`).
//!
//! Safety model (the host-owns / language-pokes pattern, as in the
//! LocusNexus IDE this is ported from): the buffer lives here; Lisp holds
//! only a raw address into its data and writes without locking. `open`,
//! the pixel pokes, and `present` all run on the single Lisp thread that
//! owns the canvas, sequentially, so there is no data race on the live
//! host buffer. `present` *copies* the buffer into an independent `Arc`
//! that the GUI thread paints, so the GUI never reads the buffer Lisp is
//! actively writing.

#![cfg(windows)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::batch;

const MAX_DIM: i64 = 1 << 16; // 65536 — generous upper bound per axis

struct CanvasBuf {
    w: u32,
    h: u32,
    /// `w * h` BGRA32 pixels. Lisp pokes this directly via the address
    /// returned from `canvas_open`; the heap allocation is stable (we
    /// never reallocate except on an explicit resize), so the address
    /// stays valid across frames.
    pixels: Vec<u32>,
}

static CANVAS: Mutex<Option<HashMap<i64, CanvasBuf>>> = Mutex::new(None);

/// Open (or resize) the canvas for `child_id` to `w`×`h` and return the
/// base address of its pixel buffer, or 0 on bad dimensions. The buffer
/// is zero-filled (transparent black). The address is stable until the
/// next `canvas_open` with *different* dimensions for the same child.
/// Write a pixel with `(buffer-set-u32 base (* (+ (* y w) x) 4) argb)`.
pub fn canvas_open(child_id: i64, w: i64, h: i64) -> usize {
    if w <= 0 || h <= 0 || w > MAX_DIM || h > MAX_DIM {
        return 0;
    }
    let (w, h) = (w as u32, h as u32);
    let n = (w as usize) * (h as usize);
    let mut guard = CANVAS.lock().expect("CANVAS poisoned");
    let map = guard.get_or_insert_with(HashMap::new);
    let entry = map.entry(child_id).or_insert_with(|| CanvasBuf {
        w,
        h,
        pixels: vec![0u32; n],
    });
    if entry.w != w || entry.h != h {
        entry.w = w;
        entry.h = h;
        entry.pixels = vec![0u32; n];
    }
    entry.pixels.as_ptr() as usize
}

/// Snapshot the canvas for `child_id`, emit a `Blit` for this frame, and
/// return the (unchanged) base address for the next frame's writes — or
/// 0 if no canvas is open for `child_id`. Copy-on-present: the GUI thread
/// paints an independent `Arc` snapshot, never the live host buffer.
pub fn canvas_present(child_id: i64) -> usize {
    let guard = CANVAS.lock().expect("CANVAS poisoned");
    let Some(buf) = guard.as_ref().and_then(|m| m.get(&child_id)) else {
        return 0;
    };
    let (w, h) = (buf.w, buf.h);
    let base = buf.pixels.as_ptr() as usize;
    let snapshot = Arc::new(buf.pixels.clone());
    drop(guard);
    batch::present_pixels(child_id, w, h, snapshot);
    base
}

/// Current base address and dimensions for `child_id`, or `None`.
#[allow(dead_code)]
pub fn canvas_info(child_id: i64) -> Option<(usize, u32, u32)> {
    let guard = CANVAS.lock().expect("CANVAS poisoned");
    guard
        .as_ref()
        .and_then(|m| m.get(&child_id))
        .map(|b| (b.pixels.as_ptr() as usize, b.w, b.h))
}

/// Drop the canvas for `child_id` (called when its child window closes).
#[allow(dead_code)]
pub fn forget(child_id: i64) {
    let mut guard = CANVAS.lock().expect("CANVAS poisoned");
    if let Some(map) = guard.as_mut() {
        map.remove(&child_id);
    }
}
