//! solve for each theme value against the hardest background it can land on.
//!
//! contrast responds to lightness and barely at all to hue, so the fix is a
//! walk along lightness with hue and chroma held. this prints where the walk
//! stops and the theme takes that value.
//!
//! two rules the walk has to respect:
//!
//! - a token has **one** value, so it is solved against the **hardest**
//!   background it can appear on rather than each one in turn. solving each in
//!   turn produces three values, and a token cannot hold three.
//! - the walking direction follows the appearance: on a dark page every
//!   foreground moves up in lightness, on a light page every one moves down.

use mbm_app::tui::theme::{
    Appearance, Color, TextSize, Theme, Weight, lightness_contrast, ratio, step,
};

/// what a role has to clear.
#[derive(Clone, Copy)]
struct Need {
    wcag: f64,
    apca: f64,
    size: TextSize,
    weight: Weight,
}

/// the floor a colour has to clear against `against`, out of both metrics.
///
/// out of two numbers is the conservative one: a value that clears the stricter
/// of the two clears whichever one the reader's tooling happens to use.
///
/// APCA weighs a text's size, so a value used at two sizes has to clear at both
/// and the smaller one is not automatically the harder case. the query field's
/// placeholder and the list's caption share a token, and they are 13pt and
/// 16pt, so both are checked.
fn clears(foreground: Color, background: Color, need: Need) -> (f64, f64, bool) {
    let w = ratio(foreground, background);
    let at = |size| lightness_contrast(foreground, background, size, need.weight).abs();
    let worst = at(TextSize::Body).min(at(TextSize::Caption)).min(at(TextSize::Title));
    let ok = w >= need.wcag && worst >= need.apca;
    (w, worst, ok)
}

/// walk lightness from `from` toward `to` and take the first that clears every
/// background, holding hue and chroma.
fn solve(
    backgrounds: &[(&str, Color)],
    need: Need,
    hue: f64,
    chroma: f64,
    from: f64,
    to: f64,
) -> Option<(f64, (u8, u8, u8))> {
    for i in 0..=400 {
        let l = from + (to - from) * (i as f64 / 400.0);
        let (r, g, b) = step(l, chroma, hue);
        let candidate = Color::Rgb(r, g, b);
        if backgrounds.iter().all(|(_, bg)| clears(candidate, *bg, need).2) {
            return Some((l, (r, g, b)));
        }
    }
    None
}

fn triple(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (0, 0, 0),
    }
}

fn show(
    label: &str,
    found: Option<(f64, (u8, u8, u8))>,
    backgrounds: &[(&str, Color)],
    need: Need,
) {
    match found {
        Some((l, (r, g, b))) => {
            let detail = backgrounds
                .iter()
                .map(|(name, bg)| {
                    let (w, a, _) = clears(Color::Rgb(r, g, b), *bg, need);
                    format!("{name} {w:.2}:1 Lc{a:.0}")
                })
                .collect::<Vec<_>>()
                .join("  ");
            println!("  {label:<24} L={l:.3}  #{r:02x}{g:02x}{b:02x}   {detail}");
        }
        None => println!("  {label:<24} no value clears every background"),
    }
}

/// the hue each ramp is held at, in oklab degrees.
///
/// the neutral is a trace of the accent rather than a pure grey, so the greys
/// and the accent sit in one family instead of merely coexisting. enough to
/// measure, not enough to name.
const NEUTRAL_HUE: f64 = 250.0;
const ACCENT_HUE: f64 = 255.0;
const DANGER_HUE: f64 = 27.0;
const WARNING_HUE: f64 = 75.0;
const SUCCESS_HUE: f64 = 145.0;

fn main() {
    for appearance in [Appearance::Dark, Appearance::Light] {
        let t = Theme::for_appearance(appearance);
        let (pr, pg, pb) = triple(t.bg_page);
        println!("\n=== {appearance:?} ===  page #{pr:02x}{pg:02x}{pb:02x}");

        let (a, b) =
            if appearance == Appearance::Dark { (0.30_f64, 0.99_f64) } else { (0.99, 0.30) };

        let caption =
            Need { wcag: 4.5, apca: 60.0, size: TextSize::Caption, weight: Weight::Regular };
        let body = Need { wcag: 4.5, apca: 60.0, size: TextSize::Body, weight: Weight::Regular };
        let disabled =
            Need { wcag: 3.0, apca: 30.0, size: TextSize::Caption, weight: Weight::Regular };
        let bar = Need { wcag: 3.0, apca: 45.0, size: TextSize::Caption, weight: Weight::Bold };
        let focus =
            Need { wcag: 3.0, apca: 40.0, size: TextSize::Caption, weight: Weight::Regular };
        // the resting frame is held a little above the bare discernibility
        // floor. the floor is where a line stops being invisible at all, and a
        // frame drawn there looks like a mistake rather than a boundary, so the
        // walk is given a margin and the result is quiet without being absent.
        let frame =
            Need { wcag: 1.2, apca: 20.0, size: TextSize::Caption, weight: Weight::Regular };

        // a caption can sit on the page, on a raised panel, or on the selected
        // row, and the selected row is whichever of the three is furthest from
        // a light caption. one value, so it has to clear the worst of them.
        let three = [("page", t.bg_page), ("surface", t.bg_surface), ("selected", t.bg_selected)];
        show("text_secondary", solve(&three, caption, NEUTRAL_HUE, 0.012, a, b), &three, caption);

        // disabled text is the one value that reaches the query field as well as
        // the list, because the field's placeholder is a disabled label. on a
        // dark page the sunken step is the darkest of the four, so it is the
        // one that decides.
        let four = [
            ("page", t.bg_page),
            ("surface", t.bg_surface),
            ("selected", t.bg_selected),
            ("sunken", t.bg_sunken),
        ];
        show("text_disabled", solve(&four, disabled, NEUTRAL_HUE, 0.008, a, b), &four, disabled);

        // the query field is the one place the accent is a text colour rather
        // than a mark, and the text in it has to clear the sunken fill
        show(
            "accent_text",
            solve(
                &[("sunken", t.bg_sunken), ("selected", t.bg_selected)],
                body,
                ACCENT_HUE,
                0.11,
                a,
                b,
            ),
            &[("sunken", t.bg_sunken), ("selected", t.bg_selected)],
            body,
        );

        // the selection bar is a solid mark on the selected row
        show(
            "accent",
            solve(&[("selected", t.bg_selected)], bar, ACCENT_HUE, 0.11, a, b),
            &[("selected", t.bg_selected)],
            bar,
        );

        // a focused frame is an indicator, and it crosses whichever fill it is
        // drawn on: the page, a raised panel, or the query field
        show(
            "border_focus",
            solve(
                &[("page", t.bg_page), ("sunken", t.bg_sunken), ("selected", t.bg_selected)],
                focus,
                NEUTRAL_HUE,
                0.02,
                a,
                b,
            ),
            &[("page", t.bg_page), ("sunken", t.bg_sunken), ("selected", t.bg_selected)],
            focus,
        );

        // a resting frame is structure rather than decoration, held to the
        // floor at which a non-text element is discernible at all
        show(
            "border",
            solve(
                &[("page", t.bg_page), ("surface", t.bg_surface)],
                frame,
                NEUTRAL_HUE,
                0.012,
                a,
                b,
            ),
            &[("page", t.bg_page), ("surface", t.bg_surface)],
            frame,
        );

        for (name, hue, chroma) in [
            ("danger", DANGER_HUE, 0.14),
            ("warning", WARNING_HUE, 0.12),
            ("success", SUCCESS_HUE, 0.11),
        ] {
            show(name, solve(&three, caption, hue, chroma, a, b), &three, caption);
        }
    }
}
