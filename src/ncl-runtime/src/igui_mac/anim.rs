//! Feel-pass animation primitives: caret blink cadence, smooth (momentum)
//! scrolling, and the cursor-shape hit test. Pure state machines driven
//! by millisecond timestamps so everything is unit-testable headlessly.
//!
//! The driver pumps these from the central loop; when work is pending it
//! asks the main thread for an animation tick (see `window::request_ide_tick`)
//! so idle time costs zero wakeups.

/// Caret blink: 500 ms visible / 500 ms hidden, re-armed steady by any
/// keystroke — the NSTextView cadence.
#[derive(Debug)]
pub struct CaretBlink {
    visible: bool,
    last_toggle_ms: u128,
    /// Remaining ms of the steady (always-visible) hold after input.
    hold_ms: u128,
}

pub const BLINK_PERIOD_MS: u128 = 500;
/// How long the caret stays steady after a keystroke before blinking
/// resumes (a touch longer than a period, like NSTextView).
const INPUT_HOLD_MS: u128 = 600;

impl CaretBlink {
    pub fn new(now_ms: u128) -> Self {
        Self { visible: true, last_toggle_ms: now_ms, hold_ms: 0 }
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    /// A keystroke: caret solid and the blink phase restarts.
    pub fn on_input(&mut self, now_ms: u128) {
        self.visible = true;
        self.last_toggle_ms = now_ms;
        self.hold_ms = INPUT_HOLD_MS;
    }

    /// Advance to `now_ms`; returns true when visibility changed (repaint).
    pub fn advance(&mut self, now_ms: u128) -> bool {
        let dt = now_ms.saturating_sub(self.last_toggle_ms);
        if self.hold_ms > 0 {
            if dt < self.hold_ms {
                return false;
            }
            // Hold elapsed — start the blink cycle from visible.
            self.last_toggle_ms = now_ms - (dt - self.hold_ms);
            self.hold_ms = 0;
            self.visible = false;
            return true;
        }
        if dt >= BLINK_PERIOD_MS {
            self.last_toggle_ms += dt - (dt % BLINK_PERIOD_MS);
            self.visible = !self.visible;
            return true;
        }
        false
    }

    /// When the next `advance` can change anything (for tick scheduling).
    pub fn next_change_ms(&self) -> Option<u128> {
        Some(self.last_toggle_ms + self.hold_ms.max(BLINK_PERIOD_MS))
    }
}

/// Smooth scrolling: wheel impulses land in a buffer and are released
/// exponentially (~150 ms feel), conserving the total distance. With
/// Reduce Motion the impulse is emitted whole, immediately.
#[derive(Debug)]
pub struct ScrollEase {
    /// Lines still to deliver.
    remaining: f32,
    /// Lines emitted but not yet a whole line (panes scroll whole lines).
    emitted_frac: f32,
    /// Accounting so the flush conserves the total to within rounding:
    /// everything ever fed vs. whole lines already applied.
    total_input: f32,
    applied: i64,
    last_ms: u128,
    /// Time constant of the decay tail.
    tau_ms: f32,
    reduce_motion: bool,
}

impl ScrollEase {
    pub fn new(now_ms: u128) -> Self {
        Self {
            remaining: 0.0,
            emitted_frac: 0.0,
            total_input: 0.0,
            applied: 0,
            last_ms: now_ms,
            tau_ms: 60.0,
            reduce_motion: false,
        }
    }

    pub fn set_reduce_motion(&mut self, on: bool) {
        self.reduce_motion = on;
    }

    pub fn is_reduce_motion(&self) -> bool {
        self.reduce_motion
    }

    pub fn pending(&self) -> f32 {
        self.remaining.abs() + self.emitted_frac.abs()
    }

    /// A wheel impulse of `lines` (positive = scroll down/content moves).
    /// Returns lines to apply right now (Reduce Motion: everything).
    pub fn feed(&mut self, lines: f32, now_ms: u128) -> i64 {
        if self.reduce_motion {
            let r = lines.round() as i64;
            self.total_input += lines;
            self.applied += r;
            return r;
        }
        self.total_input += lines;
        self.remaining += lines;
        self.last_ms = now_ms;
        0
    }

    /// Animation step: release a fraction of the buffer. Returns whole
    /// lines to apply now (0 while nothing pending).
    pub fn step(&mut self, now_ms: u128) -> i64 {
        if self.remaining == 0.0 && self.emitted_frac == 0.0 {
            return 0;
        }
        // Lines emitted by this step's release (0 if only flushing).
        let mut emitted = 0i64;
        if self.remaining != 0.0 {
            let dt = (now_ms.saturating_sub(self.last_ms)) as f32;
            self.last_ms = now_ms;
            // Exponential release; drain the last sliver whole.
            let release = if self.remaining.abs() <= 0.5 {
                self.remaining
            } else {
                self.remaining * (1.0 - (-dt / self.tau_ms).exp())
            };
            self.remaining -= release;
            self.emitted_frac += release;
            let whole = self.emitted_frac.trunc() as i64;
            self.emitted_frac -= whole as f32;
            self.applied += whole;
            emitted = whole;
            if self.remaining != 0.0 {
                return emitted;
            }
        }
        // Tail flush: buffer drained — emit the difference to the rounded
        // total so the whole impulse is conserved and `pending` hits zero.
        self.emitted_frac = 0.0;
        let target = self.total_input.round() as i64;
        let w = target - self.applied;
        self.applied = target;
        emitted + w
    }
}

/// Which cursor the window should show at a point (pure hit test).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Arrow,
    IBeam,
    ResizeUpDown,
}

/// Pane geometry the main thread needs to pick a cursor: heights of the
/// fixed chrome bands and the divider position, in window points.
#[derive(Debug, Clone, Copy, Default)]
pub struct CursorHints {
    /// Bottom of the tab strip.
    pub header_y: f32,
    /// The editor/REPL divider's y.
    pub divider_y: f32,
    /// Total content height.
    pub height: f32,
}

pub const DIVIDER_GRAB: f32 = 5.0;

/// Arrow over chrome, I-beam over both text panes, ↕ within ±5pt of the
/// divider.
pub fn cursor_shape_for(y: f32, h: &CursorHints) -> CursorShape {
    if h.height <= 0.0 {
        return CursorShape::Arrow;
    }
    if (y - h.divider_y).abs() <= DIVIDER_GRAB {
        return CursorShape::ResizeUpDown;
    }
    if y > h.header_y && y < h.height {
        return CursorShape::IBeam;
    }
    CursorShape::Arrow
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sprint 6 AC: t=0 visible, t=500 hidden, t=1000 visible; a
    /// keystroke re-arms steady and resets the phase.
    #[test]
    fn caret_blinks_half_second_cadence() {
        let mut b = CaretBlink::new(0);
        assert!(b.visible(), "starts visible");
        assert!(!b.advance(250), "no change mid-period");
        assert!(b.advance(500), "t=500 → hidden");
        assert!(!b.visible());
        assert!(!b.advance(999));
        assert!(b.advance(1000), "t=1000 → visible again");
        assert!(b.visible());
        // Keystroke while hidden: solid again, phase restarts.
        assert!(b.advance(1500), "t=1500 → hidden");
        assert!(!b.visible());
        b.on_input(1500);
        assert!(b.visible());
        assert!(!b.advance(1800), "held steady after input (600ms hold)");
        assert!(b.advance(2110), "hold elapsed → blinks off");
        assert!(!b.visible());
    }

    /// Sprint 6 AC: a 120-px impulse conserves distance (±1), never
    /// jumps all at once, settles < 300ms, and Reduce Motion bypasses
    /// the tail entirely.
    #[test]
    fn scroll_momentum_decays_but_conserves() {
        let mut s = ScrollEase::new(0);
        // 120px at 16px/line ≈ 7.5 lines; use lines directly for clarity.
        assert_eq!(s.feed(7.5, 0), 0, "nothing applied immediately");
        let mut applied = 0i64;
        let mut peak = 0i64;
        let mut settled_at = None;
        let mut t = 0;
        while t <= 600 {
            t += 16;
            let d = s.step(t);
            applied += d;
            peak = peak.max(d);
            if settled_at.is_none() && s.pending() < 0.001 {
                settled_at = Some(t);
            }
        }
        assert!(
            (7..=8).contains(&applied),
            "conserved within ±1: 7.5 lines → {applied}"
        );
        assert!(peak < 8, "no immediate full jump (peak {peak})");
        let settled = settled_at.expect("settles");
        assert!(settled < 300, "settled at {settled}ms");

        // Reduce Motion: the whole impulse, instantly, no tail.
        s.set_reduce_motion(true);
        assert_eq!(s.feed(7.5, 1000), 8);
        assert_eq!(s.step(1016), 0, "no tail under Reduce Motion");
    }

    /// Reverse impulses subtract; nothing pending → zero work.
    #[test]
    fn scroll_ease_reverses_and_idles() {
        let mut s = ScrollEase::new(0);
        s.feed(4.0, 0);
        s.feed(-9.0, 8);
        let mut applied = 0;
        for t in (0..600).step_by(16) {
            applied += s.step(t + 16);
        }
        assert_eq!(applied, -5);
        assert_eq!(s.step(10_000), 0);
    }

    /// Sprint 6 AC: IBeam over both text panes, ↕ at the divider, arrow
    /// over the tab strip / below the status bar.
    #[test]
    fn cursor_shape_lookup() {
        let h = CursorHints { header_y: 28.0, divider_y: 400.0, height: 680.0 };
        assert_eq!(cursor_shape_for(10.0, &h), CursorShape::Arrow); // tab strip
        assert_eq!(cursor_shape_for(100.0, &h), CursorShape::IBeam); // editor
        assert_eq!(cursor_shape_for(500.0, &h), CursorShape::IBeam); // repl
        assert_eq!(cursor_shape_for(396.0, &h), CursorShape::ResizeUpDown);
        assert_eq!(cursor_shape_for(405.0, &h), CursorShape::ResizeUpDown);
        assert_eq!(cursor_shape_for(410.0, &h), CursorShape::IBeam);
        assert_eq!(cursor_shape_for(670.0, &h), CursorShape::IBeam); // repl input
        let empty = CursorHints::default();
        assert_eq!(cursor_shape_for(10.0, &empty), CursorShape::Arrow);
    }
}
