# MacNCL UI design — making the IDE feel like a real Mac app

*Design doc, September 2026. Target: the `igui_mac` IDE (`ncl --windows`).*

## Where we are

The IDE works and is fast, but visually it reads as "a dark custom toolkit",
not "a Mac app". Concretely, against what a Mac user's fingers and eyes expect:

| Area | Today | Mac expectation |
|---|---|---|
| Menu | In-window, custom-drawn bar (`ide/app.rs` `MENUS`) | Menus live in the **system menu bar**, with ⌘-glyphs, Edit menu, App menu |
| Theme | One hardcoded dark palette (`Theme::default()`) | Light **and** dark, tracking the system; semantic colors; accent color |
| Fonts | Menlo everywhere, incl. chrome | SF Pro for chrome, mono only for code; standard sizes |
| Window | Plain titled `NSWindow` + `NSImageView` (`window.rs` `open`) | Title/subtitle, frame autosave, ⌘Q/⌘H/⌘M/⌘` behave, tabs under the traffic lights |
| Files | No dialogs (roadmap: `NSOpenPanel`/`NSSavePanel`) | Native open/save, drag-drop, recents |
| Feel | Instant-scroll, lit 4px divider, no caret blink | Smooth scroll, NSSplitView-like divider, NSTextView blink cadence |

The architecture is right and we keep it: the whole UI is a `SurfaceCmd` batch
rasterised by `CgCanvas` into an `NSImageView`. Xcode previews and Terminal
prove a canvas can feel native — the gap is not the canvas, it's that we ignore
every system convention around it. This design closes that gap in layers,
smallest-risk first.

## Design principles

1. **Native where it's free, custom where it pays.** Chrome (menus, dialogs,
   window behaviour, cursors, colors, fonts) moves to the system. The editor
   canvas, rope, and paredit stack stay custom — they're tested headlessly and
   a rewrite buys nothing.
2. **Semantic, not literal, colors.** Never hardcode a grey; resolve
   `labelColor`, `separatorColor`, `controlAccentColor`, … per appearance.
   The batch IR keeps concrete `Rgba` values; a main-thread **theme snapshot**
   publishes resolved values to the worker (see *Theme plumbing*).
3. **Both appearances, always.** Every element specified in light and dark.
   Syntax colors come in two matched palettes (modelled on Xcode's defaults).
4. **Measurements from AppKit.** 13pt UI text, 11pt small, 5–6pt corner radii,
   8pt spacing grid, 0.5s caret blink, ⌘ in menus. When in doubt, open Xcode
   and measure it.
5. **No regressions to headless tests.** Everything visual stays a
   `SurfaceCmd`; tests assert pixels with a fixed test palette injected.

---

## 1. Window & chrome

### Window setup (`window.rs::open`)

- `styleMask` adds `FullSizeContentView` + `titlebarAppearsTransparent`; keep
  Titled/Closable/Miniaturizable/Resizable. The custom tab strip then runs
  under the traffic lights, Xcode-style; reserve ~78pt left inset in the tab
  bar for the lights.
- `setFrameAutosaveName("MacNCL-IDE")` — window position/size persists across
  launches. One line, immediately native feel. Drop `center()` once this is in
  (only center when no autosaved frame exists).
- Title = active buffer name (`set_title` already exists); `subtitle` = its
  directory (via `NSWindow.subtitle`, 11pt system-dim). For untitled buffers:
  title "untitled", no subtitle.
- `setMinSize(720, 480)`; default `1000×680`.
- Inactive-window look: when `!isKeyWindow`, paint labels one step dimmer
  (label → secondaryLabel) and hide the caret. macOS dims inactive windows;
  today ours doesn't change at all, which is the single most jarring tell.
- Window background: `windowBackgroundColor` (not our own fill) for the
  outermost areas so resize-live-stretch shows the system color.

### App identity

- Ship a real `.app` bundle (build step in `run-gui.sh`): `CFBundleName`
  "MacNCL", `CFBundleDocumentTypes` for `.lisp`, an app icon — a λ mark in a
  rounded-square, template-style. Today the Dock shows a generic icon and the
  app menu says "ncl"; both break the illusion immediately.
- About panel from the bundle plist (standard `orderFrontStandardAboutPanel`).

### System menu bar — replace the in-window menu

This is the headline item. The in-window `MENUS` bar in `ide/app.rs` is
deleted; a real `NSMenu` hierarchy is installed at startup on the main thread
(`NSApplication.setMainMenu`):

```
MacNCL    About MacNCL · ⌘, Settings… · Hide/Quit (⌘Q)
File      New (⌘N) · Open… (⌘O) · Open Recent ▸ · Save (⌘S) · Save As… (⇧⌘S) · Close Tab (⌘W)
Edit      Undo (⌘Z) · Redo (⇧⌘Z) · Cut/Copy/Paste (⌘X/C/V) · Select All (⌘A) ·
          Find ▸ (⌘F, ⌘G) · Comment (⌘/)
Eval      Run Buffer (⌘R) · Eval Form at Point (⌘↩) · Clear REPL (⌘K) ·
          Macroexpand (⌃⌘M — future)
View      Focus Editor (⌘E) · Focus REPL (⌘L) · Font Bigger/Smaller (⌘+/⌘−)
Window    Minimize (⌘M) · Zoom · Bring All to Front · window list      ← mostly free
Help      Keyboard Shortcuts (⌘?)
```

Mechanism (the doc comment in `app.rs` feared "fragile target/action
plumbing"; it isn't, kept in one place): a tiny dispatcher object —
`objc2::declare::ClassBuilder` subclass of `NSObject` with one
`menuAction:`(or per-menu `fileOpen:`, `evalRun:` …) method — lives on the
main thread and **posts the existing `IGuiEvent`s** into `igui_events::push`.
The worker-side `Ide::handle_event` already routes keys; menu commands reuse
exactly the `MenuCmd` enum that exists today, arriving as synthetic key events
or a small new `IGuiEvent::Command`. Nothing about IDE logic changes.

What we get free, that we cannot fake: ⌘Q/⌘H/⌘M/⌘`/Focus-follows-menu-bar,
correct ⌘-glyph rendering, Alt-key navigation, system "Help" search,
menu-item enable/disable, and the menu bar no longer consumes a strip of every
window.

## 2. Color system

### Semantic tokens (resolved per appearance, main thread)

| Token | NSColor source | Light approx | Dark approx |
|---|---|---|---|
| `text` | `labelColor` | #000000 E0 | #FFFFFF E0 |
| `text_secondary` | `secondaryLabelColor` | #3C3C43 60% | #EBEBF5 60% |
| `text_tertiary` | `tertiaryLabelColor` | #3C3C43 30% | #EBEBF5 30% |
| `chrome_bg` | `windowBackgroundColor` | #ECECEC | #38383A |
| `content_bg` | `controlBackgroundColor` / editor keeps own bg | #FFFFFF | #1E1E20 |
| `raised_bg` | `controlColor`-ish for tabs/hover | #FFFFFF | #4A4A4C |
| `separator` | `separatorColor` | #3C3C43 15% | #5B5B5E |
| `accent` | `controlAccentColor` | follows user | follows user |
| `selection_bg` | accent @ ~28% alpha | | |
| `focus_ring` | `keyboardFocusRingColor` | accent glow | accent glow |

`Rgba` values in the table are fallbacks for tests/headless; on macOS they are
resolved from `NSColor` (`usingColorSpace(sRGB)`) and never hardcoded.

### Editor syntax palettes (two, matched)

Keep the current hue assignments (they're good and One-Dark-adjacent) but
specify a light counterpart per role, e.g.:

| Role | Dark (today) | Light (new) |
|---|---|---|
| fg | #DCDFE4 | #1D1D1F |
| special | #C6A0F6 | #7C3AED |
| keyword | #F4BF75 | #AD3DA4 |
| number/string | #A6DA95 | #1F7A3D |
| char | #8ADE C8 | #0E8A80 |
| comment | #6E7887 | #707077 |
| paren | #8C96A8 | #9A9AA0 |

Selection follows the **system accent** (today's blue is hardcoded and
disagrees with a user's purple/graphite accent — a classic "not from here"
tell).

### Theme plumbing (fits the existing threads)

The worker builds batches; only the main thread may resolve `NSColor`. So:

- Main thread owns `Arc<SystemTheme>` (all tokens above, plus font metrics) in
  a `OnceLock`/swap-slot, refreshed when (a) `effectiveAppearance` flips —
  checked cheaply each 60 Hz tick — or (b) accent change notification fires.
- The worker reads the snapshot when building IDE batches (`ide.render` takes
  tokens like it takes `Theme` today). Batch commands remain concrete `Rgba`,
  so `CgCanvas` and every pixel test are untouched; tests inject a fixed
  snapshot.

## 3. Typography & metrics

- **Chrome text** (menus-free now; tabs, status, REPL prompt labels): system
  font via `NSFont.systemFont(ofSize:` — surfaced to `CgCanvas` by resolving
  family `"__system"` / `"__system-bold"` to the `NSFont` (toll-free `CTFont`)
  in the font cache, instead of `new_from_name`. Sizes: 13pt regular, 11pt
  small (status bar, subtitles), 10pt mini (rare).
- **Code**: try `monospacedSystemFont` (SF Mono — the native Xcode font),
  falling back to Menlo. Cell metrics are already measured at runtime
  (`set_metrics`), so the swap is safe; keep Menlo for headless tests where
  SF Mono may be absent. Code size stays 15pt default with ⌘+/⌘− (new) steps
  12–20pt.
- **Grid**: 8pt spacing system. Editor content inset 8pt sides; tab bar height
  28pt; status bar 24pt; gutter width = digits×cell + 16pt right pad.
- **Corner radii**: 6pt dropdown/panels, 4pt tab chips, 2pt grab-handle.
- **Caret**: 2pt wide, accent color, blinking 0.5s on / 0.5s off (NSTextView
  cadence), steady (no blink) while typing, hidden when window inactive or
  pane unfocused.
- **Focus**: the focused pane's container gets the standard 3pt focus ring
  glow *only when reachable via keyboard* (⌘E/⌘L); not on mouse click (macOS
  suppresses rings on mouse focus — another tell if we get it wrong).

## 4. Layout, pane by pane

```
┌──────────────────────────────────────────────────────┐
│ ●●●   [ scratch.lisp ] [ othello-gui.lisp ] [+]       │  ← tab strip under lights, 28pt
├──────────────────────────────────────────────────────┤ │ 1px separator
│  1   (defun square (x)                                │ │ gutter 11pt tertiary
│  2     (* x x))                                       │ │ editor: SF Mono/Menlo on content_bg
│  3                                                    │ │
│  4                                                    │ │
├──────────────────────────────────────────────────────┤ │ ← divider: 10pt hit, 1px line
│ CL-USER> (+ 1 2)                                      │ │ REPL transcript
│ 3                                                     │ │
│ CL-USER> ▮                                            │ │ input line, prompt in accent
├──────────────────────────────────────────────────────┤ │
│ scratch.lisp • 12:8 • modified          ● evaluating  │  ← status bar 24pt, 11pt secondary
└──────────────────────────────────────────────────────┘
```

- **Tab strip**: replaces today's 200pt-wide blocks. Xcode-style chips —
  active tab = `raised_bg` fill + `text`, inactive = transparent + `text_secondary`,
  modified dot `●` before the name (as today), close on hover later. `+` button
  right of the last tab for ⌘T discoverability. Overflow: `»` menu past ~8 tabs.
- **Divider**: today's 4pt lit bar becomes a 10pt **hit target** with a 1px
  `separator` line down the middle — visually gone, but easy to grab.
  `NSCursor.resizeUpDown` over it (cursor shapes below). Double-click resets
  the split to 0.62 (standard "reset" affordance).
- **Status bar**: `chrome_bg` fill, 1px `separator` top border, left = status
  string (as today), right = eval busy indicator (`●` pulsing accent while the
  worker is compiling — today you can't tell a long eval from a hang).
- **REPL**: transcript lines stay as-is; prompt label `CL-USER>` in accent,
  input line gets the focus ring when focused. Errors keep their tint but use
  the semantic `systemRed`-family rather than raw RGB.

## 5. Feel: the details that read as "Mac"

- **Smooth scrolling**: accumulate `scrollingDeltaY` (already pixel-accurate
  from the trackpad) with momentum easing — a ~150ms exponential tail.
  Discrete wheel = 3 lines × cell_h, animated. Today each tick jumps; the
  difference is immediately visible on a trackpad.
- **Cursor shapes** (in the existing event monitor, `window.rs`): I-beam over
  editor/REPL text, `resizeUpDown` over the divider, arrow elsewhere, via
  `NSCursor.set` on mouse-moved. Today the arrow never changes — a dead
  giveaway.
- **Double/triple-click**: select word / select line (form?) in the editor;
  shift-click extends the selection. Triple-click selects the *enclosing
  top-level form* — a Lisp-native twist users will love.
- **Drag & drop**: window registers for `.lisp`/`.txt` file drags (small
  overlay `NSView` as dragging destination — `NSImageView` alone can't) →
  opens a tab. Also drop on the Dock icon via the app bundle.
- **Keyboard**: ⌘Q, ⌘H, ⌘M, ⌘`, ⌘, all behave (free with the menu + Regular
  activation policy). New: ⌘N new buffer, ⌘O open, ⇧⌘S save-as. Keep the
  Command-for-IDE / Control-for-paredit convention (documented in NCLMac.md) —
  it's sound and Mac-legal.
- **Open/Save**: `NSOpenPanel`/`NSSavePanel` from the menu dispatcher (roadmap
  item; the design makes it natural — the menu's `openDocument:` posts into
  the mailbox and the worker calls `ide.load_file`). Open Recent submenu
  persists ~10 paths.
- **Quiet**: no sounds, no bouncing — macOS editors are still.

## 6. Accessibility & system integration (later, honest scope)

- Set `accessibilityLabel` on the window ("Lisp editor and REPL"); expose
  `AXValue` per pane summary. A full custom-canvas AX story (line-level
  navigation) is a real project — flagged, not promised.
- Respect Reduce Motion (skip scroll easing), and Increase Contrast (thicken
  separators, drop translucency) — both cheap checks at theme-snapshot time.
- Full keyboard navigation of menus comes free from `NSMenu`.

## 7. What stays custom — on purpose

The editor canvas, rope buffer, paredit, the `SurfaceCmd` IR, `CgCanvas`, the
headless pixel tests, the event mailbox, and the Lisp side-window model. Every
phase below layers system conventions **around** these, never inside them.

---

## Phasing (each phase is a shippable, testable PR)

1. **Native shell** — system `NSMenu` main menu + dispatcher posting into the
   mailbox; delete the in-window menu; `setFrameAutosaveName`; ⌘Q/⌘H/⌘M live;
   `.app` bundle + icon + About. *(Biggest single jump in "feels native".)*
2. **System theme** — `SystemTheme` snapshot (appearance + accent tracking),
   semantic tokens through tabs/status/divider/REPL, light+dark syntax
   palettes, SF Pro chrome font, SF Mono code font, inactive-window dimming.
3. **Native files** — `NSOpenPanel`/`NSSavePanel`, recents, drag-drop onto
   window and Dock, ⌘N/⌘O/⇧⌘S.
4. **Feel pass** — cursor shapes, smooth scrolling, caret blink cadence,
   double/triple-click, focus-ring discipline, tab-strip restyle with
   traffic-light inset (`FullSizeContentView`), window title/subtitle.
5. **Polish** — inline diagnostics squiggles, overflow tab menu, font-size
   commands, Reduce Motion/Increase Contrast, AX labels, vibrancy for a
   future sidebar/doc browser.

Each phase keeps `cargo test -p ncl-runtime igui` green (fixed test palette)
and `NCL_IGUI_DUMP` frames for eyeballing without a display.
