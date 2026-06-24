//! Synchronous-query reply channel.
//!
//! Sync text queries (`MeasureTextRun`, `CharIndexAtPoint`,
//! `PointAtCharIndex`) cannot be answered on the language thread
//! because the authoritative DirectWrite layout lives on the GUI
//! thread. The CP-side helper:
//!
//! 1. allocates a fresh `request_id`
//! 2. installs a oneshot reply slot keyed on that id (`install`)
//! 3. submits a batch carrying the matching `SurfaceCmd::Measure*` /
//!    `Char*` / `Point*` variant
//! 4. blocks on the reply slot with a 5-second guard (`wait`).
//!
//! The GUI thread's executor calls `deliver` once it has run the
//! DirectWrite query.

#![cfg(windows)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, Clone)]
pub enum Reply {
    Metrics {
        width: f32,
        height: f32,
        ascent: f32,
        line_count: u32,
    },
    HitTestPoint {
        char_index: u32,
        is_inside: bool,
        is_trailing: bool,
    },
    HitTestPosition {
        x: f32,
        y: f32,
        height: f32,
    },
    /// The query ran but produced no usable result (e.g. the layout
    /// failed to build). Caller treats it as "not found".
    Failed {
        message: String,
    },
}

static NEXT_REQUEST_ID: AtomicU32 = AtomicU32::new(1);
static SLOTS: Mutex<Option<HashMap<u32, SyncSender<Reply>>>> = Mutex::new(None);

pub fn alloc_id() -> u32 {
    NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

/// Install a oneshot reply slot for `request_id` and return the
/// receiver. Caller is responsible for waiting on the receiver and
/// for `deliver` not being called twice for the same id.
pub fn install(request_id: u32) -> Receiver<Reply> {
    let (tx, rx) = sync_channel::<Reply>(1);
    let mut guard = SLOTS.lock().expect("replies SLOTS poisoned");
    let map = guard.get_or_insert_with(HashMap::new);
    map.insert(request_id, tx);
    rx
}

/// Send `reply` to whichever waiter installed `request_id`.
/// No-op if the slot is missing (caller already gave up / timed out).
pub fn deliver(request_id: u32, reply: Reply) {
    let tx_opt = {
        let mut guard = SLOTS.lock().expect("replies SLOTS poisoned");
        guard.as_mut().and_then(|m| m.remove(&request_id))
    };
    if let Some(tx) = tx_opt {
        let _ = tx.send(reply);
    }
}

/// Block on the receiver with the iGui standard 5-second guard.
/// Returns `None` on timeout; callers translate that to a CP-side
/// failure return code.
pub fn wait(rx: Receiver<Reply>) -> Option<Reply> {
    rx.recv_timeout(Duration::from_secs(5)).ok()
}
