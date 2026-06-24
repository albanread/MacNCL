//! Roger Corman's THREADS package, ported to a cross-platform Rust
//! foundation. Every Lisp thread here is an OS thread spawned by
//! `std::thread::spawn`; we hand out monotonically increasing
//! integer IDs and stash the `JoinHandle` in a global registry.
//!
//! Cross-platform notes:
//!
//!  * `create-thread` uses `std::thread::Builder` — works on every
//!    platform Rust supports.
//!  * `suspend-thread` / `resume-thread` are **cooperative**, not
//!    `SuspendThread`-style hard pre-emption. The target thread
//!    parks at its next safepoint. The driver emits safepoints at
//!    function-call boundaries (eventually — for v1 the user can
//!    insert `(thread-safepoint)` calls explicitly).
//!  * `terminate-thread` is also cooperative: it sets a flag that
//!    the target thread observes at a safepoint, and that thread
//!    then unwinds by raising the same panic-sentinel as
//!    `exit-thread`.
//!  * `exit-thread` panics with a sentinel payload that the
//!    spawn-thunk's `catch_unwind` recognises and swallows.
//!  * `critical-section` is implemented as a reentrant mutex
//!    (a `Mutex<(owner, count)>` + `Condvar` pair), keyed by an
//!    integer handle. It's not a Win32 CRITICAL_SECTION; matches
//!    Corman's semantics (reentrance, same-thread re-enter is OK).
//!
//! What we deliberately don't expose:
//!
//!  * Per-thread dynamic binding stacks. Corman's docs say
//!    rebinding `*print-base*` in a child thread doesn't bleed into
//!    the parent. NewCormanLisp's per-thread binding stack hasn't
//!    landed yet (Tier 3 in the porting plan); for v1, `setq` on a
//!    special variable is process-global. Don't rebind specials
//!    across threads expecting Corman semantics until that lands.
//!  * Raw Win32 HANDLEs. `thread-handle` returns the same integer
//!    ID; `*current-thread-handle*` / `*current-process-handle*`
//!    are the same integers as their `*-id*` counterparts. Keeping
//!    the surface uniform across platforms.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};

use crate::mutator::MutatorState;
use crate::word::Word;

// ─── Per-thread Lisp ID ─────────────────────────────────────────────────

thread_local! {
    /// Lisp thread id of the current OS thread. 0 means "unassigned"
    /// — the very first thread to call `current_thread_id` gets id 1
    /// retroactively (typically the Lisp primary thread).
    static CURRENT_THREAD_ID: Cell<i64> = const { Cell::new(0) };
}

/// First-call-wins flag: the very first thread to reach
/// `current_thread_id` gets id 1 (the Lisp primary / "main"
/// thread). Subsequent unassigned threads get fresh ids from
/// the registry's next_id counter — they're "implicit" threads
/// (e.g. the GUI thread, hot-reload watcher, test runner pool)
/// that observe the THREADS API without going through
/// `create-thread`.
static MAIN_THREAD_INIT: OnceLock<()> = OnceLock::new();

/// Currently-executing Lisp thread id. The first thread to ask
/// gets id 1; every other unassigned OS thread gets a fresh id
/// auto-registered in the thread table. A thread launched by
/// `create_thread` always has its id set explicitly before this
/// function ever runs there.
pub fn current_thread_id() -> i64 {
    let cached = CURRENT_THREAD_ID.with(|c| c.get());
    if cached != 0 {
        return cached;
    }
    // Race: pick "main" for the first caller, fresh ids for the
    // rest. `get_or_init` returning to its initialiser tells us
    // we're the first caller.
    let mut was_first = false;
    MAIN_THREAD_INIT.get_or_init(|| {
        was_first = true;
    });
    let id = if was_first { 1 } else { alloc_thread_id() };
    let entry = new_entry(id, &format!("implicit-{id}"));
    registry().lock().unwrap().threads.insert(id, entry);
    CURRENT_THREAD_ID.with(|c| c.set(id));
    id
}

/// Force-resolve a Lisp id for the main thread. Called from
/// `create_thread` so the caller has an id BEFORE the new thread
/// races with it.
fn ensure_main_thread_record() {
    let _ = current_thread_id();
}

// ─── Thread registry ────────────────────────────────────────────────────

struct ThreadEntry {
    #[allow(dead_code)]
    id: i64,
    /// Cooperative termination request. Checked at safepoints.
    terminate_requested: AtomicBool,
    /// Cooperative suspend request. Checked at safepoints.
    suspend_requested: AtomicBool,
    /// CV/mutex pair the suspended thread parks on.
    suspend_mu: Mutex<()>,
    suspend_cv: Condvar,
    /// If true, print a line to stderr when this thread terminates.
    report_when_finished: AtomicBool,
    /// Set to true by the spawn-thunk right before it returns. A
    /// (join-thread tid) before this becomes true blocks; after,
    /// it returns immediately.
    finished: AtomicBool,
    /// Condvar broadcast when `finished` flips, so `join-thread`
    /// can wait without spinning.
    finish_mu: Mutex<()>,
    finish_cv: Condvar,
    #[allow(dead_code)]
    name: String,
}

fn new_entry(id: i64, name: &str) -> Arc<ThreadEntry> {
    Arc::new(ThreadEntry {
        id,
        terminate_requested: AtomicBool::new(false),
        suspend_requested: AtomicBool::new(false),
        suspend_mu: Mutex::new(()),
        suspend_cv: Condvar::new(),
        report_when_finished: AtomicBool::new(false),
        finished: AtomicBool::new(false),
        finish_mu: Mutex::new(()),
        finish_cv: Condvar::new(),
        name: name.to_string(),
    })
}

/// JoinHandles live in a separate map from ThreadEntry so the entry
/// can be reaped on thread exit while the handle stays available
/// for a later `(join-thread tid)`. Removed by the call to
/// `join_thread` itself.
fn join_handles() -> &'static Mutex<HashMap<i64, JoinHandle<()>>> {
    static H: OnceLock<Mutex<HashMap<i64, JoinHandle<()>>>> = OnceLock::new();
    H.get_or_init(|| Mutex::new(HashMap::new()))
}

struct ThreadRegistry {
    next_id: i64,
    threads: HashMap<i64, Arc<ThreadEntry>>,
}

fn registry() -> &'static Mutex<ThreadRegistry> {
    static R: OnceLock<Mutex<ThreadRegistry>> = OnceLock::new();
    R.get_or_init(|| {
        Mutex::new(ThreadRegistry {
            // Id 1 is reserved for the main thread (assigned on
            // first ensure_main_thread_record). User threads from 2.
            next_id: 2,
            threads: HashMap::new(),
        })
    })
}

fn alloc_thread_id() -> i64 {
    let mut r = registry().lock().unwrap();
    let id = r.next_id;
    r.next_id += 1;
    id
}

fn get_entry(id: i64) -> Option<Arc<ThreadEntry>> {
    registry().lock().unwrap().threads.get(&id).cloned()
}

fn unregister(id: i64) {
    registry().lock().unwrap().threads.remove(&id);
}

// ─── exit-thread sentinel ───────────────────────────────────────────────

// `exit-thread` is implemented via `panic_any` with an
// ExitThreadPayload sentinel. SEH tables are registered for
// every JIT'd Lisp function (see ncl-llvm/src/jit_mm.rs) so the
// panic unwinds back through any depth of JIT frames to the
// spawn thunk's `catch_unwind`. `terminate-thread` (from another
// thread) is cooperative: it flags the target's ThreadEntry, and
// the next `(thread-safepoint)` call panics from inside the
// target thread.

struct ExitThreadPayload {
    #[allow(dead_code)]
    condition_raw: u64,
}

/// Install a panic hook on first use that silences the default
/// "thread '...' panicked at ...: Box<dyn Any>" stderr line when
/// the payload is our ExitThreadPayload sentinel. The unwind path
/// still proceeds normally; we just spare the user the spurious
/// noise on a clean `(exit-thread)`. Other panics (real bugs)
/// fall through to the default hook.
fn install_quiet_panic_hook() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if info.payload().downcast_ref::<ExitThreadPayload>().is_some() {
                return;
            }
            prev(info);
        }));
    });
}

// ─── create-thread ──────────────────────────────────────────────────────

/// Spawn a new OS thread that runs `fn_word` (a Function-tagged
/// Lisp value) with zero arguments. Returns the new thread's
/// Lisp id (≥ 2). The caller's mutator is only used to fetch the
/// shared `GcCoordinator`; we register a fresh mutator on the new
/// thread so allocations there use a per-thread TLAB.
///
/// `fn_word`'s raw bits are passed across the thread boundary.
/// This is safe because:
///   * Function objects live in the pinned static area (see
///     `gc_function::alloc_function_in_static`) and are never
///     moved by the GC.
///   * The Function's env pointer is read fresh from inside the
///     Function object on each invocation (acquire load), so if
///     the captured environment moves during a GC, the new
///     thread observes the updated location automatically.
pub fn create_thread(
    caller: &MutatorState,
    fn_word: Word,
    report_when_finished: bool,
) -> i64 {
    ensure_main_thread_record();
    install_quiet_panic_hook();
    let coord = Arc::clone(caller.coord());
    let id = alloc_thread_id();
    let entry = new_entry(id, &format!("ncl-thread-{id}"));
    entry
        .report_when_finished
        .store(report_when_finished, Ordering::Release);
    registry().lock().unwrap().threads.insert(id, Arc::clone(&entry));

    let fn_raw = fn_word.raw();
    let entry_for_thread = Arc::clone(&entry);

    let join = thread::Builder::new()
        .name(format!("ncl-thread-{id}"))
        .spawn(move || {
            CURRENT_THREAD_ID.with(|c| c.set(id));

            // Each Lisp thread needs its own MutatorState.
            let mut m = Box::new(coord.register_mutator());
            let mptr: *mut MutatorState = &mut *m;

            // The user-supplied closure is called via the standard
            // Lisp ABI. catch_unwind is cheap insurance: today
            // exit-thread/terminate-thread are cooperative (the
            // worker returns normally when it observes the
            // termination flag at a safepoint), but if a runtime
            // helper somewhere does panic, we'd rather log it than
            // take the whole process down.
            let result = std::panic::catch_unwind(
                std::panic::AssertUnwindSafe(|| unsafe {
                    crate::abi::ncl_funcall(mptr, fn_raw, std::ptr::null(), 0)
                }),
            );
            if entry_for_thread
                .report_when_finished
                .load(Ordering::Acquire)
            {
                match &result {
                    Ok(_) => eprintln!("[threads] thread {id} finished normally"),
                    Err(p) => {
                        if p.downcast_ref::<ExitThreadPayload>().is_some() {
                            eprintln!("[threads] thread {id} called exit-thread");
                        } else {
                            eprintln!(
                                "[threads] thread {id} died with an unhandled condition"
                            );
                        }
                    }
                }
            }
            drop(m);

            // Mark finished and wake any join-thread waiters BEFORE
            // unregister, so a racing waiter that has its Arc to the
            // entry observes the flag.
            {
                let _g = entry_for_thread.finish_mu.lock().unwrap();
                entry_for_thread.finished.store(true, Ordering::Release);
                entry_for_thread.finish_cv.notify_all();
            }
            unregister(id);
        })
        .expect("std::thread::spawn failed");

    join_handles().lock().unwrap().insert(id, join);
    id
}

// ─── join-thread ────────────────────────────────────────────────────────

/// Block until the thread with `id` has finished its function and
/// the OS thread has been joined. Returns true on success; false if
/// the id was not a thread spawned by create-thread, or if it had
/// already been joined.
///
/// Idempotent only in the trivial sense: a second join on the same
/// id sees no JoinHandle and returns false.
pub fn join_thread(id: i64) -> bool {
    // Wait for the finished flag, if the entry still exists. The
    // entry might have been unregister'd already (if the worker
    // finished and we got here just after), in which case the
    // JoinHandle is still in join_handles and we proceed straight
    // to .join().
    if let Some(entry) = get_entry(id) {
        let mut g = entry.finish_mu.lock().unwrap();
        while !entry.finished.load(Ordering::Acquire) {
            g = entry.finish_cv.wait(g).unwrap();
        }
    }
    let h_opt = join_handles().lock().unwrap().remove(&id);
    match h_opt {
        Some(h) => {
            let _ = h.join();
            true
        }
        None => false,
    }
}

// ─── exit-thread (preemptive via SEH-registered unwind) ───────────────

/// Unwind the current thread immediately. Panics with the
/// ExitThreadPayload sentinel; the spawn thunk's `catch_unwind`
/// catches it and the OS thread joins. Matches Roger's Corman
/// contract that exit-thread never returns.
pub fn exit_thread(condition: Word) -> ! {
    std::panic::panic_any(ExitThreadPayload {
        condition_raw: condition.raw(),
    });
}

// ─── suspend / resume / terminate ───────────────────────────────────────

pub fn suspend_thread(id: i64) -> bool {
    match get_entry(id) {
        Some(e) => {
            e.suspend_requested.store(true, Ordering::Release);
            true
        }
        None => false,
    }
}

pub fn resume_thread(id: i64) -> bool {
    match get_entry(id) {
        Some(e) => {
            e.suspend_requested.store(false, Ordering::Release);
            let _g = e.suspend_mu.lock().unwrap();
            e.suspend_cv.notify_all();
            true
        }
        None => false,
    }
}

pub fn terminate_thread(id: i64) -> bool {
    match get_entry(id) {
        Some(e) => {
            e.terminate_requested.store(true, Ordering::Release);
            // Also clear suspend so the target wakes and observes the
            // terminate flag instead of staying parked forever.
            e.suspend_requested.store(false, Ordering::Release);
            let _g = e.suspend_mu.lock().unwrap();
            e.suspend_cv.notify_all();
            true
        }
        None => false,
    }
}

/// Safepoint. ONE call covers both concerns a Lisp thread has to
/// poll for at quiet moments:
///
///   1. GC stop-the-world — `MutatorState::safepoint` parks if
///      another mutator is about to collect.
///   2. THREADS suspend / terminate — read the per-thread registry
///      entry's flags and either park (suspend) or unwind via
///      `panic_any(ExitThreadPayload)` (terminate). The unwind is
///      caught by the spawn thunk's `catch_unwind`; SEH tables
///      registered at JIT-time (see ncl-llvm/src/jit_mm.rs) carry
///      it cleanly through any depth of JIT frames.
///
/// Tight Lisp loops that don't allocate (and therefore never hit
/// the GC's TLAB-refill safepoint by accident) MUST call this
/// periodically. Once the compiler emits safepoints automatically
/// at loop back-edges this becomes a belt-and-braces hook.
pub fn thread_safepoint(m: &mut MutatorState) -> Word {
    m.safepoint();

    let tid = CURRENT_THREAD_ID.with(|c| c.get());
    if tid == 0 {
        return Word::NIL;
    }
    let Some(entry) = get_entry(tid) else { return Word::NIL };

    if entry.suspend_requested.load(Ordering::Acquire) {
        let mut g = entry.suspend_mu.lock().unwrap();
        while entry.suspend_requested.load(Ordering::Acquire) {
            g = entry.suspend_cv.wait(g).unwrap();
            if entry.terminate_requested.load(Ordering::Acquire) {
                drop(g);
                std::panic::panic_any(ExitThreadPayload {
                    condition_raw: Word::NIL.raw(),
                });
            }
        }
    }

    if entry.terminate_requested.load(Ordering::Acquire) {
        std::panic::panic_any(ExitThreadPayload {
            condition_raw: Word::NIL.raw(),
        });
    }
    Word::NIL
}

// ─── sleep ──────────────────────────────────────────────────────────────

/// Sleep for `seconds` (floats accepted via the Lisp wrapper).
/// While parked, the thread is unresponsive to GC-stop and to
/// suspend/terminate — by design, since std::thread::sleep can't
/// be cancelled. Use small slices and (thread-safepoint) in
/// between if you need responsiveness.
pub fn sleep_seconds(seconds: f64) {
    if seconds <= 0.0 || !seconds.is_finite() {
        return;
    }
    std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
}

// ─── High-resolution monotonic clock ────────────────────────────────────
//
// Backs `(get-internal-real-time)` for the Lisp-level `time` macro.
// Returns nanoseconds since process start as a fixnum. 60-bit signed
// fixnum reach is 1.15×10^18 ns ≈ 36 years, so the wrap-around is a
// theoretical concern only.

fn process_start() -> std::time::Instant {
    static T0: OnceLock<std::time::Instant> = OnceLock::new();
    *T0.get_or_init(std::time::Instant::now)
}

pub fn get_internal_real_time_ns() -> i64 {
    process_start().elapsed().as_nanos() as i64
}

// ─── Atomic counters ────────────────────────────────────────────────────
//
// Lock-free integer cells shared across threads. The canonical
// use case is a global "how many work items have I processed"
// counter that scales without contention.
//
// Stored as `Arc<AtomicI64>` keyed by integer handle.

use std::sync::atomic::AtomicI64;

struct AtomicRegistry {
    next_id: i64,
    cells: HashMap<i64, Arc<AtomicI64>>,
}

fn atomic_registry() -> &'static Mutex<AtomicRegistry> {
    static R: OnceLock<Mutex<AtomicRegistry>> = OnceLock::new();
    R.get_or_init(|| {
        Mutex::new(AtomicRegistry { next_id: 1, cells: HashMap::new() })
    })
}

fn atomic_get_cell(id: i64) -> Option<Arc<AtomicI64>> {
    atomic_registry().lock().unwrap().cells.get(&id).cloned()
}

pub fn make_atomic_counter(init: i64) -> i64 {
    let mut r = atomic_registry().lock().unwrap();
    let id = r.next_id;
    r.next_id += 1;
    r.cells.insert(id, Arc::new(AtomicI64::new(init)));
    id
}

pub fn release_atomic_counter(id: i64) -> bool {
    atomic_registry().lock().unwrap().cells.remove(&id).is_some()
}

pub fn atomic_incf(id: i64, delta: i64) -> Option<i64> {
    let cell = atomic_get_cell(id)?;
    Some(cell.fetch_add(delta, Ordering::AcqRel) + delta)
}

pub fn atomic_get(id: i64) -> Option<i64> {
    let cell = atomic_get_cell(id)?;
    Some(cell.load(Ordering::Acquire))
}

pub fn atomic_set(id: i64, v: i64) -> bool {
    match atomic_get_cell(id) {
        Some(cell) => {
            cell.store(v, Ordering::Release);
            true
        }
        None => false,
    }
}

pub fn atomic_cas(id: i64, expected: i64, new: i64) -> Option<i64> {
    let cell = atomic_get_cell(id)?;
    match cell.compare_exchange(expected, new, Ordering::AcqRel, Ordering::Acquire) {
        Ok(v) => Some(v),
        Err(observed) => Some(observed),
    }
}

// ─── Mailboxes (multi-producer multi-consumer queues) ───────────────────
//
// A mailbox is a bounded-or-unbounded queue of Word values.
// (send mb v) pushes; (receive mb &optional timeout) pops, blocking
// until something arrives (or timeout expires). Multiple senders
// and multiple receivers are supported.
//
// HEAP-POINTER CAVEAT for v1: messages stored mid-flight are NOT
// tracked as GC roots. Pass immediates (fixnum, T, NIL, char) or
// interned symbols (which live in the never-moving static area)
// freely. A cons/string/vector passed to send() may have its
// payload moved by GC, leaving a stale pointer in the queue. Until
// we add a GC root-source for mailboxes this is a documented
// limitation. Most thread-pool patterns pass small commands /
// fixnum work-ids anyway.

struct Mailbox {
    /// Words stored verbatim. Bounded by `cap` if Some.
    queue: Mutex<std::collections::VecDeque<u64>>,
    cap: Option<usize>,
    not_empty: Condvar,
    not_full: Condvar,
}

struct MailboxRegistry {
    next_id: i64,
    boxes: HashMap<i64, Arc<Mailbox>>,
}

fn mailbox_registry() -> &'static Mutex<MailboxRegistry> {
    static R: OnceLock<Mutex<MailboxRegistry>> = OnceLock::new();
    R.get_or_init(|| {
        Mutex::new(MailboxRegistry { next_id: 1, boxes: HashMap::new() })
    })
}

fn mailbox_get(id: i64) -> Option<Arc<Mailbox>> {
    mailbox_registry().lock().unwrap().boxes.get(&id).cloned()
}

pub fn make_mailbox(capacity: Option<usize>) -> i64 {
    let mut r = mailbox_registry().lock().unwrap();
    let id = r.next_id;
    r.next_id += 1;
    r.boxes.insert(
        id,
        Arc::new(Mailbox {
            queue: Mutex::new(std::collections::VecDeque::new()),
            cap: capacity,
            not_empty: Condvar::new(),
            not_full: Condvar::new(),
        }),
    );
    id
}

pub fn release_mailbox(id: i64) -> bool {
    mailbox_registry().lock().unwrap().boxes.remove(&id).is_some()
}

/// Block until there is room and push the word.
pub fn mailbox_send(id: i64, raw: u64) -> bool {
    let mb = match mailbox_get(id) {
        Some(mb) => mb,
        None => return false,
    };
    let mut q = mb.queue.lock().unwrap();
    while let Some(cap) = mb.cap {
        if q.len() < cap {
            break;
        }
        q = mb.not_full.wait(q).unwrap();
    }
    q.push_back(raw);
    mb.not_empty.notify_one();
    true
}

/// Block (up to timeout_ms; -1 = forever, 0 = non-blocking) and pop
/// the next word. Returns Some(raw) on success, None on timeout or
/// unknown id.
pub fn mailbox_receive(id: i64, timeout_ms: i64) -> Option<u64> {
    let mb = mailbox_get(id)?;
    let mut q = mb.queue.lock().unwrap();
    if timeout_ms < 0 {
        while q.is_empty() {
            q = mb.not_empty.wait(q).unwrap();
        }
    } else if timeout_ms == 0 {
        if q.is_empty() {
            return None;
        }
    } else {
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(timeout_ms as u64);
        while q.is_empty() {
            let now = std::time::Instant::now();
            if now >= deadline {
                return None;
            }
            let (g, wr) = mb.not_empty.wait_timeout(q, deadline - now).unwrap();
            q = g;
            if wr.timed_out() && q.is_empty() {
                return None;
            }
        }
    }
    let v = q.pop_front()?;
    mb.not_full.notify_one();
    Some(v)
}

pub fn mailbox_len(id: i64) -> Option<usize> {
    let mb = mailbox_get(id)?;
    Some(mb.queue.lock().unwrap().len())
}

// ─── Condition variables ────────────────────────────────────────────────
//
// Thin wrapper around std::sync::Condvar paired with the
// reentrant critical section. The CL idiom:
//
//     (with-synchronization *cs*
//       (loop
//         (when (predicate) (return))
//         (cv-wait *cv* *cs*)))
//
// where `cv-wait` atomically releases the section, parks, and
// re-acquires the section when signaled. Pairs with (cv-notify)
// and (cv-broadcast) from another thread.

struct CondvarEntry {
    cv: Condvar,
    /// Auxiliary lock for cv.wait — std::sync::Condvar requires a
    /// MutexGuard. The user-visible critical section is what
    /// protects the shared state; this lock is just the rendezvous
    /// the Condvar needs internally.
    aux: Mutex<()>,
}

struct CondvarRegistry {
    next_id: i64,
    vars: HashMap<i64, Arc<CondvarEntry>>,
}

fn condvar_registry() -> &'static Mutex<CondvarRegistry> {
    static R: OnceLock<Mutex<CondvarRegistry>> = OnceLock::new();
    R.get_or_init(|| {
        Mutex::new(CondvarRegistry { next_id: 1, vars: HashMap::new() })
    })
}

fn condvar_get(id: i64) -> Option<Arc<CondvarEntry>> {
    condvar_registry().lock().unwrap().vars.get(&id).cloned()
}

pub fn make_condvar() -> i64 {
    let mut r = condvar_registry().lock().unwrap();
    let id = r.next_id;
    r.next_id += 1;
    r.vars.insert(
        id,
        Arc::new(CondvarEntry {
            cv: Condvar::new(),
            aux: Mutex::new(()),
        }),
    );
    id
}

pub fn release_condvar(id: i64) -> bool {
    condvar_registry().lock().unwrap().vars.remove(&id).is_some()
}

/// Wait on cv. Caller must have entered the named critical section
/// `cs_id`; the section is temporarily released for the duration of
/// the wait and re-acquired before returning. Returns false on
/// unknown cv id, true on a normal wake; timeout semantics: -1 wait
/// forever, 0 no-wait, positive milliseconds (true on wake, false
/// on timeout).
///
/// LOST-WAKEUP AVOIDANCE: we acquire `entry.aux` BEFORE releasing
/// the user critical section. condvar_notify also acquires/releases
/// entry.aux around its notify_one, so a notifier that arrives
/// between our CS release and our cv.wait must wait for us to call
/// cv.wait (which atomically releases aux). Without this ordering,
/// a notify in the gap is lost and we park forever.
pub fn condvar_wait(cv_id: i64, cs_id: i64, timeout_ms: i64) -> bool {
    let entry = match condvar_get(cv_id) {
        Some(e) => e,
        None => return false,
    };
    let cs_arc = match cs_registry().lock().unwrap().sections.get(&cs_id) {
        Some(cs) => Arc::clone(cs),
        None => return false,
    };
    let me = current_thread_id();

    // Step 1: claim entry.aux. From this point, condvar_notify (which
    // also acquires entry.aux) cannot fire-and-forget past us.
    let aux_g = entry.aux.lock().unwrap();

    // Step 2: verify ownership and fully release the user CS.
    let saved_count = {
        let mut s = cs_arc.state.lock().unwrap();
        if s.owner != me {
            return false;
        }
        let c = s.count;
        s.owner = 0;
        s.count = 0;
        cs_arc.cv.notify_one();
        c
    };

    // Step 3: park on entry.cv. cv.wait atomically releases aux and
    // suspends; on wake it re-acquires aux. Any notify between
    // step 1 and step 3 is now waiting for aux and will fire after
    // we're parked.
    let (final_aux, waked) = if timeout_ms < 0 {
        let g2 = entry.cv.wait(aux_g).unwrap();
        (g2, true)
    } else if timeout_ms == 0 {
        (aux_g, false)
    } else {
        let (g2, wr) = entry
            .cv
            .wait_timeout(aux_g, std::time::Duration::from_millis(timeout_ms as u64))
            .unwrap();
        (g2, !wr.timed_out())
    };
    drop(final_aux);

    // Step 4: re-acquire the user CS at the same reentrance count.
    {
        let mut s = cs_arc.state.lock().unwrap();
        while s.owner != 0 {
            s = cs_arc.cv.wait(s).unwrap();
        }
        s.owner = me;
        s.count = saved_count;
    }
    waked
}

pub fn condvar_notify(id: i64) -> bool {
    match condvar_get(id) {
        Some(entry) => {
            // Lock+unlock entry.aux: this serialises us against a
            // waiter mid-setup so the notify can't slip past.
            let _g = entry.aux.lock().unwrap();
            entry.cv.notify_one();
            true
        }
        None => false,
    }
}

pub fn condvar_broadcast(id: i64) -> bool {
    match condvar_get(id) {
        Some(entry) => {
            let _g = entry.aux.lock().unwrap();
            entry.cv.notify_all();
            true
        }
        None => false,
    }
}

// ─── Critical sections (reentrant) ──────────────────────────────────────

struct CriticalSection {
    state: Mutex<CsState>,
    cv: Condvar,
}

struct CsState {
    /// 0 = unowned, else the Lisp thread id of the current holder.
    owner: i64,
    /// Reentrance count. Each `enter` from the owner increments, each
    /// `leave` decrements; when this hits 0 the section is released.
    count: u32,
}

struct CsRegistry {
    next_id: i64,
    sections: HashMap<i64, Arc<CriticalSection>>,
}

fn cs_registry() -> &'static Mutex<CsRegistry> {
    static R: OnceLock<Mutex<CsRegistry>> = OnceLock::new();
    R.get_or_init(|| {
        Mutex::new(CsRegistry {
            next_id: 1,
            sections: HashMap::new(),
        })
    })
}

pub fn allocate_critical_section() -> i64 {
    let mut reg = cs_registry().lock().unwrap();
    let id = reg.next_id;
    reg.next_id += 1;
    reg.sections.insert(
        id,
        Arc::new(CriticalSection {
            state: Mutex::new(CsState { owner: 0, count: 0 }),
            cv: Condvar::new(),
        }),
    );
    id
}

pub fn deallocate_critical_section(id: i64) -> bool {
    cs_registry().lock().unwrap().sections.remove(&id).is_some()
}

pub fn enter_critical_section(id: i64) -> bool {
    let cs = match cs_registry().lock().unwrap().sections.get(&id) {
        Some(cs) => Arc::clone(cs),
        None => return false,
    };
    let me = current_thread_id();
    let mut s = cs.state.lock().unwrap();
    loop {
        if s.owner == 0 {
            s.owner = me;
            s.count = 1;
            return true;
        }
        if s.owner == me {
            s.count += 1;
            return true;
        }
        s = cs.cv.wait(s).unwrap();
    }
}

pub fn leave_critical_section(id: i64) -> bool {
    let cs = match cs_registry().lock().unwrap().sections.get(&id) {
        Some(cs) => Arc::clone(cs),
        None => return false,
    };
    let me = current_thread_id();
    let mut s = cs.state.lock().unwrap();
    if s.owner != me {
        return false;
    }
    s.count -= 1;
    if s.count == 0 {
        s.owner = 0;
        cs.cv.notify_one();
    }
    true
}

// ─── Shim entry points (extern "C-unwind") ──────────────────────────────

unsafe fn arg(args: *const u64, i: u64) -> Word {
    Word::from_raw(unsafe { *args.add(i as usize) })
}

unsafe fn arg_fixnum_or(args: *const u64, i: u64, default: i64) -> i64 {
    unsafe { arg(args, i) }.as_fixnum().unwrap_or(default)
}

/// `(create-thread function)` — spawn a new thread running FUNCTION
/// with zero arguments. Returns the new thread's integer id.
///
/// Corman's signature includes `&key (report-when-finished t)`;
/// the runtime always returns the id, and the Lisp wrapper in
/// Library/threads.lisp handles the keyword argument.
pub extern "C-unwind" fn create_thread_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args < 1 {
        panic!("create-thread: expected at least 1 arg (function), got {n_args}");
    }
    let fn_word = unsafe { arg(args, 0) };
    let report = if n_args >= 2 {
        unsafe { arg(args, 1) }.raw() != Word::NIL.raw()
    } else {
        true
    };
    let m = unsafe { &*mutator };
    let id = create_thread(m, fn_word, report);
    Word::fixnum(id).raw()
}

/// `(exit-thread &optional condition)` — unwind the current thread.
/// Calling from the main thread is undefined behaviour (we don't
/// install a catch_unwind there); caller's responsibility.
pub extern "C-unwind" fn exit_thread_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    let cond = if n_args >= 1 {
        unsafe { arg(args, 0) }
    } else {
        Word::NIL
    };
    exit_thread(cond);
}

/// `(%test-panic)` — Diagnostic: panic_any from a Rust shim, then
/// see if catch_unwind elsewhere catches it without crashing the
/// JIT call chain. Used to isolate the SEH-unwind-through-JIT
/// pathway from the threading layer. Don't call this in real code.
pub extern "C-unwind" fn test_panic_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    _args: *const u64,
    _n_args: u64,
) -> u64 {
    std::panic::panic_any(ExitThreadPayload {
        condition_raw: Word::NIL.raw(),
    });
}

/// `(thread-handle thread-id)` — Corman's API returns a Win32
/// HANDLE. We return the same integer id (no separate handle), or
/// NIL if the id isn't a live registered thread.
pub extern "C-unwind" fn thread_handle_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    _n_args: u64,
) -> u64 {
    let tid = unsafe { arg_fixnum_or(args, 0, -1) };
    if get_entry(tid).is_some() {
        Word::fixnum(tid).raw()
    } else {
        Word::NIL.raw()
    }
}

pub extern "C-unwind" fn suspend_thread_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    _n_args: u64,
) -> u64 {
    let tid = unsafe { arg_fixnum_or(args, 0, -1) };
    if suspend_thread(tid) { Word::T.raw() } else { Word::NIL.raw() }
}

pub extern "C-unwind" fn resume_thread_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    _n_args: u64,
) -> u64 {
    let tid = unsafe { arg_fixnum_or(args, 0, -1) };
    if resume_thread(tid) { Word::T.raw() } else { Word::NIL.raw() }
}

pub extern "C-unwind" fn terminate_thread_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    _n_args: u64,
) -> u64 {
    let tid = unsafe { arg_fixnum_or(args, 0, -1) };
    if terminate_thread(tid) { Word::T.raw() } else { Word::NIL.raw() }
}

pub extern "C-unwind" fn current_thread_id_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    _args: *const u64,
    _n_args: u64,
) -> u64 {
    Word::fixnum(current_thread_id()).raw()
}

pub extern "C-unwind" fn current_process_id_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    _args: *const u64,
    _n_args: u64,
) -> u64 {
    Word::fixnum(std::process::id() as i64).raw()
}

pub extern "C-unwind" fn allocate_critical_section_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    _args: *const u64,
    _n_args: u64,
) -> u64 {
    Word::fixnum(allocate_critical_section()).raw()
}

pub extern "C-unwind" fn deallocate_critical_section_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    _n_args: u64,
) -> u64 {
    let id = unsafe { arg_fixnum_or(args, 0, -1) };
    if deallocate_critical_section(id) { Word::T.raw() } else { Word::NIL.raw() }
}

pub extern "C-unwind" fn enter_critical_section_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    _n_args: u64,
) -> u64 {
    let id = unsafe { arg_fixnum_or(args, 0, -1) };
    if enter_critical_section(id) { Word::T.raw() } else { Word::NIL.raw() }
}

pub extern "C-unwind" fn leave_critical_section_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    _n_args: u64,
) -> u64 {
    let id = unsafe { arg_fixnum_or(args, 0, -1) };
    if leave_critical_section(id) { Word::T.raw() } else { Word::NIL.raw() }
}

/// `(thread-safepoint)` — returns T if a termination has been
/// requested for the current thread (caller should bail out of its
/// loop), NIL otherwise. Always performs the GC + suspend poll
/// regardless of return value.
pub extern "C-unwind" fn join_thread_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    _n_args: u64,
) -> u64 {
    let tid = unsafe { arg_fixnum_or(args, 0, -1) };
    // Park while waiting — see comment on sleep_shim.
    let m = unsafe { &mut *mutator };
    m.enter_blocked();
    let ok = join_thread(tid);
    m.leave_blocked();
    if ok { Word::T.raw() } else { Word::NIL.raw() }
}

/// `(sleep seconds)` — accepts fixnum or float seconds.
///
/// Marks the thread as parked for the duration so a peer GC
/// trigger isn't waiting for us to reach a safepoint we'll never
/// hit. Without this, eight workers + one sleeping main thread
/// would deadlock the first time young fills up: the worker that
/// becomes the trigger waits for `parked_count` to reach 8, but
/// main never parks because it's blocked in OS sleep.
pub extern "C-unwind" fn sleep_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    _n_args: u64,
) -> u64 {
    let w = unsafe { arg(args, 0) };
    let seconds = crate::float::to_f64(w).unwrap_or(0.0);
    let m = unsafe { &mut *mutator };
    m.enter_blocked();
    sleep_seconds(seconds);
    m.leave_blocked();
    Word::T.raw()
}

/// `(get-internal-real-time)` — monotonic nanoseconds since process
/// start, as a fixnum. Pair with `INTERNAL-TIME-UNITS-PER-SECOND`
/// (= 1_000_000_000) to get seconds.
pub extern "C-unwind" fn get_internal_real_time_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    _args: *const u64,
    _n_args: u64,
) -> u64 {
    Word::fixnum(get_internal_real_time_ns()).raw()
}

pub extern "C-unwind" fn make_atomic_counter_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    let init = if n_args >= 1 {
        unsafe { arg_fixnum_or(args, 0, 0) }
    } else {
        0
    };
    Word::fixnum(make_atomic_counter(init)).raw()
}

pub extern "C-unwind" fn release_atomic_counter_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    _n_args: u64,
) -> u64 {
    let id = unsafe { arg_fixnum_or(args, 0, -1) };
    if release_atomic_counter(id) { Word::T.raw() } else { Word::NIL.raw() }
}

pub extern "C-unwind" fn atomic_incf_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    let id = unsafe { arg_fixnum_or(args, 0, -1) };
    let delta = if n_args >= 2 { unsafe { arg_fixnum_or(args, 1, 1) } } else { 1 };
    match atomic_incf(id, delta) {
        Some(v) => Word::fixnum(v).raw(),
        None => Word::NIL.raw(),
    }
}

pub extern "C-unwind" fn atomic_get_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    _n_args: u64,
) -> u64 {
    let id = unsafe { arg_fixnum_or(args, 0, -1) };
    match atomic_get(id) {
        Some(v) => Word::fixnum(v).raw(),
        None => Word::NIL.raw(),
    }
}

pub extern "C-unwind" fn atomic_set_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    _n_args: u64,
) -> u64 {
    let id = unsafe { arg_fixnum_or(args, 0, -1) };
    let v = unsafe { arg_fixnum_or(args, 1, 0) };
    if atomic_set(id, v) { Word::T.raw() } else { Word::NIL.raw() }
}

pub extern "C-unwind" fn atomic_cas_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    _n_args: u64,
) -> u64 {
    let id = unsafe { arg_fixnum_or(args, 0, -1) };
    let expected = unsafe { arg_fixnum_or(args, 1, 0) };
    let new = unsafe { arg_fixnum_or(args, 2, 0) };
    match atomic_cas(id, expected, new) {
        Some(observed) => Word::fixnum(observed).raw(),
        None => Word::NIL.raw(),
    }
}

pub extern "C-unwind" fn make_mailbox_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    let cap = if n_args >= 1 {
        let w = unsafe { arg(args, 0) };
        if w.raw() == Word::NIL.raw() {
            None
        } else {
            w.as_fixnum().map(|n| n.max(0) as usize)
        }
    } else {
        None
    };
    Word::fixnum(make_mailbox(cap)).raw()
}

pub extern "C-unwind" fn release_mailbox_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    _n_args: u64,
) -> u64 {
    let id = unsafe { arg_fixnum_or(args, 0, -1) };
    if release_mailbox(id) { Word::T.raw() } else { Word::NIL.raw() }
}

pub extern "C-unwind" fn mailbox_send_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    _n_args: u64,
) -> u64 {
    let id = unsafe { arg_fixnum_or(args, 0, -1) };
    let v = unsafe { arg(args, 1) }.raw();
    if mailbox_send(id, v) { Word::T.raw() } else { Word::NIL.raw() }
}

/// `(%mailbox-receive id timeout-ms)`. timeout-ms: -1 wait forever,
/// 0 non-blocking, positive milliseconds. Returns the value, or
/// the immediate value `(values nil :timeout)` is represented here
/// as just `NIL` — Lisp wrappers distinguish via `mailbox-len`.
pub extern "C-unwind" fn mailbox_receive_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    _n_args: u64,
) -> u64 {
    let id = unsafe { arg_fixnum_or(args, 0, -1) };
    let timeout_ms = unsafe { arg_fixnum_or(args, 1, -1) };
    let m = unsafe { &mut *mutator };
    // Park if we're going to actually wait. (Non-blocking
    // try-receive — timeout 0 — doesn't need to park.)
    if timeout_ms == 0 {
        return match mailbox_receive(id, 0) {
            Some(v) => v,
            None => Word::NIL.raw(),
        };
    }
    m.enter_blocked();
    let result = mailbox_receive(id, timeout_ms);
    m.leave_blocked();
    match result {
        Some(v) => v,
        None => Word::NIL.raw(),
    }
}

pub extern "C-unwind" fn mailbox_len_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    _n_args: u64,
) -> u64 {
    let id = unsafe { arg_fixnum_or(args, 0, -1) };
    match mailbox_len(id) {
        Some(n) => Word::fixnum(n as i64).raw(),
        None => Word::NIL.raw(),
    }
}

pub extern "C-unwind" fn make_condvar_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    _args: *const u64,
    _n_args: u64,
) -> u64 {
    Word::fixnum(make_condvar()).raw()
}

pub extern "C-unwind" fn release_condvar_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    _n_args: u64,
) -> u64 {
    let id = unsafe { arg_fixnum_or(args, 0, -1) };
    if release_condvar(id) { Word::T.raw() } else { Word::NIL.raw() }
}

pub extern "C-unwind" fn condvar_wait_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    _n_args: u64,
) -> u64 {
    let cv_id = unsafe { arg_fixnum_or(args, 0, -1) };
    let cs_id = unsafe { arg_fixnum_or(args, 1, -1) };
    let timeout_ms = unsafe { arg_fixnum_or(args, 2, -1) };
    let m = unsafe { &mut *mutator };
    if timeout_ms == 0 {
        return if condvar_wait(cv_id, cs_id, 0) { Word::T.raw() } else { Word::NIL.raw() };
    }
    m.enter_blocked();
    let result = condvar_wait(cv_id, cs_id, timeout_ms);
    m.leave_blocked();
    if result { Word::T.raw() } else { Word::NIL.raw() }
}

pub extern "C-unwind" fn condvar_notify_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    _n_args: u64,
) -> u64 {
    let id = unsafe { arg_fixnum_or(args, 0, -1) };
    if condvar_notify(id) { Word::T.raw() } else { Word::NIL.raw() }
}

pub extern "C-unwind" fn condvar_broadcast_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    _n_args: u64,
) -> u64 {
    let id = unsafe { arg_fixnum_or(args, 0, -1) };
    if condvar_broadcast(id) { Word::T.raw() } else { Word::NIL.raw() }
}

/// `(thread-safepoint)` — runs the safepoint poll. Returns NIL on
/// the normal path; never returns at all when a terminate-thread
/// or exit-thread is in flight (unwinds via panic instead). Lisp
/// callers can still use the historical pattern of
/// `(loop (when (thread-safepoint) (return)) ...)` — it'll just
/// never see T, which is harmless.
pub extern "C-unwind" fn thread_safepoint_shim(
    mutator: *mut MutatorState,
    _env: u64,
    _args: *const u64,
    _n_args: u64,
) -> u64 {
    let m = unsafe { &mut *mutator };
    thread_safepoint(m).raw()
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn critical_section_serialises() {
        // Two threads each increment a shared counter 1000 times,
        // inside a critical section. Final value must be 2000.
        let cs = allocate_critical_section();
        let counter = Arc::new(Mutex::new(0i64));
        let n = 1000;

        let c1 = Arc::clone(&counter);
        let c2 = Arc::clone(&counter);
        let t1 = thread::spawn(move || {
            // Each thread gets a fresh CURRENT_THREAD_ID via the
            // critical section's enter/leave reentrance check (which
            // uses current_thread_id internally).
            for _ in 0..n {
                enter_critical_section(cs);
                let mut g = c1.lock().unwrap();
                *g += 1;
                drop(g);
                leave_critical_section(cs);
            }
        });
        let t2 = thread::spawn(move || {
            for _ in 0..n {
                enter_critical_section(cs);
                let mut g = c2.lock().unwrap();
                *g += 1;
                drop(g);
                leave_critical_section(cs);
            }
        });
        t1.join().unwrap();
        t2.join().unwrap();
        assert_eq!(*counter.lock().unwrap(), 2 * n);
        deallocate_critical_section(cs);
    }

    #[test]
    fn critical_section_is_reentrant_same_thread() {
        let cs = allocate_critical_section();
        assert!(enter_critical_section(cs));
        assert!(enter_critical_section(cs)); // reentrant
        assert!(enter_critical_section(cs));
        assert!(leave_critical_section(cs));
        assert!(leave_critical_section(cs));
        assert!(leave_critical_section(cs));
        // Now unowned — a fresh thread can grab it.
        let cs_id = cs;
        let h = thread::spawn(move || enter_critical_section(cs_id));
        assert!(h.join().unwrap());
        // Leave from that thread is implicit on thread exit? No —
        // we never call leave there. The CS stays owned by t2.
        // Test cleanup: just drop the registry entry.
        deallocate_critical_section(cs);
    }

    #[test]
    fn leave_from_non_owner_returns_false() {
        let cs = allocate_critical_section();
        // Acquired by the main thread.
        assert!(enter_critical_section(cs));
        // A different thread tries to leave — must fail without
        // releasing the lock.
        let cs_id = cs;
        let h = thread::spawn(move || leave_critical_section(cs_id));
        assert!(!h.join().unwrap());
        assert!(leave_critical_section(cs));
        deallocate_critical_section(cs);
    }

    #[test]
    fn unknown_cs_id_is_rejected() {
        assert!(!enter_critical_section(99999));
        assert!(!leave_critical_section(99999));
        assert!(!deallocate_critical_section(99999));
    }

    #[test]
    fn unknown_thread_id_is_rejected() {
        assert!(!suspend_thread(99999));
        assert!(!resume_thread(99999));
        assert!(!terminate_thread(99999));
    }

    #[test]
    fn current_thread_id_is_stable_and_unique() {
        // The same OS thread sees the same Lisp id every call.
        let a = current_thread_id();
        let b = current_thread_id();
        assert_eq!(a, b);
        assert!(a > 0);
        // A spawned thread sees a *different* id from the parent.
        let h = thread::spawn(|| current_thread_id());
        let child = h.join().unwrap();
        assert_ne!(a, child, "spawned thread shouldn't share parent id");
    }
}
