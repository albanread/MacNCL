// When compiled as the GUI release package (`--features gui-app`):
//   * WINDOWS subsystem → the OS never attaches a console window
//   * --windows surface is implied without an explicit flag
// In debug / plain console builds this attribute is absent and the
// binary behaves exactly as before.
#![cfg_attr(feature = "gui-app", windows_subsystem = "windows")]

use std::cell::Cell;
use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::mem::MaybeUninit;
use std::process::ExitCode;
use std::sync::mpsc;
use std::sync::Mutex;
use std::thread;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn usage() {
    eprintln!("usage: ncl [--lean] [--windows] [--repl | (--eval <src> | --load <file> | --check <file>)...] [--repl]");
    eprintln!("       ncl --version | --help");
    eprintln!("  --eval,  -e <src>    evaluate a source string");
    eprintln!("  --load,  -l <file>   read and evaluate the file");
    eprintln!("  --check, -c <file>   dry-run: parse + macroexpand + lower each top-level form,");
    eprintln!("                       executing only definitions (defun, defmacro, defparameter,");
    eprintln!("                       defconstant, require, …). Non-definition forms get lowered");
    eprintln!("                       through the JIT pipeline but never run, so the file's main");
    eprintln!("                       side-effects (FFI calls, network I/O, window creation) are");
    eprintln!("                       suppressed. Surfaces reader, macroexpand, and compile errors.");
    eprintln!("  --repl,  -r          enter the interactive REPL (default if no flags given)");
    eprintln!("  --lean,  -L          start with core only (no CLOS, no Library/init.lisp)");
    eprintln!("  --opt-level, -O <n>  JIT optimisation level 0..3 (default 2 = -O2)");
    eprintln!("  --windows, -W        macOS: open the Cocoa IDE window (editor + REPL) on the");
    eprintln!("                       main thread, with Lisp on a worker. Without it, the plain");
    eprintln!("                       console REPL runs.");
    eprintln!("  --run-window         macOS: run a Lisp GUI app standalone (no IDE window).");
    eprintln!("                       The app opens its own window via open-child and the");
    eprintln!("                       process quits when the last window closes. Combine with");
    eprintln!("                       --load/--eval, e.g. ncl --run-window -l app.lisp -e '(run)'");
    eprintln!("  --version, -V        print version and exit");
    eprintln!("  --help,    -h        print this message and exit");
    eprintln!("  multiple --eval / --load / --check can be chained; --repl runs after them");
    eprintln!();
    eprintln!("Environment variables:");
    eprintln!("  NCL_LIBRARY          override the Library/ directory location");
    eprintln!("  NCL_YOUNG_MB         young-heap reservation in MB (default 256)");
    eprintln!("  NCL_OLD_MB           old-heap reservation in MB (default 2048)");
    eprintln!("  NCL_STATIC_MB        static-area reservation in MB (default 1024,");
    eprintln!("                       elastic on Windows — only committed as used)");
    eprintln!("  NCL_TLAB_KB          per-mutator TLAB size in KB (default 2048)");
    eprintln!("                       smaller values force GC pressure for testing");
}

fn main() -> ExitCode {
    // Install the Windows last-resort SEH filter before anything that
    // could fault. On non-Windows this is a no-op. Idempotent.
    ncl_runtime::brk::install_crash_handler();

    let raw_args: Vec<String> = env::args().skip(1).collect();

    // Early-exit flags. Scan ALL of argv so position doesn't matter —
    // `ncl --version`, `ncl --lean --version`, `ncl -e foo -V` all
    // print version-and-exit before any session work.
    if raw_args.iter().any(|a| a == "--version" || a == "-V") {
        println!("NCL {VERSION}");
        return ExitCode::SUCCESS;
    }
    if raw_args.iter().any(|a| a == "--help" || a == "-h") {
        usage();
        return ExitCode::SUCCESS;
    }

    // --opt-level N: set JIT optimisation level before the first
    // compilation. Scanned here (before session creation) so it
    // takes effect from the very first defun in Library/init.lisp.
    {
        let mut i = 0;
        while i < raw_args.len() {
            if (raw_args[i] == "--opt-level" || raw_args[i] == "-O") && i + 1 < raw_args.len() {
                match raw_args[i + 1].parse::<u32>() {
                    Ok(n) => ncl_llvm::set_opt_level(n),
                    Err(_) => {
                        eprintln!("ncl: --opt-level requires an integer 0..3");
                        usage();
                        return ExitCode::from(2);
                    }
                }
                i += 2;
            } else {
                i += 1;
            }
        }
    }

    // --run-window: macOS standalone-app mode. Run a Lisp GUI app with NO IDE
    // window — the app opens its own window(s) via open-child and the process
    // quits when the last one closes. Routed before the --windows logic so it
    // takes precedence (it's its own surface). macOS + mac-gui only.
    #[cfg(all(target_os = "macos", feature = "mac-gui"))]
    if raw_args.iter().any(|a| a == "--run-window") {
        return run_mac_app(raw_args);
    }

    // --windows: thread 0 becomes the Win32 UI thread (message pump),
    // Lisp eval moves to a worker thread. See docs/WINDOWS_FFI.md.
    //
    // In the GUI release build (`gui-app` feature) there is no console,
    // so we always start the windows surface regardless of flags.
    // In the console build the flag must be explicit.
    #[cfg(feature = "gui-app")]
    let want_windows = true;
    #[cfg(not(feature = "gui-app"))]
    let want_windows = raw_args.iter().any(|a| a == "--windows" || a == "-W");

    if want_windows {
        // macOS: open the Cocoa IDE window (REPL) on the main thread, with
        // the Lisp session on a worker. See `run_mac_gui`.
        #[cfg(all(target_os = "macos", feature = "mac-gui"))]
        {
            return run_mac_gui(raw_args);
        }
        // Without the mac-gui surface there is no windowed mode (the Win32
        // surface was removed); `--windows` just runs the console session.
        #[cfg(not(all(target_os = "macos", feature = "mac-gui")))]
        {
            run_without_windows_surface(raw_args)
        }
    } else {
        run_without_windows_surface(raw_args)
    }
}

/// macOS Cocoa IDE entry. The main thread runs the AppKit window; the Lisp
/// session is built and driven on a worker thread that owns a `Repl` pane,
/// evaluating submitted forms through the compiler and presenting the
/// rendered transcript back to the window.
#[cfg(all(target_os = "macos", feature = "mac-gui"))]
fn run_mac_gui(raw_args: Vec<String>) -> ExitCode {
    use ncl_runtime::igui_events::{self, IGuiEvent};
    use ncl_runtime::igui_mac::ide::{Ide, IdeAction};
    use ncl_runtime::igui_mac::render::CgCanvas;
    use ncl_runtime::igui_mac::window;
    use ncl_runtime::igui_paint::{
        FontStretch, FontStyle, Point, Rect, Rgba, TextAlign, TextRun, TextTrimming,
    };

    // Default IDE window size (points). The window remembers its frame
    // across launches (autosave); this is the first-launch size.
    const W: f64 = 1000.0;
    const H: f64 = 680.0;

    // Measure the monospace cell for a family/size via Core Text.
    fn metrics(family: &str, size: f32) -> (f32, f32, f32) {
        let run = TextRun {
            text: "M".into(),
            origin: Point { x: 0.0, y: 0.0 },
            family: family.into(),
            size,
            weight: 400,
            style: FontStyle::Normal,
            stretch: FontStretch::Normal,
            locale: "en-us".into(),
            color: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
            max_width: None,
            alignment: TextAlign::Leading,
            trimming: TextTrimming::None,
        };
        match CgCanvas::measure_text_run(&run) {
            Some(m) => (m.width.max(1.0), m.height.max(size * 1.2), m.ascent),
            None => (size * 0.6, size * 1.3, size),
        }
    }

    let worker = move || {
        // The IDE render area. Tracks the live window size — updated when a
        // Resize event for the main window arrives (see the central loop).
        let mut area = Rect { x0: 0.0, y0: 0.0, x1: W as f32, y1: H as f32 };
        // The semantic theme snapshot (resolved on the main thread before
        // this worker starts; re-resolved on appearance/accent changes).
        let sys = ncl_runtime::igui_mac::theme::current();
        let theme = sys.to_editor_theme();
        let (cw, ch, asc) = metrics(&theme.family, theme.size);
        let mut ide = Ide::new((*sys).clone());
        ide.set_metrics(cw, ch, asc);
        ide.set_reduce_motion(window::reduce_motion_pref());
        // Test hook: pin the key-window state so dimming can be verified
        // headlessly (unset → follow Focus events from the real window).
        let forced_active = std::env::var("NCL_GUI_FAKE_KEY_WINDOW").ok().and_then(|v| {
            match v.as_str() {
                "0" => Some(false),
                "1" => Some(true),
                _ => None,
            }
        });
        if let Some(a) = forced_active {
            ide.set_active(a);
        }
        ide.info("Booting NCL standard library…");
        window::present_main(ide.render(area));

        // Drive the loading bar (the main-thread timer reads this and paints
        // a progress bar while we JIT the stdlib — the worker is busy here and
        // can't repaint). ~820 ≈ functions in core + clos + Library (no xp).
        ncl_runtime::load_progress::begin(820);

        let mut session = match ncl_compiler::Session::with_stdlib() {
            Ok(s) => s,
            Err(e) => {
                ide.error(&format!("stdlib bootstrap failed: {e:?}"));
                window::present_main(ide.render(area));
                return;
            }
        };
        // Activate so `(eval-string …)` / the event-loop's :eval-buffer
        // handler can re-enter this session — mirrors the console path
        // (lisp_main). The worker owns `session` for its whole lifetime, so
        // its address is stable.
        session.activate();

        // Load the user Library (Library/init.lisp) into the GUI session,
        // exactly as the console path does in lisp_main. Without this, the
        // session has only the baked-in core+CLOS stdlib — so `events`
        // (on-window / event-loop / with-events-from) and the rest of the
        // standard modules are undefined, and every graphics demo fails to
        // compile (e.g. `(on-window …)` lowers as an unknown call). macOS
        // keeps `(windows-enabled-p)` NIL, so init.lisp's `(when
        // (windows-enabled-p) …)` Win32 block is skipped.
        if let Some(library_dir) = find_library_dir() {
            ide.info("Loading standard library…");
            window::present_main(ide.render(area));
            let setup = format!(
                "(setq *load-path* (cons \"{}\" *load-path*))",
                library_dir.replace('\\', "/")
            );
            if let Err(e) = session.eval(&setup) {
                ide.error(&format!("could not extend *load-path*: {e:?}"));
            }
            let init_path = format!("{library_dir}/init.lisp");
            if std::path::Path::new(&init_path).exists() {
                let load = format!("(load \"{}\")", init_path.replace('\\', "/"));
                if let Err(e) = session.eval(&load) {
                    ide.error(&format!("Library/init.lisp failed: {e:?}"));
                }
            }
        } else {
            ide.error("Library/ not found — graphics demos (on-window, …) unavailable.");
        }
        ncl_runtime::load_progress::finish(); // hide the loading bar; IDE takes over

        ide.info(&format!("NCL {VERSION} on Apple Silicon — ready."));
        ide.info(&format!(
            "; font: {}",
            ncl_runtime::igui_mac::render::resolved_font_name(&theme.family, theme.size)
        ));
        ide.info("Cmd-R run buffer · Cmd-Return eval form · Cmd-S save · Cmd-E/L focus");

        // Demo: open a graphics side-window and draw a scene (proves the
        // multi-window + SurfaceCmd canvas path that graphics apps use).
        if std::env::var_os("NCL_GUI_DEMO_WINDOW").is_some() {
            use ncl_runtime::igui_paint::{Point, Rect as R, SurfaceCmd as C};
            let rgb = |r: u8, g: u8, b: u8| Rgba {
                r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: 1.0,
            };
            let txt = |s: &str, x: f32, y: f32, col: Rgba| C::DrawTextRun {
                run: TextRun {
                    text: s.into(), origin: Point { x, y }, family: "Menlo".into(), size: 20.0,
                    weight: 400, style: FontStyle::Normal, stretch: FontStretch::Normal,
                    locale: "en-us".into(), color: col, max_width: None,
                    alignment: TextAlign::Leading, trimming: TextTrimming::None,
                },
            };
            window::open_window(2, 480.0, 360.0, "Shapes");
            window::present(2, vec![
                C::Clear { color: rgb(30, 36, 46) },
                C::FillRect { rect: R { x0: 40.0, y0: 40.0, x1: 140.0, y1: 140.0 }, corner_radius: 0.0, color: rgb(235, 76, 76) },
                C::StrokeRect { rect: R { x0: 200.0, y0: 40.0, x1: 320.0, y1: 140.0 }, corner_radius: 0.0, half_thickness: 1.5, color: rgb(76, 217, 242) },
                C::FillCircle { center: Point { x: 260.0, y: 90.0 }, radius: 38.0, color: rgb(76, 217, 242) },
                C::DrawLine { p0: Point { x: 40.0, y: 200.0 }, p1: Point { x: 320.0, y: 280.0 }, half_thickness: 2.0, color: rgb(102, 230, 102) },
                txt("Hello from NewCormanLisp!", 40.0, 300.0, rgb(255, 255, 255)),
            ]);
            ide.info("; opened graphics window (id 2)");
        }
        // Process args: --eval/--load run app code (so graphics apps open
        // their own side windows); a bare `file.lisp` opens in an editor tab.
        let mut it = raw_args.iter();
        while let Some(a) = it.next() {
            let run_src = |ide: &mut Ide, session: &mut ncl_compiler::Session, src: &str| {
                ncl_runtime::output::begin_capture();
                let r = session.eval(src);
                if let Some(p) = ncl_runtime::output::end_capture() {
                    let p = p.trim_end_matches('\n');
                    if !p.is_empty() {
                        ide.output(p);
                    }
                }
                match r {
                    Ok(s) => ide.output(&s),
                    Err(e) => ide.error(&format!("{e:?}")),
                }
            };
            match a.as_str() {
                "--eval" | "-e" => {
                    if let Some(src) = it.next() {
                        run_src(&mut ide, &mut session, src);
                    }
                }
                "--load" | "-l" => {
                    if let Some(path) = it.next() {
                        match std::fs::read_to_string(path) {
                            Ok(src) => {
                                ide.info(&format!("; load {path}"));
                                run_src(&mut ide, &mut session, &src);
                            }
                            Err(e) => ide.error(&format!("read {path}: {e}")),
                        }
                    }
                }
                s if !s.starts_with('-')
                    && (s.ends_with(".lisp") || s.ends_with(".lsp") || s.ends_with(".cl")) =>
                {
                    ide.load_file(s);
                }
                _ => {}
            }
        }
        window::present_main(ide.render(area));

        // Self-test: inject a canned form so eval can be verified without a
        // human typing (NCL_GUI_SELFTEST=<form>). The events flow through
        // the real mailbox + repl.handle_event + session.eval path.
        if let Some(form) = std::env::var_os("NCL_GUI_SELFTEST") {
            let form = form.to_string_lossy().into_owned();
            let run_buffer = std::env::var_os("NCL_GUI_RUNBUFFER").is_some();
            std::thread::spawn(move || {
                // Optionally run the editor buffer first (Cmd-R), so a form
                // it defines is available to the REPL.
                if run_buffer {
                    igui_events::push(IGuiEvent::Key {
                        child_id: 1,
                        vkey: 0x52, // R
                        scancode: 0,
                        mods: ncl_runtime::igui_events::modifier::WIN,
                        repeat: 0,
                        down: true,
                        time_ms: 0,
                    });
                }
                for c in form.chars() {
                    igui_events::push(IGuiEvent::Char {
                        child_id: 1,
                        codepoint: c as i64,
                        mods: 0,
                        time_ms: 0,
                    });
                }
                igui_events::push(IGuiEvent::Key {
                    child_id: 1,
                    vkey: 0x0D, // Return
                    scancode: 0,
                    mods: 0,
                    repeat: 0,
                    down: true,
                    time_ms: 0,
                });
            });
        }

        // Menu-pick injection (NCL_GUI_MENU=<name>): verify a system-menu
        // command end-to-end without a mouse. Flows through the real
        // mailbox + Menu routing, like a click on the menu item.
        if let Some(name) = std::env::var_os("NCL_GUI_MENU") {
            let name = name.to_string_lossy().into_owned();
            match ncl_runtime::igui_mac::menu::opcode_for_name(&name) {
                Some(op) => igui_events::push(IGuiEvent::Menu {
                    menu_id: ncl_runtime::igui_events::menu_cmd::IDE,
                    item_id: op,
                }),
                None => ide.error(&format!("NCL_GUI_MENU: unknown command {name:?}")),
            }
        }

        // Open injection (NCL_GUI_OPEN=<path>): verify the panel/recents/
        // drop pipeline end-to-end without a human picking in the panel.
        if let Some(path) = std::env::var_os("NCL_GUI_OPEN") {
            igui_events::push(IGuiEvent::Open {
                path: path.to_string_lossy().into_owned(),
            });
        }

        // ── Central cooperative event loop ───────────────────────────────
        //
        // ONE loop, on this single worker (language) thread, serves every
        // pane. We drain the *catch-all* queue (empty filter) so the loop
        // sees every event, then route each by its target window:
        //
        //   • window 1 (MAIN_ID)  → the IDE REPL/editor, handled in Rust
        //   • any other window    → that pane's Lisp handler, dispatched
        //                           through %dispatch-event (on-window …)
        //   • global events       → both (the IDE and any global handlers)
        //
        // Apps no longer run their own blocking (event-loop-for …): they
        // register a handler and return, so launching one from the REPL
        // doesn't freeze the loop, and N panes coexist. The AppKit UI
        // thread only ever posts into the mailbox, so it never blocks here.
        igui_events::clear_filter();

        // Last title/subtitle posted to the window, so we only send a
        // command when the active buffer actually changes identity.
        let mut last_title = "MacNCL — REPL".to_string();
        let mut last_subtitle = String::new();

        loop {
            let Some(ev) = igui_events::next_event(-1) else {
                break; // mailbox closed — process shutting down
            };

            // App-wide shutdown: the whole frame closing, or the IDE's own
            // window. A *child* window closing only retires that pane (its
            // handler unregisters), so it must NOT break the loop.
            match &ev {
                IGuiEvent::FrameClose => break,
                IGuiEvent::Close { child_id } if *child_id == window::MAIN_ID => break,
                _ => {}
            }

            // The main window resized → grow/shrink the IDE render area so the
            // panes re-lay-out to fill the new client area.
            if let IGuiEvent::Resize { child_id, width, height } = &ev {
                if *child_id == window::MAIN_ID {
                    area = Rect { x0: 0.0, y0: 0.0, x1: *width as f32, y1: *height as f32 };
                }
            }

            // Appearance/accent changed on the system: adopt the fresh
            // snapshot the main thread published.
            if matches!(&ev, IGuiEvent::ThemeChange) {
                ide.set_theme((*ncl_runtime::igui_mac::theme::current()).clone());
            }

            // Key-window state drives inactive-window dimming (unless the
            // NCL_GUI_FAKE_KEY_WINDOW test hook pinned it).
            if let IGuiEvent::Focus { child_id, gained } = &ev {
                if *child_id == window::MAIN_ID && forced_active.is_none() {
                    ide.set_active(*gained);
                }
            }

            let target = ev.child_id(); // Some(id) per-child, None for globals
            let to_ide = target.is_none() || target == Some(window::MAIN_ID);
            let to_app = target.is_none() || target != Some(window::MAIN_ID);

            // Skip the IDE present churn for bare pointer motion.
            let is_move = matches!(
                &ev,
                IGuiEvent::Mouse { op, .. } if *op == igui_events::mouse_op::MOVE
            );
            if std::env::var_os("NCL_GUI_DEBUG").is_some() && !is_move {
                eprintln!("[gui] ev={ev:?} -> ide={to_ide} app={to_app}");
            }

            if to_ide {
                match ide.handle_event(&ev) {
                    IdeAction::Eval(src) => {
                        // Show the busy ● while the worker evaluates.
                        ide.set_busy(true);
                        window::present_main(ide.render(area));
                        // Capture the program's printed output so
                        // `(format t …)` / print show up in the transcript.
                        ncl_runtime::output::begin_capture();
                        let result = session.eval(&src);
                        ide.set_busy(false);
                        if let Some(printed) = ncl_runtime::output::end_capture() {
                            let printed = printed.trim_end_matches('\n');
                            if !printed.is_empty() {
                                ide.output(printed);
                            }
                        }
                        match result {
                            Ok(s) => ide.output(&s),
                            Err(e) => ide.error(&format!("{e:?}")),
                        }
                    }
                    // ⌘+/⌘−: re-measure the code font's cell metrics so the
                    // next render lays out at the new size.
                    IdeAction::Remeasure => {
                        let (cw, ch, asc) = metrics(ide.font_family(), ide.font_size());
                        ide.set_metrics(cw, ch, asc);
                    }
                    IdeAction::None => {}
                }
                if !is_move {
                    // Keep the menu's Save enablement in step with the
                    // active buffer's dirty flag (read by validateMenuItem:
                    // when the menu opens).
                    ncl_runtime::igui_mac::menu::set_save_enabled(ide.can_save());
                    // Window title/subtitle track the active buffer; posted
                    // only on change so tab switches don't spam the queue.
                    let want_title = ide.window_title();
                    if want_title != last_title {
                        window::set_window_title(window::MAIN_ID, &want_title);
                        ncl_runtime::igui_mac::menu::set_save_suggested_name(&want_title);
                        last_title = want_title;
                    }
                    let want_subtitle = ide.window_subtitle();
                    if want_subtitle != last_subtitle {
                        window::set_window_subtitle(window::MAIN_ID, &want_subtitle);
                        last_subtitle = want_subtitle;
                    }
                    window::present_main(ide.render(area));
                }
            }

            if to_app {
                // Route to the Lisp pane handler registered via (on-window …).
                // The handler paints its own window, so the IDE is not
                // re-presented for app events.
                session.dispatch_gui_event(ev);
            }

            // Feel-pass pump: advance blink + scroll release, repaint if
            // anything moved, re-arm the animation tick and refresh the
            // cursor geometry. Idle ⇒ no deadline ⇒ zero wakeups.
            let now = window::now_ms();
            if ide.animate(now) {
                window::present_main(ide.render(area));
            }
            window::request_ide_tick(ide.next_wake_ms(now).map_or(0, |t| t as u64));
            window::set_cursor_hints(ide.layout_hints());
        }
    };

    match window::run("MacNCL — REPL", W, H, worker) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ncl: {e}");
            ExitCode::from(1)
        }
    }
}

/// macOS standalone-app entry (`--run-window`). Like `run_mac_gui` but with NO
/// IDE window: the Lisp worker boots, loads the Library, runs the app code
/// passed via `--load`/`--eval` (which opens its own window(s) and registers
/// `(on-window …)` handlers), then runs the central cooperative loop routing
/// EVERY event to its Lisp pane handler. The AppKit layer quits the process
/// when the last window closes. This is how Lisp ships a GUI app — Othello,
/// Life, etc. — without the REPL/editor chrome.
#[cfg(all(target_os = "macos", feature = "mac-gui"))]
fn run_mac_app(raw_args: Vec<String>) -> ExitCode {
    use ncl_runtime::igui_events::{self, IGuiEvent};
    use ncl_runtime::igui_mac::window;

    let worker = move || {
        let mut session = match ncl_compiler::Session::with_stdlib() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("ncl: stdlib bootstrap failed: {e:?}");
                return;
            }
        };
        session.activate();

        // Load the user Library (events/on-window, graphics helpers, loop,
        // sequences, …) — same as the IDE path; without it on-window and the
        // graphics demos are undefined.
        if let Some(library_dir) = find_library_dir() {
            let setup = format!(
                "(setq *load-path* (cons \"{}\" *load-path*))",
                library_dir.replace('\\', "/")
            );
            if let Err(e) = session.eval(&setup) {
                eprintln!("ncl: warning: could not extend *load-path*: {e:?}");
            }
            let init_path = format!("{library_dir}/init.lisp");
            if std::path::Path::new(&init_path).exists() {
                let load = format!("(load \"{}\")", init_path.replace('\\', "/"));
                if let Err(e) = session.eval(&load) {
                    eprintln!("ncl: warning: Library/init.lisp failed: {e:?}");
                }
            }
        } else {
            eprintln!("ncl: warning: Library/ not found — on-window unavailable.");
        }

        // Run the app code. --load <file>, --eval <form>, and bare `file.lisp`
        // are evaluated in order; the app is expected to open a window and
        // register an (on-window …) handler, then return.
        let run_src = |session: &mut ncl_compiler::Session, label: &str, src: &str| {
            ncl_runtime::output::begin_capture();
            let r = session.eval(src);
            if let Some(p) = ncl_runtime::output::end_capture() {
                let p = p.trim_end_matches('\n');
                if !p.is_empty() {
                    println!("{p}");
                }
            }
            if let Err(e) = r {
                eprintln!("ncl: {label}: {e:?}");
            }
        };
        let mut it = raw_args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--run-window" => {}
                "--eval" | "-e" => {
                    if let Some(src) = it.next() {
                        run_src(&mut session, "eval", src);
                    }
                }
                "--load" | "-l" => {
                    if let Some(path) = it.next() {
                        match std::fs::read_to_string(path) {
                            Ok(src) => run_src(&mut session, path, &src),
                            Err(e) => eprintln!("ncl: read {path}: {e}"),
                        }
                    }
                }
                s if !s.starts_with('-')
                    && (s.ends_with(".lisp") || s.ends_with(".lsp") || s.ends_with(".cl")) =>
                {
                    match std::fs::read_to_string(s) {
                        Ok(src) => run_src(&mut session, s, &src),
                        Err(e) => eprintln!("ncl: read {s}: {e}"),
                    }
                }
                _ => {}
            }
        }

        // Central cooperative loop: every event → its Lisp pane handler.
        // No IDE pane to special-case (window 1 doesn't exist here).
        igui_events::clear_filter();
        loop {
            let Some(ev) = igui_events::next_event(-1) else { break };
            if matches!(ev, IGuiEvent::FrameClose) {
                break;
            }
            session.dispatch_gui_event(ev);
        }
    };

    match window::run_app(worker) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ncl: {e}");
            ExitCode::from(1)
        }
    }
}

/// Today's startup path. Lisp runs on a worker thread; no message
/// pump; no Windows surface. The main thread just waits for the
/// worker to finish and returns its exit code.
///
/// Why a worker thread (not just thread 0): on Windows, the
/// console-subsystem main thread starts with the PE-header stack
/// reserve (Rust's default ≈ 1 MB), which isn't enough headroom for
/// the recursive Lisp stdlib bootstrap — `nclterm.exe` would
/// stack-overflow on startup. A spawned `std::thread` gets Rust's
/// default 2 MB stack, matching the `--windows` worker path. (The
/// gui-app build *also* runs Lisp on a worker thread for the same
/// reason, plus to keep the UI thread free for the message pump.)
fn run_without_windows_surface(raw_args: Vec<String>) -> ExitCode {
    // 8 MB — comfortably larger than the deepest recursion the Lisp
    // bootstrap reaches. The gui-app build's UI-thread worker gets
    // away with Rust's 2 MB default because some `--load`-driven
    // paths there don't recurse as deep before yielding to the
    // message pump; the bare `--eval` / `--check` flows the console
    // binary uses go through compilation and macroexpansion on the
    // same thread end-to-end, and 2 MB isn't enough headroom.
    const LISP_WORKER_STACK: usize = 8 * 1024 * 1024;
    let worker = match std::thread::Builder::new()
        .name("ncl-lisp-worker".into())
        .stack_size(LISP_WORKER_STACK)
        .spawn(move || lisp_main(raw_args))
    {
        Ok(j) => j,
        Err(e) => {
            eprintln!("ncl: cannot spawn worker thread: {e}");
            return ExitCode::from(2);
        }
    };
    match worker.join() {
        Ok(code) => code,
        Err(_) => {
            eprintln!("ncl: worker thread panicked");
            ExitCode::from(2)
        }
    }
}

/// The Lisp side of startup — runs on thread 0 without `--windows`,
/// on the worker thread with `--windows`. Builds the session, loads
/// stdlib + Library/init.lisp, processes `--eval` / `--load` flags,
/// optionally runs the REPL.
fn lisp_main(raw_args: Vec<String>) -> ExitCode {
    let startup_timing = std::env::var("NCL_STARTUP_TIMING").is_ok();
    let t_total = std::time::Instant::now();

    // Bare `ncl` invocation drops into the REPL with the stdlib loaded.
    // `--windows` without any explicit work (--eval/--load/--check) also
    // implies --repl: the GUI was launched to be interactive, not to run
    // a script and immediately exit.
    let has_work = raw_args.iter().any(|a| matches!(a.as_str(),
        "--eval" | "-e" | "--load" | "-l" | "--check" | "-c"));
    let want_repl = raw_args.is_empty()
        || raw_args.iter().any(|a| a == "--repl" || a == "-r")
        || (raw_args.iter().any(|a| a == "--windows" || a == "-W") && !has_work);

    // --lean: skip CLOS, skip Library/init.lisp. User explicitly opted
    // out of the standard auto-loaded surface. Useful for scripts or
    // sandboxing.
    let lean = raw_args.iter().any(|a| a == "--lean" || a == "-L");

    let session_result = if lean {
        ncl_compiler::Session::with_minimal_stdlib()
    } else {
        ncl_compiler::Session::with_stdlib()
    };
    let session = match session_result {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ncl: stdlib load failed: {e}");
            return ExitCode::from(1);
        }
    };
    // Park the Session at a stable address so `(eval-string ...)` can
    // route into it from inside Lisp.
    let mut session = Box::new(session);
    session.activate();

    // ─── User library bootstrap ──────────────────────────────────────────
    //
    // Look for `Library/` next to the executable. If it exists, push
    // it onto *load-path* and (if Library/init.lisp is present) load
    // that init file. Failures here are warnings, not fatal — the
    // user can still drop into the REPL and work with just the baked-
    // in stdlib.
    //
    // Skipped entirely when --lean is set. In lean mode there's no
    // load / require / *load-path* in the session at all (those live
    // in the bottom of core.lisp — still loaded, since they don't
    // depend on CLOS — so library bootstrap is suppressed by choice,
    // not by absence).
    if !lean {
        if let Some(library_dir) = find_library_dir() {
            let setup = format!(
                "(setq *load-path* (cons \"{}\" *load-path*))",
                library_dir.replace('\\', "/")
            );
            if let Err(e) = session.eval(&setup) {
                eprintln!("ncl: warning: could not extend *load-path*: {e}");
            }
            let init_path = format!("{library_dir}/init.lisp");
            if std::path::Path::new(&init_path).exists() {
                let t_lib = std::time::Instant::now();
                let load = format!("(load \"{}\")", init_path.replace('\\', "/"));
                if let Err(e) = session.eval(&load) {
                    eprintln!("ncl: warning: Library/init.lisp failed: {e}");
                }
                if startup_timing {
                    let lib_ms = t_lib.elapsed().as_millis();
                    eprintln!("[timing] Library/init.lisp: {}ms", lib_ms);
                    ncl_compiler::Session::drain_startup_timing("Library (top-10 slowest)", lib_ms);
                }
            }
        }
    }

    let mut last_output: Option<String> = None;
    let mut args = raw_args.into_iter().peekable();

    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--eval" | "-e" => {
                let Some(src) = args.next() else {
                    eprintln!("ncl: {flag} requires a source string");
                    usage();
                    return ExitCode::from(2);
                };
                let t_eval = std::time::Instant::now();
                match session.eval(&src) {
                    Ok(s) => last_output = Some(s),
                    Err(e) => {
                        eprintln!("ncl: {e}");
                        return ExitCode::from(1);
                    }
                }
                if startup_timing {
                    let snippet: String = src.chars().take(40).collect();
                    eprintln!("[timing] --eval {:?}: {}ms", snippet, t_eval.elapsed().as_millis());
                }
            }
            "--load" | "-l" => {
                let Some(path) = args.next() else {
                    eprintln!("ncl: {flag} requires a file path");
                    usage();
                    return ExitCode::from(2);
                };
                let src = match fs::read_to_string(&path) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("ncl: cannot read {path}: {e}");
                        return ExitCode::from(1);
                    }
                };
                let t_load = std::time::Instant::now();
                match session.eval(&src) {
                    Ok(s) => last_output = Some(s),
                    Err(e) => {
                        eprintln!("ncl: {path}: {e}");
                        return ExitCode::from(1);
                    }
                }
                if startup_timing {
                    eprintln!("[timing] --load {path}: {}ms", t_load.elapsed().as_millis());
                }
            }
            "--check" | "-c" => {
                // Dry-run: parse + macroexpand + lower each form,
                // executing only definitions. Non-definition forms
                // pass through the JIT pipeline (so syntax / macro /
                // lowering errors surface) but never run.
                let Some(path) = args.next() else {
                    eprintln!("ncl: {flag} requires a file path");
                    usage();
                    return ExitCode::from(2);
                };
                let src = match fs::read_to_string(&path) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("ncl: cannot read {path}: {e}");
                        return ExitCode::from(1);
                    }
                };
                match session.check(&src) {
                    Ok(n) => {
                        println!("[CHECK] {path}: OK ({n} forms)");
                        last_output = None;
                    }
                    Err(e) => {
                        eprintln!("ncl: {path}: {e}");
                        return ExitCode::from(1);
                    }
                }
            }
            "--repl" | "-r" => {
                // Handled below; just accept and continue scanning.
            }
            "--lean" | "-L" => {
                // Handled at session-construction time above; accept here.
            }
            "--windows" | "-W" => {
                // Handled in main() before session creation; accept here.
            }
            "--opt-level" | "-O" => {
                // Already applied in the main() early scan; just consume
                // the value token so the unknown-arg arm doesn't see it.
                let _ = args.next();
            }
            other => {
                eprintln!("ncl: unknown argument '{other}'");
                usage();
                return ExitCode::from(2);
            }
        }
    }

    if let Some(s) = &last_output {
        println!("{s}");
    }

    if startup_timing {
        eprintln!("[timing] TOTAL to first prompt: {}ms", t_total.elapsed().as_millis());
    }

    if want_repl {
        // Console REPL. (The windowed IDE is run_mac_gui, dispatched from
        // main() on the `--windows` path; this is the plain stdin REPL.)
        return run_repl(&mut session);
    }

    ExitCode::SUCCESS
}

/// Resolve the path to `Library/` next to the executable. Returns
/// the absolute path string if the directory exists, else None.
///
/// Search order:
///   1. NCL_LIBRARY env var (override for dev / install bundles)
///   2. <exe_dir>/Library  (the shipping shape)
///   3. <exe_dir>/../../Lisp/Library  (developer running cargo run)
///
/// Anything not found falls through to None; the loader is optional.
fn find_library_dir() -> Option<String> {
    if let Ok(p) = std::env::var("NCL_LIBRARY") {
        if std::path::Path::new(&p).is_dir() {
            return Some(p);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    let beside = exe_dir.join("Library");
    if beside.is_dir() {
        return Some(beside.to_string_lossy().into_owned());
    }
    // Dev fallback: target/release/ncl.exe → repo-root/Lisp/Library
    let dev = exe_dir
        .ancestors()
        .nth(2)
        .map(|p| p.join("Lisp").join("Library"));
    if let Some(d) = dev {
        if d.is_dir() {
            return Some(d.to_string_lossy().into_owned());
        }
    }
    None
}

// ─── setjmp/longjmp bindings ────────────────────────────────────────────
//
// libc doesn't expose setjmp on Windows because the MSVC ABI for
// setjmp / longjmp is compiler-specific (it's a builtin, technically).
// We declare the C runtime entry points by hand. The jmp_buf size
// is platform-dependent — on x86_64 MSVC it's 16 × 8 = 128 bytes,
// plus 16-byte alignment slack — 256 bytes with 16-byte alignment is
// comfortably oversized for every target we care about.

#[repr(C, align(16))]
struct JmpBuf([u8; 256]);

unsafe extern "C" {
    /// Save calling-thread register state into env. Returns 0 on
    /// the initial call, returns the `val` passed to longjmp on
    /// the longjmp resume.
    #[link_name = "_setjmp"]
    fn setjmp_raw(env: *mut JmpBuf) -> i32;

    /// Restore registers from env and resume at the setjmp call
    /// site as if it returned `val`.
    fn longjmp(env: *mut JmpBuf, val: i32) -> !;
}

// ─── REPL panic-recovery via setjmp/longjmp ────────────────────────────
//
// Most user-level errors (undefined function, unbound variable) are
// converted to catchable Lisp conditions inside the runtime — the
// REPL wraps each input in a top-level `handler-case` and prints
// the result. But some Rust panics (ncl_car of non-cons, length on
// improper list, etc.) still escape: panicking out of a Rust runtime
// helper fails to unwind cleanly through MCJIT-emitted JIT frames on
// Windows because the unwinder needs SEH .pdata tables that MCJIT
// doesn't reliably register.
//
// The standard workaround is the same one Lisp REPLs have used since
// the 1970s: a setjmp at the prompt and a longjmp from a global
// panic hook. setjmp captures registers, longjmp restores them; no
// frame unwinding involved. The OS is happy, the JIT frames are
// happy, and the user gets back to a prompt instead of a crashed
// process.

thread_local! {
    /// Per-thread pointer to the active jmp_buf, set by `run_repl`
    /// before each input form. The panic hook reads this; if non-
    /// null, longjmps to it. Cleared after each successful eval so
    /// panics outside the REPL fall through to the default handler.
    static REPL_JMP_BUF: Cell<*mut JmpBuf> = const { Cell::new(std::ptr::null_mut()) };

    /// One-line description of what was running when a panic fired,
    /// captured by the hook before the longjmp. The REPL reads it
    /// after recovering and prints it as the recovery message.
    static REPL_PANIC_MSG: Mutex<Option<String>> = const { Mutex::new(None) };
}

fn install_repl_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .map(String::from)
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<unknown panic>".to_string());
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()));
        let full = match location {
            Some(loc) => format!("panic at {loc}: {msg}"),
            None => format!("panic: {msg}"),
        };
        REPL_PANIC_MSG.with(|cell| {
            if let Ok(mut guard) = cell.lock() {
                *guard = Some(full);
            }
        });

        let buf_ptr = REPL_JMP_BUF.with(|c| c.get());
        if !buf_ptr.is_null() {
            // Clear before the longjmp — we won't return here.
            REPL_JMP_BUF.with(|c| c.set(std::ptr::null_mut()));
            unsafe { longjmp(buf_ptr, 1) };
        }
        // No REPL active: let the default handler print and abort.
        eprintln!("{}", REPL_PANIC_MSG.with(|cell| {
            cell.lock().ok().and_then(|g| g.clone()).unwrap_or_default()
        }));
    }));
}

/// Wrap the user's source in a top-level handler-case so the
/// runtime's converted-to-condition panics (undefined function,
/// unbound variable) print as messages instead of crashing the
/// session. Returns NIL or the formatted error string for the
/// REPL to display.
fn wrap_for_repl(src: &str) -> String {
    format!(
        "(handler-case (progn {src}) (error (c) (format nil \"** ~A\" c)))"
    )
}

/// Interactive read-eval-print loop. Reads from stdin, accumulates
/// input until the form is parseable (handles multi-line entry by
/// detecting an UnexpectedEof from the reader and prompting again),
/// hands it to the session, prints the result.
///
/// Exit on Ctrl+D / EOF, or by typing `(exit)` or `(quit)` at the
/// top-level prompt. Panics inside the eval are caught via a
/// setjmp/longjmp pair and the prompt is restored.
fn run_repl(session: &mut ncl_compiler::Session) -> ExitCode {
    install_repl_panic_hook();

    println!("NCL {VERSION} REPL");
    println!("  (exit) or Ctrl+D / Ctrl+Z to leave");
    println!();

    let stdin_rx = spawn_stdin_reader();
    let mut buf = String::new();

    'repl: loop {
        // Between prompts, drain any hot-reload pending queue. This
        // is a Lisp-level call; if hot-reload was never enabled,
        // (check-reloads) is a NIL-returning no-op. We swallow any
        // Err so a broken reload doesn't take the REPL down — the
        // Lisp handler-case inside check-reloads handles per-file
        // errors; this is the safety net for the wrapper itself.
        if buf.trim().is_empty() {
            let _ = session.eval("(check-reloads)");
        }
        print_prompt(buf.trim().is_empty());

        // Wait for the next line of input (blocking).
        let line_result = match stdin_rx.recv() {
            Ok(r) => r,
            Err(_) => {
                // Reader thread died / EOF.
                println!();
                break 'repl;
            }
        };

        let line = match line_result {
            Ok(s) => s,
            Err(e) => {
                eprintln!("ncl: stdin: {e}");
                return ExitCode::from(1);
            }
        };
        if line.is_empty() {
            // EOF (Ctrl+D / Ctrl+Z).
            println!();
            break;
        }

        buf.push_str(&line);
        let trimmed = buf.trim();
        if trimmed.is_empty() {
            buf.clear();
            continue;
        }
        if trimmed == "(exit)" || trimmed == "(quit)" {
            break;
        }

        // Probe the reader for completeness.
        match ncl_reader::read_all(&buf) {
            Ok(_) => {
                let src = wrap_for_repl(&buf);
                eval_with_recovery(session, &src);
                buf.clear();
            }
            Err(e) => {
                if is_incomplete(&e) {
                    // Multi-line continuation: keep buf, prompt with
                    // "...> " next iteration.
                    continue;
                }
                eprintln!("ncl: read error: {:?}", e.kind);
                buf.clear();
            }
        }
    }

    let _ = std::panic::take_hook();
    ExitCode::SUCCESS
}

/// Print "ncl> " for a fresh input or "...> " when continuing a
/// multi-line form. Flush stdout so the prompt actually appears
/// before we block on stdin.
fn print_prompt(fresh: bool) {
    let prompt = if fresh { "ncl> " } else { "...> " };
    print!("{prompt}");
    let _ = io::stdout().flush();
}

/// Spawn a thread that drains stdin line-by-line into a channel.
/// Each item is either `Ok(line)` (with the trailing `\n`) or
/// `Ok("")` to signal EOF; on read error we send `Err(e)` once and
/// exit. Decoupling stdin from the main thread lets the main loop
/// also poll the iGui mailbox.
fn spawn_stdin_reader() -> mpsc::Receiver<io::Result<String>> {
    let (tx, rx) = mpsc::channel::<io::Result<String>>();
    thread::spawn(move || {
        let stdin = io::stdin();
        let mut handle = stdin.lock();
        loop {
            let mut line = String::new();
            match handle.read_line(&mut line) {
                Ok(0) => {
                    // EOF — send empty line as sentinel and exit.
                    let _ = tx.send(Ok(String::new()));
                    break;
                }
                Ok(_) => {
                    if tx.send(Ok(line)).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                    break;
                }
            }
        }
    });
    rx
}

/// Run one eval inside a setjmp shield. If the eval panics, the
/// installed hook longjmps back here; we print the captured panic
/// message and return without crashing the REPL.
fn eval_with_recovery(session: &mut ncl_compiler::Session, src: &str) {
    let mut jmpbuf: MaybeUninit<JmpBuf> = MaybeUninit::uninit();
    REPL_JMP_BUF.with(|c| c.set(jmpbuf.as_mut_ptr()));

    let r = unsafe { setjmp_raw(jmpbuf.as_mut_ptr()) };
    if r == 0 {
        // First entry — try the eval.
        match session.eval(src) {
            Ok(result) => println!("{result}"),
            Err(e) => eprintln!("ncl: {e}"),
        }
    } else {
        // We just got longjmp'd back. Read whatever the panic hook
        // captured and print it.
        let msg = REPL_PANIC_MSG.with(|cell| {
            cell.lock().ok().and_then(|mut g| g.take()).unwrap_or_default()
        });
        eprintln!("ncl: ** recovered from {msg} **");
    }

    // Disarm the buf so panics OUTSIDE this eval can't longjmp into
    // a stale stack frame.
    REPL_JMP_BUF.with(|c| c.set(std::ptr::null_mut()));
}

/// Is this reader error "input is unfinished, please type more"?
/// Matches end-of-input both from the lexer (mid-string, mid-#\, etc.)
/// and from the parser (unclosed list, dangling `'`/`,`, etc.).
fn is_incomplete(e: &ncl_reader::ReaderError) -> bool {
    matches!(e.kind, ncl_reader::ReaderErrorKind::UnexpectedEof(_))
        || matches!(
            &e.kind,
            ncl_reader::ReaderErrorKind::Lex(ncl_reader::LexErrorKind::UnexpectedEof(_))
        )
}
