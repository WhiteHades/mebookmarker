//! print the measured contrast of every pair the interface draws.
//!
//! a theme is a set of numbers, and a number nobody measured is a guess. this
//! prints the pairs against the requirement each one carries, so a value that
//! drifts fails here rather than on someone's screen.

use mbm_app::tui::theme::{Appearance, Color, TextSize, Theme, Weight, lightness_contrast, ratio};

/// one pair, the requirement it has to clear, and why it exists.
struct Pair {
    name: &'static str,
    foreground: Color,
    background: Color,
    /// the WCAG 2 ratio it has to clear, which is the number a conformance
    /// claim rests on.
    wcag: f64,
    /// the APCA lightness contrast it has to clear, which is the number the
    /// palette was designed against.
    apca: f64,
    /// the size the text renders at, which APCA weighs.
    size: TextSize,
    /// whether the text is bold, which APCA weighs.
    weight: Weight,
}

fn main() {
    let mut failing = 0usize;
    for appearance in [Appearance::Dark, Appearance::Light] {
        let t = Theme::for_appearance(appearance);
        println!("\n=== {appearance:?} ===");
        for pair in pairs(&t) {
            // a token can be drawn at more than one size and APCA weighs size,
            // so the worst of the two is the number that has to clear
            let wcag = ratio(pair.foreground, pair.background);
            let at = |size| {
                lightness_contrast(pair.foreground, pair.background, size, pair.weight).abs()
            };
            let apca = at(TextSize::Body).min(at(TextSize::Caption));
            let _ = pair.size;
            let ok = wcag >= pair.wcag && apca >= pair.apca;
            if !ok {
                failing += 1;
            }
            println!(
                "{:>4}  wcag {wcag:>5.2} (needs {:.1})  apca {apca:>5.1} (needs {:.1})  {}",
                if ok { "ok" } else { "FAIL" },
                pair.wcag,
                pair.apca,
                pair.name
            );
        }
    }
    if failing > 0 {
        eprintln!("\n{failing} pairs below their requirement");
        std::process::exit(1);
    }
}

fn pairs(t: &Theme) -> Vec<Pair> {
    let body = TextSize::Body;
    let caption = TextSize::Caption;
    let regular = Weight::Regular;
    let bold = Weight::Bold;
    vec![
        Pair {
            name: "title on page",
            foreground: t.text_primary,
            background: t.bg_page,
            wcag: 4.5,
            apca: 75.0,
            size: body,
            weight: bold,
        },
        Pair {
            name: "body on page",
            foreground: t.text_primary,
            background: t.bg_page,
            wcag: 4.5,
            apca: 75.0,
            size: body,
            weight: regular,
        },
        Pair {
            name: "caption on page",
            foreground: t.text_secondary,
            background: t.bg_page,
            wcag: 4.5,
            apca: 60.0,
            size: caption,
            weight: regular,
        },
        Pair {
            name: "caption on surface",
            foreground: t.text_secondary,
            background: t.bg_surface,
            wcag: 4.5,
            apca: 60.0,
            size: caption,
            weight: regular,
        },
        Pair {
            name: "caption on selected",
            foreground: t.text_secondary,
            background: t.bg_selected,
            wcag: 4.5,
            apca: 60.0,
            size: caption,
            weight: regular,
        },
        Pair {
            name: "title on selected",
            foreground: t.text_primary,
            background: t.bg_selected,
            wcag: 4.5,
            apca: 75.0,
            size: body,
            weight: bold,
        },
        Pair {
            name: "title on sunken",
            foreground: t.text_primary,
            background: t.bg_sunken,
            wcag: 4.5,
            apca: 75.0,
            size: body,
            weight: regular,
        },
        Pair {
            name: "query text on sunken",
            foreground: t.accent_text,
            background: t.bg_sunken,
            wcag: 4.5,
            apca: 60.0,
            size: body,
            weight: regular,
        },
        Pair {
            name: "placeholder on sunken",
            foreground: t.text_disabled,
            background: t.bg_sunken,
            wcag: 3.0,
            apca: 30.0,
            size: body,
            weight: regular,
        },
        // a disabled control is still text, and APCA's floor for one is Lc 30
        Pair {
            name: "disabled on page",
            foreground: t.text_disabled,
            background: t.bg_page,
            wcag: 3.0,
            apca: 30.0,
            size: caption,
            weight: regular,
        },
        // the selection bar: a non-text element, so 3:1 and Lc 30
        Pair {
            name: "selection bar on selected",
            foreground: t.accent,
            background: t.bg_selected,
            wcag: 3.0,
            apca: 45.0,
            size: caption,
            weight: bold,
        },
        Pair {
            name: "focused frame on page",
            foreground: t.border_focus,
            background: t.bg_page,
            wcag: 3.0,
            apca: 40.0,
            size: caption,
            weight: regular,
        },
        Pair {
            name: "focused frame on sunken",
            foreground: t.border_focus,
            background: t.bg_sunken,
            wcag: 3.0,
            apca: 40.0,
            size: caption,
            weight: regular,
        },
        // a resting frame is decoration, so it is held to the floor for a
        // non-text element to be discernible at all rather than to 3:1
        Pair {
            name: "resting frame on page",
            foreground: t.border,
            background: t.bg_page,
            wcag: 1.2,
            apca: 20.0,
            size: caption,
            weight: regular,
        },
        Pair {
            name: "resting frame on surface",
            foreground: t.border,
            background: t.bg_surface,
            wcag: 1.2,
            apca: 20.0,
            size: caption,
            weight: regular,
        },
        Pair {
            name: "danger on page",
            foreground: t.danger,
            background: t.bg_page,
            wcag: 4.5,
            apca: 60.0,
            size: caption,
            weight: regular,
        },
        Pair {
            name: "warning on page",
            foreground: t.warning,
            background: t.bg_page,
            wcag: 4.5,
            apca: 60.0,
            size: caption,
            weight: regular,
        },
        Pair {
            name: "danger on selected",
            foreground: t.danger,
            background: t.bg_selected,
            wcag: 4.5,
            apca: 60.0,
            size: caption,
            weight: regular,
        },
    ]
}
