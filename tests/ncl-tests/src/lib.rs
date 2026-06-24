//! Unit and integration tests for the NewCormanLisp implementation.
//! Demo-level conformance tests live in ncl-corman-demos.

use std::sync::atomic::Ordering;

use ncl_compiler::Session;

/// Snapshot of `GcStats` at a point in time, plus a few derived
/// readings (used / capacity bytes). Cheap to take — a handful of
/// atomic loads. Used by integration tests to print the GC's
/// activity so we can see when a workload actually exercises the
/// collector vs sits inside one nursery.
#[derive(Clone, Copy, Debug)]
pub struct GcStatsSnapshot {
    pub backend: &'static str,
    pub minor_gcs: u64,
    pub full_gcs: u64,
    pub bytes_promoted_total: u64,
    pub objects_pinned_total: u64,
    pub pinned_residual_cells: u64,
    pub peak_young_used_bytes: u64,
    pub last_minor_pause_us: u64,
    pub min_minor_pause_us: u64,
    pub max_minor_pause_us: u64,
    pub total_minor_pause_us: u64,
    pub young_used_bytes: usize,
    pub old_used_bytes: usize,
}

impl GcStatsSnapshot {
    pub fn from_session(s: &Session) -> GcStatsSnapshot {
        let coord = s.coord();
        let st = &coord.stats;
        let raw_min = st.min_minor_pause_us.load(Ordering::Relaxed);
        GcStatsSnapshot {
            backend: ncl_runtime::gc::ACTIVE_BACKEND_NAME,
            minor_gcs: st.minor_gcs.load(Ordering::Relaxed),
            full_gcs: st.full_gcs.load(Ordering::Relaxed),
            bytes_promoted_total: st.bytes_promoted_total.load(Ordering::Relaxed),
            objects_pinned_total: st.objects_pinned_total.load(Ordering::Relaxed),
            pinned_residual_cells: st.pinned_residual_cells.load(Ordering::Relaxed),
            peak_young_used_bytes: st.peak_young_used_bytes.load(Ordering::Relaxed),
            last_minor_pause_us: st.last_minor_pause_us.load(Ordering::Relaxed),
            // u64::MAX is the "no cycle ran yet" sentinel from
            // `GcStats::default()`. Surface 0 for human reading.
            min_minor_pause_us: if raw_min == u64::MAX { 0 } else { raw_min },
            max_minor_pause_us: st.max_minor_pause_us.load(Ordering::Relaxed),
            total_minor_pause_us: st.total_minor_pause_us.load(Ordering::Relaxed),
            #[allow(deprecated)]
            young_used_bytes: coord.young_used_bytes(),
            #[allow(deprecated)]
            old_used_bytes: coord.old_used_bytes(),
        }
    }

    /// Mean minor-cycle pause in microseconds, or 0 if no cycle
    /// ran. Convenient for one-line summaries.
    pub fn mean_minor_pause_us(&self) -> u64 {
        if self.minor_gcs == 0 {
            0
        } else {
            self.total_minor_pause_us / self.minor_gcs
        }
    }
}

/// Print a one-line GC summary to stderr. Visible during `cargo
/// test` runs with `-- --nocapture`; otherwise captured silently.
///
/// `label` is a free-form tag (test name, phase, etc.) so multiple
/// reports in one test can be told apart.
pub fn report_gc(s: &Session, label: &str) {
    let snap = GcStatsSnapshot::from_session(s);
    eprintln!(
        "[gc/{}/{}] minor_gcs={} full_gcs={} pauses_us[last/min/max/total]={}/{}/{}/{} \
         promoted={}b pinned_objs={} residual_cells={} peak_young={}b \
         young_used={}b old_used={}b mean_pause_us={}",
        snap.backend,
        label,
        snap.minor_gcs,
        snap.full_gcs,
        snap.last_minor_pause_us,
        snap.min_minor_pause_us,
        snap.max_minor_pause_us,
        snap.total_minor_pause_us,
        snap.bytes_promoted_total,
        snap.objects_pinned_total,
        snap.pinned_residual_cells,
        snap.peak_young_used_bytes,
        snap.young_used_bytes,
        snap.old_used_bytes,
        snap.mean_minor_pause_us(),
    );
}

/// Force a minor GC and then report. Used when a test wants to
/// guarantee at least one cycle ran so the stats aren't all zero —
/// most natural workloads under default `GcConfig` don't fill the
/// 16 MB nursery and would otherwise show `minor_gcs=0`.
pub fn force_gc_and_report(s: &mut Session, label: &str) {
    s.force_gc();
    report_gc(s, label);
}

/// Test-fixture wrapper around `Session` that:
///
///   1. Derefs to `Session`, so callers write `s.eval(...)` exactly
///      as before — adoption is a one-line change in each test's
///      `fresh_session_*` helper.
///   2. On Drop (i.e., at test-function exit, including panic
///      unwind), forces one minor GC and prints a one-line GC
///      summary tagged with the test name (or the supplied label).
///
/// The Drop happens whether the test passed or panicked, so GC
/// behaviour is visible even on failures. With `cargo test --
/// --nocapture` the summary lands in the test output; without
/// `--nocapture`, cargo still surfaces it for any failing test.
pub struct TestSession {
    inner: Session,
    label: String,
}

impl TestSession {
    /// Wrap a session with the given label (appears in the report
    /// line). If you want the current thread name as the label —
    /// the cargo-test default that reflects the test function —
    /// use [`TestSession::with_thread_name`].
    pub fn new(session: Session, label: impl Into<String>) -> Self {
        TestSession { inner: session, label: label.into() }
    }

    /// Wrap a session, labelled with the current thread's name.
    /// Inside a `#[test]` function, cargo names the thread after
    /// the test, so the report tag matches the failing test for
    /// easy grep.
    pub fn with_thread_name(session: Session) -> Self {
        let label = std::thread::current()
            .name()
            .unwrap_or("(unnamed)")
            .to_string();
        TestSession::new(session, label)
    }

    /// Borrow as a `&Session` (deref shortcut, sometimes useful
    /// to spell out for clarity).
    pub fn as_session(&self) -> &Session { &self.inner }

    /// Take the inner Session, suppressing the Drop report. Useful
    /// when a test wants to hand the session somewhere else.
    pub fn into_inner(self) -> Session {
        // Don't run the drop report — caller is taking ownership.
        let inner = unsafe { std::ptr::read(&self.inner) };
        std::mem::forget(self);
        inner
    }
}

impl std::ops::Deref for TestSession {
    type Target = Session;
    fn deref(&self) -> &Session { &self.inner }
}

impl std::ops::DerefMut for TestSession {
    fn deref_mut(&mut self) -> &mut Session { &mut self.inner }
}

impl Drop for TestSession {
    fn drop(&mut self) {
        // Force a minor cycle so the report shows real GC work
        // even for small workloads that wouldn't naturally trigger
        // collection. The extra cycle is cheap (microseconds for a
        // small nursery) and makes the summary meaningful.
        self.inner.force_gc();
        report_gc(&self.inner, &self.label);
    }
}
