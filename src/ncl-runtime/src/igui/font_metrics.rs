//! Standalone font measurement service.
//!
//! Decoupled from the per-pane batch system: takes font params (and
//! optional text) and returns DirectWrite metrics synchronously. No
//! childId, no batch, no reply channel. `IDWriteFactory2` is
//! free-threaded so this is safe to call from any thread — typical
//! callers are CP code on the language thread asking
//! `Fonts.Font.GetBounds` / `StringWidth`.
//!
//! Implementation is deliberately allocation-per-call: build a
//! one-shot `IDWriteTextFormat` + `IDWriteTextLayout`, read metrics,
//! drop. If profiling ever shows this hot, slot in a (family, size,
//! weight, italic) -> format cache here — the call sites won't have
//! to change.

#![cfg(windows)]

use windows::core::PCWSTR;
use windows::Win32::Graphics::DirectWrite::{
    IDWriteTextFormat, IDWriteTextLayout, DWRITE_FONT_STRETCH_NORMAL,
    DWRITE_FONT_STYLE_ITALIC, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
    DWRITE_LINE_METRICS, DWRITE_TEXT_METRICS, DWRITE_WORD_WRAPPING_NO_WRAP,
};

use super::renderer;

/// Per-typeface cell metrics, all in DIPs.
pub struct FontMetrics {
    /// Distance from the top of the cell to the baseline.
    pub ascent: f32,
    /// Distance from the baseline to the bottom of the cell.
    pub descent: f32,
    /// Total cell height (ascent + descent + line gap).
    pub line_height: f32,
    /// Width of "M" — the natural cell width for a monospace face.
    pub advance_m: f32,
}

/// Measure the cell of a (family, size, weight, italic) combination.
/// Uses "M" as the canonical cell character. Returns `None` if any
/// DirectWrite call fails (typically "no such typeface" on a
/// hostile system, in which case the caller should retry with a
/// fallback family).
pub fn measure_font(
    family: &str,
    size_dip: f32,
    weight: u16,
    italic: bool,
) -> Option<FontMetrics> {
    let format = create_one_shot_format(family, size_dip, weight, italic)?;
    let layout = create_layout(&format, "M")?;

    let mut metrics = DWRITE_TEXT_METRICS::default();
    if unsafe { layout.GetMetrics(&mut metrics) }.is_err() {
        return None;
    }

    let mut line_metrics = [DWRITE_LINE_METRICS::default(); 1];
    let mut actual: u32 = 0;
    let _ = unsafe { layout.GetLineMetrics(Some(&mut line_metrics), &mut actual) };

    let (ascent, descent) = if actual > 0 {
        let lm = line_metrics[0];
        (lm.baseline, (lm.height - lm.baseline).max(0.0))
    } else {
        // Reasonable fallback if GetLineMetrics didn't populate.
        (metrics.height * 0.8, metrics.height * 0.2)
    };

    Some(FontMetrics {
        ascent,
        descent,
        line_height: metrics.height,
        advance_m: metrics.widthIncludingTrailingWhitespace,
    })
}

/// Measure the rendered width of an arbitrary string in a given
/// font. Returns `None` on any DirectWrite failure.
pub fn measure_string(
    text: &str,
    family: &str,
    size_dip: f32,
    weight: u16,
    italic: bool,
) -> Option<f32> {
    let format = create_one_shot_format(family, size_dip, weight, italic)?;
    let layout = create_layout(&format, text)?;
    let mut metrics = DWRITE_TEXT_METRICS::default();
    if unsafe { layout.GetMetrics(&mut metrics) }.is_err() {
        return None;
    }
    Some(metrics.widthIncludingTrailingWhitespace)
}

fn create_one_shot_format(
    family: &str,
    size_dip: f32,
    weight: u16,
    italic: bool,
) -> Option<IDWriteTextFormat> {
    let factory = &renderer::ctx().dwrite.factory;
    let family_w: Vec<u16> = family.encode_utf16().chain(std::iter::once(0)).collect();
    let locale_w: Vec<u16> = "en-us".encode_utf16().chain(std::iter::once(0)).collect();
    let style = if italic {
        DWRITE_FONT_STYLE_ITALIC
    } else {
        DWRITE_FONT_STYLE_NORMAL
    };
    unsafe {
        factory.CreateTextFormat(
            PCWSTR(family_w.as_ptr()),
            None,
            DWRITE_FONT_WEIGHT(weight as i32),
            style,
            DWRITE_FONT_STRETCH_NORMAL,
            size_dip.max(0.5), // guard against 0/negative
            PCWSTR(locale_w.as_ptr()),
        )
    }
    .ok()
}

fn create_layout(format: &IDWriteTextFormat, text: &str) -> Option<IDWriteTextLayout> {
    let factory = &renderer::ctx().dwrite.factory;
    let text_w: Vec<u16> = text.encode_utf16().collect();
    let layout =
        unsafe { factory.CreateTextLayout(&text_w, format, f32::MAX, f32::MAX) }.ok()?;
    let _ = unsafe { layout.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP) };
    Some(layout)
}
