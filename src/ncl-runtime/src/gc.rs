//! GC backend — page-heap.
//!
//! The semispace GC has been removed. Page-heap is the one and only
//! backend. `gc::Heap` and `gc::RootScanner` alias the page-heap
//! concrete types; all callers compile against these names unchanged.

#[cfg(not(feature = "gc-page-heap"))]
compile_error!("ncl-runtime: the gc-page-heap feature must be enabled.");

/// Display name for the active backend. Surfaces in `(gc-stats)` as
/// `:heap-backend`.
pub const ACTIVE_BACKEND_NAME: &str = "page-heap";

/// The concrete heap type for this build: the `newgc-core` page heap
/// specialised to NCL's 3-bit-tag `Word` + `HeapHeader` layout via
/// `LispLayout`. Call sites compile against `gc::Heap` unchanged.
pub type Heap = newgc_core::PageHeap<newgc_core::LispLayout>;

/// The evacuation-pass root scanner. `LispLayout` is fixed for this
/// build, so call sites keep writing `RootScanner<'_, '_>`.
pub type RootScanner<'s, 'a: 's> =
    newgc_core::page_heap::scanner::RootScanner<'s, 'a, newgc_core::LispLayout>;

/// The mark-pass scanner (minor-cycle live marking), likewise pinned
/// to `LispLayout`.
pub type MarkScanner<'s, 'a: 's> =
    newgc_core::page_heap::mark::MarkScanner<'s, 'a, newgc_core::LispLayout>;
