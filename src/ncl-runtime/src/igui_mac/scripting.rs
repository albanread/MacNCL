//! Scripting bridge: Apple Events → the Lisp worker and back.
//!
//! MacNCL is scriptable (`tell application id "com.albanread.macncl"`)
//! with three commands — `eval` (run Lisp, return the result), `transcript`
//! (read the REPL), and `clear repl`. The Apple Event handlers run on the
//! main thread (see `apple_events.rs` for registration); this module is the
//! thread-safe hand-off to the worker that owns the compiler `Session`:
//!
//! ```text
//! osascript ──Apple Event──► main-thread handler
//!     │  send ScriptRequest + wake the worker (a mailbox Tick)
//!     ▼
//! worker loop polls take_request() ──► session.eval / ide …
//!     │  reply.send(result)
//!     ▼
//! handler recv_timeout(…) ──reply Apple Event──► osascript
//! ```
//!
//! The channel side lives here so the driver can poll it without any
//! AppKit dependency; a small unit test drives a request end-to-end.

use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::Mutex;

/// What the script side asks the worker to do.
#[derive(Debug)]
pub enum ScriptKind {
    /// Evaluate Lisp source; the reply is the printed result (or error).
    Eval(String),
    /// Reply with the REPL transcript text.
    Transcript,
    /// Clear the REPL; the reply is "OK".
    ClearRepl,
}

#[derive(Debug)]
pub struct ScriptRequest {
    pub kind: ScriptKind,
    /// Reply channel back to the Apple Event handler.
    pub reply: Sender<String>,
}

fn slot() -> &'static Mutex<Option<Sender<ScriptRequest>>> {
    static S: std::sync::OnceLock<Mutex<Option<Sender<ScriptRequest>>>> = std::sync::OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

/// The worker-side receiver (installed once, polled forever after).
fn rx_slot() -> &'static Mutex<Option<Receiver<ScriptRequest>>> {
    static RX: std::sync::OnceLock<Mutex<Option<Receiver<ScriptRequest>>>> =
        std::sync::OnceLock::new();
    RX.get_or_init(|| Mutex::new(None))
}

/// Install the request channel (once, at GUI startup). Returns the sender
/// the Apple Event handlers use.
pub fn install() -> Sender<ScriptRequest> {
    let mut guard = slot().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = guard.as_ref() {
        return existing.clone();
    }
    let (tx, rx) = channel();
    *rx_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(rx);
    *guard = Some(tx.clone());
    tx
}

/// Worker-side poll: the next pending script request, if any.
pub fn take_request() -> Option<ScriptRequest> {
    let guard = rx_slot().lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref()?.try_recv() {
        Ok(req) => Some(req),
        Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => None,
    }
}

/// Send a request from the main thread and wake the worker (its central
/// loop blocks on the event mailbox, so a Tick makes it re-poll us).
pub fn submit(kind: ScriptKind) -> Option<Receiver<String>> {
    let guard = slot().lock().unwrap_or_else(|e| e.into_inner());
    let tx = guard.as_ref()?;
    let (reply_tx, reply_rx) = channel();
    if tx
        .send(ScriptRequest { kind, reply: reply_tx })
        .is_err()
    {
        return None;
    }
    // Wake the worker's central loop (a Tick for the IDE window, 1).
    crate::igui_events::push(crate::igui_events::IGuiEvent::Tick { child_id: 1, time_ms: 0 });
    Some(reply_rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip() {
        install();
        // Drain anything left by other tests.
        while take_request().is_some() {}
        let rx = submit(ScriptKind::Eval("(* 6 7)".into())).expect("submit");
        let req = take_request().expect("request queued");
        match req.kind {
            ScriptKind::Eval(src) => assert_eq!(src, "(* 6 7)"),
            other => panic!("wrong kind: {other:?}"),
        }
        req.reply.send("42".into()).unwrap();
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(1)), Ok("42".into()));
        // Queue drained.
        assert!(take_request().is_none());
    }
}
