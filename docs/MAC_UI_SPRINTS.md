# MacNCL UI sprints — plan & acceptance criteria

*Companion to [`docs/MAC_UI_DESIGN.md`](MAC_UI_DESIGN.md). Seven sprints off the
five design phases; every sprint is independently shippable and leaves
`cargo test -p ncl-runtime igui` green.*

## Ground rules (definition of done, every sprint)

- `cargo check --workspace` and `cargo check -p ncl-driver --features mac-gui`
  clean (warnings unchanged or fewer).
- `cargo test -p ncl-runtime igui` green — including the new tests named below.
- Live smoke: `./run-gui.sh` launches, `NCL_GUI_SELFTEST='(* 6 7)'` still
  evaluates to `42` in the REPL (the end-to-end tripwire — if this breaks,
  the sprint isn't done).
- Visual eyeball via `NCL_IGUI_DUMP=frame.ppm` for each appearance
  (or a screenshot) — chrome must look right in **light and dark** from
  Sprint 3 onward.
- No new hardcoded `Rgba` literals outside the token table and test palettes.

## Test hooks we build as we go

Headless acceptance needs injection points; add them in the sprint that first
uses them:

| Hook | Purpose | Sprint |
|---|---|---|
| `NCL_IGUI_THEME=light\|dark\|auto` | Force a `SystemTheme` snapshot without touching System Settings | 3 |
| `NCL_GUI_MENU=<MenuCmd>` | Simulate a system-menu pick without a mouse (posts the same mailbox event the dispatcher does) | 1 |
| `NCL_GUI_QUIT_AFTER_MS=<n>` | Auto-quit so ⌘Q/lifecycle tests can assert clean process exit | 1 |
| `NCL_GUI_FAKE_KEY_WINDOW=0\|1` | Drive inactive-window dimming in frame dumps | 3 |

---

## Sprint 1 — Native menu bar (design §1, "the headline item")

**Goal:** the system menu bar owns the menus; the in-window menu bar is gone;
⌘Q/⌘H/⌘M behave. Size: **M**.

**Scope**
- `ide/app.rs`: delete `MENUS`/`MMenu`/`MItem`, `menu_*` geometry/hit-testing,
  dropdown rendering, and their tests. Keep `MenuCmd` + `run_menu_cmd`.
- New `igui_mac/menu.rs` (main thread): build the `NSMenu` hierarchy
  (MacNCL / File / Edit / Eval / View / Window / Help per the design), with
  key equivalents. Dispatcher = `declare::ClassBuilder` NSObject subclass
  whose action methods push `IGuiEvent::Command { cmd }` into
  `igui_events::push` (extend the enum with the existing `MenuCmd` mapping).
- `window.rs::run_inner`: install the main menu before `app.run()`.
- `ide/app.rs::handle_event`: route `IGuiEvent::Command` through
  `run_menu_cmd` (today only reachable from clicks).
- Enable/disable state: Save disabled when buffer clean; Eval items disabled
  when no buffer (always false for now — wire the plumbing, assert it flips).

**Automated acceptance**
1. `menu_dispatch_maps_every_item` — for every menu item in the built
   hierarchy (introspect the `NSMenu` on the main thread in a #[test] with
   `mac-gui`), the dispatcher's selector table yields a defined `MenuCmd`,
   and every `MenuCmd` variant is reachable from some item. No orphans either
   direction.
2. `menu_command_routes_like_the_shortcut` — pushing
   `IGuiEvent::Command(MenuCmd::RunBuffer)` through `Ide::handle_event`
   returns the same `IdeAction::Eval(src)` as the old Cmd-R path (port of the
   existing `menu_run_buffer_requests_an_eval` test).
3. `ncl_gui_selftest_still_evaluates` — with `NCL_GUI_SELFTEST` +
   `NCL_GUI_QUIT_AFTER_MS=3000`, the process exits 0 and the transcript
   contains `42` (extends the existing selftest harness in the driver).
4. Existing key-shortcut tests (`cmd_r_runs_the_buffer`, tab switching…)
   stay green unchanged — keyboard paths are untouched.

**Manual acceptance (scripted)**
5. Launch `./run-gui.sh`: the **system** menu bar shows exactly seven menus
   in this order: MacNCL, File, Edit, Eval, View, Window, Help. Every item
   shows a ⌘-glyph key equivalent matching NCLMac.md's table.
6. Click Eval ▸ Run Buffer with the scratch buffer → `; run buffer` appears
   in the REPL and the buffer evaluates (same as ⌘R).
7. ⌘Q quits (window closes, process exits, Dock icon goes away); ⌘H hides;
   ⌘M minimizes; ⌘` cycles if a side window (e.g. `--demo bouncing`) is open.
8. Tab into the menu bar with Ctrl-F2 → arrow keys navigate (free from
   NSMenu, proves it's real).

**Out of scope:** Open/Save panels (Sprint 5), item enable/disable nuances
beyond Save, Help content beyond the shortcuts listing.

---

## Sprint 2 — Window polish & app bundle (design §1)

**Goal:** the window behaves like a document window: remembers its frame,
has a real title/subtitle, the app has an identity. Size: **M**.

**Scope**
- `window.rs::open`: `setFrameAutosaveName` (per-window stable names:
  `"MacNCL-IDE"`, `"MacNCL-child-<id>"`), `setMinSize(720, 480)`,
  default `1000×680`; `center()` only when no autosaved frame.
- Title already flows via `UiCmd::Title`; add subtitle (`NSWindow.subtitle`)
  = directory of the active buffer; untitled → subtitle cleared.
- `FullSizeContentView` + `titlebarAppearsTransparent`; `ide/app.rs` tab strip
  gains a `traffic_inset` (78pt when full-size-content, 0 in tests).
- Build step in `run-gui.sh` (or `scripts/`… note: `scripts/` is gitignored —
  put it in `run-gui.sh` itself or `tools/mac/`): assemble `MacNCL.app` with
  `CFBundleDocumentTypes` for `.lisp` and a λ template icon (iconutil or a
  prebuilt icns committed under `resources/`).
- Help ▸ MacNCL Help → shortcuts listing (reuse `show_shortcuts`).

**Automated acceptance**
1. `tab_strip_clears_traffic_lights` — with `traffic_inset = 78`, the first
   tab's `x0 ≥ 78` and the `+` button sits right of the last tab; with inset
   `0` layout matches today's (existing tab tests ported).
2. `title_tracks_active_buffer` — switching tabs / loading a file emits
   `UiCmd::Title` with the buffer's file name; dirty→clean doesn't spam
   duplicate Title commands (assert dedupe).
3. `frame_dump_shows_tab_strip_under_titlebar` — `NCL_IGUI_DUMP` with
   full-size-content: row 0 of the dump is tab-strip chrome, not a title-bar
   gap (pixel assert: row 0 ≠ window background).

**Manual acceptance**
4. Move + resize the window, ⌘Q, relaunch → same frame (autosave). Do it
   twice to be sure it's not `center()` luck.
5. Dock shows the λ icon; app menu says "About MacNCL" and opens a real
   About panel; ⌘, is present (no-op allowed this sprint).
6. Title = `scratch`-or-filename and updates on tab switch; subtitle shows
   the directory; the traffic lights sit **inside** the tab strip with the
   first tab starting to their right — no overlap at any width ≥ 720.
7. Resize to the minimum: nothing clips (status bar, prompt line, one editor
   line all visible at 720×480).

**Out of scope:** vibrancy, sidebar, window restoration of *tab sessions*.

---

## Sprint 3 — Theme system: tokens, appearances, accent (design §2)

**Goal:** both system appearances + user accent drive the IDE; zero hardcoded
chrome colors. Size: **L** (the plumbing sprint).

**Scope**
- New `igui_mac/theme.rs`: `SystemTheme` — all semantic tokens from the design
  table + syntax palette + font metrics. Resolve on the main thread
  (`NSColor … usingColorSpace(sRGB)`), store behind an `Arc` swap-slot the
  worker reads when building batches.
- Refresh triggers: `effectiveAppearance` name check each 60 Hz tick (cheap
  string compare); accent-change distributed notification. `NCL_IGUI_THEME`
  overrides for tests. Inactive-key-window flag rides the same snapshot
  (`NCL_GUI_FAKE_KEY_WINDOW` to drive it headlessly).
- `ide/app.rs` chrome (menu-area remnants, tab strip, status bar, divider,
  REPL prompt/busy) re-templated from tokens; `ide/editor.rs` `Theme` derives
  its syntax colors + selection from tokens (selection = accent @ 28%).
- Inactive-window dimming: labels step down one tier (`text → text_secondary`
  → `text_tertiary`), caret hidden, focus ring suppressed.
- Test palette: fixed light+dark `SystemTheme` consts for pixel tests.

**Automated acceptance**
1. `every_token_differs_between_appearances` — resolving light vs dark
   yields different `Rgba` for every chrome token (catches "resolved a
   dynamic color wrong" bugs, the classic `labelColor`-in-wrong-space trap).
2. `forced_theme_changes_frame` — render the IDE with the fixed-light vs
   fixed-dark test snapshots into `CgCanvas`, assert the tab-strip and
   status-bar pixel regions differ (reuses the renderer's existing
   bitmap-assert test style).
3. `inactive_window_dims_labels` — same render with
   `NCL_GUI_FAKE_KEY_WINDOW=0`: tab label color equals the token table's
   one-tier-dimmer value; caret pixels absent.
4. `selection_uses_accent` — build batches with two different accent values;
   the selection rect color tracks it (no blue hardcoded anywhere).
5. `no_chrome_rgba_literals` — small lint-ish unit test (or grep in CI):
   `ide/app.rs` contains no `Rgba {` outside token lookups and tests.

**Manual acceptance**
6. Toggle System Settings ▸ Appearance light↔dark **while the IDE is open**:
   the whole IDE (tabs, status, divider, REPL prompt, editor syntax) flips
   within ~1 tick (~100 ms — no relaunch, no flash of wrong colors).
7. Change the accent color: selection + REPL prompt + focus glow follow.
8. Light mode readability pass: every syntax role legible on the light
   editor bg; gutter numerals quiet but visible (compare against Xcode light).
9. Click a side window (e.g. Othello demo) → IDE window dims labels and
   drops its caret; click back → restores.

**Out of scope:** SF fonts (Sprint 4), vibrancy/materials.

---

## Sprint 4 — Typography (design §3)

**Goal:** SF Pro chrome, SF Mono code (Menlo fallback), font-size commands.
Size: **M**.

**Scope**
- `igui_mac/render.rs` font cache: families `"__system"` / `"__system-bold"`
  resolve to `NSFont.systemFont/boldSystemFont` (cast to `CTFont`), and
  `"__mono"` to `monospacedSystemFont`, falling back to Menlo when either
  returns None (headless/CI).
- `ide/editor.rs`: code font family `"__mono"`; `set_metrics` already
  re-measures cells, so layout adapts. Log once which font actually resolved.
- Chrome (`run()` in `ide/app.rs`): family `"__system"`, sizes 13/11 per the
  design's type scale.
- View menu: Font Bigger/Smaller (⌘+ / ⌘−), clamp 12–20pt, re-measure +
   re-layout; code font size applies to REPL too.

**Automated acceptance**
1. `system_font_families_resolve_or_fallback` — asking the cache for
   `__system`/`__mono` yields either the system font (metrics differ from
   Menlo's) or the Menlo fallback — never an error, and the resolved family
   name is recorded for the log line.
2. `font_size_commands_clamp` — ⌘+ eleven times from 15pt lands on 20pt;
   ⌘− underflows to 12pt; cell metrics re-measured after each step (editor +
   REPL consistent: same `cell_w` for both).
3. `chrome_uses_11pt_status_13pt_tabs` — token/font plumbing asserts the
   sizes asked of the font cache for each chrome element (no 15pt chrome).
4. `mono_fallback_matches_menlo_baseline` — rendering the editor with forced
   Menlo reproduces today's golden frame bytes (guards the fallback path
   against metric drift).

**Manual acceptance**
5. Chrome text next to TextEdit's default: same face (SF Pro), not Menlo.
6. Code next to Xcode: SF Mono (log line confirms); line height comfortable
   (~1.25× size), gutter right-padded 16pt, nothing clipped at 15pt.
7. ⌘+ / ⌘− live-resizes everything including the REPL and gutter; ⌘+ then
   ⌘− returns to the original layout.

**Out of scope:** per-buffer font settings, ligatures.

---

## Sprint 5 — Native files: open/save/recents/drag-drop (design §5)

**Goal:** file IO through real macOS surfaces. Size: **M**.

**Scope**
- `igui_mac/menu.rs`: `openDocument:` runs `NSOpenPanel` (allowed
   `.lisp`/`.txt`), posts the chosen path into the mailbox; `saveDocumentAs:`
   runs `NSSavePanel`. Panels on the main thread; paths cross to the worker
   as `IGuiEvent::Open { path }` / `SaveAs { path }`.
- `ide/app.rs`: handle both (load_file exists; save-as writes via
   `Editor::save_to`), mark buffer's file, update title/subtitle.
- File menu: New (⌘N → new buffer), Open… (⌘O), Open Recent ▸ (last 10,
   persisted to `~/Library/Application Support/MacNCL/recents` — small JSON).
- Drag-drop: transparent overlay `NSView` (dragging destination, file types
   as above) mounted over the content view, forwarding paths to the same
   `Open` event; `NSWindow` drag destination for the Dock via the bundle.

**Automated acceptance**
1. `open_event_loads_file` — `IGuiEvent::Open{path}` on a temp `.lisp` file:
   new tab, title = filename, `; loaded` info line (port of `load_file`
   semantics; no panel involved).
2. `save_as_writes_file_and_retitles` — `SaveAs` to a temp path writes the
   buffer contents, clears dirty, emits `UiCmd::Title` with the new name.
3. `recents_update_and_cap` — ten `Open` events on distinct paths → recents
   list is those ten, most-recent-first; an eleventh evicts the oldest;
   persists across a simulated relaunch (re-read the JSON file).
4. `drag_types_filter` — the overlay's dragging destination accepts `.lisp`
   and rejects `.png` (unit-test the type filter helper).

**Manual acceptance**
5. ⌘O shows a real open panel (icon, sidebar, ⌘⇧G works); picking
   `Lisp/demos/othello-gui.lisp` opens a tab titled `othello-gui.lisp` with
   subtitle `…/demos`.
6. ⌘S on a loaded file saves; ⇧⌘S prompts with a sensible default name;
   after save-as, ⌘S saves to the new path without prompting.
7. Drag `draw-square.lisp` from Finder onto the IDE window → opens a tab.
   Drag a `.png` → spring-back rejection.
8. Open Recent lists the files from steps 5–7 after quit + relaunch.

**Out of scope:** tabs-as-documents (NSDocument), untitled-save flow beyond
save-as.

---

## Sprint 6 — Feel pass: cursors, scroll, caret, clicks (design §4–5)

**Goal:** the interactions stop feeling foreign. Size: **M**.

**Scope**
- `window.rs` event monitor: cursor shape by location — I-beam over
   editor/REPL text (needs a cheap hit-test query against the IDE's pane
   rects, exported from `ide/app.rs`), `resizeUpDown` within ±5pt of the
   divider, arrow otherwise; via `NSCursor` on mouse-moved.
- Scroll: accumulator + ~150ms exponential momentum tail on
   `scrollingDeltaY`; discrete-wheel = 3 `cell_h` lines animated; skip all
   easing when Reduce Motion is on (from the theme snapshot).
- Caret: 2pt accent bar, 0.5s/0.5s blink driven from the existing 60 Hz tick,
   resets steady on keystrokes, hidden when unfocused/inactive.
- Clicks: double-click selects word; triple-click selects the enclosing
   top-level form (`ide/sexp.rs` machinery); shift-click extends selection.
- Divider: double-click resets `split = 0.62`.
- Status bar: `●` busy indicator in accent while a worker eval is in flight
   (driver sets/clears around `Session::eval`).

**Automated acceptance**
1. `scroll_momentum_decays_but_conserves` — a 120px impulse: total scroll
   after settling = 120px ± 1; peak velocity ≤ immediate-jump baseline;
   settles < 300ms; Reduce Motion ⇒ identical instantaneous jump (no tail).
2. `caret_blinks_half_second_cadence` — tick-driven state machine: at t=0
   visible, t=0.5s hidden, t=1.0s visible; any keystroke re-arms visible +
   resets phase.
3. `triple_click_selects_enclosing_top_level_form` — editor with two defuns:
   triple-click inside the second returns exactly `(defun …)` #2 (the
   Lisp-native twist — this one's the demo).
4. `shift_click_extends_and_word_click_selects_word` — port of textedit
   semantics against the rope (word boundaries honor `*package*`-legal
   symbol chars).
5. `divider_double_click_resets_split` — drag to 0.3, double-click → 0.62
   (extends the existing divider drag test).
6. `busy_indicator_renders_dot_when_evaluating` — render with busy=true:
   pixel assert an accent-colored dot in the status bar's right zone.
7. `cursor_shape_lookup` — pane-rect hit-test: points in editor/REPL →
   IBeam; ±5pt of divider → ResizeUpDown; menu/status → Arrow (pure fn,
   headless).

**Manual acceptance**
8. Trackpad two-finger scroll: glides with momentum, no visible stepping;
   mouse wheel scrolls ~3 lines per notch.
9. Caret blinks ~1Hz, stops while typing, disappears when the REPL has focus
   or the window is inactive.
10. Hover: I-beam over both text panes, ↕ over the divider, arrow over
    chrome; double-click the divider → panes reset.
11. Run `(loop 1000000)` style long eval → status `●` appears immediately,
    disappears on completion; UI stays responsive (cooperative loop already
    guarantees this — the dot now *shows* it).

**Out of scope:** column (rectangular) selection, mouse-3 paste.

---

## Sprint 7 — Diagnostics, accessibility & polish (design §6)

**Goal:** the roadmap's inline diagnostics, plus the system-courtesy flags.
Size: **L**.

**Scope**
- Diagnostics plumbing: eval errors from the compiler session already return
   to the driver; extend with source ranges → `Editor::set_diagnostics`.
- Rendering: squiggly underline (accent-red) + gutter dot; hover text later;
   diagnostics clear on successful re-eval of the form.
- Reduce Motion + Increase Contrast honored at snapshot time (skip easing /
   thicken separators, drop tints).
- AX: `accessibilityLabel` on the window; per-pane `AXValue` summaries
   (first line / prompt state). Overflow `»` tab menu past 8 tabs.
- README refresh (the stale one) folded in here as docs debt.

**Automated acceptance**
1. `error_range_underlines_offending_form` — feed a synthetic error range
   for line 2 cols 5–11; frame dump: squiggle pixels present under exactly
   those columns (column ↔ pixel via cell metrics), none elsewhere.
2. `diagnostics_clear_on_successful_eval` — set, re-eval clean, gone.
3. `increase_contrast_thickens_separators` — snapshot with the flag: divider
   ≥ 2px vs 1px baseline; tints dropped to plain strokes.
4. `ax_labels_present` — window and both panes expose non-empty labels
   (introspect via the accessibility APIs in a mac-gui test).
5. `overflow_menu_lists_hidden_tabs` — 12 buffers: `»` menu enumerates
   buffers 9–12; selecting one activates it.

**Manual acceptance**
6. Type `(+ 1 foo-undefined)` in the editor, ⌘↩ — with a real compile error
   the offending form squiggles red with a gutter dot; fix + re-eval clears.
7. VoiceOver on: window announces "Lisp editor and REPL"; arrows explore
   pane summaries.
8. Full regression run of NCLMac.md's keybinding table — every entry still
   true (this doc is the contract; update both if anything intentionally
   changed in Sprints 1–6).

**Out of scope:** line-level AX editing navigation, vibrancy sidebar,
Metal fast-path.

---

## Sequencing & risk

```
S1 menus ──► S2 window/bundle ──► S3 theme ──► S4 typography ──► S5 files ──► S6 feel ──► S7 diagnostics/a11y
              (S2 independent of S1; can run in either order or parallel)
```

- S3 is the long pole and everything after it consumes its tokens; if it
  slips, split it (tokens+dark-mode first, accent+dimming second) rather
  than letting it block S4.
- S1 and S2 touch only chrome and can land in either order.
- The riskiest single item is S3's `NSColor` resolution on the right thread
  with the right color space — acceptance test 1 exists specifically to
  catch it.
