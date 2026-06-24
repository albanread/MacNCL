//! Shared heap types — re-export shim.
//!
//! `CardTable`, `HeapHeader`, `HeapType`, `GcBit`, `StartBits`, the
//! card-table geometry constants, and `MAX_OBJECT_CELLS` now live in
//! `newgc-core` (`newgc_core::heap_common`). Re-exported here so the
//! runtime shares the *same* header/card types as the GC engine and
//! existing `crate::heap_common::…` paths (plus `heap.rs`'s
//! `pub use crate::heap_common::*`) keep compiling unchanged.
//!
//! Note: the header bit-field shift/mask constants (`TYPE_SHIFT`,
//! `GC_SHIFT`, …) are `pub(crate)` in newgc-core and intentionally do
//! not cross the crate boundary — decode headers via `HeapHeader`'s
//! accessors (`ty()`, `gc_bits()`, `length_cells()`) instead.

pub use newgc_core::heap_common::*;
