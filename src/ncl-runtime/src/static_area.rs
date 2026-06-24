//! Static area: pinned, never-moved memory for things that never die.
//!
//! See `docs/GC.md`. The static area is for:
//!   - JIT-emitted machine code (when the JIT lands)
//!   - The loaded image's interned constants
//!   - The package and symbol registries
//!
//! Allocation is bump-pointer with atomic CAS so multiple Lisp
//! threads can allocate concurrently without locks. Storage is never
//! freed individually — the static area lives for the process
//! lifetime. The loader's retirement-with-quiescent-epoch mechanism
//! handles "old code that's no longer reachable" separately from the
//! GC; see MANIFESTO.md, "The loader".
//!
//! Static→young pointers are handled by a card table (same kind as
//! old's). Mutators dirty cards on every store of a heap pointer
//! into a static cell. Minor GC scans dirty cards for young
//! pointers; full GC scans the whole static area for any heap
//! pointer.
//!
//! Static cells are read and written by multiple threads; all access
//! goes through `*mut u64` after a successful CAS bump. The
//! park/unpark synchronisation barrier in the GC coordinator
//! provides the cross-thread happens-before relationship; within a
//! thread, the alloc-then-write pattern guarantees exclusive access
//! to a freshly allocated range until the next safe point.

use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::heap::{CardTable, HeapHeader, HeapType};

/// Internal storage strategy for the static area. Tests and small
/// configurations use `Boxed` (a single Rust heap allocation, all
/// memory committed up front). The production default uses `Virtual`
/// (Windows `VirtualAlloc(MEM_RESERVE)` for a large address range,
/// committing pages on demand) — see `new_elastic`.
///
/// On non-Windows targets, `Virtual` falls back to `Boxed` with the
/// reservation size, which still works correctly but commits all of
/// it eagerly. A proper `mmap(MAP_NORESERVE)`-based Unix backing is
/// future work; Windows is the primary platform and that's where
/// the win matters.
enum Backing {
    /// Owned heap allocation. Full reservation = full commit.
    Boxed(Box<[u64]>),
    /// VirtualAlloc-reserved, page-granular committed range.
    /// `Drop` releases via `VirtualFree(MEM_RELEASE)`.
    #[cfg(windows)]
    Virtual {
        base: *mut u64,
        reserved_cells: usize,
    },
}

// SAFETY: Backing::Virtual holds a raw pointer to a VirtualAlloc'd
// region. The region is process-lifetime stable and access goes
// through the CAS-bump discipline (no aliased mutation). Box<[u64]>
// is naturally Send+Sync via its impl.
unsafe impl Send for Backing {}
unsafe impl Sync for Backing {}

impl Backing {
    fn base(&self) -> *mut u64 {
        match self {
            Backing::Boxed(b) => b.as_ptr() as *mut u64,
            #[cfg(windows)]
            Backing::Virtual { base, .. } => *base,
        }
    }

    fn reserved_cells(&self) -> usize {
        match self {
            Backing::Boxed(b) => b.len(),
            #[cfg(windows)]
            Backing::Virtual { reserved_cells, .. } => *reserved_cells,
        }
    }
}

#[cfg(windows)]
impl Drop for Backing {
    fn drop(&mut self) {
        if let Backing::Virtual { base, .. } = self {
            // VirtualFree(addr, 0, MEM_RELEASE) releases the entire
            // reservation. Size MUST be 0 for MEM_RELEASE — passing
            // anything else returns an error and leaks the reservation.
            use windows::Win32::System::Memory::{VirtualFree, MEM_RELEASE};
            unsafe {
                let _ = VirtualFree(
                    *base as *mut _,
                    0,
                    MEM_RELEASE,
                );
            }
        }
    }
}

/// Pinned, never-moved memory region. One per process.
pub struct StaticArea {
    /// Backing storage. See `Backing` for the two strategies.
    storage: Backing,
    /// Bump pointer (cell index). Atomically advanced on every alloc.
    top: AtomicUsize,
    /// Number of cells currently backed by committed pages. For
    /// `Boxed` backing this equals reserved_cells (everything is
    /// committed up front). For `Virtual` backing this advances in
    /// page-aligned chunks as `try_alloc_cells` crosses the
    /// committed frontier.
    committed_cells: AtomicUsize,
    /// Serializes the commit-grow path so two threads racing to
    /// extend `committed_cells` only call `VirtualAlloc` once. The
    /// fast path (allocation fits within already-committed range)
    /// doesn't touch this lock.
    commit_lock: Mutex<()>,
    /// Card table for static→young pointers. Cleared after minor GC.
    cards: Arc<CardTable>,
}

/// Size of a single commit grow in cells. Each grow rounds up to
/// this many cells and commits in one VirtualAlloc call. 128 KB of
/// cells (= 1 MB of backing) is the sweet spot: large enough that
/// commit-grow overhead is negligible, small enough that an idle
/// session doesn't pay for headroom it'll never touch.
#[cfg(windows)]
const COMMIT_GRANULARITY_CELLS: usize = 128 * 1024;

impl StaticArea {
    /// Old constructor: allocate `size_bytes` from the regular heap
    /// (Box-backed). Used by tests for predictable, exactly-sized
    /// regions. Production code prefers `new_elastic`.
    pub fn new(size_bytes: usize) -> StaticArea {
        let n_cells = size_bytes / 8;
        let cells = vec![0u64; n_cells].into_boxed_slice();
        let cards = Arc::new(CardTable::new(size_bytes));
        StaticArea {
            storage: Backing::Boxed(cells),
            top: AtomicUsize::new(0),
            committed_cells: AtomicUsize::new(n_cells),
            commit_lock: Mutex::new(()),
            cards,
        }
    }

    /// Reserve `reserved_bytes` of address space and commit only
    /// the first `initial_commit_bytes` up front. Subsequent
    /// allocations commit additional pages on demand.
    ///
    /// On Windows: uses `VirtualAlloc(MEM_RESERVE, PAGE_NOACCESS)`
    /// for the reservation and `VirtualAlloc(MEM_COMMIT,
    /// PAGE_READWRITE)` for the initial chunk. The reservation
    /// costs almost nothing — only an entry in the VAD tree;
    /// committed pages charge against the process working set and
    /// page-file commit budget.
    ///
    /// On non-Windows: falls back to `new(reserved_bytes)` for now
    /// (a proper `mmap(MAP_NORESERVE)` backing is future work).
    ///
    /// The card table covers the full reservation. At 512-byte
    /// cards, a 256 MB reservation costs 512 KB of card-table
    /// memory — paid up front, but trivial.
    pub fn new_elastic(reserved_bytes: usize, initial_commit_bytes: usize) -> StaticArea {
        #[cfg(windows)]
        {
            use windows::Win32::System::Memory::{
                VirtualAlloc, MEM_COMMIT, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE,
            };
            // Round up to 64 KB allocation granularity (Windows
            // VirtualAlloc requirement for the base address).
            let alloc_gran = 64 * 1024;
            let reserved = reserved_bytes.next_multiple_of(alloc_gran);
            let initial = initial_commit_bytes
                .next_multiple_of(alloc_gran)
                .min(reserved);
            let reserved_cells = reserved / 8;
            // Reserve the address space.
            let base = unsafe {
                VirtualAlloc(
                    None,
                    reserved,
                    MEM_RESERVE,
                    PAGE_NOACCESS,
                )
            };
            if base.is_null() {
                panic!(
                    "StaticArea::new_elastic: VirtualAlloc(MEM_RESERVE, {reserved}) failed"
                );
            }
            // Commit the initial chunk.
            if initial > 0 {
                let p = unsafe {
                    VirtualAlloc(
                        Some(base as *const _),
                        initial,
                        MEM_COMMIT,
                        PAGE_READWRITE,
                    )
                };
                if p.is_null() {
                    panic!(
                        "StaticArea::new_elastic: VirtualAlloc(MEM_COMMIT, {initial}) failed"
                    );
                }
            }
            let cards = Arc::new(CardTable::new(reserved));
            StaticArea {
                storage: Backing::Virtual {
                    base: base as *mut u64,
                    reserved_cells,
                },
                top: AtomicUsize::new(0),
                committed_cells: AtomicUsize::new(initial / 8),
                commit_lock: Mutex::new(()),
                cards,
            }
        }
        #[cfg(not(windows))]
        {
            let _ = initial_commit_bytes;
            Self::new(reserved_bytes)
        }
    }

    pub fn capacity_cells(&self) -> usize { self.storage.reserved_cells() }
    pub fn capacity_bytes(&self) -> usize { self.storage.reserved_cells() * 8 }
    pub fn used_cells(&self) -> usize { self.top.load(Ordering::Acquire) }
    pub fn used_bytes(&self) -> usize { self.used_cells() * 8 }
    pub fn free_cells(&self) -> usize { self.capacity_cells() - self.used_cells() }
    pub fn free_bytes(&self) -> usize { self.free_cells() * 8 }
    /// Bytes currently committed (backed by physical pages or
    /// page-file). For `Boxed` backing this equals
    /// `capacity_bytes`. For `Virtual` backing this is the
    /// page-aligned commit frontier — lets the diagnostics path
    /// report "resident set" vs "reserved address space."
    pub fn committed_bytes(&self) -> usize {
        self.committed_cells.load(Ordering::Acquire) * 8
    }

    pub fn cards(&self) -> &Arc<CardTable> { &self.cards }

    pub fn base_ptr(&self) -> *const u8 { self.storage.base() as *const u8 }

    pub fn contains_ptr(&self, ptr: *const u8) -> bool {
        let base = self.storage.base() as usize;
        let end = base + self.storage.reserved_cells() * 8;
        let p = ptr as usize;
        p >= base && p < end
    }

    /// Cell index of `ptr` within this area, or `None` if outside.
    pub fn cell_index_of(&self, ptr: *const u8) -> Option<usize> {
        let base = self.storage.base() as usize;
        let end = base + self.storage.reserved_cells() * 8;
        let p = ptr as usize;
        if p >= base && p < end { Some((p - base) / 8) } else { None }
    }

    /// Pointer to the cell at index `idx`. Caller must ensure
    /// `idx < capacity_cells()`. Used by the GC scanner; allowed to
    /// be dead in some configurations.
    #[allow(dead_code)]
    pub(crate) fn cell_ptr(&self, idx: usize) -> *mut u64 {
        debug_assert!(idx < self.storage.reserved_cells());
        unsafe { self.storage.base().add(idx) }
    }

    /// Commit additional pages so that `committed_cells` >= `target`.
    /// Called from the slow path of `try_alloc_cells` when the bump
    /// pointer crosses the committed frontier. Holds `commit_lock`
    /// for the duration so two threads can't both `VirtualAlloc` the
    /// same range.
    #[cfg(windows)]
    fn grow_committed_to(&self, target_cells: usize) -> bool {
        let _g = match self.commit_lock.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        // Re-check under lock — another thread may have just grown.
        let current = self.committed_cells.load(Ordering::Acquire);
        if current >= target_cells {
            return true;
        }
        // Commit in COMMIT_GRANULARITY_CELLS chunks, rounded up to
        // cover `target_cells`.
        let needed = target_cells - current;
        let chunks = needed.div_ceil(COMMIT_GRANULARITY_CELLS);
        let grow_cells = chunks * COMMIT_GRANULARITY_CELLS;
        let new_committed = (current + grow_cells).min(self.storage.reserved_cells());
        let commit_bytes = (new_committed - current) * 8;
        let commit_addr = unsafe { self.storage.base().add(current) };
        use windows::Win32::System::Memory::{VirtualAlloc, MEM_COMMIT, PAGE_READWRITE};
        let result = unsafe {
            VirtualAlloc(
                Some(commit_addr as *const _),
                commit_bytes,
                MEM_COMMIT,
                PAGE_READWRITE,
            )
        };
        if result.is_null() {
            return false;
        }
        self.committed_cells
            .store(new_committed, Ordering::Release);
        true
    }

    /// Allocate `cells` cells via lock-free CAS. Returns a pointer
    /// to the first cell, or `None` if the reservation is exhausted.
    /// Multiple threads may call concurrently; each gets a disjoint
    /// range.
    ///
    /// Fast path: top stays below `committed_cells`. Pure CAS, no
    /// system calls.
    ///
    /// Slow path: if the new top would cross the committed frontier
    /// (Virtual backing only), call `grow_committed_to` to commit
    /// more pages before completing the bump.
    pub fn try_alloc_cells(&self, cells: usize) -> Option<NonNull<u64>> {
        loop {
            let cur = self.top.load(Ordering::Acquire);
            let new = cur.checked_add(cells)?;
            if new > self.storage.reserved_cells() {
                return None;
            }
            // Commit-grow check. Boxed backing has committed_cells
            // == reserved_cells, so this branch never fires.
            let committed = self.committed_cells.load(Ordering::Acquire);
            if new > committed {
                #[cfg(windows)]
                {
                    if !self.grow_committed_to(new) {
                        return None;
                    }
                }
                #[cfg(not(windows))]
                {
                    // Boxed backing only; committed == reserved.
                    // Reaching here means reserved was undersized.
                    return None;
                }
            }
            if self.top
                .compare_exchange(cur, new, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let p = unsafe { self.storage.base().add(cur) };
                return Some(unsafe { NonNull::new_unchecked(p) });
            }
            // Lost the race — retry.
        }
    }

    /// Allocate a header'd object in static. Caller fills the payload
    /// cells.
    pub fn try_alloc_with_header(
        &self,
        ty: HeapType,
        length_cells: u32,
    ) -> Option<NonNull<HeapHeader>> {
        let total = 1 + length_cells as usize;
        let p = self.try_alloc_cells(total)?;
        unsafe { *p.as_ptr() = HeapHeader::new(ty, length_cells).raw(); }
        Some(unsafe { NonNull::new_unchecked(p.as_ptr() as *mut HeapHeader) })
    }

    /// Allocate a headerless cons cell in static. Used for quoted
    /// list literals, which the compiler builds at lower time.
    /// Returns a Cons-tagged Word.
    pub fn try_alloc_cons(&self, car: crate::word::Word, cdr: crate::word::Word) -> Option<crate::word::Word> {
        let p = self.try_alloc_cells(2)?;
        unsafe {
            *p.as_ptr() = car.raw();
            *p.as_ptr().add(1) = cdr.raw();
        }
        Some(crate::word::Word::from_ptr(
            p.as_ptr() as *const u8,
            crate::word::Tag::Cons,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn static_starts_empty() {
        let s = StaticArea::new(1024);
        assert_eq!(s.used_bytes(), 0);
        assert_eq!(s.free_bytes(), 1024);
        assert_eq!(s.capacity_bytes(), 1024);
    }

    #[test]
    fn static_basic_alloc() {
        let s = StaticArea::new(1024);
        let p = s.try_alloc_cells(4).expect("alloc 4 cells");
        unsafe {
            *p.as_ptr() = 11;
            *p.as_ptr().add(1) = 22;
            *p.as_ptr().add(2) = 33;
            *p.as_ptr().add(3) = 44;
        }
        assert_eq!(s.used_cells(), 4);

        // Read back via cell_ptr.
        let idx = s.cell_index_of(p.as_ptr() as *const u8).expect("in range");
        unsafe {
            assert_eq!(*s.cell_ptr(idx), 11);
            assert_eq!(*s.cell_ptr(idx + 1), 22);
        }
    }

    #[test]
    fn static_exhaustion_returns_none() {
        let s = StaticArea::new(64); // 8 cells
        let _ = s.try_alloc_cells(8);
        assert_eq!(s.used_cells(), 8);
        assert!(s.try_alloc_cells(1).is_none());
        assert!(s.try_alloc_cells(100).is_none());
    }

    #[test]
    fn static_pointer_classification() {
        let s = StaticArea::new(1024);
        let p = s.try_alloc_cells(2).unwrap();
        assert!(s.contains_ptr(p.as_ptr() as *const u8));
        let stack: u64 = 0;
        assert!(!s.contains_ptr(&stack as *const u64 as *const u8));
    }

    #[test]
    fn static_alloc_with_header() {
        let s = StaticArea::new(1024);
        let p = s.try_alloc_with_header(HeapType::Symbol, 7).unwrap();
        unsafe {
            assert_eq!((*p.as_ptr()).ty(), HeapType::Symbol);
            assert_eq!((*p.as_ptr()).length_cells(), 7);
        }
        assert_eq!(s.used_cells(), 8); // 1 header + 7 payload
    }

    #[test]
    fn static_concurrent_alloc_disjoint() {
        // Multiple threads CAS-bump simultaneously; we verify each
        // gets a disjoint range with no overlap.
        let s = Arc::new(StaticArea::new(64 * 1024)); // 8K cells
        let n_threads = 4;
        let allocs_per_thread = 100;

        let handles: Vec<_> = (0..n_threads).map(|_| {
            let s = Arc::clone(&s);
            thread::spawn(move || {
                let mut my_ptrs = Vec::new();
                for _ in 0..allocs_per_thread {
                    let p = s.try_alloc_cells(2).expect("alloc");
                    my_ptrs.push(p.as_ptr() as usize);
                }
                my_ptrs
            })
        }).collect();

        let mut all: Vec<usize> = Vec::new();
        for h in handles {
            all.extend(h.join().expect("thread"));
        }
        // All pointers distinct and 8-byte aligned.
        all.sort();
        for w in all.windows(2) {
            assert!(w[0] < w[1], "overlap: {} >= {}", w[0], w[1]);
            assert!(w[1] - w[0] >= 16, "alloc 2 cells = 16 bytes apart minimum");
            assert_eq!(w[0] & 7, 0);
        }
        assert_eq!(s.used_cells(), n_threads * allocs_per_thread * 2);
    }

    #[test]
    fn static_cards_are_initially_clean() {
        let s = StaticArea::new(1024);
        assert_eq!(s.cards.dirty_count(), 0);
    }
}
