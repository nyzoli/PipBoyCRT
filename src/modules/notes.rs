//! NOTES module: sticky notes ("cetlik") — a list of notes with a built-in editor.
//!
//! `notes.md` is a sequence of notes: each `# Title` line starts a note, the
//! lines up to the next `# ` line are its body. Leading lines before the
//! first heading form an "Untitled" note.

use crate::module::{Ctx, Module, Notice, Slot};
use crate::style::Theme;
use edtui::{EditorEventHandler, EditorMode, EditorState, EditorTheme, EditorView, Lines};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use std::cell::{Cell, RefCell};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

#[derive(serde::Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct NotesCfg {
    pub file: String,
}

impl Default for NotesCfg {
    fn default() -> Self {
        Self { file: "notes.md".to_string() }
    }
}

enum NotesEvent {
    Content(String),
    Missing,
}

/// One sticky note: a title and its body lines.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Note {
    pub title: String,
    pub body: Vec<String>,
}

/// Parse `notes.md` content into notes. Each `# Title` line starts a note;
/// the lines up to the next `# ` line are its body. A single blank line
/// immediately before the next heading (or at EOF) is treated as the
/// inter-note separator and stripped, not kept in the body. Leading lines
/// before any heading form an "Untitled" note (only if non-empty).
pub fn parse_notes(s: &str) -> Vec<Note> {
    fn flush(started: bool, title: &mut Option<String>, body: &mut Vec<String>, notes: &mut Vec<Note>) {
        if body.last().map(|l| l.is_empty()).unwrap_or(false) {
            body.pop();
        }
        if started {
            notes.push(Note { title: title.take().unwrap_or_default(), body: std::mem::take(body) });
        } else if !body.is_empty() {
            notes.push(Note { title: "Untitled".to_string(), body: std::mem::take(body) });
        }
    }

    let mut notes = Vec::new();
    let mut title: Option<String> = None;
    let mut body: Vec<String> = Vec::new();
    let mut started = false;

    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("# ") {
            flush(started, &mut title, &mut body, &mut notes);
            title = Some(rest.to_string());
            started = true;
        } else {
            body.push(line.to_string());
        }
    }
    flush(started, &mut title, &mut body, &mut notes);
    notes
}

/// Render notes back to `notes.md` content: `render_notes(&parse_notes(x)) == x`
/// for a normalized `x` (`# Title`, body lines, one blank line between notes).
pub fn render_notes(notes: &[Note]) -> String {
    let mut out = String::new();
    for (i, note) in notes.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str("# ");
        out.push_str(&note.title);
        out.push('\n');
        for line in &note.body {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Clamp a scroll offset so it never exceeds `len.saturating_sub(page)`.
pub fn clamp_scroll(scroll: usize, len: usize, page: usize) -> usize {
    scroll.min(len.saturating_sub(page))
}

/// Turn an editor buffer's text into body lines: trailing empty lines are
/// dropped (an empty buffer becomes `vec![]`, not `vec![""]`), internal
/// blank lines are kept.
fn body_from_lines(lines: &Lines) -> Vec<String> {
    let mut body: Vec<String> = lines.to_string().split('\n').map(str::to_string).collect();
    while body.last().map(|l| l.is_empty()).unwrap_or(false) {
        body.pop();
    }
    body
}

/// Resolve a configured file path: relative → next to the executable, absolute stays.
pub fn resolve(file: &str, exe_dir: &Path) -> PathBuf {
    let p = Path::new(file);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        exe_dir.join(p)
    }
}

/// An editor session: the buffer plus the *same* `EditorEventHandler` for
/// every keystroke of the session. `EditorEventHandler` carries pending
/// multi-key state (`d` waiting for a second `d`, `g` waiting for `g`, a
/// pending `f<char>` motion, …); building a fresh `default()` per key would
/// silently discard that state and such chords would never resolve.
struct EditSession {
    state: EditorState,
    handler: EditorEventHandler,
}

/// The editor mode / interaction state of the tab.
enum Mode {
    /// Browsing: `scroll` is the selected note's body scroll offset.
    View { scroll: usize },
    /// Editing the selected note's body with the built-in `edtui` editor.
    Edit(RefCell<EditSession>),
    /// Typing a title for a new note.
    NewTitle(String),
    /// `d` was pressed once; a second `d` deletes the selected note.
    ConfirmDelete,
}

/// The one-line title-prompt text: `TITLE> ` plus what's typed so far.
/// Pure so it's directly testable; also doubles as the cursor-position ruler
/// (the real terminal cursor sits right after its last character).
fn title_prompt(input: &str) -> String {
    format!("TITLE> {input}")
}

/// The header's mode label, uppercase, matching `t.title`/`t.warn`/`t.danger`
/// in `draw` (kept as a separate match there since it needs a `Theme`).
fn mode_label(mode: &Mode) -> &'static str {
    match mode {
        Mode::View { .. } => "VIEW",
        // ponytail: Visual/Search (reachable via vim keys like `v`, `/`)
        // fold into "EDIT" too — spec only names insert/normal.
        Mode::Edit(_) => "EDIT",
        Mode::NewTitle(_) => "NEW TITLE",
        Mode::ConfirmDelete => "DELETE?",
    }
}

/// Border + title style for one pane: the focused pane is drawn bright
/// (`t.title` border and label), the other stays in the plain `t.frame`
/// style. Brightening the focused one (rather than dimming the other) works
/// in the mono theme too, where `frame` is already DIM.
fn pane_block(title: &'static str, focused: bool, t: Theme) -> Block<'static> {
    let (border, label) = if focused { (t.title, t.title) } else { (t.frame, t.frame) };
    Block::bordered().border_style(border).title(Span::styled(title, label))
}

/// Side effect a pure key-transition method wants the caller (which holds `Ctx`) to perform.
enum Effect {
    /// Consumed, nothing further to do.
    None,
    /// Send a footer notice.
    Footer(String),
    /// Persist `self.notes` to disk (a note was deleted).
    Save,
}

/// Side effect `edit_key` wants the `Ctx`-holding caller to perform.
#[derive(PartialEq, Eq, Debug)]
enum EditEffect {
    /// Key was consumed by the editor, nothing further to do.
    None,
    /// Ctrl+S: persist the buffer, stay in Edit mode.
    Save,
    /// Esc: persist the buffer and return to View mode.
    SaveAndView,
}

/// Shared last-seen mtime between the watcher thread and the UI thread, so a
/// save from the UI thread doesn't make the watcher re-read and re-send the
/// file it just wrote.
type SharedMtime = Arc<Mutex<Option<Option<SystemTime>>>>;

pub struct Notes {
    cfg: NotesCfg,
    path: PathBuf,
    notes: Vec<Note>,
    selected: usize,
    mode: Mode,
    missing: bool,
    rx: Option<Receiver<NotesEvent>>,
    watch_mtime: SharedMtime,
    list_state: RefCell<ListState>,
    /// Visible body rows from the last `draw`, used to clamp scrolling in `on_key`.
    view_page: Cell<usize>,
}

impl Notes {
    pub fn new() -> Self {
        Self {
            cfg: NotesCfg::default(),
            path: PathBuf::new(),
            notes: Vec::new(),
            selected: 0,
            mode: Mode::View { scroll: 0 },
            missing: false,
            rx: None,
            watch_mtime: Arc::new(Mutex::new(None)),
            list_state: RefCell::new(ListState::default()),
            view_page: Cell::new(0),
        }
    }

    fn apply(&mut self, ev: NotesEvent) {
        match ev {
            NotesEvent::Content(content) => {
                self.notes = parse_notes(&content);
                self.missing = false;
            }
            NotesEvent::Missing => {
                self.notes.clear();
                self.missing = true;
            }
        }
        if self.selected >= self.notes.len() {
            self.selected = self.notes.len().saturating_sub(1);
        }
        self.reset_scroll();
    }

    /// Direct, synchronous re-read for the `r` key (small local file, acceptable on the UI thread).
    fn reload(&mut self) {
        match fs::read_to_string(&self.path) {
            Ok(content) => self.apply(NotesEvent::Content(content)),
            Err(_) => self.apply(NotesEvent::Missing),
        }
    }

    fn reset_scroll(&mut self) {
        if let Mode::View { scroll } = &mut self.mode {
            *scroll = 0;
        }
    }

    fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        self.reset_scroll();
    }

    fn select_next(&mut self) {
        if self.selected + 1 < self.notes.len() {
            self.selected += 1;
        }
        self.reset_scroll();
    }

    fn scroll_view(&mut self, code: KeyCode) {
        let page = self.view_page.get().max(1);
        let len = self.notes.get(self.selected).map(|n| n.body.len()).unwrap_or(0);
        if let Mode::View { scroll } = &mut self.mode {
            let raw = match code {
                KeyCode::PageUp => scroll.saturating_sub(page),
                KeyCode::PageDown => scroll.saturating_add(page),
                KeyCode::Home => 0,
                KeyCode::End => len,
                _ => *scroll,
            };
            *scroll = clamp_scroll(raw, len, page);
        }
    }

    fn enter_edit(&mut self) {
        let Some(note) = self.notes.get(self.selected) else { return };
        let mut state = EditorState::new(Lines::from(note.body.join("\n")));
        state.mode = EditorMode::Insert;
        let session = EditSession { state, handler: EditorEventHandler::default() };
        self.mode = Mode::Edit(RefCell::new(session));
    }

    /// Delete the selected note. Deleting the last note selects the previous
    /// one; deleting the only note leaves an empty list without panicking.
    fn delete_selected(&mut self) {
        if self.notes.is_empty() {
            return;
        }
        self.notes.remove(self.selected);
        if self.selected >= self.notes.len() {
            self.selected = self.notes.len().saturating_sub(1);
        }
    }

    fn save(&mut self, ctx: &Ctx) {
        let content = render_notes(&self.notes);
        match fs::write(&self.path, &content) {
            Ok(()) => {
                // mtime is read right after the write completes, same as the
                // watcher does; a write landing between this read and the
                // watcher's next poll (sub-ms) can still cause one harmless
                // self-reload, not worth guarding further.
                let mtime = fs::metadata(&self.path).and_then(|m| m.modified()).ok();
                let mut last = self.watch_mtime.lock().unwrap_or_else(|e| e.into_inner());
                *last = Some(mtime);
                self.missing = false;
            }
            Err(e) => {
                let _ = ctx.notify.send(Notice::Footer(format!("notes save: {e}")));
            }
        }
    }

    // --- Pure key-transition methods (no `Ctx` needed, so they're directly testable). ---

    fn apply_view_key(&mut self, code: KeyCode) -> Option<Effect> {
        match code {
            KeyCode::Up => {
                self.select_prev();
                Some(Effect::None)
            }
            KeyCode::Down => {
                self.select_next();
                Some(Effect::None)
            }
            KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End => {
                self.scroll_view(code);
                Some(Effect::None)
            }
            KeyCode::Char('e') => {
                self.enter_edit();
                Some(Effect::None)
            }
            KeyCode::Char('n') => {
                self.mode = Mode::NewTitle(String::new());
                Some(Effect::None)
            }
            KeyCode::Char('d') => {
                if self.notes.is_empty() {
                    None
                } else {
                    self.mode = Mode::ConfirmDelete;
                    Some(Effect::Footer("d again to delete".to_string()))
                }
            }
            KeyCode::Char('r') => {
                self.reload();
                Some(Effect::None)
            }
            _ => None,
        }
    }

    fn apply_new_title_key(&mut self, code: KeyCode) -> Option<Effect> {
        let input = match &mut self.mode {
            Mode::NewTitle(s) => s,
            _ => return None,
        };
        match code {
            KeyCode::Enter => {
                let title = if input.trim().is_empty() { "Untitled".to_string() } else { std::mem::take(input) };
                self.notes.push(Note { title, body: Vec::new() });
                self.selected = self.notes.len() - 1;
                self.enter_edit();
                Some(Effect::None)
            }
            KeyCode::Esc => {
                self.mode = Mode::View { scroll: 0 };
                Some(Effect::None)
            }
            KeyCode::Backspace => {
                input.pop();
                Some(Effect::None)
            }
            KeyCode::Char(c) => {
                input.push(c);
                Some(Effect::None)
            }
            _ => Some(Effect::None),
        }
    }

    fn apply_confirm_delete_key(&mut self, code: KeyCode) -> Option<Effect> {
        let deleted = code == KeyCode::Char('d');
        if deleted {
            self.delete_selected();
        }
        self.mode = Mode::View { scroll: 0 };
        Some(if deleted { Effect::Save } else { Effect::None })
    }

    /// Route a key through the current edit session's *persistent*
    /// `EditorEventHandler` (see `EditSession`), or handle Ctrl+S / Esc as a
    /// save point. The editor is single-mode: typing inserts, Esc leaves
    /// (ponytail: no vim Normal mode, edtui stays in Insert). `Ctx`-free, so
    /// it's directly testable.
    fn edit_key(&mut self, key: KeyEvent) -> EditEffect {
        let mut new_body: Option<Vec<String>> = None;
        let mut effect = EditEffect::None;
        if let Mode::Edit(session_cell) = &self.mode {
            let mut session = session_cell.borrow_mut();
            let ctrl_s = key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL);
            let esc = key.code == KeyCode::Esc;
            if ctrl_s || esc {
                new_body = Some(body_from_lines(&session.state.lines));
                effect = if esc { EditEffect::SaveAndView } else { EditEffect::Save };
            } else {
                let EditSession { state, handler } = &mut *session;
                handler.on_key_event(key, state);
            }
        }
        if let Some(body) = new_body {
            if let Some(note) = self.notes.get_mut(self.selected) {
                note.body = body;
            }
        }
        effect
    }

    /// Edit mode needs `Ctx` (to save and to notify on a save error), so it
    /// stays separate from the `Ctx`-free `edit_key` / `apply_*_key` methods.
    fn apply_edit_key(&mut self, key: KeyEvent, ctx: &Ctx) {
        match self.edit_key(key) {
            EditEffect::None => {}
            EditEffect::Save => self.save(ctx),
            EditEffect::SaveAndView => {
                self.save(ctx);
                self.mode = Mode::View { scroll: 0 };
            }
        }
    }

    fn draw_list(&self, f: &mut Frame, area: Rect, t: Theme, focused: bool) {
        let block = pane_block(" NOTES ", focused, t);
        let inner = block.inner(area);
        f.render_widget(block, area);

        let (items_area, input_area) = match &self.mode {
            Mode::NewTitle(_) if inner.height > 1 => {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(0), Constraint::Length(1)])
                    .split(inner);
                (chunks[0], Some(chunks[1]))
            }
            _ => (inner, None),
        };

        let items: Vec<ListItem> = self.notes.iter().map(|n| ListItem::new(n.title.clone())).collect();
        // ponytail: an unfocused list keeps a quiet selection so the bright EDIT pane wins.
        let highlight = if focused { t.tab_active } else { t.frame.add_modifier(Modifier::REVERSED) };
        let list = List::new(items).highlight_style(highlight);
        let mut state = self.list_state.borrow_mut();
        state.select(if self.notes.is_empty() { None } else { Some(self.selected) });
        f.render_stateful_widget(list, items_area, &mut state);

        if let (Some(input_area), Mode::NewTitle(input)) = (input_area, &self.mode) {
            let line = Line::from(vec![Span::styled("TITLE> ", t.title), Span::raw(input.clone())]);
            f.render_widget(Paragraph::new(line), input_area);
            if input_area.width > 0 {
                let prompt_len = title_prompt(input).chars().count() as u16;
                let x = (input_area.x + prompt_len).min(input_area.x + input_area.width - 1);
                f.set_cursor_position((x, input_area.y));
            }
        }
    }
}

impl Module for Notes {
    fn id(&self) -> &'static str {
        "notes"
    }
    fn title(&self) -> &'static str {
        "NOTES"
    }
    fn describe(&self) -> &'static str {
        "Sticky notes in notes.md with a built-in editor"
    }
    fn manual(&self) -> &'static str {
        "\
NOTES keeps sticky notes in notes.md next to the exe - plain
markdown you can open in any editor, no database in sight.

  ↑/↓   pick a note      PgUp/PgDn  scroll its body
  e     edit the selected note
  n     new note: type a title, enter creates it
  d     delete - press d again to confirm, any key cancels
  r     reload from disk

In the editor typing inserts, the arrows move, ctrl+s saves
and esc saves and returns to the list. While it is open it
owns every key except ctrl+c, so q types a q.

RADIO's * files tracks in here too, under \"Favorite tracks\"."
    }
    fn help(&self) -> &'static str {
        match &self.mode {
            Mode::View { .. } => "↑/↓ note   e edit   n new   d delete   r reload   1-9 tabs   q quit",
            Mode::Edit(_) => "type to insert · ↑↓←→ move · ctrl+s save · esc save & back",
            Mode::NewTitle(_) => "type title · enter create · esc cancel",
            Mode::ConfirmDelete => "d again to delete · any key cancels",
        }
    }

    fn start(&mut self, ctx: &Ctx) {
        let (cfg, notice) = ctx.config.section::<NotesCfg>(self.id());
        self.cfg = cfg;
        if let Some(n) = notice {
            let _ = ctx.notify.send(Notice::Footer(n));
        }
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default();
        self.path = resolve(&self.cfg.file, &exe_dir);

        let (tx, rx): (Sender<NotesEvent>, Receiver<NotesEvent>) = mpsc::channel();
        self.rx = Some(rx);
        let path = self.path.clone();
        let watch_mtime = self.watch_mtime.clone();
        std::thread::spawn(move || loop {
            let mtime = fs::metadata(&path).and_then(|m| m.modified()).ok();
            let changed = {
                let mut last = watch_mtime.lock().unwrap_or_else(|e| e.into_inner());
                if *last == Some(mtime) {
                    false
                } else {
                    *last = Some(mtime);
                    true
                }
            };
            if changed {
                let sent = match mtime {
                    Some(_) => match fs::read_to_string(&path) {
                        Ok(content) => tx.send(NotesEvent::Content(content)),
                        Err(_) => tx.send(NotesEvent::Missing),
                    },
                    None => tx.send(NotesEvent::Missing),
                };
                if sent.is_err() {
                    return;
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        });
    }

    fn poll(&mut self, _ctx: &Ctx) -> usize {
        let mut events = Vec::new();
        if let Some(rx) = &self.rx {
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
            }
        }
        let n = events.len();
        // Watcher events during Edit are dropped, not queued: the next save
        // overwrites the file anyway, so the saved content wins.
        let editing = matches!(self.mode, Mode::Edit(_));
        if !editing {
            for ev in events {
                self.apply(ev);
            }
        }
        n
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &Ctx) -> bool {
        // Ctrl+C must still quit the app even while the editor or the title
        // prompt owns every other key (q, digits, ... all type text).
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        if matches!(self.mode, Mode::Edit(_)) {
            self.apply_edit_key(key, ctx);
            return true;
        }
        let effect = match self.mode {
            Mode::View { .. } => self.apply_view_key(key.code),
            Mode::NewTitle(_) => self.apply_new_title_key(key.code),
            Mode::ConfirmDelete => self.apply_confirm_delete_key(key.code),
            Mode::Edit(_) => unreachable!("handled above"),
        };
        match effect {
            Some(Effect::Footer(msg)) => {
                let _ = ctx.notify.send(Notice::Footer(msg));
                true
            }
            Some(Effect::Save) => {
                self.save(ctx);
                true
            }
            Some(Effect::None) => true,
            None => false,
        }
    }

    fn draw(&self, f: &mut Frame, area: Rect, t: Theme) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let label = mode_label(&self.mode);
        let label_style = match &self.mode {
            Mode::View { .. } => t.title,
            Mode::Edit(_) => t.warn,
            Mode::NewTitle(_) => t.warn,
            // `danger` is this Theme's alert-red field (already used for
            // battery-low / high-load / stream-error elsewhere).
            Mode::ConfirmDelete => t.danger,
        };
        let filename = self.path.file_name().and_then(|s| s.to_str()).unwrap_or("notes.md");
        let prefix = format!(" NOTES  {filename} \u{b7} {} notes \u{b7} ", self.notes.len());
        let header = Line::from(vec![Span::styled(prefix, t.title), Span::styled(label, label_style)]);
        f.render_widget(Paragraph::new(header), Rect { height: 1, ..area });

        let body_area = Rect { y: area.y + 1, height: area.height.saturating_sub(1), ..area };
        if body_area.height == 0 {
            return;
        }

        // A missing file only replaces the panes while viewing: `n` must show
        // its TITLE> prompt (and the editor after it) even before the first save.
        if self.missing && matches!(self.mode, Mode::View { .. }) {
            let msg = format!("  no notes yet \u{2014} press n to create one ({})", self.path.display());
            f.render_widget(Paragraph::new(msg).style(t.frame), body_area);
            return;
        }

        let wide = area.width >= 80;
        let (list_area, content_area) = if wide {
            let chunks =
                Layout::default().direction(Direction::Horizontal).constraints([Constraint::Length(32), Constraint::Min(0)]).split(body_area);
            (chunks[0], chunks[1])
        } else {
            let list_h = body_area.height.min(6).max(1);
            let chunks =
                Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(list_h), Constraint::Min(0)]).split(body_area);
            (chunks[0], chunks[1])
        };

        let editing = matches!(self.mode, Mode::Edit(_));
        self.draw_list(f, list_area, t, !editing);
        let content_title = if editing { " EDIT " } else { " PREVIEW " };
        let content_block = pane_block(content_title, editing, t);
        // A bordered block always eats exactly one row/col per side, whatever
        // its style/title — reuse that for the inner rect without building
        // (and discarding) a second styled block.
        let content_inner = Block::bordered().inner(content_area);
        self.view_page.set(content_inner.height as usize);

        match &self.mode {
            Mode::Edit(session_cell) => {
                let mut session = session_cell.borrow_mut();
                let editor_theme = EditorTheme::default().block(content_block);
                f.render_widget(EditorView::new(&mut session.state).theme(editor_theme), content_area);
                if let Some(pos) = session.state.cursor_screen_position() {
                    let max_x = (content_inner.x + content_inner.width).saturating_sub(1).max(content_inner.x);
                    let max_y = (content_inner.y + content_inner.height).saturating_sub(1).max(content_inner.y);
                    let x = pos.x.clamp(content_inner.x, max_x);
                    let y = pos.y.clamp(content_inner.y, max_y);
                    f.set_cursor_position((x, y));
                }
            }
            _ => {
                f.render_widget(content_block, content_area);
                let body: Vec<Line> = self
                    .notes
                    .get(self.selected)
                    .map(|n| n.body.iter().map(|l| Line::from(Span::styled(l.clone(), t.text))).collect())
                    .unwrap_or_default();
                let scroll = match self.mode {
                    Mode::View { scroll } => scroll.min(u16::MAX as usize) as u16,
                    _ => 0,
                };
                let p = Paragraph::new(body).wrap(Wrap { trim: false }).scroll((scroll, 0));
                f.render_widget(p, content_inner);
            }
        }
    }

    fn overview(&self, width: u16, _height: u16, t: Theme) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(Span::styled(" NOTES", t.title)), Line::from(format!("  {} notes", self.notes.len()))];
        if let Some(note) = self.notes.get(self.selected) {
            let first = note.body.iter().find(|l| !l.trim().is_empty());
            let combined = match first {
                Some(f) => format!("{}: {f}", note.title),
                None => note.title.clone(),
            };
            let max = (width as usize).saturating_sub(4);
            let truncated: String = combined.chars().take(max).collect();
            lines.push(Line::from(format!("  {truncated}")));
        }
        lines
    }

    fn overview_slot(&self) -> Slot {
        Slot::Right(3)
    }

    fn status(&self) -> String {
        let mode = match self.mode {
            Mode::View { .. } => "view",
            Mode::Edit(_) => "edit",
            Mode::NewTitle(_) => "new",
            Mode::ConfirmDelete => "delete",
        };
        format!("notes {} notes, mode={mode}", self.notes.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(title: &str, body: &[&str]) -> Note {
        Note { title: title.to_string(), body: body.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn title_prompt_prefixes_with_title_arrow() {
        assert_eq!(title_prompt(""), "TITLE> ");
        assert_eq!(title_prompt("Groceries"), "TITLE> Groceries");
    }

    #[test]
    fn mode_label_matches_every_variant() {
        assert_eq!(mode_label(&Mode::View { scroll: 0 }), "VIEW");
        assert_eq!(mode_label(&Mode::NewTitle(String::new())), "NEW TITLE");
        assert_eq!(mode_label(&Mode::ConfirmDelete), "DELETE?");
        let session = EditSession { state: EditorState::default(), handler: EditorEventHandler::default() };
        assert_eq!(mode_label(&Mode::Edit(RefCell::new(session))), "EDIT");
    }

    #[test]
    fn parse_notes_empty_is_zero() {
        assert_eq!(parse_notes("").len(), 0);
    }

    #[test]
    fn parse_notes_untitled_preamble() {
        let notes = parse_notes("stray line\nanother\n# First\nbody\n");
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].title, "Untitled");
        assert_eq!(notes[0].body, vec!["stray line".to_string(), "another".to_string()]);
        assert_eq!(notes[1].title, "First");
        assert_eq!(notes[1].body, vec!["body".to_string()]);
    }

    #[test]
    fn parse_notes_three_notes() {
        let notes = parse_notes("# A\na1\n# B\nb1\n# C\nc1\n");
        let titles: Vec<&str> = notes.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(titles, vec!["A", "B", "C"]);
        assert_eq!(notes[1].body, vec!["b1".to_string()]);
    }

    #[test]
    fn parse_notes_blank_lines_kept_inside_body() {
        let notes = parse_notes("# A\nline1\n\nline2\n# B\nx\n");
        assert_eq!(notes[0].body, vec!["line1".to_string(), String::new(), "line2".to_string()]);
    }

    #[test]
    fn render_after_parse_is_idempotent_on_normalized_input() {
        let x = "# First\nline one\nline two\n\n# Second\nonly line\n\n# Third\nlast note body\n";
        assert_eq!(render_notes(&parse_notes(x)), x);
    }

    #[test]
    fn delete_last_selects_previous() {
        let mut n = Notes::new();
        n.notes = vec![note("A", &[]), note("B", &[]), note("C", &[])];
        n.selected = 2;
        n.delete_selected();
        assert_eq!(n.notes.len(), 2);
        assert_eq!(n.selected, 1);
    }

    #[test]
    fn delete_only_note_empties_list_without_panic() {
        let mut n = Notes::new();
        n.notes = vec![note("A", &[])];
        n.selected = 0;
        n.delete_selected();
        assert!(n.notes.is_empty());
        assert_eq!(n.selected, 0);
    }

    #[test]
    fn confirm_delete_second_d_deletes_other_key_cancels() {
        let mut n = Notes::new();
        n.notes = vec![note("A", &[]), note("B", &[])];
        n.selected = 0;
        n.mode = Mode::ConfirmDelete;
        n.apply_confirm_delete_key(KeyCode::Char('x'));
        assert_eq!(n.notes.len(), 2, "any other key cancels, nothing deleted");
        assert!(matches!(n.mode, Mode::View { .. }));

        n.mode = Mode::ConfirmDelete;
        n.apply_confirm_delete_key(KeyCode::Char('d'));
        assert_eq!(n.notes.len(), 1, "second d deletes");
    }

    /// F3 regression: a confirmed delete must ask the caller to persist, so
    /// the deletion survives a restart — not just live in memory.
    #[test]
    fn confirm_delete_asks_caller_to_save_and_drops_the_note_from_rendered_content() {
        let mut n = Notes::new();
        n.notes = vec![note("A", &["a-body"]), note("B", &["b-body"])];
        n.selected = 0;
        n.mode = Mode::ConfirmDelete;
        let effect = n.apply_confirm_delete_key(KeyCode::Char('d'));
        assert!(matches!(effect, Some(Effect::Save)), "a confirmed delete must ask the caller to persist");
        let rendered = render_notes(&n.notes);
        assert!(!rendered.contains("# A"), "deleted note must be gone from the rendered content: {rendered:?}");
        assert!(rendered.contains("# B"));

        n.mode = Mode::ConfirmDelete;
        let effect = n.apply_confirm_delete_key(KeyCode::Char('x'));
        assert!(matches!(effect, Some(Effect::None)), "a cancelled delete must not ask for a save");
    }

    /// M4: in Edit mode the editor owns every key (q, digits, ...) except
    /// Ctrl+C, which must still reach the shell so it can quit.
    #[test]
    fn edit_mode_lets_ctrl_c_through_to_quit() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let mut n = Notes::new();
        n.notes = vec![note("A", &["line"])];
        n.selected = 0;
        n.enter_edit();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!n.on_key(ctrl_c, &ctx), "Ctrl+C must not be consumed, so the shell can quit");
        assert!(matches!(n.mode, Mode::Edit(_)), "still in Edit mode, just not consumed");
        n.mode = Mode::NewTitle(String::new());
        assert!(!n.on_key(ctrl_c, &ctx), "Ctrl+C is not typed into the title prompt either");
    }

    #[test]
    fn new_title_enter_creates_note_and_enters_edit() {
        let mut n = Notes::new();
        n.mode = Mode::NewTitle("Groceries".to_string());
        n.apply_new_title_key(KeyCode::Enter);
        assert_eq!(n.notes.len(), 1);
        assert_eq!(n.notes[0].title, "Groceries");
        assert_eq!(n.selected, 0);
        assert!(matches!(n.mode, Mode::Edit(_)));
    }

    #[test]
    fn new_title_esc_cancels_without_creating() {
        let mut n = Notes::new();
        n.mode = Mode::NewTitle("Groceries".to_string());
        n.apply_new_title_key(KeyCode::Esc);
        assert!(n.notes.is_empty());
        assert!(matches!(n.mode, Mode::View { .. }));
    }

    #[test]
    fn new_title_backspace_edits_input() {
        let mut n = Notes::new();
        n.mode = Mode::NewTitle("Grocery".to_string());
        n.apply_new_title_key(KeyCode::Backspace);
        assert!(matches!(&n.mode, Mode::NewTitle(s) if s == "Grocer"));
    }

    #[test]
    fn clamp_scroll_never_exceeds_len_minus_page() {
        assert_eq!(clamp_scroll(0, 10, 5), 0);
        assert_eq!(clamp_scroll(100, 10, 5), 5);
        assert_eq!(clamp_scroll(3, 10, 5), 3);
        assert_eq!(clamp_scroll(100, 3, 10), 0, "page bigger than content clamps to 0");
    }

    #[test]
    fn resolve_relative_and_absolute() {
        let exe_dir = Path::new("C:/pipboy");
        assert_eq!(resolve("notes.md", exe_dir), PathBuf::from("C:/pipboy/notes.md"));
        #[cfg(windows)]
        assert_eq!(resolve("D:/data/notes.md", exe_dir), PathBuf::from("D:/data/notes.md"));
    }

    /// Single-mode editor: typing inserts, Esc saves and returns to View.
    #[test]
    fn typing_inserts_and_esc_saves_and_leaves() {
        let mut n = Notes::new();
        n.notes = vec![note("A", &["line1"])];
        n.selected = 0;
        n.enter_edit();
        for c in "xy".chars() {
            assert_eq!(n.edit_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)), EditEffect::None);
        }
        assert_eq!(n.edit_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), EditEffect::SaveAndView, "Esc is a save point, not a vim mode switch");
        assert_eq!(n.notes[0].body, vec!["xyline1".to_string()], "typed text was inserted and kept");
    }

    #[test]
    fn body_from_lines_empty_buffer_is_empty_vec() {
        assert_eq!(body_from_lines(&Lines::from("")), Vec::<String>::new());
    }

    #[test]
    fn body_from_lines_trims_trailing_blank_keeps_internal() {
        let body = body_from_lines(&Lines::from("a\n\nb\n\n\n"));
        assert_eq!(body, vec!["a".to_string(), String::new(), "b".to_string()]);
    }

    #[test]
    fn default_cfg() {
        let cfg = NotesCfg::default();
        assert_eq!(cfg.file, "notes.md");
    }
}
