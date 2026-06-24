//! Opens the macOS iGui window and drives it from the (stand-in) Lisp
//! worker thread: a scene is rendered with a circle that follows the
//! mouse, proving the full Phase 2.2 loop end to end —
//! `NSEvent` → translation (`igui_mac::events`) → mailbox (`igui_events`)
//! → worker thread → `window::present` → Core Graphics render → window.
//!
//! Run on a logged-in Mac GUI session:
//!
//! ```sh
//! cargo run -p ncl-runtime --example mac_window --features mac-gui
//! ```
//!
//! Move the mouse over the window — the circle follows. Close to quit.

#[cfg(feature = "mac-gui")]
fn main() {
    use ncl_runtime::igui_events::{self, IGuiEvent};
    use ncl_runtime::igui_mac::window;
    use ncl_runtime::igui_paint::{
        FontStretch, FontStyle, Point, Rect, Rgba, SurfaceCmd, TextAlign, TextRun, TextTrimming,
    };

    const W: f32 = 720.0;
    const H: f32 = 480.0;

    let rgb = |r: u8, g: u8, b: u8| Rgba {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: 1.0,
    };
    let text = move |s: String, x: f32, y: f32, col: Rgba| SurfaceCmd::DrawTextRun {
        run: TextRun {
            text: s,
            origin: Point { x, y },
            family: "Menlo".into(),
            size: 22.0,
            weight: 400,
            style: FontStyle::Normal,
            stretch: FontStretch::Normal,
            locale: "en-us".into(),
            color: col,
            max_width: None,
            alignment: TextAlign::Leading,
            trimming: TextTrimming::None,
        },
    };

    let scene = move |mx: f32, my: f32| -> Vec<SurfaceCmd> {
        vec![
            SurfaceCmd::Clear { color: rgb(24, 26, 33) },
            text("MacNCL · iGui — move the mouse".into(), 24.0, 24.0, rgb(231, 231, 231)),
            SurfaceCmd::FillCircle {
                center: Point { x: mx, y: my },
                radius: 28.0,
                color: rgb(231, 111, 81),
            },
            SurfaceCmd::StrokeCircle {
                center: Point { x: mx, y: my },
                radius: 28.0,
                half_thickness: 1.5,
                color: rgb(244, 162, 97),
            },
            text(format!("({mx:.0}, {my:.0})"), 24.0, H - 40.0, rgb(148, 210, 189)),
        ]
    };

    let worker = move || {
        window::present(scene(W / 2.0, H / 2.0));
        loop {
            match igui_events::next_event(-1) {
                Some(IGuiEvent::Mouse { x, y, .. }) => {
                    window::present(scene(x as f32, y as f32));
                }
                Some(IGuiEvent::Key { vkey, down, .. }) if down => {
                    println!("[worker] key vkey={vkey:#x}");
                }
                Some(_) => {}
                None => break,
            }
        }
    };

    println!("Opening MacNCL iGui window — move the mouse; close to quit.");
    if let Err(e) = window::run("MacNCL — iGui (Phase 2.2c)", W as f64, H as f64, worker) {
        eprintln!("igui_mac::window::run failed: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(feature = "mac-gui"))]
fn main() {
    eprintln!("rebuild with `--features mac-gui` to run the macOS window example");
}
