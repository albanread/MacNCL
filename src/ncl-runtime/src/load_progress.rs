//! Cross-platform startup-load progress.
//!
//! The slow part of startup is JIT-compiling the standard library
//! (`core.lisp` + `clos.lisp` + `Library/init.lisp`) — hundreds of functions,
//! every launch. The Lisp worker `tick()`s this once per compiled function and
//! `set_phase()`s at module boundaries. A UI that runs on a *different* thread
//! (the macOS main-thread repaint timer) reads `snapshot()` to draw a loading
//! bar while the worker is busy and can't repaint itself.
//!
//! It's just a few atomics + a short string, and a no-op until `begin()` — so
//! the per-function `tick()` on the hot compile path costs a relaxed load.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static DONE: AtomicU32 = AtomicU32::new(0);
static TOTAL: AtomicU32 = AtomicU32::new(1);
static PHASE: Mutex<String> = Mutex::new(String::new());

/// Begin a load with an (approximate) total function count. Resets the counter.
pub fn begin(total: u32) {
    DONE.store(0, Ordering::Relaxed);
    TOTAL.store(total.max(1), Ordering::Relaxed);
    set_phase_raw(String::new());
    ACTIVE.store(true, Ordering::Relaxed);
}

/// End the load — the UI stops drawing the bar.
pub fn finish() {
    ACTIVE.store(false, Ordering::Relaxed);
}

pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Count one compiled function. No-op unless a load is active.
pub fn tick() {
    if ACTIVE.load(Ordering::Relaxed) {
        DONE.fetch_add(1, Ordering::Relaxed);
    }
}

/// Name the module being loaded (shown under the bar). No-op unless active.
pub fn set_phase(s: &str) {
    if ACTIVE.load(Ordering::Relaxed) {
        set_phase_raw(s.to_string());
    }
}

fn set_phase_raw(s: String) {
    *PHASE.lock().unwrap_or_else(|e| e.into_inner()) = s;
}

/// `(done, total, phase)` for the UI.
pub fn snapshot() -> (u32, u32, String) {
    (
        DONE.load(Ordering::Relaxed),
        TOTAL.load(Ordering::Relaxed),
        PHASE.lock().unwrap_or_else(|e| e.into_inner()).clone(),
    )
}
