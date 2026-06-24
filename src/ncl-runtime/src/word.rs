//! Tagged 64-bit Lisp values — re-export shim.
//!
//! The canonical `Word` / `Tag` and the tag-layout constants now live
//! in the `newgc-core` crate (`newgc_core::word`). Re-exporting them
//! here means the runtime and the GC engine share the *same* Rust
//! type: a `PageHeap<LispLayout>` consumes exactly the `Word` the
//! runtime produces, with no conversion at the boundary.
//!
//! Kept as `crate::word` so the call sites across ncl-runtime
//! compile unchanged. See `docs/GC.md` for the value representation.

pub use newgc_core::word::*;
