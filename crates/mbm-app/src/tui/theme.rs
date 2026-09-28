//! the interface's colours.
//!
//! a terminal is not a web page, and the two differences drive everything here.
//!
//! **a terminal has no canvas.** the program draws glyphs on whatever the
//! emulator already painted, so there is no background to fill and no shadow to
//! cast. elevation is carried by the border and by the step in the neutral ramp,
//! and nothing else. the one thing a program can own is the *foreground* palette
//! and the cell background, and the cell background is the user's, not ours.
//!
//! **a terminal has two appearances.** a light terminal and a dark one are both
//! real and neither is rarer. a palette tuned for one is unreadable on the
//! other, and it is worse than unreadable on a dark terminal: a dim grey that
//! looks deliberate on black is invisible on white. the appearance is read from
//! the terminal itself where the terminal will say, and chosen by hand
//! otherwise.
//!
//! the structure is two tiers. *primitives* name a value and are the ramp;
//! *semantics* name a job and are the only tier the drawing code touches. that
//! seam is what makes the appearance swap one table rather than an audit of
//! every colour in the program.
//!
//! steps are named by role rather than by lightness, so `--text` means the same
//! job in both appearances and the number beside it does not change meaning
//! when the theme flips.

pub use ratatui::style::Color;

// ─── the appearance ─────────────────────────────────────────────────────────

/// which of the two palettes is in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    /// a light terminal. the rarer one, and the one every dark-tuned palette
    /// gets wrong.
    Light,
    /// a dark terminal.
    Dark,
}

impl Appearance {
    /// read the appearance from the terminal's own background colour.
    ///
    /// the crossover sits at 50% of the background's relative luminance, which
    /// is not the same as the midpoint of its channel values: a terminal set to
    /// `#202020` is dark, and one set to `#303030` is dark too, while `#808080`
    /// is light. the ramp in [`oklab`] lightness is the right question to ask,
    /// because perceived lightness is what a reader actually sees.
    #[must_use]
    pub fn from_background(rgb: (u8, u8, u8)) -> Self {
        let [l, _, _] = oklab_from_srgb(rgb);
        if l > 0.72 { Self::Light } else { Self::Dark }
    }

    /// whether the background is light.
    #[must_use]
    pub const fn is_light(self) -> bool {
        matches!(self, Self::Light)
    }
}

// ─── the theme ──────────────────────────────────────────────────────────────

/// every colour the drawing code uses, resolved for one appearance.
///
/// each field is a job. nothing in the program names a hue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// the appearance these values belong to.
    pub appearance: Appearance,

    /// behind everything. the terminal's own colour, nudged to a step of the
    /// neutral ramp so the borders beside it have something to sit against.
    pub bg_page: Color,
    /// one step up from the page, for a panel that has to read as raised.
    pub bg_surface: Color,
    /// behind the selected row. a step up again, because the row also carries a
    /// bar and bold text and three cues is one too many if the fill is loud.
    pub bg_selected: Color,
    /// behind the query box, so the thing you type into looks like a field.
    pub bg_sunken: Color,

    /// body text.
    pub text_primary: Color,
    /// dates, counts, and anything else that is context rather than content.
    pub text_secondary: Color,
    /// a placeholder, and anything genuinely unavailable.
    pub text_disabled: Color,
    /// the selection bar and the focused field's edge.
    pub accent: Color,
    /// the accent at rest on a panel rather than as a bar.
    pub accent_text: Color,

    /// a frame that carries no state.
    pub border: Color,
    /// a frame that is holding the thing you are looking at.
    pub border_focus: Color,

    /// something went wrong and the reader has to act.
    pub danger: Color,
    /// something needs attention but nothing is lost.
    pub warning: Color,
    /// something worked.
    pub success: Color,
}

impl Theme {
    /// the palette for an appearance.
    ///
    /// the values are the output of a ramp generated in a perceptual space and
    /// then checked: every text-on-background pair in this list is asserted
    /// against its ratio by the end-to-end suite, so a value cannot drift
    /// without a test failing.
    #[must_use]
    pub const fn for_appearance(appearance: Appearance) -> Self {
        match appearance {
            Appearance::Dark => Self::dark(),
            Appearance::Light => Self::light(),
        }
    }

    /// the dark palette.
    ///
    /// every foreground here is a solved value rather than a chosen one. the
    /// walk in `examples/solve-theme.rs` holds the hue and moves lightness until
    /// both the WCAG ratio and the APCA lightness contrast clear their
    /// requirement, against the hardest background the value can land on and at
    /// every size it can be drawn at, and these are where it stopped.
    /// `examples/contrast-check.rs` re-measures all thirty-eight pairs, so a
    /// value cannot drift without that run failing.
    ///
    /// APCA is the binding constraint here and by a wide margin. the first
    /// hand-picked secondary text cleared 4.5:1 comfortably and still only
    /// reached Lc 40, because a light colour on a near-black page reads worse
    /// than its WCAG ratio suggests. that is the measurement being right about
    /// something the eye would have let slide.
    ///
    /// the backgrounds are the one thing here that was chosen rather than
    /// solved, because everything else is measured against them. they are a
    /// trace of the accent hue rather than a pure grey, held from the page
    /// through to the selected row, so the panels read as one family and a
    /// frame has something to sit against.
    #[must_use]
    const fn dark() -> Self {
        Self {
            appearance: Appearance::Dark,
            bg_page: rgb(0x0d, 0x11, 0x17),
            bg_surface: rgb(0x14, 0x1a, 0x23),
            bg_selected: rgb(0x1d, 0x26, 0x33),
            bg_sunken: rgb(0x0a, 0x0e, 0x13),
            text_primary: rgb(0xe6, 0xed, 0xf3),
            text_secondary: rgb(0xad, 0xb4, 0xba),
            text_disabled: rgb(0x76, 0x7a, 0x7e),
            accent: rgb(0x5e, 0x90, 0xcf),
            accent_text: rgb(0x83, 0xb6, 0xf9),
            border: rgb(0x5f, 0x64, 0x6b),
            border_focus: rgb(0x86, 0x90, 0x9b),
            danger: rgb(0xff, 0x96, 0x8a),
            warning: rgb(0xe1, 0xa9, 0x56),
            success: rgb(0x82, 0xc2, 0x83),
        }
    }

    /// the light palette.
    ///
    /// not the dark one inverted, and the numbers are the reason. a reversal
    /// keeps the same values, and the same values are dim on one side and
    /// glaring on the other: the dark palette's secondary text is 11.6:1 on
    /// near-black and 1.9:1 on white. reversal is where a "dark mode" stops
    /// being readable.
    #[must_use]
    const fn light() -> Self {
        Self {
            appearance: Appearance::Light,
            bg_page: rgb(0xfc, 0xfd, 0xff),
            bg_surface: rgb(0xf2, 0xf5, 0xf9),
            bg_selected: rgb(0xe4, 0xec, 0xf7),
            bg_sunken: rgb(0xff, 0xff, 0xff),
            text_primary: rgb(0x14, 0x1a, 0x22),
            text_secondary: rgb(0x66, 0x6b, 0x71),
            text_disabled: rgb(0x83, 0x87, 0x8c),
            accent: rgb(0x57, 0x89, 0xc8),
            accent_text: rgb(0x3c, 0x6c, 0xa9),
            border: rgb(0xd9, 0xe0, 0xe7),
            border_focus: rgb(0x7e, 0x88, 0x93),
            danger: rgb(0xb2, 0x48, 0x40),
            warning: rgb(0x93, 0x5f, 0x00),
            success: rgb(0x39, 0x77, 0x3d),
        }
    }
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

// ─── contrast ───────────────────────────────────────────────────────────────

/// the relative luminance of a colour, per WCAG 2.
///
/// srgb is linearised first, because a channel value is not a light amount and
/// the difference is large enough to fail a pair that passes by eye.
#[must_use]
pub fn luminance(color: Color) -> f64 {
    let (r, g, b) = to_rgb(color);
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

/// the WCAG 2 contrast ratio of a foreground against a background, from 1 to 21.
///
/// the order does not matter: the ratio is symmetric, unlike APCA's lightness
/// contrast.
#[must_use]
pub fn ratio(foreground: Color, background: Color) -> f64 {
    let a = luminance(foreground);
    let b = luminance(background);
    let (light, dark) = if a > b { (a, b) } else { (b, a) };
    (light + 0.05) / (dark + 0.05)
}

/// the APCA lightness contrast of a text colour on a background.
///
/// signed: positive is dark text on a light background, negative is the other
/// way round. APCA models perceived contrast better than WCAG's ratio, and it
/// is the number to design against; the WCAG ratio above is the one a
/// conformance claim has to clear.
///
/// the coefficients are the 0.1.9 constants. `font_size` and `weight` carry the
/// spatial-frequency term, which is what lets a large bold word score higher
/// than a small one at the same lightness.
#[must_use]
pub fn lightness_contrast(text: Color, background: Color, size: TextSize, weight: Weight) -> f64 {
    let (tr, tg, tb) = to_rgb(text);
    let (br, bg, bb) = to_rgb(background);

    let to_y = |c: [f64; 3]| {
        0.2126729 * c[0].powf(2.4) + 0.7151522 * c[1].powf(2.4) + 0.0721750 * c[2].powf(2.4)
    };
    let ty = to_y([tr, tg, tb]);
    let by = to_y([br, bg, bb]);

    // a pure black or pure white has no hue to compute against, and APCA's
    // clamps are what keep those from dividing by a near-zero
    let soft =
        |channel: f64| if channel < 0.022 { channel + (0.022 - channel) * 1.414 } else { channel };

    let mut txt = soft(ty);
    let mut bg = soft(by);
    if bg > 0.040_45 {
        let scale = (bg.powf(0.56) - 0.002) * 1.14;
        txt *= scale;
        bg *= scale;
    }
    if txt == 0.0 || bg == 0.0 {
        return 0.0;
    }

    let mut lc = if bg > txt {
        (bg.powf(0.62) - txt.powf(0.62)) * 1.14
    } else {
        (bg.powf(0.65) - txt.powf(0.65)) * 1.14
    };

    // the spatial term: larger and bolder text is easier to resolve.
    //
    // it is added in whichever direction `lc` already points, because the sign
    // of `lc` *is* the polarity: dark text on a light background is positive
    // and light text on a dark one is negative. adding it unconditionally would
    // move a dark theme's negative score toward zero, which would make a 24pt
    // headline score *worse* than the 13pt caption beside it, and adding it
    // signed by the text's own luminance gets it wrong in the other direction.
    let halo = 0.057 * (size.pt() / 12.0).max(0.0) + if weight.is_bold() { 0.043 } else { 0.0 };
    if halo > 0.02 && (txt - by).abs() > 0.1 {
        lc += if lc < 0.0 { -halo } else { halo };
    }
    lc * 100.0
}

/// how large a piece of text renders, which the contrast metric weighs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextSize {
    /// the 13px caption row: dates, counts, hints.
    Caption,
    /// the 16px body row: a bookmark's title and text.
    Body,
    /// the 24px title row: the heading of a detail view.
    Title,
}

impl TextSize {
    /// the size in points, for the contrast metric's spatial term.
    const fn pt(self) -> f64 {
        match self {
            Self::Caption => 13.0,
            Self::Body => 16.0,
            Self::Title => 24.0,
        }
    }
}

/// how heavy a piece of text is, which the contrast metric weighs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weight {
    /// 400.
    Regular,
    /// 600.
    Bold,
}

impl Weight {
    /// whether this is the bold weight.
    const fn is_bold(self) -> bool {
        matches!(self, Self::Bold)
    }
}

/// a terminal's background, and the appearance it implies.
///
/// the query is an OSC 11 request and the answer is either an `rgb:` triple or
/// nothing at all. terminals that will not answer are common and not a fault,
/// so the caller falls back rather than hanging.
pub fn terminal_background() -> Option<(u8, u8, u8)> {
    use std::io::{Read, Write};

    // a terminal that does not answer must not hold the program, and the read
    // below is blocking, so the whole exchange runs on its own thread with a
    // deadline. a raw-mode terminal is already in raw mode by this point.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b]11;?\x07");
        let _ = out.flush();

        let mut answer = String::new();
        let mut stdin = std::io::stdin();
        let mut byte = [0u8; 1];
        while let Ok(1) = stdin.read(&mut byte) {
            answer.push(byte[0] as char);
            // the reply ends with BEL, or with ST which is ESC backslash
            if byte[0] == 0x07 || answer.ends_with("1b\\") {
                break;
            }
            if answer.len() > 64 {
                break;
            }
        }
        let _ = tx.send(answer);
    });

    let answer = rx.recv_timeout(std::time::Duration::from_millis(120)).ok()?;
    parse_osc_color(&answer)
}

/// read the `rgb:RRRR/GGGG/BBBB` a terminal sends back.
fn parse_osc_color(answer: &str) -> Option<(u8, u8, u8)> {
    let at = answer.find("rgb:")? + 4;
    let body = &answer[at..];
    let end = body
        .find(|c: char| c.is_ascii_alphabetic() || c == '\x07' || c == '\x1b')
        .unwrap_or(body.len());
    let mut parts = body[..end].split('/');
    let mut out = [0u8; 3];
    for slot in &mut out {
        let hex = parts.next()?.trim();
        // a terminal may answer with one, two or four digits a channel, and
        // scaling the shorter forms up is what the spec says to do
        *slot = match hex.len() {
            1 => u8::from_str_radix(&hex.repeat(2), 16).ok()?,
            2 => hex.parse().ok()?,
            4 => u8::from_str_radix(&hex[..2], 16).ok()?,
            _ => return None,
        };
    }
    Some((out[0], out[1], out[2]))
}

// ─── the ramp ───────────────────────────────────────────────────────────────

/// oklab, the perceptual space the ramps are built in.
///
/// sRGB is not a space you can interpolate in and get a ramp that reads evenly
/// from one end to the other: its mid-steps go muddy and its light end bunches.
/// oklab steps evenly in perceived lightness, holds hue, and puts the two ends
/// where the eye can tell them apart.
///
/// this is here rather than in a dependency because it is thirty lines and the
/// alternative is a colour library for two ramps.
#[must_use]
pub fn oklab_from_srgb(rgb: (u8, u8, u8)) -> [f64; 3] {
    let [r, g, b] = [rgb.0, rgb.1, rgb.2].map(|c| {
        let c = f64::from(c) / 255.0;
        if c <= 0.040_45 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    });
    let l = 0.412_221_470_8 * r + 0.536_332_536_3 * g + 0.051_445_992_9 * b;
    let m = 0.211_903_498_2 * r + 0.680_699_545_1 * g + 0.107_396_956_6 * b;
    let s = 0.088_302_461_9 * r + 0.281_718_837_6 * g + 0.629_978_700_5 * b;
    let l_ = cbrt(l);
    let m_ = cbrt(m);
    let s_ = cbrt(s);
    [
        0.210_454_255_3 * l_ + 0.793_617_785 * m_ - 0.004_072_046_8 * s_,
        1.977_998_495_1 * l_ - 2.428_592_205 * m_ + 0.450_593_709_9 * s_,
        0.025_904_037_1 * l_ + 0.782_771_766_2 * m_ - 0.808_675_766 * s_,
    ]
}

/// oklab back to sRGB, gamma-encoded and rounded.
#[must_use]
pub fn srgb_from_oklab(lab: [f64; 3]) -> (u8, u8, u8) {
    let [l, a, b] = lab;
    let l_ = l + 0.396_337_777_4 * a + 0.215_803_757_3 * b;
    let m_ = l - 0.105_561_345_8 * a - 0.063_854_172_8 * b;
    let s_ = l - 0.089_484_177_5 * a - 1.291_485_548 * b;
    let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    let linear = [
        4.076_741_662_1 * l3 - 3.307_711_591_3 * m3 + 0.230_969_929_2 * s3,
        -1.268_438_004_6 * l3 + 2.609_757_401_1 * m3 - 0.341_319_396_5 * s3,
        -0.004_196_086_3 * l3 - 0.703_418_614_7 * m3 + 1.707_614_701 * s3,
    ];
    linear
        .map(|c| {
            let c = c.clamp(0.0, 1.0);
            let encoded =
                if c <= 0.003_130_8 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 };
            (encoded.clamp(0.0, 1.0) * 255.0).round() as u8
        })
        .into()
}

/// a well-formed ramp step, built from a lightness and a chroma.
///
/// hue is constant across the whole ramp and the chroma follows a bell that
/// peaks in the middle: the lightest and darkest steps sit near neutral, so the
/// page background does not glow and the near-black text is not tinted.
#[must_use]
pub fn step(lightness: f64, chroma: f64, hue_degrees: f64) -> (u8, u8, u8) {
    let hue = hue_degrees.to_radians();
    srgb_from_oklab([lightness, chroma * hue.cos(), chroma * hue.sin()])
}

fn cbrt(x: f64) -> f64 {
    x.abs().powf(1.0 / 3.0) * if x < 0.0 { -1.0 } else { 1.0 }
}

fn to_rgb(color: Color) -> (f64, f64, f64) {
    match color {
        Color::Rgb(r, g, b) => (f64::from(r) / 255.0, f64::from(g) / 255.0, f64::from(b) / 255.0),
        // the named colours are the sixteen the ANSI standard fixes, and a
        // theme that reaches one of them has already lost its palette
        Color::White => (1.0, 1.0, 1.0),
        Color::Black => (0.0, 0.0, 0.0),
        Color::Gray => (0.5, 0.5, 0.5),
        Color::DarkGray => (0.25, 0.25, 0.25),
        Color::Red => (1.0, 0.0, 0.0),
        Color::Green => (0.0, 1.0, 0.0),
        Color::Blue => (0.0, 0.0, 1.0),
        other => {
            // every other ratatui colour is an index into a palette this program
            // does not control. treating it as mid grey is the honest answer:
            // it is wrong, and it is wrong loudly rather than silently.
            let _ = other;
            (0.5, 0.5, 0.5)
        }
    }
}

fn linear(channel: f64) -> f64 {
    if channel <= 0.039_28 { channel / 12.92 } else { ((channel + 0.055) / 1.055).powf(2.4) }
}
