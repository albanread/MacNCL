//! The Mac-native NCL IDE: a Lisp editor pane and a REPL pane built on the
//! platform-neutral stack (rope buffer + `SurfaceCmd` renderer + event
//! mailbox). Ports the rich behaviour of the Windows `igui::ledit` /
//! `igui::repl_child` panes onto Core Graphics + Core Text + AppKit events.

pub mod editor;
pub mod repl;

pub use editor::{Editor, Theme};
pub use repl::Repl;
