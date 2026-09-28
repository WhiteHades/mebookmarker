//! motion.
//!
//! a terminal redraws a whole frame, so there is no compositor and no
//! compositor means motion is not free: every frame is a full repaint, and a
//! terminal on a slow link makes a 300ms animation cost thirty full-screen
//! writes. what follows is therefore held to the rule that matters most in a
//! terminal, which is the one the browser skills state and a terminal makes
//! sharper:
//!
//! - **high-frequency interactions get instant feedback.** a keystroke, a row
//!   of the list moving, a frame appearing. there is no transition on any of
//!   them, because a person typing is not waiting for a picture and paying for
//!   it in repaints is how a list feels laggy.
//! - **infrequent moments get the motion.** opening an item, an action landing,
//!   an empty state arriving. those happen a few times a minute rather than
//!   ten times a second, so they can afford it.
//! - **every animated change leaves a static cue.** a row that slides in is
//!   also drawn bold; a status that brightens and fades is also text that
//!   remains. motion is a second channel, never the only one.
//! - **the whole thing can be turned off.** a terminal has no way to read the
//!   operating system's motion preference, so the reader sets it:
//!   `MBM_NO_MOTION=1`, or `--no-motion` on the command.
//!
//! the durations below are the ones the web recipes give, used unchanged: a
//! 300ms entrance, a 150ms exit, a 100ms stagger, and `cubic-bezier(0.2, 0, 0, 1)`
//! as the curve for both directions. a shorter or smaller animation is preferred
//! where it says the same thing, so a selection change uses none at all.

use std::time::{Duration, Instant};

/// how long an entrance runs.
pub const ENTER: Duration = Duration::from_millis(300);

/// how long an exit runs. shorter than the entrance, because an exit is not
/// asking for attention: the reader's attention has already moved on.
pub const EXIT: Duration = Duration::from_millis(150);

/// the delay between two chunks of a staged entrance.
pub const STAGGER: Duration = Duration::from_millis(100);

/// whether motion is on.
///
/// off by nothing and on by default, because a terminal gives the program no
/// way to know. the reader asks for less and this is the switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Motion {
    enabled: bool,
}

impl Default for Motion {
    fn default() -> Self {
        // the environment first, because a person who has set it once does not
        // want to set it on every command
        let asked_for_none =
            std::env::var("MBM_NO_MOTION").is_ok_and(|v| !v.is_empty() && v != "0");
        Self { enabled: !asked_for_none }
    }
}

impl Motion {
    /// motion off, whatever the environment said.
    #[must_use]
    pub const fn none() -> Self {
        Self { enabled: false }
    }

    /// whether anything animates.
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        self.enabled
    }

    /// how far through an animation of `duration` we are, from 0 to 1.
    ///
    /// a disabled clock jumps straight to 1, so every caller can ask the same
    /// question and get "finished" without checking the switch first.
    #[must_use]
    pub fn progress(self, started: Instant, duration: Duration) -> f64 {
        if !self.enabled || duration.is_zero() {
            return 1.0;
        }
        let elapsed = started.elapsed().as_secs_f64();
        (elapsed / duration.as_secs_f64()).clamp(0.0, 1.0)
    }

    /// whether an animation of `duration` started at `started` is still going.
    #[must_use]
    pub fn running(self, started: Instant, duration: Duration) -> bool {
        self.enabled && !duration.is_zero() && started.elapsed() < duration
    }
}

/// the curve both directions use: `cubic-bezier(0.2, 0, 0, 1)`.
///
/// it leaves fast and arrives slower, which is what an entrance wants, and it is
/// the same curve for the exit so a reversal reads as one motion rather than
/// two. a terminal has no compositor to interpolate for us, so the curve is
/// solved here: find the time whose x equals the elapsed fraction, then read
/// the y at that time.
#[must_use]
pub fn ease_out(t: f64) -> f64 {
    /// the two control points of `cubic-bezier(0.2, 0, 0, 1)`.
    const P1: (f64, f64) = (0.2, 0.0);
    const P2: (f64, f64) = (0.0, 1.0);

    let elapsed = t.clamp(0.0, 1.0);
    if elapsed <= 0.0 {
        return 0.0;
    }
    if elapsed >= 1.0 {
        return 1.0;
    }

    // a cubic bezier is parameterised by its own parameter, not by time: the
    // animation asks "which parameter is at this fraction of the way along",
    // and the answer is the y at that parameter. using the parameter as if it
    // were the value is the mistake that makes this curve look almost right
    // and be subtly wrong everywhere.
    let axis = |c1: f64, c2: f64, s: f64| {
        let u = 1.0 - s;
        3.0 * u * u * s * c1 + 3.0 * u * s * s * c2 + s * s * s
    };
    let x = |s: f64| axis(P1.0, P2.0, s);
    let y = |s: f64| axis(P1.1, P2.1, s);
    let dx = |s: f64| {
        let u = 1.0 - s;
        3.0 * u * u * P1.0 + 6.0 * u * s * (P2.0 - P1.0) + 3.0 * s * s * (1.0 - P2.0)
    };

    // newton converges in three or four steps here, and the fall back to a plain
    // bisection covers the flat stretch at the start where the slope vanishes
    let mut s = elapsed;
    for _ in 0..8 {
        let error = x(s) - elapsed;
        if error.abs() < 1e-7 {
            break;
        }
        let slope = dx(s);
        if slope.abs() < 1e-7 {
            break;
        }
        s = (s - error / slope).clamp(0.0, 1.0);
    }
    y(s).clamp(0.0, 1.0)
}

/// the row offset a staged entrance starts from, given how far along it is.
///
/// this is the terminal's `translateY`. a view that arrives from one row below
/// and settles says "it opened" without a border flashing, and it is the only
/// direction a text grid can move in that reads as motion rather than as a
/// mistake.
#[must_use]
pub fn enter_offset(progress: f64) -> u16 {
    // a full row of travel is enough. more than that and the content appears to
    // be sliding in from somewhere off screen
    ((1.0 - ease_out(progress)) * 1.0).round().clamp(0.0, 1.0) as u16
}

/// which chunk of a staged entrance has arrived.
///
/// the chunks are the semantic parts of a view, and they arrive 100ms apart so
/// the order the eye reads them in is the order they appear in. the last chunk
/// is always considered arrived, so a run of motion with nothing to show at the
/// end still finishes.
#[must_use]
pub fn arrived(progress: f64, chunks: usize) -> usize {
    if chunks == 0 {
        return 0;
    }
    if progress >= 1.0 {
        return chunks;
    }
    // the first chunk is there from the first frame rather than after a delay,
    // because a view that starts empty reads as broken rather than as arriving
    ((progress * chunks as f64).ceil() as usize).max(1).min(chunks)
}

/// blend a colour toward another by `t`, from 0 to 1.
///
/// a terminal has no opacity, so this is how a fade is done: the foreground is
/// mixed into the background it is drawn on. the result is a real colour rather
/// than a dither pattern, and because it is a pure function of the elapsed time
/// it is interruptible — a new animation simply starts from wherever the last
/// one had got to.
#[must_use]
pub fn fade(
    from: ratatui::style::Color,
    to: ratatui::style::Color,
    t: f64,
) -> ratatui::style::Color {
    let t = t.clamp(0.0, 1.0);
    let channel = |a: u8, b: u8| {
        let mixed = f64::from(a) + (f64::from(b) - f64::from(a)) * t;
        mixed.round().clamp(0.0, 255.0) as u8
    };
    match (from, to) {
        (ratatui::style::Color::Rgb(r1, g1, b1), ratatui::style::Color::Rgb(r2, g2, b2)) => {
            ratatui::style::Color::Rgb(channel(r1, r2), channel(g1, g2), channel(b1, b2))
        }
        // blending into anything this program did not pick would be a guess, so
        // a fade that cannot be computed snaps to the end state instead
        _ => {
            if t < 1.0 {
                from
            } else {
                to
            }
        }
    }
}

/// a clock that restarts when a new animation begins.
///
/// one of these per animated thing. `restart` is what makes a second action
/// interrupt the first: the animation is a function of `since` and the new value
/// of `since`, so retargeting is instant and there is no timeline to finish.
#[derive(Debug)]
pub struct Clock {
    since: Instant,
}

impl Clock {
    /// a clock that has already finished, so the first frame is the end state.
    ///
    /// this is the "no animation on page load" rule: the first thing drawn is
    /// the settled thing, and motion only ever happens in response to something
    /// the reader did.
    #[must_use]
    pub fn settled() -> Self {
        Self { since: Instant::now() }
    }

    /// a clock that has just started.
    #[must_use]
    pub fn started() -> Self {
        Self { since: Instant::now() }
    }

    /// start again from now.
    pub fn restart(&mut self) {
        self.since = Instant::now();
    }

    /// when this animation began.
    #[must_use]
    pub fn since(&self) -> Instant {
        self.since
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::settled()
    }
}
