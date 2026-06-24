//! Redirectable standard output.
//!
//! All Lisp output bound for the terminal funnels through `(format t …)`
//! (see `format.rs`), which calls [`emit`]. By default that writes to the
//! process stdout, exactly as before. A host (e.g. the macOS GUI REPL) can
//! call [`begin_capture`] around an evaluation to divert that output into
//! an in-memory buffer instead, then [`end_capture`] to collect it and
//! display it in the transcript.
//!
//! Capture is **thread-local**: the buffer follows the thread that runs the
//! evaluation, so concurrent evaluators don't cross streams.

use std::cell::RefCell;

thread_local! {
    static SINK: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Start capturing this thread's stdout-bound output into a fresh buffer.
pub fn begin_capture() {
    SINK.with(|s| *s.borrow_mut() = Some(String::new()));
}

/// Stop capturing and return the buffered output (`None` if not capturing).
pub fn end_capture() -> Option<String> {
    SINK.with(|s| s.borrow_mut().take())
}

/// True while this thread is capturing.
pub fn is_capturing() -> bool {
    SINK.with(|s| s.borrow().is_some())
}

/// Emit `text` to the active capture buffer, or to the process stdout if
/// this thread is not capturing.
pub fn emit(text: &str) {
    let captured = SINK.with(|s| {
        if let Some(buf) = s.borrow_mut().as_mut() {
            buf.push_str(text);
            true
        } else {
            false
        }
    });
    if !captured {
        use std::io::Write;
        let stdout = std::io::stdout();
        let mut h = stdout.lock();
        let _ = h.write_all(text.as_bytes());
        let _ = h.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_round_trip() {
        let _ = end_capture(); // clear any prior thread-local state
        assert!(!is_capturing());
        begin_capture();
        assert!(is_capturing());
        emit("hello ");
        emit("world");
        assert_eq!(end_capture().as_deref(), Some("hello world"));
        assert!(!is_capturing());
        // After capture ends, emit goes to stdout and nothing is buffered.
        emit("");
        assert_eq!(end_capture(), None);
    }
}
