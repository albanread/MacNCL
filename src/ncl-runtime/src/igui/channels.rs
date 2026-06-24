//! Event mailbox: GUI thread → language thread(s).
//!
//! ## Architecture — dispatcher model
//!
//! ```text
//!  GUI thread ─SyncSender──► raw MPSC channel ──► dispatcher thread
//!                                                        │
//!                          ┌─────────────────────────────┤ routes each event
//!                          ▼                             ▼
//!                   per-child Arc<ChildQueue>    CATCH_ALL Arc<ChildQueue>
//!                   (one per registered child)   (for filter-less next_event)
//! ```
//!
//! Each Lisp thread that calls `filter_on_window(id)` or
//! `next_event_for(id, …)` blocks on its own `Arc<ChildQueue>`
//! (a `Mutex<VecDeque> + Condvar`). Multiple threads block independently
//! on *different* queues — unlike the old single `Mutex<Receiver>` design,
//! no thread starves the others.
//!
//! ## Routing rules (dispatcher thread)
//!
//! | Event kind                                   | Destination           |
//! |----------------------------------------------|-----------------------|
//! | Global (FrameClose, ThemeChange, Menu,       | **Broadcast** to ALL  |
//! |          EvalBuffer)                          | registered queues     |
//! | Per-child (Key, Char, Mouse, Tick, Close, …) | Target child queue    |
//! |                                              | **+** CATCH_ALL queue |
//!
//! ## Thread-local filter (`next_event`)
//!
//! Each Lisp thread maintains its own `HashSet<i64>` of windows it cares
//! about. `next_event(ms)` reads that per-thread set and:
//!
//! | Filter size | Action                                             |
//! |-------------|---------------------------------------------------|
//! | 0 entries   | Wait on CATCH_ALL queue (all events)              |
//! | 1 entry     | Delegate to `next_event_for(id, ms)`              |
//! | N entries   | Spin-poll all matching per-child queues (rare)    |
//!
//! This means `(event-loop-for WIN …)` — which does
//! `filter-on-window WIN` then `next-event -1` in a loop — correctly
//! delegates to the per-child queue for `WIN`, letting a second thread
//! concurrently do `(event-loop-for WIN2 …)` on a completely independent
//! queue.

#![cfg(windows)]

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

// ── Event kinds / sub-kinds (stable ABI, exported to CP) ───────────────────

/// Stable enum tags exported to CP as `iGui.Ev*` constants.
pub mod kind {
    pub const NONE: i64 = 0;
    pub const KEY: i64 = 1;
    pub const CHAR: i64 = 2;
    pub const MOUSE: i64 = 3;
    pub const FOCUS: i64 = 4;
    pub const RESIZE: i64 = 5;
    pub const PAINT: i64 = 6;
    pub const CLOSE: i64 = 7;
    pub const FRAME_CLOSE: i64 = 8;
    pub const MENU: i64 = 9;
    pub const THEME_CHANGE: i64 = 10;
    pub const DPI_CHANGE: i64 = 11;
    pub const SURFACE_REPLY: i64 = 12;
    pub const TICK: i64 = 13;
    pub const EVAL_BUFFER: i64 = 14;
    pub const REPL_SUBMIT: i64 = 15;
}

/// Mouse-event sub-kinds packed into the `mouse_op` field. Each is a
/// distinct value (not a bitmask) so the language side can match directly.
pub mod mouse_op {
    pub const MOVE: i64 = 0;
    pub const LEFT_DOWN: i64 = 1;
    pub const LEFT_UP: i64 = 2;
    pub const RIGHT_DOWN: i64 = 3;
    pub const RIGHT_UP: i64 = 4;
    pub const MIDDLE_DOWN: i64 = 5;
    pub const MIDDLE_UP: i64 = 6;
    pub const WHEEL: i64 = 7;
}

/// Modifier-key bits as a packed `i64`. Matches Win32 GetKeyState bit
/// layout where convenient; CP code reads the named bits via
/// `iGui.Mod*` constants.
pub mod modifier {
    pub const SHIFT: i64 = 1 << 0;
    pub const CONTROL: i64 = 1 << 1;
    pub const ALT: i64 = 1 << 2;
    pub const WIN: i64 = 1 << 3;
    pub const CAPS: i64 = 1 << 4;
}

// ── Event enum ──────────────────────────────────────────────────────────────

/// All input and lifecycle events flow as one of these variants.
/// Specialised carriers per kind keep the variant fields self-describing
/// without a tagged-union ABI on the wire.
#[derive(Debug, Clone)]
pub enum IGuiEvent {
    Key {
        child_id: i64,
        vkey: i64,
        scancode: i64,
        mods: i64,
        repeat: i64,
        down: bool,
        time_ms: i64,
    },
    Char {
        child_id: i64,
        codepoint: i64,
        mods: i64,
        time_ms: i64,
    },
    Mouse {
        child_id: i64,
        x: i64,
        y: i64,
        op: i64, // mouse_op::*
        button: i64,
        mods: i64,
        wheel_delta: i64,
        wheel_lines: i64,
        time_ms: i64,
    },
    Focus {
        child_id: i64,
        gained: bool,
    },
    Resize {
        child_id: i64,
        width: i64,
        height: i64,
    },
    Close {
        child_id: i64,
    },
    FrameClose,
    ThemeChange,
    DpiChange {
        child_id: i64,
        dpi_x: i64, // ×100
        dpi_y: i64,
    },
    Menu {
        menu_id: i64,
        item_id: i64,
    },
    /// Animation tick — fires from a Win32 timer running on a child's
    /// render host. Win32 auto-coalesces queued WM_TIMERs so the
    /// language thread sees at most one tick per child per drain cycle.
    Tick {
        child_id: i64,
        time_ms: i64,
    },
    /// "Evaluate this Lisp source." Fired when the user hits Ctrl+R
    /// inside the ledit (Lisp editor) pane. The event-loop macros in
    /// Library/events.lisp dispatch this automatically.
    EvalBuffer {
        source: String,
    },
    /// A complete Lisp form has been submitted from the REPL input pane.
    /// The text has already been pushed onto the REPL child's pending
    /// queue; the worker thread should call `(repl-pop-input child-id)`.
    ReplSubmit {
        child_id: i64,
    },
}

// ── Per-child queue ──────────────────────────────────────────────────────────

/// Sentinel child_id for the catch-all queue. Language threads that use
/// `(next-event …)` without any `filter-on-window` call consume from here
/// and receive every event (legacy single-thread demos).
const CATCH_ALL_ID: i64 = i64::MIN;

struct ChildQueue {
    events: Mutex<VecDeque<IGuiEvent>>,
    cv: Condvar,
}

impl ChildQueue {
    fn new() -> Arc<Self> {
        Arc::new(ChildQueue {
            events: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
        })
    }

    fn push(&self, ev: IGuiEvent) {
        {
            let mut guard = self.events.lock().unwrap_or_else(|e| e.into_inner());
            guard.push_back(ev);
        } // release before notify so waiters don't spin on a held mutex
        self.cv.notify_one();
    }

    /// Non-blocking pop; returns None if empty.
    fn try_pop(&self) -> Option<IGuiEvent> {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }

    /// Block until an event is available, or until `deadline` is reached.
    /// `deadline = None` means block forever.
    fn wait_pop(&self, deadline: Option<Instant>) -> Option<IGuiEvent> {
        let mut guard = self.events.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(ev) = guard.pop_front() {
                return Some(ev);
            }
            match deadline {
                None => {
                    guard = self.cv.wait(guard).unwrap_or_else(|e| e.into_inner());
                }
                Some(dl) => {
                    let now = Instant::now();
                    if now >= dl {
                        return None;
                    }
                    let (g, _) = self
                        .cv
                        .wait_timeout(guard, dl - now)
                        .unwrap_or_else(|e| e.into_inner());
                    guard = g;
                    // Loop back: either an event arrived, or we timed out
                    // and the next `Instant::now() >= dl` check will catch it.
                }
            }
        }
    }
}

// ── Queue registry ───────────────────────────────────────────────────────────

static QUEUES: OnceLock<Mutex<HashMap<i64, Arc<ChildQueue>>>> = OnceLock::new();

fn queues_lock() -> &'static Mutex<HashMap<i64, Arc<ChildQueue>>> {
    QUEUES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn get_or_create_queue(id: i64) -> Arc<ChildQueue> {
    queues_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(id)
        .or_insert_with(ChildQueue::new)
        .clone()
}

fn try_get_queue(id: i64) -> Option<Arc<ChildQueue>> {
    queues_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .cloned()
}

// ── Thread-local filter ──────────────────────────────────────────────────────

// Each Lisp thread has its own filter set so two threads each doing
// `(event-loop-for WIN …)` with different WIN values don't share state
// and don't fall through to the catch-all queue.

thread_local! {
    static THREAD_FILTER: RefCell<HashSet<i64>> = RefCell::new(HashSet::new());
}

pub fn filter_on_window(child_id: i64) {
    THREAD_FILTER.with(|f| f.borrow_mut().insert(child_id));
    // Pre-create the queue so the dispatcher can push to it immediately,
    // even if this thread hasn't called next_event_for yet.
    get_or_create_queue(child_id);
}

pub fn unfilter_window(child_id: i64) {
    THREAD_FILTER.with(|f| f.borrow_mut().remove(&child_id));
}

pub fn clear_filter() {
    THREAD_FILTER.with(|f| f.borrow_mut().clear());
}

/// No-op in the dispatcher design — there is no global stash to drain.
/// Kept for API compatibility with the old stash-based implementation.
pub fn discard_stashed_events() {}

// ── Raw MPSC channel + dispatcher ───────────────────────────────────────────

const CAPACITY: usize = 1024;

struct Mailbox {
    tx: SyncSender<IGuiEvent>,
}

static MAILBOX: OnceLock<Mailbox> = OnceLock::new();

/// Extract the child_id for per-child events, or None for global events.
/// Global events are broadcast to every registered queue.
fn event_target(ev: &IGuiEvent) -> Option<i64> {
    match ev {
        // Globals: broadcast
        IGuiEvent::FrameClose
        | IGuiEvent::ThemeChange
        | IGuiEvent::EvalBuffer { .. }
        | IGuiEvent::Menu { .. } => None,
        // Per-child: route to specific queue + CATCH_ALL
        IGuiEvent::Key { child_id, .. }
        | IGuiEvent::Char { child_id, .. }
        | IGuiEvent::Mouse { child_id, .. }
        | IGuiEvent::Focus { child_id, .. }
        | IGuiEvent::Resize { child_id, .. }
        | IGuiEvent::Close { child_id }
        | IGuiEvent::DpiChange { child_id, .. }
        | IGuiEvent::Tick { child_id, .. }
        | IGuiEvent::ReplSubmit { child_id } => Some(*child_id),
    }
}

fn dispatcher(rx: Receiver<IGuiEvent>) {
    loop {
        let ev = match rx.recv() {
            Ok(ev) => ev,
            Err(_) => break, // SyncSender dropped; process is shutting down
        };

        let target = event_target(&ev);

        // Snapshot Arc refs while holding the map lock, then release the lock
        // before pushing. This avoids a potential deadlock: pushing notifies a
        // waiter which could try to create a new queue (which also needs the
        // map lock).
        let targets: Vec<Arc<ChildQueue>> = {
            let guard = queues_lock().lock().unwrap_or_else(|e| e.into_inner());
            match target {
                None => {
                    // Broadcast: push a clone to every registered queue.
                    guard.values().cloned().collect()
                }
                Some(tid) => {
                    // Per-child: target queue + catch-all.
                    let mut v = Vec::with_capacity(2);
                    if let Some(q) = guard.get(&tid) {
                        v.push(Arc::clone(q));
                    }
                    // Always include CATCH_ALL so single-thread demos that
                    // use (next-event …) without a filter still work.
                    if let Some(q) = guard.get(&CATCH_ALL_ID) {
                        v.push(Arc::clone(q));
                    }
                    v
                }
            }
        };

        // Push the event to every target queue. All but the last need a clone.
        let n = targets.len();
        for (i, q) in targets.into_iter().enumerate() {
            if i + 1 < n {
                q.push(ev.clone());
            } else {
                q.push(ev.clone()); // last element — still clone for simplicity
            }
        }
    }
}

/// Initialise the mailbox and start the dispatcher thread. Idempotent.
pub fn install() {
    MAILBOX.get_or_init(|| {
        // Pre-create the catch-all queue before the dispatcher starts so the
        // first event never races with queue creation.
        get_or_create_queue(CATCH_ALL_ID);

        let (tx, rx) = sync_channel(CAPACITY);

        std::thread::Builder::new()
            .name("igui-dispatcher".into())
            .spawn(move || dispatcher(rx))
            .expect("failed to spawn igui-dispatcher thread");

        Mailbox { tx }
    });
}

/// Push an event from the GUI thread. Non-blocking: drops on full queue
/// rather than blocking the message pump.
pub fn push(ev: IGuiEvent) {
    let Some(mb) = MAILBOX.get() else { return };
    match mb.tx.try_send(ev) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            // A stalled language thread is already a bigger problem.
            eprintln!("[igui] event mailbox full, dropping event");
        }
        Err(TrySendError::Disconnected(_)) => {
            // Receiver gone (dispatcher exited). Ignore silently.
        }
    }
}

// ── Consumer API ─────────────────────────────────────────────────────────────

fn make_deadline(timeout_ms: i64) -> Option<Instant> {
    if timeout_ms < 0 {
        None
    } else {
        Some(Instant::now() + Duration::from_millis(timeout_ms as u64))
    }
}

/// Wait for the next event, filtered by this thread's `filter_on_window` set.
///
/// | Filter size | Behaviour                                              |
/// |-------------|--------------------------------------------------------|
/// | 0 entries   | Block on the CATCH_ALL queue (receives all events)    |
/// | 1 entry     | Delegate to `next_event_for(id, timeout_ms)`          |
/// | N entries   | Spin-poll all matching per-child queues (rare case)   |
///
/// `timeout_ms < 0` blocks indefinitely; `0` polls without blocking;
/// positive values set a wall-clock deadline.
pub fn next_event(timeout_ms: i64) -> Option<IGuiEvent> {
    let filter: HashSet<i64> = THREAD_FILTER.with(|f| f.borrow().clone());

    match filter.len() {
        0 => {
            // No filter: block on the catch-all queue.
            get_or_create_queue(CATCH_ALL_ID).wait_pop(make_deadline(timeout_ms))
        }
        1 => {
            // Common case: exactly one window of interest.
            let target = *filter.iter().next().unwrap();
            next_event_for(target, timeout_ms)
        }
        _ => {
            // Multiple filters: spin-poll all matching queues.
            // This is the rare case (event-loop-for always uses exactly one
            // window per thread). A 1 ms sleep keeps CPU at ~0% when idle.
            let deadline = make_deadline(timeout_ms);
            loop {
                for &id in &filter {
                    if let Some(q) = try_get_queue(id) {
                        if let Some(ev) = q.try_pop() {
                            return Some(ev);
                        }
                    }
                }
                if let Some(dl) = deadline {
                    if Instant::now() >= dl {
                        return None;
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

/// Block until an event arrives for `target` (or a global event).
/// `timeout_ms < 0` blocks indefinitely; the deadline is wall-clock, not
/// reset by the arrival of other events.
pub fn next_event_for(target: i64, timeout_ms: i64) -> Option<IGuiEvent> {
    get_or_create_queue(target).wait_pop(make_deadline(timeout_ms))
}
