//! the terminal interface.
//!
//! four views over one list, and the whole thing is keyboard-driven because
//! that is how a terminal works:
//!
//! ```text
//!   browse     everything, newest first
//!   search     the same list, filtered as you type
//!   detail     one bookmark in full
//!   tags       what the archive is tagged with
//! ```
//!
//! the design rules this file follows:
//!
//! - **the query is the interface.** every list view has a search box, and the
//!   search runs on every keystroke. a laggy list is worse than a narrow one.
//! - **the ranking toggle is visible.** hybrid by default, exact and fuzzy on
//!   demand, because a half-typed word wants fuzzy and a finished one does not.
//! - **nothing is destructive without a key.** tagging and deleting both say
//!   what they are about to do and undo is `u`.
//! - **the status line is always there.** what is in the archive, what the
//!   current view is, and what the last action did.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Instant;

use mbm_core::bookmark::Bookmark;
use mbm_core::error::{Error, Result};
use mbm_store::{Filter, Mode, Repo};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use rusqlite::Connection;

/// which view is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// the archive, newest first.
    Browse,
    /// the archive, filtered.
    Search,
    /// one bookmark in full.
    Detail,
    /// what the archive is tagged with.
    Tags,
}

impl View {
    /// the label the status line shows.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Browse => "browse",
            Self::Search => "search",
            Self::Detail => "detail",
            Self::Tags => "tags",
        }
    }
}

/// a key the interface understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// a printable character.
    Type(char),
    /// the backspace key.
    Backspace,
    /// move the selection up.
    Up,
    /// move the selection down.
    Down,
    /// move the selection to the top.
    Home,
    /// move the selection to the bottom.
    End,
    /// open the selected item.
    Open,
    /// leave the current view.
    Back,
    /// show the tag list.
    Tags,
    /// clear the query.
    Clear,
    /// change the ranking mode.
    Rank,
    /// add a tag to the selected item.
    Tag,
    /// remove the selected item.
    Delete,
    /// put back the last thing removed.
    Undo,
    /// leave the program.
    Quit,
    /// redraw.
    Refresh,
    /// a key the interface ignores.
    None,
}

/// read one key event into an action.
///
/// the raw key first, because `KeyCode::Char` with a control modifier is a
/// command rather than a character, and reading it as text would put a `c` in
/// the search box every time someone pressed ctrl-c.
#[must_use]
pub fn action_for(event: &crossterm::event::KeyEvent) -> Action {
    use crossterm::event::{KeyCode, KeyModifiers};

    // every unmodified key is a character for the search box, so the commands
    // live on ctrl, where there are twenty-six of them and one is in use
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        return match event.code {
            KeyCode::Char('c') => Action::Quit,
            KeyCode::Char('u') => Action::Clear,
            KeyCode::Char('r') => Action::Refresh,
            KeyCode::Char('z') => Action::Undo,
            KeyCode::Char('g') => Action::Tags,
            KeyCode::Char('d') => Action::Delete,
            _ => Action::None,
        };
    }
    match event.code {
        KeyCode::Char(c) => Action::Type(c),
        KeyCode::Backspace => Action::Backspace,
        KeyCode::Up => Action::Up,
        KeyCode::Down => Action::Down,
        KeyCode::Home => Action::Home,
        KeyCode::End => Action::End,
        KeyCode::Enter => Action::Open,
        KeyCode::Esc => Action::Back,
        KeyCode::Tab => Action::Rank,
        KeyCode::Delete => Action::Delete,
        KeyCode::F(2) => Action::Tag,
        _ => Action::None,
    }
}

/// what the interface is showing.
pub struct App {
    conn: Arc<std::sync::Mutex<Connection>>,
    view: View,
    /// the search text.
    pub query: String,
    /// the items in the list, with their relevance scores.
    pub items: Vec<(Bookmark, f64)>,
    /// which item is selected.
    pub selected: usize,
    /// how the results are ranked.
    pub mode: Mode,
    /// the tags in the archive, with their counts.
    pub tags: Vec<(String, usize)>,
    /// the tag being typed, for the tag prompt.
    pub tag_input: Option<String>,
    /// what the last action did. a short borrowed message costs nothing to
    /// set, and the long ones are the only ones that allocate.
    pub status: Cow<'static, str>,
    /// the last thing removed, for undo.
    undo: Option<Bookmark>,
    /// when the list was last rebuilt, so a held key does not thrash the index.
    last_query: Option<Instant>,
    /// the query the list on screen was built from.
    ///
    /// the debounce is a timer and not a gate, and a timer needs to know what it
    /// is timing towards: a burst of typing ends with a rebuild even though the
    /// window has not passed, otherwise the last few characters a person typed
    /// are never searched at all.
    loaded: String,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // the store is a handle and the item list is a screen's worth of
        // bookmarks; neither belongs in a log line
        f.debug_struct("App")
            .field("view", &self.view)
            .field("query", &self.query)
            .field("items", &self.items.len())
            .field("selected", &self.selected)
            .field("mode", &self.mode)
            .field("tags", &self.tags.len())
            .field("status", &self.status)
            .field("tag_input", &self.tag_input)
            .field("undo", &self.undo.as_ref().map(|b| b.id.get()))
            .field("last_query", &self.last_query)
            .finish_non_exhaustive()
    }
}

impl App {
    /// build the interface over a store.
    pub fn new(conn: Arc<std::sync::Mutex<Connection>>, query: &str) -> Self {
        let mut app = Self {
            conn,
            view: View::Browse,
            query: query.to_owned(),
            items: Vec::new(),
            selected: 0,
            mode: Mode::Hybrid,
            tags: Vec::new(),
            tag_input: None,
            status: Cow::Borrowed("ready"),
            undo: None,
            // the first keystroke rebuilds immediately rather than waiting out
            // the debounce window over an empty list
            last_query: None,
            loaded: String::new(),
        };
        app.reload();
        app
    }

    /// the view showing.
    #[must_use]
    pub fn view(&self) -> View {
        self.view
    }

    /// how many items the list holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// whether the list is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// the label the list frame shows.
    ///
    /// a filtered list is a search however it got there, so the label follows
    /// the query rather than the enum.
    #[must_use]
    pub fn list_label(&self) -> String {
        if self.view == View::Tags {
            return self.view.label().to_owned();
        }
        if self.query.trim().is_empty() {
            return self.view.label().to_owned();
        }
        format!("search · {}", mode_label(self.mode))
    }

    /// the selected item.
    #[must_use]
    pub fn selected_item(&self) -> Option<&Bookmark> {
        self.items.get(self.selected).map(|(b, _)| b)
    }

    /// rebuild the list from the query.
    pub fn reload(&mut self) {
        let conn = self.conn.lock().expect("the store lock is held for one frame");
        let limit = 500usize;

        self.items = if self.query.trim().is_empty() {
            // `list` reads the row and nothing else, which is right for a
            // count and wrong for a screen: a list row shows the author and the
            // tags, and a detail view shows the links
            let repo = Repo::new(&conn);
            repo.list(limit, 0)
                .map(|mut items| {
                    for item in &mut items {
                        let _ = repo.load_satellites(item);
                    }
                    items.into_iter().map(|b| (b, 0.0)).collect()
                })
                .unwrap_or_default()
        } else {
            // the same entry point the command line uses. a second search path
            // here is how the interface ends up ranking differently from the
            // terminal it is standing in for, and this one had no prefilter, so
            // its fuzzy half was answering nothing at all.
            crate::pipeline::search_mode(&conn, &self.query, self.mode, limit).unwrap_or_default()
        };

        self.tags = Repo::new(&conn).tags_with_counts(200).unwrap_or_default();
        self.selected = self.selected.min(self.items.len().saturating_sub(1));
        self.loaded.clone_from(&self.query);
        self.last_query = Some(Instant::now());
    }

    /// run one action.
    pub fn act(&mut self, action: Action) -> bool {
        // a tag prompt takes every key until it is done
        if self.tag_input.is_some() {
            self.tag_prompt(action);
            return true;
        }

        match action {
            Action::Quit => return false,
            Action::Type(c) => {
                self.query.push(c);
                self.selected = 0;
                self.debounced_reload();
            }
            Action::Backspace => {
                self.query.pop();
                self.selected = 0;
                self.debounced_reload();
            }
            Action::Clear => {
                self.query.clear();
                self.selected = 0;
                self.status = Cow::Borrowed("query cleared");
                self.view = View::Browse;
                self.reload();
            }
            Action::Up => self.selected = self.selected.saturating_sub(1),
            Action::Down => {
                if self.selected + 1 < self.items.len() {
                    self.selected += 1;
                }
            }
            Action::Home => self.selected = 0,
            Action::End => self.selected = self.items.len().saturating_sub(1),
            Action::Open => {
                if self.selected_item().is_some() {
                    self.view = View::Detail;
                }
            }
            // every view but the two overlays goes back to the list
            Action::Back => self.view = View::Browse,
            Action::Tags => {
                self.view = if self.view == View::Tags { View::Browse } else { View::Tags };
            }
            Action::Rank => {
                self.mode = match self.mode {
                    Mode::Hybrid => Mode::Exact,
                    Mode::Exact => Mode::Fuzzy,
                    Mode::Fuzzy => Mode::Hybrid,
                };
                self.status = Cow::Owned(format!("ranking: {}", mode_label(self.mode)));
                self.reload();
            }
            Action::Tag => {
                if self.selected_item().is_some() {
                    self.tag_input = Some(String::new());
                    self.status = Cow::Borrowed("tag: type a tag, enter to save, esc to cancel");
                }
            }
            Action::Delete => {
                if let Some(bookmark) = self.selected_item().cloned() {
                    self.remove(&bookmark);
                }
            }
            Action::Undo => self.put_back(),
            Action::Refresh => {
                self.status = Cow::Borrowed("reloaded");
                self.reload();
            }
            // a key the interface does not use
            Action::None => {}
        }
        true
    }

    /// rebuild the list at most every 120 milliseconds.
    ///
    /// a search on every keystroke over a large archive is measurable, and the
    /// difference between a reload per key and a reload every eighth of a second
    /// is the difference between typing and waiting.
    fn debounced_reload(&mut self) {
        if self.stale() {
            self.reload();
        }
    }

    /// whether enough time has passed for another rebuild.
    fn stale(&self) -> bool {
        if self.loaded == self.query {
            return false;
        }
        self.last_query.is_none_or(|at| at.elapsed() >= std::time::Duration::from_millis(120))
    }

    /// force a reload, for the end of a burst of typing.
    pub fn settle(&mut self) {
        if self.loaded != self.query {
            self.reload();
        }
    }

    fn tag_prompt(&mut self, action: Action) {
        let Some(buffer) = self.tag_input.as_mut() else { return };
        match action {
            Action::Quit | Action::Back | Action::None => self.tag_input = None,
            Action::Type(c) => buffer.push(c),
            Action::Backspace => {
                buffer.pop();
            }
            Action::Open => {
                let tag = buffer.trim().to_owned();
                self.tag_input = None;
                if tag.is_empty() {
                    self.status = Cow::Borrowed("no tag given");
                    return;
                }
                self.apply_tag(&tag);
            }
            _ => {}
        }
    }

    /// add a tag to the selected bookmark.
    pub fn apply_tag(&mut self, tag: &str) {
        let Some(id) = self.selected_item().map(|b| b.id) else { return };
        let conn = self.conn.lock().expect("the store lock is held for one frame");
        let repo = Repo::new(&conn);
        let Ok(Some(mut bookmark)) = repo.load(id) else {
            self.status = Cow::Borrowed("could not read that bookmark");
            return;
        };
        bookmark.push_tag(tag);
        let tags: ahash::AHashSet<String> = bookmark.tags.iter().cloned().collect();
        match repo.set_tags(id, &tags) {
            Ok(()) => {
                self.status = Cow::Owned(format!("tagged {id} with `{tag}`"));
                if let Some(slot) = self.items.iter_mut().find(|(b, _)| b.id == id) {
                    slot.0 = bookmark;
                }
                self.tags = repo.tags_with_counts(200).unwrap_or_default();
            }
            Err(e) => self.status = Cow::Owned(format!("could not tag: {e}")),
        }
    }

    /// remove a bookmark, remembering it for undo.
    pub fn remove(&mut self, bookmark: &Bookmark) {
        let id = bookmark.id;
        let conn = self.conn.lock().expect("the store lock is held for one frame");
        let deleted = conn
            .execute("DELETE FROM bookmark WHERE id = ?1", [id.get() as i64])
            .is_ok_and(|n| n > 0);
        if deleted {
            self.undo = Some(bookmark.clone());
            self.status = Cow::Owned(format!("removed {id}, u to put it back"));
            drop(conn);
            self.reload();
        } else {
            self.status = Cow::Owned(format!("could not remove {id}"));
        }
    }

    /// put the last removed bookmark back.
    pub fn put_back(&mut self) {
        let Some(bookmark) = self.undo.take() else {
            self.status = Cow::Borrowed("nothing to put back");
            return;
        };
        let conn = self.conn.lock().expect("the store lock is held for one frame");
        match Repo::new(&conn).insert(&bookmark) {
            Ok(()) => {
                self.status = Cow::Owned(format!("put {id} back", id = bookmark.id));
                drop(conn);
                self.reload();
            }
            Err(e) => self.status = Cow::Owned(format!("could not put it back: {e}")),
        }
    }

    /// the archive's counts, for the status line.
    #[must_use]
    pub fn totals(&self) -> (usize, usize) {
        let conn = self.conn.lock().expect("the store lock is held for one frame");
        let repo = Repo::new(&conn);
        (repo.count().unwrap_or(0), repo.count_matching(&Filter::default()).unwrap_or(0))
    }
}

/// the ranking mode, as the status line spells it.
#[must_use]
pub const fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Hybrid => "hybrid",
        Mode::Exact => "exact",
        Mode::Fuzzy => "fuzzy",
    }
}

// ─── drawing ────────────────────────────────────────────────────────────────

/// the palette.
///
/// one accent, and three greys. an archive browser is a reading tool, and a
/// reading tool that spends colour on decoration is harder to read than one
/// that does not.
mod style {
    use ratatui::style::Color;

    pub(super) const ACCENT: Color = Color::Rgb(0x8a, 0xb4, 0xf8);
    pub(super) const MUTED: Color = Color::Rgb(0x9a, 0xa1, 0xab);
    pub(super) const SELECTED: Color = Color::Rgb(0x1f, 0x29, 0x37);
    pub(super) const WARN: Color = Color::Rgb(0xf5, 0x9e, 0x0b);
}

/// draw one frame.
pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(frame.area());

    draw_query(frame, areas[1], app);
    match app.view {
        View::Detail => draw_detail(frame, areas[2], app),
        View::Tags => draw_tags(frame, areas[2], app),
        _ => draw_list(frame, areas[2], app),
    }
    draw_status(frame, areas[3], app);
}

fn draw_query(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let hint = if app.tag_input.is_some() {
        format!("tag: {}", app.tag_input.as_deref().unwrap_or_default())
    } else {
        format!("search: {}{}", app.query, if app.query.is_empty() { "…" } else { "" })
    };
    let block =
        Block::default().borders(Borders::ALL).border_style(Style::default().fg(style::MUTED));
    let paragraph = Paragraph::new(hint).block(block).style(Style::default().fg(style::ACCENT));
    frame.render_widget(paragraph, area);
}

fn draw_list(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if app.items.is_empty() {
        let text = if app.query.trim().is_empty() {
            "nothing in the archive yet.\n\n  mbm add https://example.com\n  mbm import ~/Downloads/bookmarks.html"
        } else {
            "nothing matches that query."
        };
        frame.render_widget(
            Paragraph::new(text)
                .style(Style::default().fg(style::MUTED))
                .wrap(Wrap { trim: true })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(style::MUTED)),
                ),
            area,
        );
        return;
    }

    let items: Vec<ListItem<'_>> = app
        .items
        .iter()
        .map(|(bookmark, _)| {
            let mut spans = vec![
                Span::styled(
                    format!(
                        "{:>10} ",
                        mbm_sink::date_only(bookmark.created_at.unwrap_or(bookmark.ingested_at))
                    ),
                    Style::default().fg(style::MUTED),
                ),
                Span::styled(
                    format!("{:<13}", bookmark.source.medium.name()),
                    Style::default().fg(style::MUTED),
                ),
                Span::raw(mbm_sink::display_title(bookmark)),
            ];
            if let Some(author) = &bookmark.author {
                spans.push(Span::styled(
                    format!("  {}", author.display()),
                    Style::default().fg(style::MUTED),
                ));
            }
            if !bookmark.tags.is_empty() {
                let tags: Vec<String> = bookmark.tags.iter().take(4).cloned().collect();
                spans.push(Span::styled(
                    format!("  {}", tags.join(" ")),
                    Style::default().fg(style::MUTED),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} ", app.list_label()))
                .border_style(Style::default().fg(style::MUTED)),
        )
        .highlight_style(Style::default().bg(style::SELECTED).add_modifier(Modifier::BOLD));

    let mut state = ListState::default();
    state.select(Some(app.selected.min(app.items.len().saturating_sub(1))));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_detail(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(bookmark) = app.selected_item() else {
        return;
    };

    // an explicit list of lines rather than a `Text` built by appending: a
    // blank row has to be a real `Line::default()`, and that is the one thing
    // a string-appending builder makes easy to get wrong
    let mut lines: Vec<Line<'_>> = Vec::new();
    let muted = Style::default().fg(style::MUTED);

    lines.push(Line::from(Span::styled(
        mbm_sink::display_title(bookmark),
        Style::default().fg(style::ACCENT).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::default());

    let mut byline: Vec<Span<'_>> = Vec::new();
    if let Some(when) = bookmark.created_at {
        byline.push(Span::styled(mbm_sink::date_time(when), muted));
    }
    if let Some(author) = &bookmark.author {
        byline.push(Span::raw(format!("  {}", author.display())));
    }
    if let Some(collection) = &bookmark.source.collection {
        byline.push(Span::styled(format!("  {collection}"), muted));
    }
    if !byline.is_empty() {
        lines.push(Line::from(byline));
    }
    lines.push(Line::default());

    if let Some(url) = &bookmark.url {
        lines.push(Line::from(Span::styled(url.to_string(), Style::default().fg(style::ACCENT))));
        lines.push(Line::default());
    }

    if let Some(summary) = mbm_sink::summary_of(bookmark) {
        lines.push(Line::from(Span::raw(summary.to_owned())));
        lines.push(Line::default());
    }

    for line in bookmark.text.lines() {
        lines.push(Line::from(line.to_owned()));
    }

    if !bookmark.links.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled("links", muted)));
        for link in &bookmark.links {
            let blocked =
                link.blocked.map(|reason| format!("  [{}]", reason.name())).unwrap_or_default();
            lines.push(Line::from(format!("  {}{}", link.resolved, blocked)));
        }
    }
    if !bookmark.tags.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            format!("tags: {}", bookmark.tags.iter().cloned().collect::<Vec<_>>().join(" ")),
            muted,
        )));
    }
    if !bookmark.categories.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(
                "in: {}",
                bookmark.categories.iter().map(|c| c.slug.as_str()).collect::<Vec<_>>().join(" ")
            ),
            muted,
        )));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(format!("id {}", bookmark.id.get()), muted)));

    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" detail — esc to go back ")
                .border_style(Style::default().fg(style::MUTED)),
        ),
        area,
    );
}

fn draw_tags(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if app.tags.is_empty() {
        frame.render_widget(
            Paragraph::new("no tags yet.")
                .style(Style::default().fg(style::MUTED))
                .block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    }
    let items: Vec<ListItem<'_>> = app
        .tags
        .iter()
        .map(|(tag, count)| {
            ListItem::new(Line::from(vec![
                Span::raw(tag.clone()),
                Span::styled(format!("  {count}"), Style::default().fg(style::MUTED)),
            ]))
        })
        .collect();
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" tags ")
                .border_style(Style::default().fg(style::MUTED)),
        ),
        area,
    );
}

fn draw_status(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let (total, _) = app.totals();
    let shown = if app.items.len() == total {
        format!("{total} bookmarks")
    } else {
        format!("{} of {} bookmarks", app.items.len(), total)
    };
    let left = format!("{shown} · {}", mode_label(app.mode));
    let middle = app.status.to_string();
    let right = "↑↓ move · enter open · tab rank · f2 tag · ctrl-d remove · ctrl-u undo · ctrl-g tags ·          ctrl-c quit";

    let line = Line::from(vec![
        Span::styled(format!(" {left} "), Style::default().fg(style::MUTED)),
        Span::styled(
            format!("│ {middle} "),
            Style::default().fg(if middle.contains("could not") {
                style::WARN
            } else {
                style::MUTED
            }),
        ),
        Span::styled(right.to_owned(), Style::default().fg(style::MUTED)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// a centred box, for a future modal.
pub fn centred(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

/// a terminal error, in the shape the workspace uses.
///
/// the terminal is a stream the program does not own, so there is no path to
/// name. the message is what a person needs, and it is enough.
fn ui(error: std::io::Error) -> Error {
    backend(error)
}

/// a backend error, which is displayable but not an io error.
fn backend<E: std::fmt::Display>(error: E) -> Error {
    Error::Invalid(format!("terminal: {error}"))
}

/// run the interface until the user leaves.
///
/// this is the one function here that touches the real terminal, and it is
/// written so the rest of the file never has to.
pub fn run(conn: Arc<std::sync::Mutex<Connection>>, query: &str) -> Result<()> {
    use crossterm::execute;
    use crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };

    enable_raw_mode().map_err(ui)?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(ui)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(ui)?;

    let mut app = App::new(conn, query);
    let outcome = event_loop(&mut terminal, &mut app);

    disable_raw_mode().map_err(ui)?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).map_err(ui)?;
    terminal.show_cursor().map_err(ui)?;
    outcome
}

fn event_loop<B>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()>
where
    B: ratatui::backend::Backend,
    B::Error: std::fmt::Display,
{
    use crossterm::event::{self, Event, KeyEventKind};

    loop {
        terminal.draw(|frame| draw(frame, app)).map_err(backend)?;

        // the wait has a timeout, because the rebuild that ends a burst of
        // typing is owed whether or not another key is coming. blocking on the
        // next key instead leaves the last characters of a query unsearched
        // until the person presses something else.
        if !event::poll(std::time::Duration::from_millis(40)).map_err(ui)? {
            app.settle();
            continue;
        }
        let Event::Key(key) = event::read().map_err(ui)? else {
            continue;
        };
        // windows and some terminals send both press and release; a release
        // would type a character twice
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if !app.act(action_for(&key)) {
            app.settle();
            return Ok(());
        }
    }
}

/// clear the screen, for a caller that wants the terminal back afterwards.
pub fn clear<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>) {
    let _ = terminal.clear();
}

/// a blank frame, for a test that only cares about the layout maths.
pub fn blank<'a>() -> Text<'a> {
    Text::default()
}

/// draw into a buffer, for a test.
pub fn render_to(width: u16, height: u16, app: &mut App) -> String {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("a test backend always builds");
    terminal.draw(|frame| draw(frame, app)).expect("drawing into a test backend cannot fail");
    let buffer = terminal.backend().buffer();
    let cols = usize::from(width);
    buffer
        .content()
        .chunks(cols)
        .map(|row| row.iter().map(ratatui::buffer::Cell::symbol).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// the colours the interface uses, exposed so a test can assert on them.
pub fn palette() -> [Color; 4] {
    [style::ACCENT, style::MUTED, style::SELECTED, style::WARN]
}

/// a `Clear` widget over a rect, for a future modal.
pub fn clear_area(frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(Clear, area);
}
