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
//! the file is split three ways, and the split is the design:
//!
//! - [`theme`] is every colour, in two appearances, each value measured.
//! - [`motion`] is the clock and the curve, and what is deliberately not
//!   animated at all.
//! - [`draw`] is the frame. this file is the state, the keys and the loop.
//!
//! the rules the interface holds itself to:
//!
//! - **the query is the interface.** every list view has a field at the top
//!   and the search runs as you type, debounced so a held key does not thrash
//!   the index.
//! - **the ranking is a control.** hybrid by default, exact and fuzzy on tab,
//!   shown as a chip in the field rather than as a word in the status line.
//! - **nothing is destructive by accident.** removing asks for nothing but
//!   says what it did in a colour of its own, and `ctrl-u` puts it back.
//! - **the status line is always there and always fits.** what is in the
//!   archive, what the last action did, and what the keys do, giving way in the
//!   order of how much each matters as the terminal narrows.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Instant;

use mbm_core::bookmark::Bookmark;
use mbm_core::error::{Error, Result};
use mbm_store::{Filter, Mode, Repo};
use ratatui::Terminal;
use rusqlite::Connection;

pub mod draw;
pub mod motion;
pub mod theme;

use motion::{Clock, Motion};
use theme::{Appearance, Theme};

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
    /// move up a screen.
    PageUp,
    /// move down a screen.
    PageDown,
    /// a key the interface ignores.
    None,
}

/// read one key event into an action.
///
/// the raw key first, because `KeyCode::Char` with a control modifier is a
/// command rather than a character, and reading it as text would put a `c` in
/// the search box every time someone pressed ctrl-c.
///
/// every command is on a modifier. a terminal has no function key it can
/// promise, and a letter is a letter: someone typing a query that happens to
/// contain `g` should not leave the tags view.
#[must_use]
pub fn action_for(event: &crossterm::event::KeyEvent) -> Action {
    use crossterm::event::{KeyCode, KeyModifiers};

    if event.modifiers.contains(KeyModifiers::CONTROL) {
        return match event.code {
            KeyCode::Char('c') => Action::Quit,
            // `ctrl-u` is undo, because that is what the help line says it is
            // and because the thing behind it is the only action here that
            // loses data. it used to clear the query, which meant the help was
            // lying about the one key that matters.
            KeyCode::Char('u' | 'z') => Action::Undo,
            // clearing moves to ctrl-k, the kill-the-line key, so the two do not
            // share a binding
            KeyCode::Char('k') => Action::Clear,
            KeyCode::Char('r') => Action::Refresh,
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
        KeyCode::PageUp => Action::PageUp,
        KeyCode::PageDown => Action::PageDown,
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

    /// when the detail view last opened, which is what its entrance runs on.
    ///
    /// it starts settled rather than running, because the first thing drawn
    /// should be the finished thing and motion should only ever answer
    /// something the reader did.
    opened: Clock,
    /// when the status message last changed, which is what its settling runs on.
    said: Clock,
    /// how many rows the list shows, so a page is a page rather than a guess.
    rows: usize,
    /// how many bookmarks the archive holds, cached so the status line does not
    /// run a count on every frame.
    total: usize,
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
            .field("rows", &self.rows)
            .field("total", &self.total)
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
            opened: Clock::settled(),
            said: Clock::settled(),
            rows: 20,
            total: 0,
        };
        app.reload();
        app
    }

    /// how many rows the list has to show.
    ///
    /// the terminal is asked once per resize rather than per frame, because a
    /// frame is a repaint and a size query is a syscall.
    pub fn set_rows(&mut self, rows: usize) {
        self.rows = rows.max(1);
    }

    /// the archive's size, as the status line spells it.
    #[must_use]
    pub fn total(&self) -> usize {
        self.total
    }

    /// how far through an entrance the detail view is, from 0 to 1.
    #[must_use]
    pub fn open_progress(&self, motion: Motion) -> f64 {
        motion.progress(self.opened.since(), motion::ENTER)
    }

    /// how far through its settling the status message is, from 0 to 1.
    #[must_use]
    pub fn status_progress(&self, motion: Motion) -> f64 {
        motion.progress(self.said.since(), motion::EXIT)
    }

    /// say something, and start its settling.
    ///
    /// every message goes through here so the one that changes the colour of the
    /// bar is the one that restarts the clock. a message set directly would
    /// never brighten, and a status line whose text never changes emphasis is a
    /// status line nobody notices.
    pub fn say(&mut self, message: impl Into<Cow<'static, str>>) {
        self.status = message.into();
        self.said.restart();
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

    /// how many rows the showing list has.
    fn shown_len(&self) -> usize {
        if self.view == View::Tags { self.tags.len() } else { self.items.len() }
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
        self.total = Repo::new(&conn).count().unwrap_or(self.total);
        // the list and the tag list are two different lengths sharing one
        // cursor, so the clamp is whichever of the two is showing
        let shown = if self.view == View::Tags { self.tags.len() } else { self.items.len() };
        self.selected = self.selected.min(shown.saturating_sub(1));
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
                self.view = View::Browse;
                self.say("query cleared");
                self.reload();
            }
            Action::Up => self.selected = self.selected.saturating_sub(1),
            Action::Down => {
                if self.selected + 1 < self.shown_len() {
                    self.selected += 1;
                }
            }
            Action::Home => self.selected = 0,
            Action::End => self.selected = self.shown_len().saturating_sub(1),
            // a page is a screenful, which is the only sense of "a lot" a
            // keyboard can express here
            Action::PageUp => self.selected = self.selected.saturating_sub(self.rows),
            Action::PageDown => {
                self.selected = (self.selected + self.rows).min(self.shown_len().saturating_sub(1));
            }
            Action::Open => {
                if self.view == View::Tags {
                    // a tag list that cannot be used is a picture of a tag
                    // list. picking a tag filters the archive by it, which is
                    // the only reason anyone opens it.
                    if let Some((tag, count)) = self.tags.get(self.selected).cloned() {
                        self.query.clone_from(&tag);
                        self.selected = 0;
                        self.view = View::Browse;
                        self.say(format!("showing {count} with {tag}"));
                        self.reload();
                    }
                } else if self.selected_item().is_some() {
                    self.view = View::Detail;
                    // the view is already on screen when this returns, so the
                    // entrance starts from the next frame rather than from a
                    // frame the reader never saw
                    self.opened.restart();
                }
            }
            // every view but the tag list goes back to the list
            Action::Back => {
                if self.view == View::Detail {
                    // the selection is remembered, so coming back lands on the
                    // row that was open rather than at the top
                    self.view = View::Browse;
                } else {
                    self.view = View::Browse;
                }
            }
            Action::Tags => {
                self.view = if self.view == View::Tags { View::Browse } else { View::Tags };
            }
            Action::Rank => {
                self.mode = match self.mode {
                    Mode::Hybrid => Mode::Exact,
                    Mode::Exact => Mode::Fuzzy,
                    Mode::Fuzzy => Mode::Hybrid,
                };
                self.say(format!("ranking: {}", mode_label(self.mode)));
                self.reload();
            }
            Action::Tag => {
                if self.selected_item().is_some() {
                    self.tag_input = Some(String::new());
                    self.say("type a tag, enter to save, esc to cancel");
                }
            }
            Action::Delete => {
                if let Some(bookmark) = self.selected_item().cloned() {
                    self.remove(&bookmark);
                }
            }
            Action::Undo => self.put_back(),
            Action::Refresh => {
                self.say("reloaded");
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
            Action::Quit | Action::Back | Action::None => {
                self.tag_input = None;
                self.say("tag cancelled");
            }
            Action::Type(c) => buffer.push(c),
            Action::Backspace => {
                buffer.pop();
            }
            Action::Open => {
                let tag = buffer.trim().to_owned();
                self.tag_input = None;
                if tag.is_empty() {
                    self.say("no tag given, so nothing was added");
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
        // the work happens under the lock and the message after it, because
        // saying something borrows the app and the lock borrows the app too.
        let outcome = {
            let conn = self.conn.lock().expect("the store lock is held for one frame");
            let repo = Repo::new(&conn);
            match repo.load(id) {
                Ok(Some(mut bookmark)) => {
                    bookmark.push_tag(tag);
                    let tags: ahash::AHashSet<String> = bookmark.tags.iter().cloned().collect();
                    match repo.set_tags(id, &tags) {
                        Ok(()) => {
                            let tags = repo.tags_with_counts(200).unwrap_or_default();
                            Ok((bookmark, tags))
                        }
                        Err(e) => Err(e.to_string()),
                    }
                }
                Ok(None) => Err("that bookmark is no longer in the archive".to_owned()),
                Err(e) => Err(e.to_string()),
            }
        };
        match outcome {
            Ok((bookmark, tags)) => {
                if let Some(slot) = self.items.iter_mut().find(|(b, _)| b.id == id) {
                    slot.0 = bookmark;
                }
                self.tags = tags;
                self.say(format!("tagged {id} with `{tag}`"));
            }
            Err(why) => self.say(format!("could not tag it: {why}. nothing was changed.")),
        }
    }

    /// remove a bookmark, remembering it for undo.
    pub fn remove(&mut self, bookmark: &Bookmark) {
        let id = bookmark.id;
        let deleted = {
            let conn = self.conn.lock().expect("the store lock is held for one frame");
            conn.execute("DELETE FROM bookmark WHERE id = ?1", [id.get() as i64])
                .is_ok_and(|n| n > 0)
        };
        if deleted {
            self.undo = Some(bookmark.clone());
            self.say(format!("removed {id}. ctrl-u puts it back."));
            self.reload();
        } else {
            self.say(format!("could not remove {id}. it is still in the archive."));
        }
    }

    /// put the last removed bookmark back.
    pub fn put_back(&mut self) {
        let Some(bookmark) = self.undo.take() else {
            self.say("nothing to put back");
            return;
        };
        let restored = {
            let conn = self.conn.lock().expect("the store lock is held for one frame");
            Repo::new(&conn).insert(&bookmark).map_err(|e| e.to_string())
        };
        match restored {
            Ok(()) => {
                self.say(format!("put {id} back", id = bookmark.id));
                self.reload();
            }
            Err(why) => self.say(format!("could not put it back: {why}. it is still gone.")),
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

/// the ranking mode, as a person reads it.
#[must_use]
pub const fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Hybrid => "hybrid",
        Mode::Exact => "exact",
        Mode::Fuzzy => "fuzzy",
    }
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
///
/// the appearance is settled before the first frame, because a theme that
/// changed halfway through would repaint every colour on screen at once and
/// there is no transition to soften that. the terminal is asked what its
/// background is and the answer is used; a terminal that will not answer falls
/// back to dark, and `MBM_THEME` overrides both.
pub fn run(conn: Arc<std::sync::Mutex<Connection>>, query: &str) -> Result<()> {
    use crossterm::execute;
    use crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };

    enable_raw_mode().map_err(ui)?;
    let theme = resolve_theme();
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(ui)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(ui)?;
    let size = terminal.size().map_err(ui)?;

    let mut app = App::new(conn, query);
    app.set_rows(usize::from(size.height).saturating_sub(2));
    let outcome = event_loop(&mut terminal, &mut app, &theme);

    disable_raw_mode().map_err(ui)?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).map_err(ui)?;
    terminal.show_cursor().map_err(ui)?;
    outcome
}

/// the palette to draw with.
///
/// the terminal's own background decides, because a palette chosen for the
/// wrong one is unreadable rather than merely ugly: the dark palette's secondary
/// text is 9:1 on near-black and 1.9:1 on white. three overrides, in order of
/// how much they are about the answer rather than about the person:
/// `MBM_THEME` names the appearance outright, `NO_COLOR` drops to the terminal's
/// own sixteen, and failing all of that the terminal is asked.
fn resolve_theme() -> Theme {
    if let Ok(asked) = std::env::var("MBM_THEME") {
        let asked = asked.trim().to_ascii_lowercase();
        if asked == "light" {
            return Theme::for_appearance(Appearance::Light);
        }
        if asked == "dark" {
            return Theme::for_appearance(Appearance::Dark);
        }
    }
    if std::env::var("NO_COLOR").is_ok_and(|v| !v.is_empty()) {
        // the palette a terminal without colour support draws is the one it
        // draws itself, so the interface stops asking for anything
        return Theme::no_colour();
    }
    match theme::terminal_background() {
        Some(rgb) => Theme::for_appearance(Appearance::from_background(rgb)),
        None => Theme::for_appearance(Appearance::Dark),
    }
}

fn event_loop<B>(terminal: &mut Terminal<B>, app: &mut App, theme: &Theme) -> Result<()>
where
    B: ratatui::backend::Backend,
    B::Error: std::fmt::Display,
{
    use crossterm::event::{self, Event, KeyEventKind};

    let motion = Motion::default();
    loop {
        terminal.draw(|frame| draw::draw(frame, app, theme, motion)).map_err(backend)?;

        // a repaint is only worth doing when something changed, and something
        // changes in three ways: a key arrived, the terminal was resized, or an
        // animation is still running. polling with a short timeout covers all
        // three in one place, and the frame above is skipped entirely when
        // none of them is true, so an idle interface costs no writes at all.
        if !event::poll(std::time::Duration::from_millis(40)).map_err(ui)? {
            // the rebuild that ends a burst of typing is owed whether or not
            // another key is coming: blocking on the next key instead leaves the
            // last characters of a query unsearched
            app.settle();
            continue;
        }
        match event::read().map_err(ui)? {
            Event::Key(key) => {
                // windows and some terminals send both press and release, and a
                // release would type a character twice
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if !app.act(action_for(&key)) {
                    app.settle();
                    return Ok(());
                }
            }
            // a resize changes the column set and the row count, and both are
            // asked for again rather than remembered
            Event::Resize(_, height) => {
                app.set_rows(usize::from(height).saturating_sub(2));
                app.say("resized");
            }
            _ => {}
        }
    }
}

/// clear the screen, for a caller that wants the terminal back afterwards.
pub fn clear<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>) {
    let _ = terminal.clear();
}

/// draw one frame into an offscreen buffer and read it back as text.
///
/// the interface is a program that draws, so the only honest way to check what
/// it drew is to let it draw. a caller that wants a string rather than a
/// terminal gets exactly the pixels the real frame would have had, which is what
/// the end-to-end suite asserts on.
pub fn render_to(width: u16, height: u16, app: &mut App, theme: &Theme) -> String {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("a test backend always builds");
    app.set_rows(usize::from(height).saturating_sub(2));
    terminal
        .draw(|frame| draw::draw(frame, app, theme, Motion::none()))
        .expect("drawing into a test backend cannot fail");
    let buffer = terminal.backend().buffer();
    let cols = usize::from(width);
    buffer
        .content()
        .chunks(cols)
        .map(|row| row.iter().map(ratatui::buffer::Cell::symbol).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}
