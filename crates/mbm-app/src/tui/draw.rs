//! drawing the interface.
//!
//! four views over one list. the whole thing is a grid of cells, which removes
//! most of what a web interface has to decide about and adds a few things it
//! does not:
//!
//! - **there is no canvas.** the program owns the foreground and the cell
//!   background and nothing else, so a frame cannot cast a shadow and a
//!   background cannot bleed. grouping is done with filled bars and with space.
//! - **chrome costs rows.** every row spent on a box is a row of content not
//!   shown, so the views carry no frame at all. the query bar and the status bar
//!   are filled, which is a shape rather than a line, and a shape groups
//!   without eating a row the way a rule does.
//! - **a terminal is 24 rows on a laptop and 60 on a desk.** the column set
//!   changes with the width, and the help changes before the count does.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};

use crate::tui::App;
use crate::tui::motion::{self, Motion};
use crate::tui::theme::Theme;

/// the glyph that marks the selected row.
///
/// a bar rather than a colour change, because a row selected by colour alone
/// is invisible to anyone who cannot tell the two colours apart, and a bar is
/// also the one cue that survives a monochrome terminal.
const BAR: &str = "▎";

/// the widths at which a column appears and disappears.
///
/// they are where a column stops leaving enough room for the title to say
/// anything, not where a device preset says. a list of 500 rows read in a
/// 60-column pane is a normal thing to want.
pub mod breakpoint {
    /// the date, the medium and the author.
    pub const WIDE: u16 = 96;
    /// the date and the author.
    pub const MEDIUM: u16 = 72;
    /// the date alone.
    pub const NARROW: u16 = 52;
}

/// draw one frame.
pub fn draw(frame: &mut Frame<'_>, app: &mut App, theme: &Theme, motion: Motion) {
    let area = frame.area();
    // a terminal can be resized to something too small to hold a row of content
    // and a bar. nothing here has a floor, so the smallest sizes simply clip,
    // and a one-row terminal draws the query bar and nothing else.
    if area.height == 0 {
        return;
    }

    let rows = Layout::default()
        .direction(ratatui::layout::Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    draw_query(frame, rows[0], app, theme);
    match app.view() {
        crate::tui::View::Detail => draw_detail(frame, rows[1], app, theme, motion),
        crate::tui::View::Tags => draw_tags(frame, rows[1], app, theme),
        _ => draw_list(frame, rows[1], app, theme),
    }
    draw_status(frame, rows[2], app, theme, motion);
}

/// the query bar: a filled field with a label, the query, and the ranking.
///
/// a bar rather than a three-row box. one row of text in a box that is three
/// rows tall is two rows of chrome spent on nothing, and the fill is what makes
/// it read as a field.
fn draw_query(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) {
    // the hint names the key that does the thing it describes. the earlier one
    // offered "esc to clear" and escape does not clear: it leaves a view, and in
    // the list there is no view to leave, so it did nothing at all.
    let (label, text, hint) = if let Some(buffer) = app.tag_input.as_deref() {
        ("tag", buffer.to_owned(), "enter to add · esc to cancel")
    } else if app.query.is_empty() {
        ("search", String::new(), "type to search the archive")
    } else {
        ("search", app.query.clone(), "ctrl-k to clear")
    };

    let width = area.width as usize;
    // the label, its trailing space, the ranking, and a margin either side
    let fixed = label.len() + 2 + ranking_label(app).len() + 2;
    let room = width.saturating_sub(fixed + 2);
    let shown = ellipsize_start(&text, room);

    let mut spans = vec![
        Span::styled(format!(" {label} "), Style::default().fg(theme.text_disabled)),
        Span::styled(
            if shown.is_empty() { hint.to_owned() } else { shown },
            Style::default().fg(if text.is_empty() {
                theme.text_disabled
            } else {
                theme.accent_text
            }),
        ),
    ];

    // the ranking sits at the trailing edge as a chip, so the mode is a control
    // in the field rather than a word buried in the status line
    let chip = ranking_label(app);
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    if width > used + chip.len() + 3 {
        let pad = width - used - chip.len() - 2;
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(
            format!(" {chip} "),
            Style::default().fg(theme.accent_text).add_modifier(Modifier::REVERSED),
        ));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.bg_sunken)),
        area,
    );
}

/// the ranking mode, as a person reads it.
fn ranking_label(app: &App) -> &'static str {
    crate::tui::mode_label(app.mode)
}

/// the list, one row per bookmark.
fn draw_list(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) {
    if app.items.is_empty() {
        frame.render_widget(empty_state(app, theme), area);
        return;
    }

    let columns = Columns::for_width(area.width);
    let width = area.width as usize;

    let lines: Vec<Line<'_>> = app
        .items
        .iter()
        .enumerate()
        .map(|(index, (bookmark, _))| {
            let selected = index == app.selected;
            let fill = if selected { theme.bg_selected } else { theme.bg_page };
            let base = Style::default().bg(fill);
            let caption = base.fg(theme.text_secondary);
            let mut cells: Vec<(usize, Span<'static>)> = Vec::new();

            // the selection bar, which is the cue that survives a terminal with
            // no colour at all
            cells.push((
                0,
                Span::styled(
                    if selected { BAR.to_owned() } else { " ".to_owned() },
                    Style::default().fg(theme.accent).bg(fill),
                ),
            ));

            if columns.date {
                cells.push((
                    columns.date_at,
                    Span::styled(
                        format!(
                            " {}  ",
                            mbm_sink::date_only(
                                bookmark.created_at.unwrap_or(bookmark.ingested_at)
                            )
                        ),
                        caption,
                    ),
                ));
            }
            if columns.medium {
                cells.push((
                    columns.medium_at,
                    Span::styled(format!(" {}  ", bookmark.source.medium.name()), caption),
                ));
            }

            // the title takes whatever is left between the last fixed column and
            // the author, and is cut with an ellipsis rather than at the terminal
            // edge. the whole text is one keypress away, so the cut costs
            // nothing.
            if columns.title > 2 {
                let shown = ellipsize_end(&mbm_sink::display_title(bookmark), columns.title);
                let style = if selected {
                    base.fg(theme.text_primary).add_modifier(Modifier::BOLD)
                } else {
                    base.fg(theme.text_primary)
                };
                cells.push((columns.title_at, Span::styled(shown, style)));
            }

            if columns.author
                && let Some(author) = &bookmark.author
            {
                let shown = ellipsize_end(&author.display(), columns.author_width);
                cells.push((columns.author_at, Span::styled(format!(" {shown}"), caption)));
            }

            assemble(cells, width, base)
        })
        .collect();

    let inner = area.height as usize;
    let offset = window_offset(app.selected, app.items.len(), inner);
    let mut paragraph = Paragraph::new(Text::from(lines)).style(Style::default().bg(theme.bg_page));
    paragraph = paragraph.scroll((offset as u16, 0));
    frame.render_widget(paragraph, area);

    // a scrollbar whenever the list is taller than the pane. it is the only
    // visible cue that there is more below, and without it a 500-row archive
    // looks like a five-row one.
    if app.items.len() > inner && inner > 0 {
        let mut state =
            ScrollbarState::new(app.items.len().saturating_sub(inner)).position(app.selected);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .style(Style::default().fg(theme.border_focus).bg(theme.bg_page))
                .track_style(Style::default().fg(theme.border).bg(theme.bg_page))
                .thumb_style(Style::default().fg(theme.accent)),
            area,
            &mut state,
        );
    }
}

/// the gap between two columns, in cells.
const GAP: usize = 2;

/// the longest name any source medium has.
///
/// reserving for the typical one is how a column ends up a character too narrow,
/// and a column a character too narrow does not clip: it shifts.
fn widest_medium_name() -> usize {
    use mbm_core::medium::SourceMedium;
    SourceMedium::ALL.iter().map(|m| m.name().chars().count()).max().unwrap_or(12)
}

/// where a row's columns sit at a given width.
///
/// the offsets are worked out once per frame rather than per row, because a
/// column that starts one cell further along on one row than on the row above
/// it is a column nobody can read down.
///
/// the gap between two columns is two cells, and the gap from the bar to the
/// first column is one. a wider terminal buys the author a column before it
/// buys the medium one, because an author says who wrote it and a medium says
/// where it came from, and the list already says where everything came from.
#[derive(Debug)]
pub struct Columns {
    pub date: bool,
    pub date_at: usize,
    pub medium: bool,
    pub medium_at: usize,
    pub title_at: usize,
    pub title: usize,
    pub author: bool,
    pub author_at: usize,
    pub author_width: usize,
}

impl Columns {
    pub fn for_width(width: u16) -> Self {
        let width = usize::from(width);
        let bar = 1;
        let date_at = bar + 1;
        // the padding is the gap between groups, and the group gap has to be
        // wider than anything inside a group: two cells here, and the date's
        // own digits are the group.
        let date_width = 10 + GAP;
        let medium_at = date_at + date_width;
        // reserved from the longest name any medium has, not from a typical
        // one. a column that is one character too narrow does not truncate, it
        // pushes everything after it along by one.
        let medium_width = widest_medium_name() + GAP;
        let after_fixed = medium_at + medium_width;
        let author_width = 15usize;
        let reserve = |author: bool| if author { author_width } else { 2 };

        let date = width >= usize::from(breakpoint::NARROW);
        let medium = width >= usize::from(breakpoint::WIDE);
        let author = width >= usize::from(breakpoint::MEDIUM);

        let title_at = if date {
            medium_at + if medium { medium_width } else { 0 }
        } else if medium {
            medium_at
        } else {
            bar + 1
        };
        let title_end = width.saturating_sub(reserve(author));
        let title = title_end.saturating_sub(title_at);
        let _ = after_fixed;

        Self {
            date,
            date_at,
            medium,
            medium_at,
            title_at,
            title,
            author,
            author_at: width.saturating_sub(author_width),
            author_width: author_width - 1,
        }
    }
}

/// the first visible row, so a selection near either end stays in view.
///
/// a list of 500 rows has no scrollbar the mouse can use here, so the keyboard
/// is the only way to move and the window has to follow it.
fn window_offset(selected: usize, total: usize, height: usize) -> usize {
    if total <= height {
        return 0;
    }
    selected.saturating_sub(height / 2).min(total - height)
}

/// lay a row out from its column positions, filling the gaps with spaces.
///
/// every row is built from the same offsets, so two rows never disagree about
/// where a column starts, and a gap is a gap rather than a missing cell.
///
/// a cell that would start behind the one before it is placed at the current
/// position with a single space between, rather than dropped. a cell that is
/// dropped is a cell nobody sees, and what disappeared the first time this
/// happened was a title, because a source name one character longer than the
/// column had been reserved for pushed the next column backwards.
fn assemble<'a>(cells: Vec<(usize, Span<'a>)>, width: usize, base: Style) -> Line<'a> {
    let mut out: Vec<Span<'a>> = Vec::with_capacity(cells.len() * 2);
    let mut at = 0usize;
    for (column, span) in cells {
        let wide = span.content.chars().count();
        let start = column.max(at);
        if start > at {
            out.push(Span::styled(" ".repeat(start - at), base));
        } else if out.is_empty() && start == 0 && column == 0 {
            // the first cell owns the leading edge
        } else if start == at {
            out.push(Span::styled(" ", base));
        }
        // a cell that no longer fits is cut at the edge rather than allowed to
        // run past it, because a row longer than the pane wraps under some
        // terminals and shifts every row below it
        let room = width
            .saturating_sub(start + out.iter().map(|s| s.content.chars().count()).sum::<usize>());
        let shown = if wide > room && room > 1 {
            Span::styled(ellipsize_end(&span.content, room), span.style)
        } else {
            span
        };
        let shown_width = shown.content.chars().count();
        out.push(shown);
        at = start + shown_width;
    }
    if at < width {
        out.push(Span::styled(" ".repeat(width - at), base));
    }
    Line::from(out)
}

/// cut a string to `width` and say that it was cut.
fn ellipsize_end(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_owned();
    }
    let keep: String = text.chars().take(width - 1).collect();
    format!("{keep}…")
}

/// elide the middle of a string, keeping both ends.
///
/// for a value with a head and a tail that both matter — a host and a path, a
/// path and a filename — cutting the middle is the only cut that leaves the
/// value recognisable. the two ends get half each.
fn elide_middle(text: &str, width: usize) -> String {
    if text.chars().count() <= width || width < 5 {
        return text.to_owned();
    }
    let head = (width - 1) / 2;
    let tail = width - 1 - head;
    let front: String = text.chars().take(head).collect();
    let back: String = text.chars().skip(text.chars().count() - tail).collect();
    format!("{front}…{back}")
}

/// cut the tail of a string, so the end of a long query stays visible.
///
/// the field scrolls with the cursor rather than jumping, and the mark that
/// something has been cut off is the ellipsis at the leading end.
fn ellipsize_start(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_owned();
    }
    let tail: String = text.chars().skip(count - (width - 1)).collect();
    format!("…{tail}")
}

/// the detail view: one bookmark in full.
fn draw_detail(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme, motion: Motion) {
    let Some(bookmark) = app.selected_item() else {
        frame.render_widget(empty_state(app, theme), area);
        return;
    };

    // a staged entrance. the chunks are the semantic parts of the view and they
    // arrive 100ms apart in reading order, each sliding up from one row. the
    // first is drawn settled, because a view that starts empty reads as broken.
    let progress = app.open_progress(motion);
    let offset = motion::enter_offset(progress);

    // the measure. a terminal cell is narrow, so a 100-column terminal is about
    // a 90-character line, and long-form text wants 60 to 75. the body is capped
    // and the metadata is not, because a date is not a paragraph.
    //
    // 92 rather than 75: a terminal cell is about half the width of a browser
    // character, so 92 cells is nearer 160 browser pixels, and a cap that reads
    // correctly on the web and then throws away a third of a real sentence is a
    // rule transplanted without its unit.
    let measure = (area.width as usize).saturating_sub(2).clamp(20, 92);

    let mut chunks: Vec<Vec<Line<'_>>> = Vec::new();

    // the title
    chunks.push(vec![
        Line::from(Span::styled(
            mbm_sink::display_title(bookmark),
            Style::default().fg(theme.text_primary).add_modifier(Modifier::BOLD),
        )),
        Line::default(),
    ]);

    // the byline: who wrote it, when, and where it came from
    let mut byline: Vec<Span<'_>> = Vec::new();
    if let Some(when) = bookmark.created_at {
        byline.push(Span::styled(
            mbm_sink::date_time(when),
            Style::default().fg(theme.text_secondary),
        ));
    }
    if let Some(author) = &bookmark.author {
        byline.push(Span::raw("  "));
        byline.push(Span::styled(author.display(), Style::default().fg(theme.text_secondary)));
    }
    if let Some(collection) = &bookmark.source.collection {
        byline.push(Span::raw("  "));
        byline.push(Span::styled(collection.clone(), Style::default().fg(theme.text_secondary)));
    }
    let mut head: Vec<Line<'_>> = Vec::new();
    if !byline.is_empty() {
        head.push(Line::from(byline));
        head.push(Line::default());
    }
    if let Some(url) = &bookmark.url {
        // a url is a single token. wrapped, it becomes two fragments that are
        // each wrong, and a person cannot tell where one ends and the next
        // begins. so it is never wrapped: it is elided in the middle instead,
        // which keeps the two parts that identify it, the host and the leaf.
        let full = url.to_string();
        let shown = if full.chars().count() <= measure {
            full.clone()
        } else {
            elide_middle(&full, measure)
        };
        head.push(Line::from(Span::styled(shown, Style::default().fg(theme.accent_text))));
        head.push(Line::default());
    }
    if !head.is_empty() {
        chunks.push(head);
    }

    // the person's own words, and any summary an enrichment stage wrote
    let mut body: Vec<Line<'_>> = Vec::new();
    if let Some(summary) = mbm_sink::summary_of(bookmark) {
        body.push(Line::from(Span::styled(
            summary.to_owned(),
            Style::default().fg(theme.text_primary),
        )));
        body.push(Line::default());
    }
    for line in bookmark.text.lines() {
        body.push(Line::from(Span::styled(
            line.to_owned(),
            Style::default().fg(theme.text_primary),
        )));
    }
    if !body.is_empty() {
        chunks.push(body);
    }

    // the links it holds
    if !bookmark.links.is_empty() {
        let mut links = vec![
            Line::default(),
            Line::from(Span::styled("links", Style::default().fg(theme.text_secondary))),
        ];
        for link in &bookmark.links {
            let mut line = format!("  {}", link.resolved);
            if let Some(reason) = link.blocked {
                // a paywall is a fact about the link and it travels with the
                // link, not in a status line somewhere else
                use std::fmt::Write as _;
                let _ = write!(line, "  [{}]", reason.name());
            }
            links.push(Line::from(Span::styled(line, Style::default().fg(theme.text_secondary))));
        }
        chunks.push(links);
    }

    // what the archive knows about it
    let mut facts: Vec<Line<'_>> = Vec::new();
    if !bookmark.tags.is_empty() {
        facts.push(Line::from(Span::styled(
            format!("tags  {}", bookmark.tags.iter().cloned().collect::<Vec<_>>().join(" ")),
            Style::default().fg(theme.text_secondary),
        )));
    }
    if !bookmark.categories.is_empty() {
        facts.push(Line::from(Span::styled(
            format!(
                "in    {}",
                bookmark.categories.iter().map(|c| c.slug.as_str()).collect::<Vec<_>>().join(" ")
            ),
            Style::default().fg(theme.text_secondary),
        )));
    }
    facts.push(Line::default());
    facts.push(Line::from(Span::styled(
        format!("id    {}", bookmark.id.get()),
        Style::default().fg(theme.text_disabled),
    )));
    chunks.push(facts);

    let arrived = motion::arrived(progress, chunks.len());
    let mut lines: Vec<Line<'_>> = Vec::new();
    for (index, chunk) in chunks.iter().enumerate() {
        if index >= arrived {
            break;
        }
        // each chunk that has arrived gets the same one-row travel, so the
        // stagger is a stagger of the same motion rather than a different one
        let shift = if index + 1 == arrived { offset } else { 0 };
        for _ in 0..shift {
            lines.push(Line::default());
        }
        lines.extend(chunk.iter().cloned());
    }

    let wrapped = Text::from(lines);
    frame.render_widget(
        Paragraph::new(wrapped)
            .wrap(ratatui::widgets::Wrap { trim: true })
            .style(Style::default().bg(theme.bg_page)),
        // the measure applies to the text, and the block keeps the panel from
        // running the full width of a wide terminal
        Rect { width: measure as u16 + 2, ..area },
    );
}

/// the tag list.
fn draw_tags(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) {
    if app.tags.is_empty() {
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(Span::styled(
                    "nothing is tagged yet",
                    Style::default().fg(theme.text_primary).add_modifier(Modifier::BOLD),
                )),
                Line::default(),
                Line::from(Span::styled(
                    "a tag comes from where a bookmark came from, from the links inside it,",
                    Style::default().fg(theme.text_secondary),
                )),
                Line::from(Span::styled(
                    "and from f2 on any item.",
                    Style::default().fg(theme.text_secondary),
                )),
            ]))
            .wrap(ratatui::widgets::Wrap { trim: true })
            .style(Style::default().bg(theme.bg_page)),
            area,
        );
        return;
    }

    // a count is a number, and numbers align on their trailing edge
    let widest = app
        .tags
        .iter()
        .map(|(tag, _)| tag.chars().count())
        .max()
        .unwrap_or(4)
        .clamp(4, (area.width as usize).saturating_sub(10));
    let count_at = widest + 4;

    let lines: Vec<Line<'_>> = app
        .tags
        .iter()
        .enumerate()
        .map(|(index, (tag, count))| {
            let selected = index == app.selected;
            let base =
                if selected { Style::default().bg(theme.bg_selected) } else { Style::default() };
            let shown = ellipsize_end(tag, widest);
            let pad = widest - shown.chars().count();
            let mut out = vec![
                Span::styled(
                    if selected { BAR.to_owned() } else { " ".to_owned() },
                    Style::default().fg(theme.accent).bg(if selected {
                        theme.bg_selected
                    } else {
                        theme.bg_page
                    }),
                ),
                Span::raw(" "),
            ];
            out.push(Span::styled(
                format!("{shown}{} ", " ".repeat(pad)),
                if selected {
                    base.fg(theme.text_primary).add_modifier(Modifier::BOLD)
                } else {
                    base.fg(theme.text_primary)
                },
            ));
            out.push(Span::styled(format!("{count:>4}"), base.fg(theme.text_secondary)));
            Line::from(out)
        })
        .collect();
    let _ = count_at;

    let inner = area.height as usize;
    let offset = window_offset(app.selected, app.items.len().max(app.tags.len()), inner);
    let mut paragraph = Paragraph::new(Text::from(lines)).style(Style::default().bg(theme.bg_page));
    paragraph = paragraph.scroll((offset as u16, 0));
    frame.render_widget(paragraph, area);
}

/// what a list shows when it has nothing to show.
///
/// an empty state says what this place is, how to fill it, and offers one way
/// forward. "no results" alone is a shrug: it names neither the query nor the
/// way out.
fn empty_state(app: &App, theme: &Theme) -> Paragraph<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let query = app.query.trim();
    if query.is_empty() {
        lines.push(Line::from(Span::styled(
            "the archive is empty",
            Style::default().fg(theme.text_primary).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "save a url, or read a file a browser or a feed wrote.",
            Style::default().fg(theme.text_secondary),
        )));
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "mbm add https://example.com",
            Style::default().fg(theme.accent_text),
        )));
        lines.push(Line::from(Span::styled(
            "mbm import ~/Downloads/bookmarks.html",
            Style::default().fg(theme.accent_text),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!("nothing matches “{query}”"),
            Style::default().fg(theme.text_primary).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "ctrl-k clears it and brings the whole archive back.",
            Style::default().fg(theme.text_secondary),
        )));
    }
    Paragraph::new(Text::from(lines))
        .wrap(ratatui::widgets::Wrap { trim: true })
        .style(Style::default().bg(theme.bg_page))
}

/// the status bar: what is here, what just happened, and what the keys do.
///
/// three zones that give way in order of how much they matter. the help goes
/// first because it is the only one nobody is looking for, then the count, and
/// the status is the last thing to go because it is the only one that is
/// telling the reader something they did not already know.
fn draw_status(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme, motion: Motion) {
    let width = area.width as usize;
    let status = app.status.to_string();
    // a status that has just changed is brighter for a moment and settles. the
    // text is there the whole time, so the brightening is a second channel and
    // never the only one.
    let fresh = app.status_progress(motion);
    let status_color = if status.contains("could not") {
        theme.danger
    } else if status.contains("removed") {
        theme.warning
    } else {
        motion::fade(theme.accent_text, theme.text_secondary, fresh)
    };

    let count = if app.items.len() == app.total() {
        format!("{} bookmarks", app.items.len())
    } else {
        format!("{} of {}", app.items.len(), app.total())
    };

    let base = Style::default().bg(theme.bg_surface);

    // the three zones are laid out against what is left, in the order of how
    // much each matters, and each one is *shortened* rather than dropped: a
    // help line that vanishes at 100 columns is a help line that only exists on
    // a monitor nobody has.
    // the three zones are laid out against what is left, in the order of how
    // much each matters, and each is *shortened* rather than dropped: a help
    // line that vanishes at 100 columns is a help line that only exists on a
    // monitor nobody has.
    let left = width.saturating_sub(4);
    // the help is measured first, because it is the zone that has to give, and
    // measuring it first is what lets it give by a character rather than by
    // vanishing
    let status_room = left.min(left.saturating_sub(24));
    let shown = ellipsize_end(&status, status_room);
    let count_room = left.saturating_sub(shown.chars().count() + 3);
    let count = ellipsize_end(&count, count_room);
    let help_room = left.saturating_sub(shown.chars().count() + count.chars().count() + 4);
    let help = help_text(help_room);

    // the status on the leading edge, the count beside it, the help on the
    // trailing edge, and whatever is left between them. the help is anchored to
    // the trailing edge so it does not move as the count changes length.
    let left_used = 1 + shown.chars().count() + 1 + count.chars().count() + 1;
    let right_used = help.chars().count() + 1;
    let gap = width.saturating_sub(left_used + right_used);

    let spans = vec![
        Span::styled(format!(" {shown} "), base.fg(status_color)),
        Span::styled(count.clone(), base.fg(theme.text_secondary)),
        Span::styled(" ".repeat(gap), base),
        Span::styled(format!("{help} "), base.fg(theme.text_disabled)),
    ];

    frame.render_widget(Paragraph::new(Line::from(spans)).style(base), area);
}

/// the key help, as much of it as the width allows.
///
/// the keys are the ones the handler actually implements. a help line that
/// advertises a key which does something else is worse than no help line, and
/// the previous one did: it offered ctrl-u for undo while the handler used it
/// to clear the query.
///
/// the list is ordered by how often each is wanted, so the ones that survive a
/// narrow bar are the ones a person is about to press.
fn help_text(width: usize) -> String {
    const ALL: &[(&str, &str)] = &[
        ("↑↓", "move"),
        ("⏎", "open"),
        ("esc", "back"),
        ("tab", "rank"),
        ("f2", "tag"),
        ("^d", "remove"),
        ("^u", "undo"),
        ("^g", "tags"),
        ("^k", "clear"),
        ("^r", "reload"),
        ("^c", "quit"),
    ];

    let mut out: Vec<String> = Vec::new();
    let mut used = 0usize;
    for (index, (key, what)) in ALL.iter().enumerate() {
        let piece = if index == 0 { format!("{key} {what}") } else { format!(" · {key} {what}") };
        if used + piece.chars().count() <= width {
            used += piece.chars().count();
            out.push(piece);
        } else if used + 3 + key.chars().count() + 2 <= width {
            // there is room for one more key but not for what it does, so the
            // key is kept and the rest is elided. a bar ending in a bare
            // separator reads as a rendering fault rather than as a narrow
            // terminal, so the separator keeps its trailing space.
            if index > 0 {
                out.push(" · ".to_owned());
            }
            out.push(format!("{key} …"));
            break;
        } else {
            break;
        }
    }
    out.join("")
}
