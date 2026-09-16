# Handoff — cooperative GUI↔Lisp event routing  (WORKING)

**Date:** 2026-06-25
**Branch:** main (work uncommitted in the working tree)
**Status:** ✅ Othello + other GUI apps load and run from the GUI REPL,
cooperatively, without freezing the IDE. ✅ `--run-window` standalone-app mode
(no IDE chrome, quits on last window close). The original blocker is fixed.

## `--run-window` (standalone Lisp GUI apps)
`ncl --run-window -l app.lisp -e '(run-app)'` runs a Lisp GUI app with **no IDE
window**. The app opens its own window via `open-child`/`open-child-sized` and
registers an `(on-window …)` handler; the central cooperative loop routes every
event to it; the **process quits when the last window closes**.
- Driver: `run_mac_app` in `src/ncl-driver/src/main.rs` (mirrors `run_mac_gui`
  minus the IDE — loads Library, runs `-l`/`-e`, central loop dispatches ALL
  events to Lisp). Routed early in `main()` (before `--windows`).
- AppKit: `window::run_app` in `src/ncl-runtime/src/igui_mac/window.rs`.
  `run`/`run_app` now share `run_inner(main_window, quit_on_last_close, worker)`:
  headless = no MAIN_ID window; a main-thread timer calls `app.terminate(None)`
  once `had_window && wins.is_empty()`. IDE path passes
  `(Some(...), quit_on_last_close=false)` — behaviour unchanged (verified).
- Verified live: `--run-window -l othello-gui.lisp -e '(run-othello-gui)'` opens
  only window 2 (215 events, zero window-1); a self-closing test app exits the
  process on its own ~the moment it closes its window.

**Goal:** use the iGui features from Lisp so apps like Othello work — multiple
panes share cooperative handlers on the single Lisp thread; the UI thread never
blocks; the language thread dispatches each event to the right handler.

---

## The two problems (both fixed)

### 1. The blocking-loop bug (design)
One Lisp worker thread owns the `Session` + GC mutator. Apps used to call the
**blocking** `(event-loop-for WIN …)`, which owns that thread forever — so
launching an app from the REPL froze the whole IDE and blocked every other
pane. **Fix:** cooperative dispatch (below).

### 2. The real reason Othello showed `BadArity CHAR` (the actual bug hit)
`run_mac_gui` (the GUI entry) built the session with `Session::with_stdlib()`
but **never loaded `Library/init.lisp`** — unlike the console path
(`lisp_main`). So the GUI session had only baked-in core+CLOS; the `events`
module (`on-window`, `event-loop`, …) and the rest of the standard modules were
**undefined**. `(on-window …)` then lowered as an unknown call and the demo
failed to compile (`Compile(BadArity { head: "CHAR", … })`), so
`run-othello-gui` was never defined → `undefined function`.
**Fix:** `run_mac_gui` now loads `Library/init.lisp` (sets `*load-path*`,
`(load …/init.lisp)`), mirroring `lisp_main`. macOS keeps `(windows-enabled-p)`
NIL, so init.lisp's `(when (windows-enabled-p) …)` Win32 block is skipped.
The graphics wrappers (`with-batch`, `rgb`, `fill-rect`, `draw-text`) are in
`core.lisp` (baked in) — only the `events`/library layer was missing.

## The cooperative design

ONE central loop on the worker thread serves every pane:

```
AppKit UI thread → mailbox (push) → dispatcher thread → per-child queues + CATCH_ALL
                                                                  │
                                          worker thread: ONE central loop drains CATCH_ALL
                                                                  │
                          ┌───────────────────────────────────────┤ route by ev.child_id()
                          ▼                                       ▼
               window 1 (MAIN_ID) → Rust IDE            window N → Lisp %dispatch-event
               globals (child_id None) → BOTH           → per-pane handler (on-window …)
```

Apps **register a handler with `(on-window WIN …)` and RETURN**. The central
loop calls each pane's handler once per event; handlers run to completion and
yield. Cooperative contract: a handler MUST return promptly — never call
`(next-event …)` or run its own loop.

## Files changed (all uncommitted)

**Rust:**
1. `src/ncl-runtime/src/igui_events.rs` — `IGuiEvent::child_id() -> Option<i64>`
   (`None` = global). Drives routing.
2. `src/ncl-runtime/src/igui_mac/shims.rs` — `pub fn dispatch_event(mutator,
   ev) -> bool`: interns `%DISPATCH-EVENT`, builds the event plist, `ncl_funcall`s
   it. GC-safe (intern before plist; plist is the last alloc before the call).
3. `src/ncl-compiler/src/lib.rs` — `Session::dispatch_gui_event(&mut self, ev)`
   (cfg macos+mac-gui), condition-guarded like `eval_value` so a pane handler
   that signals/escapes is contained.
4. `src/ncl-driver/src/main.rs` (`run_mac_gui`):
   - **Added the Library bootstrap** after `session.activate()` (the fix for
     bug #2) — `find_library_dir()` + set `*load-path*` + `(load init.lisp)`.
   - **Rewrote the central loop**: `clear_filter()` only (drain CATCH_ALL =
     every event); break on `FrameClose` or `Close{MAIN_ID}` (a *child* close
     only retires that pane); route `to_ide` (window 1 / globals) →
     `ide.handle_event` + `present_main`; `to_app` (other windows / globals) →
     `session.dispatch_gui_event(ev)`.
   - `abi.rs` shows modified too, but that (`reset_nonlocal_exit_state`) is
     pre-existing work this relies on. Leave it.

**Lisp:**
5. `Lisp/Library/events.lisp` — cooperative layer (old `event-loop` /
   `event-loop-for` kept for standalone use): `*pane-handlers*` (hash),
   `*global-handlers*`; `on-event`/`off-event`/`on-global-event`/`stop-pane`;
   `%event-log` (uses `log-write` if `fboundp`, else `princ` — works headless);
   `%safe-call-handler` (handler-case around each call); `%dispatch-event`
   (host entry point — keep name `%DISPATCH-EVENT` + 1-arg shape stable);
   `on-window` macro (binds `ev` + `self`; auto-adds `:close → stop-pane`).
6. **Ported to `on-window`:** `othello-gui`, `life-gui`, `baby-gui`,
   `insults-gui`, `minesweepers-gui`. Pattern: `event-loop-for X` → `on-window
   X`; drop `:frame-close`/`:close (return)` clauses (auto-handled); convert
   other `(return …)` → `(stop-pane X)`.

## Verified ✓
- `cargo build -p ncl-driver --features mac-gui` — clean (pre-existing warns).
- All GUI demos `--check` OK. The 5 ported apps compile.
- Headless Lisp tests: routing, `on-window` (`ev`/`self`), error containment,
  auto-`:close`. PASS.
- **Live GUI (`NCL_GUI_DEBUG=1`):** Othello loads (`[drv] eval … -> Ok("nil")`),
  `(run-othello-gui) -> Ok("2")`, `open-child-sized -> id 2`, hundreds of
  `Tick child_id:2 app=true` dispatched while window 1 (REPL) keeps its events
  — both serviced on one thread. Confirmed from the **arg path** AND from
  **typing into the REPL** (the original frozen scenario). life-gui likewise
  runs (`run-life-gui -> Ok("2")`, ticks → app).

## Build / run quick reference
```bash
export PATH="$HOME/.cargo/bin:$PATH"            # cargo not on PATH
cargo build -p ncl-driver --features mac-gui    # GUI binary: target/debug/ncl
cargo build -p ncl-driver                        # console binary (for --check/--eval)
./target/debug/ncl --windows                     # launch GUI; type (run-othello-gui)
# diagnostics: NCL_GUI_DEBUG=1 (routing trace), NCL_GUI_SELFTEST='<form>' (auto-type into REPL)
# Debug-build JIT is slow: GUI takes ~15-20s to boot (loads stdlib + Library). Give live tests ≥25s.
```

## Follow-on (optional)
- Remaining illustrative demos still use blocking `event-loop-for` (shapes,
  hello-igui, draw-square, bouncing, mandelbrot, buttons, click-counter,
  canvas-demo, text-styles, heap-monitor, native-repl, gui-repl, etc.). They
  LOAD and run standalone fine; only freeze the REPL if launched from it. Port
  the same way if you want them REPL-launchable. `othello-repl` runs two
  `event-loop-for`s on two OS threads — rethink under the single-loop model.
- `set-redraw-rate` spawns an unkillable tick thread; after a pane closes its
  ticks are dropped by `%dispatch-event` (harmless) but the thread leaks.
- Remove the temporary `NCL_GUI_DEBUG` `eprintln!` tracing once stable.
- Visual/interaction test (clicking to place a disc) needs a human — the
  routing is proven generic over `child_id`, so clicks route like ticks.
