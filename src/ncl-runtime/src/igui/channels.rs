//! Event mailbox: GUI thread → language thread(s).
//!
//! The implementation is now platform-neutral and lives in
//! `crate::igui_events`; this module re-exports it so every existing
//! Windows iGui reference (`super::channels::*`, `channels::push`, …)
//! keeps resolving unchanged. See PORTING_DESIGN.md §4.5.

#![cfg(windows)]

pub use crate::igui_events::*;
