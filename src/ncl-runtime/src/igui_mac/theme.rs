//! System theme: semantic color tokens for the IDE chrome.
//!
//! Everything the IDE draws outside the editor's syntax colours is chosen
//! from here — never hardcoded — so the app tracks the system appearance
//! (light/dark) and the user's accent colour like a native Mac app.
//!
//! ## Threading model
//!
//! `NSColor`s may only be resolved on the main thread, but IDE batches are
//! built by the worker. So the main thread resolves a full
//! [`SystemTheme`] snapshot ([`refresh`], called at startup and whenever
//! the appearance/accent changes) and publishes it into a swap slot; the
//! worker reads [`current`] when building batches. The batch IR keeps
//! concrete [`Rgba`] values, so the renderer and its pixel tests are
//! untouched — tests inject the fixed snapshots below.
//!
//! `NCL_IGUI_THEME=light|dark` forces a snapshot for tests/frame dumps;
//! `NCL_GUI_FAKE_KEY_WINDOW=0|1` pins the key-window state the driver
//! feeds the IDE (inactive-window dimming).

use std::sync::{Arc, RwLock};

use crate::igui_mac::ide::editor::Theme as EditorTheme;
use crate::igui_paint::Rgba;

#[inline]
fn rgb(r: u8, g: u8, b: u8) -> Rgba {
    Rgba { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: 1.0 }
}

#[inline]
fn rgba(r: u8, g: u8, b: u8, a: f32) -> Rgba {
    Rgba { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a }
}

#[inline]
fn with_alpha(c: Rgba, a: f32) -> Rgba {
    Rgba { r: c.r, g: c.g, b: c.b, a }
}

/// Syntax palette (the editor's coloured roles).
#[derive(Clone)]
pub struct Syntax {
    pub fg: Rgba,
    pub special: Rgba,
    pub keyword: Rgba,
    pub number: Rgba,
    pub string: Rgba,
    pub char_lit: Rgba,
    pub comment: Rgba,
    pub paren: Rgba,
    pub quote: Rgba,
}

/// A complete set of semantic tokens, resolved for one appearance.
#[derive(Clone)]
pub struct SystemTheme {
    /// Window chrome: tab strip / status bar base (`windowBackgroundColor`).
    pub chrome_bg: Rgba,
    /// Raised surface: the active tab chip.
    pub raised_bg: Rgba,
    /// Sunken surface: inactive tab chips, the `+` chip, the search bar.
    pub sunken_bg: Rgba,
    /// Status bar fill (one step below the strip).
    pub status_bg: Rgba,
    /// Hairlines: the editor/REPL divider and its rule.
    pub separator: Rgba,
    /// Label tiers (`labelColor` family).
    pub text: Rgba,
    pub text_secondary: Rgba,
    pub text_tertiary: Rgba,
    /// `controlAccentColor` — caret, prompt, focus glow.
    pub accent: Rgba,
    /// Text selection: accent at low alpha.
    pub selection: Rgba,
    /// Errors (`systemRed` family).
    pub error: Rgba,
    /// Notices (quiet informational lines).
    pub notice: Rgba,
    /// The editor's own background (kept independently of chrome).
    pub editor_bg: Rgba,
    /// Line-number gutter text.
    pub gutter: Rgba,
    pub syntax: Syntax,
    /// True when resolved for a dark appearance (used for tweaks).
    pub dark: bool,
    /// The system Increase Contrast preference: thicker separators,
    /// stronger selection, no accent tints where a plain stroke reads
    /// better.
    pub high_contrast: bool,
}

impl SystemTheme {
    /// Editor/REPL `Theme` derived from these tokens. Metrics
    /// (family/size/cells) keep the defaults; the driver measures and
    /// calls `set_metrics` as before.
    pub fn to_editor_theme(&self) -> EditorTheme {
        let selection = if self.high_contrast {
            with_alpha(self.accent, 0.5)
        } else {
            self.selection
        };
        EditorTheme {
            // `__mono` → SF Mono via the AppKit resolver, Menlo headless.
            family: "__mono".into(),
            size: 15.0,
            selection,
            bg: self.editor_bg,
            fg: self.syntax.fg,
            caret: self.accent,
            gutter_fg: self.gutter,
            c_special: self.syntax.special,
            c_keyword: self.syntax.keyword,
            c_number: self.syntax.number,
            c_string: self.syntax.string,
            c_char: self.syntax.char_lit,
            c_comment: self.syntax.comment,
            c_paren: self.syntax.paren,
            c_quote: self.syntax.quote,
            prompt: self.accent,
            repl_input: self.text,
            repl_output: self.text_secondary,
            repl_error: self.error,
            repl_info: self.notice,
            search_current: with_alpha(self.syntax.keyword, 0.45),
            search_match: with_alpha(self.syntax.keyword, 0.22),
            search_bar_bg: self.sunken_bg,
            ..EditorTheme::default()
        }
    }
}

/// Fixed dark snapshot — today's palette, expressed as tokens (test/CI
/// default and the pre-resolution startup value).
pub fn fixed_dark() -> SystemTheme {
    let accent = rgb(92, 158, 255);
    SystemTheme {
        chrome_bg: rgb(40, 42, 48),
        raised_bg: rgb(56, 59, 68),
        sunken_bg: rgb(48, 51, 58),
        status_bg: rgb(44, 46, 52),
        separator: rgba(255, 255, 255, 0.16),
        text: rgba(235, 238, 245, 0.90),
        text_secondary: rgba(235, 238, 245, 0.60),
        text_tertiary: rgba(235, 238, 245, 0.32),
        selection: with_alpha(accent, 0.28),
        error: rgb(255, 105, 97),
        notice: rgb(148, 210, 189),
        editor_bg: rgb(24, 26, 33),
        gutter: rgba(235, 238, 245, 0.32),
        syntax: Syntax {
            fg: rgb(220, 223, 228),
            special: rgb(198, 160, 246),
            keyword: rgb(244, 191, 117),
            number: rgb(166, 218, 149),
            string: rgb(166, 218, 149),
            char_lit: rgb(138, 222, 200),
            comment: rgb(110, 120, 135),
            paren: rgb(140, 150, 168),
            quote: rgb(238, 153, 160),
        },
        accent,
        dark: true,
        high_contrast: false,
    }
}

/// Fixed light snapshot (from the design table; the readability target
/// for light mode).
pub fn fixed_light() -> SystemTheme {
    let accent = rgb(59, 130, 246);
    SystemTheme {
        chrome_bg: rgb(236, 236, 238),
        raised_bg: rgb(255, 255, 255),
        sunken_bg: rgb(222, 222, 226),
        status_bg: rgb(228, 228, 231),
        separator: rgba(60, 60, 67, 0.22),
        text: rgba(0, 0, 0, 0.85),
        text_secondary: rgba(60, 60, 67, 0.60),
        text_tertiary: rgba(60, 60, 67, 0.32),
        selection: with_alpha(accent, 0.28),
        error: rgb(212, 45, 45),
        notice: rgb(65, 135, 115),
        editor_bg: rgb(255, 255, 255),
        gutter: rgba(60, 60, 67, 0.32),
        syntax: Syntax {
            fg: rgb(29, 29, 31),
            special: rgb(124, 58, 237),
            keyword: rgb(173, 61, 164),
            number: rgb(31, 122, 61),
            string: rgb(31, 122, 61),
            char_lit: rgb(14, 138, 128),
            comment: rgb(112, 112, 119),
            paren: rgb(154, 154, 160),
            quote: rgb(196, 50, 107),
        },
        accent,
        dark: false,
        high_contrast: false,
    }
}

/// The dark snapshot with Increase Contrast applied (test helper): a
/// stronger selection alpha is the visible delta.
pub fn fixed_high_contrast() -> SystemTheme {
    let mut t = fixed_dark();
    t.high_contrast = true;
    t.selection = with_alpha(t.accent, 0.5);
    t
}

// ── Swap slot ─────────────────────────────────────────────────────────────

fn slot() -> &'static RwLock<Arc<SystemTheme>> {
    static S: std::sync::OnceLock<RwLock<Arc<SystemTheme>>> = std::sync::OnceLock::new();
    S.get_or_init(|| RwLock::new(Arc::new(fixed_dark())))
}

/// The worker-side read: the latest resolved (or forced) snapshot.
pub fn current() -> Arc<SystemTheme> {
    slot().read().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn set(theme: SystemTheme) {
    *slot().write().unwrap_or_else(|e| e.into_inner()) = Arc::new(theme);
}

// ── System resolution (main thread, AppKit) ───────────────────────────────

/// Re-resolve from AppKit if the appearance or accent changed since the
/// last call. Cheap enough to poll every main-thread tick (one string
/// compare + one color resolution); returns true when a new snapshot was
/// published. `NCL_IGUI_THEME=light|dark` forces that appearance (the
/// accent still tracks the system).
#[cfg(feature = "mac-gui")]
pub fn refresh(app: &objc2_app_kit::NSApplication) -> bool {
    use objc2_app_kit::{NSColor, NSColorSpace};

    let dark = match forced_appearance() {
        Some(d) => d,
        None => app
            .effectiveAppearance()
            .name()
            .to_string()
            .contains("Dark"),
    };
    // SAFETY: main thread, live colors.
    let accent = unsafe { srgb_components(&NSColor::controlAccentColor()) };
    let sig = (dark, accent.r.to_bits(), accent.g.to_bits(), accent.b.to_bits());
    if LAST_SIG.get().is_some_and(|&s| s == sig) {
        return false;
    }
    LAST_SIG.get_or_init(|| sig);

    // SAFETY: main thread, live colors.
    let token = |c: &NSColor| unsafe { srgb_components(c) };
    let chrome = token(&NSColor::windowBackgroundColor());
    let text = token(&NSColor::labelColor());
    let base = if dark { fixed_dark() } else { fixed_light() };
    let high_contrast = objc2_app_kit::NSWorkspace::sharedWorkspace()
        .accessibilityDisplayShouldIncreaseContrast();
    let theme = SystemTheme {
        high_contrast,
        chrome_bg: chrome,
        // Chips derive from the resolved chrome (no exact system color):
        // lift toward the label color, which reads as "raised" in both
        // appearances.
        raised_bg: blend(chrome, text, 0.10),
        sunken_bg: blend(chrome, text, 0.04),
        status_bg: token(&NSColor::controlBackgroundColor()),
        separator: token(&NSColor::separatorColor()),
        text,
        text_secondary: token(&NSColor::secondaryLabelColor()),
        text_tertiary: token(&NSColor::tertiaryLabelColor()),
        accent,
        selection: with_alpha(accent, 0.28),
        error: token(&NSColor::systemRedColor()),
        notice: token(&NSColor::secondaryLabelColor()),
        dark,
        ..base
    };
    let _ = NSColorSpace::sRGBColorSpace(); // referenced for docs parity
    set(theme);
    true
}

#[cfg(feature = "mac-gui")]
static LAST_SIG: std::sync::OnceLock<(bool, u32, u32, u32)> = std::sync::OnceLock::new();

/// Linear blend `a → b` by `t`.
fn blend(a: Rgba, b: Rgba, t: f32) -> Rgba {
    Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

/// Resolve an `NSColor` to sRGB components. Catalog colors must be
/// converted to a concrete space first or `getRed:` raises.
///
/// # Safety
/// Must run on the main thread with a live color.
#[cfg(feature = "mac-gui")]
unsafe fn srgb_components(c: &objc2_app_kit::NSColor) -> Rgba {
    use objc2::msg_send;
    use objc2_app_kit::{NSColor, NSColorSpace};

    let space = NSColorSpace::sRGBColorSpace();
    // `colorUsingColorSpace:` is the ObjC selector (Swift renames it to
    // `usingColorSpace(_:)`). Returns a concrete sRGB color, or nil for
    // incompatible patterns — fall back to the raw color then.
    let converted: Option<objc2::rc::Retained<NSColor>> =
        unsafe { msg_send![c, colorUsingColorSpace: &*space] };
    let color: &NSColor = match converted.as_deref() {
        Some(c) => c,
        None => c,
    };
    let (mut r, mut g, mut b, mut a) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    // SAFETY: valid out pointers; the color is RGB-compatible after the
    // sRGB conversion above.
    unsafe {
        color.getRed_green_blue_alpha(&mut r, &mut g, &mut b, &mut a);
    }
    Rgba { r: r as f32, g: g as f32, b: b as f32, a: a as f32 }
}

/// `NCL_IGUI_THEME` override: `Some(true)` dark, `Some(false)` light.
fn forced_appearance() -> Option<bool> {
    static F: std::sync::OnceLock<Option<bool>> = std::sync::OnceLock::new();
    *F.get_or_init(|| {
        match std::env::var("NCL_IGUI_THEME").as_deref() {
            Ok("light") => Some(false),
            Ok("dark") => Some(true),
            _ => None,
        }
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_differs(name: &str, a: Rgba, b: Rgba) {
        assert!(
            (a.r - b.r).abs() > 1e-3
                || (a.g - b.g).abs() > 1e-3
                || (a.b - b.b).abs() > 1e-3
                || (a.a - b.a).abs() > 1e-3,
            "token {name} is identical between appearances"
        );
    }

    /// Sprint 3 AC: resolving light vs dark yields different values for
    /// every chrome token (the classic wrong-color-space bug makes them
    /// collapse to the same grey).
    #[test]
    fn every_token_differs_between_appearances() {
        let l = fixed_light();
        let d = fixed_dark();
        for (name, lt, dt) in [
            ("chrome_bg", l.chrome_bg, d.chrome_bg),
            ("raised_bg", l.raised_bg, d.raised_bg),
            ("sunken_bg", l.sunken_bg, d.sunken_bg),
            ("status_bg", l.status_bg, d.status_bg),
            ("separator", l.separator, d.separator),
            ("text", l.text, d.text),
            ("text_secondary", l.text_secondary, d.text_secondary),
            ("text_tertiary", l.text_tertiary, d.text_tertiary),
            ("selection", l.selection, d.selection),
            ("error", l.error, d.error),
            ("editor_bg", l.editor_bg, d.editor_bg),
            ("gutter", l.gutter, d.gutter),
            ("syntax.fg", l.syntax.fg, d.syntax.fg),
            ("syntax.special", l.syntax.special, d.syntax.special),
            ("syntax.keyword", l.syntax.keyword, d.syntax.keyword),
            ("syntax.number", l.syntax.number, d.syntax.number),
            ("syntax.string", l.syntax.string, d.syntax.string),
            ("syntax.char_lit", l.syntax.char_lit, d.syntax.char_lit),
            ("syntax.comment", l.syntax.comment, d.syntax.comment),
            ("syntax.paren", l.syntax.paren, d.syntax.paren),
            ("syntax.quote", l.syntax.quote, d.syntax.quote),
        ] {
            assert_differs(name, lt, dt);
        }
        assert!(l.dark == false && d.dark == true);
    }

    /// Selection must follow the accent at the fixed alpha — no hardcoded
    /// blue anywhere.
    #[test]
    fn selection_uses_accent() {
        for t in [fixed_light(), fixed_dark()] {
            assert!((t.selection.a - 0.28).abs() < 1e-3);
            assert!((t.selection.r - t.accent.r).abs() < 1e-3);
            assert!((t.selection.g - t.accent.g).abs() < 1e-3);
            assert!((t.selection.b - t.accent.b).abs() < 1e-3);
        }
    }

    /// The swap slot round-trips; to_editor_theme keeps token values.
    #[test]
    fn swap_slot_and_derivation() {
        set(fixed_light());
        assert!(!current().dark);
        assert_eq!(current().accent.g, fixed_light().accent.g);
        let et = current().to_editor_theme();
        assert_eq!(et.caret, fixed_light().accent);
        assert_eq!(et.bg, fixed_light().editor_bg);
        assert_eq!(et.repl_error, fixed_light().error);
        // Restore the dark default so other tests see the CI baseline.
        set(fixed_dark());
    }
}
