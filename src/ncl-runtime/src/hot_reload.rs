//! Filesystem watcher → reload queue.
//!
//! Architecture:
//!
//!   ┌────────────────┐    ┌──────────────────┐    ┌──────────────┐
//!   │ notify thread  │───→│  PENDING (mutex) │←───│ Lisp thread  │
//!   │ (OS-fed)       │    │  Vec<PathBuf>    │    │ (poller)     │
//!   └────────────────┘    └──────────────────┘    └──────────────┘
//!
//! The watcher thread is owned by `notify` itself — we just hand
//! it an event handler closure that pushes paths into the queue.
//! The Lisp side calls `drain_pending` at safe points (between
//! REPL prompts in the driver) and re-loads each path.
//!
//! Why a queue, not direct call-into-Lisp:
//!
//! Our compiler / JIT / mutator are single-threaded (per
//! MEMORY.md's "thread boundary = OS boundary" rule for the
//! language thread). The watcher must not call eval. It just
//! marks files dirty; the Lisp thread picks them up on its own
//! schedule.

use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// Global queue of paths the watcher has seen change. Drained by
/// the Lisp side via `pending_reloads_shim`.
static PENDING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// The active watcher, held in a OnceCell-ish slot so it lives for
/// the process lifetime. notify uses RAII — dropping the watcher
/// stops the thread, so we keep the binding alive in a static.
static WATCHER: Mutex<Option<RecommendedWatcher>> = Mutex::new(None);

/// Per-path debounce: minimum interval between consecutive reload
/// pushes for the SAME path. Editors often write a file in 2-3
/// rapid bursts (lockfile dance, atomic rename, save buffer flush);
/// without debouncing we'd reload the same file multiple times.
const DEBOUNCE_MS: u128 = 500;
static LAST_SEEN: Mutex<Vec<(PathBuf, SystemTime)>> = Mutex::new(Vec::new());

fn handle_event(event: notify::Result<Event>) {
    let event = match event {
        Ok(e) => e,
        Err(_) => return,
    };
    // We care about Modify and Create. Remove/Rename get ignored
    // for now (CL doesn't have an "un-define" for whole-file
    // contents).
    match event.kind {
        EventKind::Modify(_) | EventKind::Create(_) => {}
        _ => return,
    }
    for path in event.paths {
        // Only .lisp files matter to us.
        match path.extension().and_then(|e| e.to_str()) {
            Some("lisp") | Some("LISP") => {}
            _ => continue,
        }
        if !should_push(&path) {
            continue;
        }
        if let Ok(mut q) = PENDING.lock() {
            // Dedupe within the queue — if the same path is already
            // pending, don't add a duplicate.
            if !q.contains(&path) {
                q.push(path);
            }
        }
    }
}

/// Debounce check: returns true if PATH hasn't been pushed within
/// the last DEBOUNCE_MS milliseconds.
fn should_push(path: &PathBuf) -> bool {
    let Ok(mut seen) = LAST_SEEN.lock() else { return true };
    let now = SystemTime::now();
    let cutoff = Duration::from_millis(DEBOUNCE_MS as u64);
    // Find existing entry.
    for (p, t) in seen.iter_mut() {
        if p == path {
            if let Ok(elapsed) = now.duration_since(*t) {
                if elapsed < cutoff {
                    return false;
                }
            }
            *t = now;
            return true;
        }
    }
    seen.push((path.clone(), now));
    true
}

/// Start watching DIRECTORY. Returns Err on watcher-creation
/// failure (e.g. directory doesn't exist) — the Lisp wrapper
/// signals a condition in that case.
pub fn start_watching(directory: &str) -> Result<(), String> {
    let mut w = RecommendedWatcher::new(
        |res| handle_event(res),
        Config::default(),
    )
    .map_err(|e| format!("watcher create: {e}"))?;
    let path = std::path::Path::new(directory);
    w.watch(path, RecursiveMode::Recursive)
        .map_err(|e| format!("watcher watch {directory}: {e}"))?;
    if let Ok(mut slot) = WATCHER.lock() {
        // Replace any existing watcher; the old one drops/stops.
        *slot = Some(w);
    }
    Ok(())
}

/// Drain the pending-reload queue. Lisp side calls this between
/// REPL prompts.
pub fn drain_pending() -> Vec<PathBuf> {
    PENDING.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
}

// ─── ABI shims ──────────────────────────────────────────────────────────

use crate::word::Word;

/// `(%watcher-start dir)` — start the filesystem watcher on the
/// given directory. Returns T on success; signals a condition on
/// failure.
pub extern "C-unwind" fn watcher_start_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return crate::abi::signal_condition_string(
            mutator, "%watcher-start: expected 1 arg (directory)",
        );
    }
    let path_word = Word::from_raw(unsafe { *args });
    if path_word.tag() != crate::word::Tag::String {
        return crate::abi::signal_condition_string(
            mutator, "%watcher-start: argument must be a string",
        );
    }
    let dir: String = crate::gc_string::chars_of(path_word).collect();
    match start_watching(&dir) {
        Ok(()) => Word::T.raw(),
        Err(e) => crate::abi::signal_condition_string(
            mutator, &format!("%watcher-start: {e}"),
        ),
    }
}

/// `(%watcher-pending)` — return a freshly-allocated list of
/// pathname strings that have changed since the last call.
/// Returns NIL if no changes.
pub extern "C-unwind" fn watcher_pending_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    _args: *const u64,
    _n_args: u64,
) -> u64 {
    let m = unsafe { &mut *mutator };
    let paths = drain_pending();
    let mut acc = Word::NIL;
    for path in paths.into_iter().rev() {
        let s = path.to_string_lossy().to_string();
        let path_word = crate::gc_string::alloc_string_in_young(m, &s);
        acc = m.alloc_cons(path_word, acc);
    }
    acc.raw()
}
