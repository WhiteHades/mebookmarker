//! render the interface and write out what it drew.
//!
//! a terminal interface is only reviewable by looking at it, and looking at it
//! through a text capture throws away the half that matters: the colour. this
//! writes two artifacts from one render — the frame as text, and the same frame
//! as a small html page carrying the exact foreground and background of every
//! cell — so the palette, the columns and the alignment can all be checked from
//! outside the process.
//!
//! ```sh
//! cargo run -p mbm-app --example frame -- <store> 100x30 /tmp/frame
//! ```
//!
//! the store is a `mebookmarker.db`; the appearance is `dark` or `light`, and
//! the view is `browse`, `detail` or `tags`.

use std::fmt::Write as _;
use std::path::PathBuf;

use mbm_app::tui::App;
use mbm_app::tui::motion::Motion;
use mbm_app::tui::theme::{Appearance, Theme};
use ratatui::Terminal;
use ratatui::buffer::Buffer;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let store = PathBuf::from(args.next().unwrap_or_else(|| ".".to_owned()));
    let size = args.next().unwrap_or_else(|| "100x30".to_owned());
    let out = PathBuf::from(args.next().unwrap_or_else(|| "/tmp/frame".to_owned()));
    let appearance = args.next().unwrap_or_else(|| "dark".to_owned());
    let view = args.next().unwrap_or_else(|| "browse".to_owned());

    let (width, height): (u16, u16) = size
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .ok_or("the size is WIDTHxHEIGHT")?;

    let conn = mbm_store::open(&store.join("mebookmarker.db"))?;
    let mut app = App::new(std::sync::Arc::new(std::sync::Mutex::new(conn)), "");
    if view == "detail" {
        app.act(mbm_app::tui::Action::Open);
    }
    if view == "tags" {
        app.act(mbm_app::tui::Action::Tags);
    }

    let theme = Theme::for_appearance(match appearance.as_str() {
        "light" => Appearance::Light,
        _ => Appearance::Dark,
    });

    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend)?;
    app.set_rows(usize::from(height).saturating_sub(2));
    terminal.draw(|frame| mbm_app::tui::draw::draw(frame, &mut app, &theme, Motion::none()))?;
    let buffer = terminal.backend().buffer().clone();

    std::fs::create_dir_all(&out).ok();
    let text = as_text(&buffer, width);
    std::fs::write(out.join("frame.txt"), &text)?;
    std::fs::write(out.join("frame.html"), as_html(&buffer, width, height, &theme))?;

    print!("{text}");
    println!("\nwrote {}/frame.txt and {}/frame.html", out.display(), out.display());
    Ok(())
}

/// the frame as plain text, which is what a person reads in a terminal.
fn as_text(buffer: &Buffer, width: u16) -> String {
    let cols = usize::from(width);
    buffer
        .content()
        .chunks(cols)
        .map(|row| row.iter().map(ratatui::buffer::Cell::symbol).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// the frame as html, carrying every cell's colours.
///
/// one `<span>` per run of cells that share a style, so the page is small enough
/// to open and the colours are the ones the buffer holds rather than an
/// approximation of them.
fn as_html(buffer: &Buffer, width: u16, height: u16, theme: &Theme) -> String {
    let page = if theme.appearance.is_light() { "#fcfdff" } else { "#0d1117" };
    let mut out = String::with_capacity(16 * 1024);
    out.push_str("<!doctype html><meta charset=\"utf-8\"><title>mebookmarker frame</title>");
    out.push_str("<style>");
    out.push_str("body{margin:0;background:#111;padding:24px}");
    out.push_str("pre{margin:0;color:#ccc;");
    out.push_str("font:14px/1.25 ui-monospace,SFMono-Regular,Menlo,monospace;");
    out.push_str("padding:16px;border-radius:8px;display:inline-block;white-space:pre}");
    let _ = writeln!(out, "pre{{background:{page}}}");
    out.push_str("</style><pre>");

    let cols = usize::from(width);
    for (row, chunk) in buffer.content().chunks(cols).enumerate() {
        // a run of cells sharing a style is one span, so the page stays small
        let mut run: Vec<&ratatui::buffer::Cell> = Vec::new();
        let mut run_style = None;
        for cell in chunk {
            let style = (cell.fg, cell.bg, cell.modifier);
            if Some(style) != run_style {
                flush(&mut out, &mut run, run_style, page);
                run.clear();
                run_style = Some(style);
            }
            run.push(cell);
        }
        flush(&mut out, &mut run, run_style, page);
        if row + 1 < usize::from(height) {
            out.push('\n');
        }
    }
    out.push_str("</pre>");
    out
}

/// write one run of cells as a span.
fn flush(
    out: &mut String,
    run: &mut Vec<&ratatui::buffer::Cell>,
    style: Option<(ratatui::style::Color, ratatui::style::Color, ratatui::style::Modifier)>,
    page: &str,
) {
    if run.is_empty() {
        return;
    }
    let Some((fg, bg, modifier)) = style else { return };
    let mut style_bits: Vec<String> = Vec::new();
    if fg != ratatui::style::Color::Reset {
        style_bits.push(format!("color:{}", hex(fg)));
    }
    if bg != ratatui::style::Color::Reset {
        style_bits.push(format!("background:{}", hex(bg)));
    }
    if modifier.contains(ratatui::style::Modifier::BOLD) {
        style_bits.push("font-weight:700".to_owned());
    }
    if modifier.contains(ratatui::style::Modifier::REVERSED) {
        style_bits.push("filter:invert(1)".to_owned());
    }
    if modifier.contains(ratatui::style::Modifier::DIM) {
        style_bits.push("opacity:.6".to_owned());
    }
    if modifier.contains(ratatui::style::Modifier::ITALIC) {
        style_bits.push("font-style:italic".to_owned());
    }
    let text: String = run.iter().map(|c| c.symbol()).collect();
    let escaped = text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    if style_bits.is_empty() {
        out.push_str(&escaped);
    } else {
        let _ = write!(out, "<span style=\"{}\">{escaped}</span>", style_bits.join(";"));
    }
    let _ = page;
}

/// a terminal colour as a css colour, with the page fill behind a `Reset`.
///
/// a `Reset` cell is whatever the terminal painted, which in the html has to be
/// the page's own background or the run reads as a hole.
fn hex(color: ratatui::style::Color) -> String {
    use ratatui::style::Color;
    match color {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Black => "#000000".to_owned(),
        Color::Red => "#cd3131".to_owned(),
        Color::Green => "#0dbc79".to_owned(),
        Color::Yellow => "#e5e510".to_owned(),
        Color::Blue => "#2472c8".to_owned(),
        Color::Magenta => "#bc3fbc".to_owned(),
        Color::Cyan => "#11a8cd".to_owned(),
        Color::Gray => "#cccccc".to_owned(),
        Color::DarkGray => "#767676".to_owned(),
        Color::LightRed => "#f14c4c".to_owned(),
        Color::LightGreen => "#23d18b".to_owned(),
        Color::LightYellow => "#f5f543".to_owned(),
        Color::LightBlue => "#3b8eea".to_owned(),
        Color::LightMagenta => "#d670d6".to_owned(),
        Color::LightCyan => "#29b8db".to_owned(),
        Color::White => "#ffffff".to_owned(),
        Color::Reset => "inherit".to_owned(),
        // a 256-colour index is a terminal's own palette entry, and the sixteen
        // above are the anchors of it. the browser has no such table, so the
        // nearest anchor is the honest rendering.
        Color::Indexed(index) => {
            let table = [
                "#000000", "#cd3131", "#0dbc79", "#e5e510", "#2472c8", "#bc3fbc", "#11a8cd",
                "#cccccc", "#767676", "#f14c4c", "#23d18b", "#f5f543", "#3b8eea", "#d670d6",
                "#29b8db", "#ffffff",
            ];
            let base = usize::from(index).min(table.len() - 1);
            let scale = 1.0 + f64::from(index) / 32.0;
            let hex_digits = table[base].trim_start_matches('#');
            let mut channels = [0u8; 3];
            for (slot, part) in channels.iter_mut().zip(hex_digits.as_bytes().chunks(2)) {
                let value =
                    u8::from_str_radix(std::str::from_utf8(part).unwrap_or("00"), 16).unwrap_or(0);
                *slot = (f64::from(value) * scale).round().clamp(0.0, 255.0) as u8;
            }
            format!("#{:02x}{:02x}{:02x}", channels[0], channels[1], channels[2])
        }
    }
}
