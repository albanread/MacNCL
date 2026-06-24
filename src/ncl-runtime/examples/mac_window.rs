//! Opens the macOS iGui window and prints the `IGuiEvent`s that flow from
//! the AppKit UI thread to the (stand-in) Lisp worker thread.
//!
//! Run on a logged-in Mac GUI session:
//!
//! ```sh
//! cargo run -p ncl-runtime --example mac_window --features mac-gui
//! ```
//!
//! Type, click, and scroll in the window; each event prints in the
//! terminal. Close the window to quit. This exercises the full Phase 2.2
//! boundary: `NSEvent` → translation (`igui_mac::events`) → shared mailbox
//! (`igui_events`) → worker thread `next_event`.

#[cfg(feature = "mac-gui")]
fn main() {
    use ncl_runtime::igui_events;
    use ncl_runtime::igui_mac::window;

    // The Lisp side, here a stand-in: block on the mailbox and print.
    let worker = || loop {
        match igui_events::next_event(-1) {
            Some(ev) => println!("[worker] {ev:?}"),
            None => break,
        }
    };

    println!("Opening MacNCL iGui window — type/click/scroll; close to quit.");
    if let Err(e) = window::run("MacNCL — iGui (Phase 2.2)", 720.0, 480.0, worker) {
        eprintln!("igui_mac::window::run failed: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(feature = "mac-gui"))]
fn main() {
    eprintln!("rebuild with `--features mac-gui` to run the macOS window example");
}
