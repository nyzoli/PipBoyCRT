//! NEWS module: Hacker News front page + configured RSS/Atom feeds.

use crate::module::{Ctx, Module, Notice, Slot};
use crate::style::Theme;
use crate::ui::widgets::truncate;
use chrono::{DateTime, Local};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use serde::Deserialize;
use std::cell::Cell;
use std::sync::mpsc::{self, Receiver, Sender as StdSender};
use std::time::{Duration, Instant};
use stream_download::http::reqwest;

const REFRESH: u64 = 10 * 60;
const BACKOFF_MIN: u64 = 60;
const BACKOFF_MAX: u64 = 8 * 60;
const HN_URL: &str = "https://hn.algolia.com/api/v1/search?tags=front_page";

#[derive(Deserialize, Clone)]
#[serde(default)]
pub struct NewsCfg {
    pub feeds: Vec<String>,
    pub limit: u32,
}

impl Default for NewsCfg {
    fn default() -> Self {
        Self { feeds: Vec::new(), limit: 30 }
    }
}

#[derive(Clone, Default)]
struct Item {
    title: String,
    url: String,
    meta: String,
    /// Article/entry body, stripped of markup. `None` when the source gave
    /// no content/summary text; the reader can then fetch it on demand.
    body: Option<String>,
    /// State of an on-demand fetch for this item, when `body` is `None`.
    fetch: FetchStatus,
}

/// State of an on-demand article fetch (only meaningful while `Item::body` is `None`).
#[derive(Clone, Default)]
enum FetchStatus {
    #[default]
    Idle,
    Fetching,
    Failed(String),
}

#[derive(Default)]
struct Source {
    name: String,
    items: Vec<Item>,
    err: Option<String>,
    updated: Option<DateTime<Local>>,
}

impl Source {
    fn new(name: &str) -> Self {
        Self { name: name.to_string(), ..Default::default() }
    }
}

/// Applies a completed on-demand fetch to whichever current item has this
/// `url`. A refresh may have replaced `Source::items` wholesale while the
/// fetch was in flight, moving the story to another index or dropping it
/// entirely — searching by url (instead of the old index pair) means a
/// stale completion can only ever land on the right story, or on none.
fn apply_article(sources: &mut [Source], url: &str, result: Result<String, String>) {
    for src in sources.iter_mut() {
        if let Some(item) = src.items.iter_mut().find(|it| it.url == url) {
            match result {
                Ok(text) => {
                    item.body = Some(text);
                    item.fetch = FetchStatus::Idle;
                }
                Err(msg) => item.fetch = FetchStatus::Failed(msg),
            }
            return;
        }
    }
}

#[derive(Clone)]
enum SourceSpec {
    Hn,
    Feed(String),
}

enum NewsEvent {
    Source { idx: usize, items: Vec<Item> },
    Error { idx: usize, msg: String },
    /// Result of an on-demand article fetch, keyed by the item's `url` (not
    /// an index): a refresh that lands mid-fetch replaces `Source::items`
    /// wholesale, so an index pair could silently name a different story by
    /// the time this arrives. Applied via `apply_article`.
    Article { url: String, result: Result<String, String> },
}

/// Which body the tab currently shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    List,
    Reader { scroll: u16 },
}

pub struct News {
    cfg: NewsCfg,
    sources: Vec<Source>,
    sel: (usize, usize),
    view: View,
    /// Height last given to the reader's body area, for PgUp/PgDn paging.
    /// `Cell` because `draw` only borrows `&self`.
    reader_height: Cell<u16>,
    rx: Option<Receiver<NewsEvent>>,
    tx_refresh: Option<tokio::sync::mpsc::Sender<()>>,
    /// Clone of the event channel sender, for spawning on-demand article fetches.
    tx_events: Option<StdSender<NewsEvent>>,
}

impl News {
    pub fn new() -> Self {
        Self {
            cfg: NewsCfg::default(),
            sources: Vec::new(),
            sel: (0, 0),
            view: View::List,
            reader_height: Cell::new(10),
            rx: None,
            tx_refresh: None,
            tx_events: None,
        }
    }

    fn move_sel(&mut self, delta: i32) {
        let len = self.sources.get(self.sel.0).map(|s| s.items.len()).unwrap_or(0);
        if len == 0 {
            self.sel.1 = 0;
            return;
        }
        self.sel.1 = (self.sel.1 as i32 + delta).clamp(0, len as i32 - 1) as usize;
    }

    fn switch_source(&mut self, delta: i32) {
        let n = self.sources.len();
        if n == 0 {
            self.sel = (0, 0);
            return;
        }
        self.sel.0 = (self.sel.0 as i32 + delta).rem_euclid(n as i32) as usize;
        self.sel.1 = 0;
    }

    fn selected(&self) -> Option<&Item> {
        self.sources.get(self.sel.0).and_then(|s| s.items.get(self.sel.1))
    }

    fn open_selected(&self, ctx: &Ctx) {
        if let Some(url) = self.selected().map(|it| it.url.clone()) {
            if !url.is_empty() {
                if let Err(e) = crate::open::open_url(&url) {
                    let _ = ctx.notify.send(Notice::Footer(format!("news: open failed: {e}")));
                }
            }
        }
    }

    /// Fetch the selected item's article if it has no body and no fetch is
    /// already in flight for it. No-op otherwise (body present, empty URL, or
    /// already fetching).
    fn fetch_selected(&mut self, ctx: &Ctx) {
        let key = self.sel;
        let (url, already) = match self.selected() {
            Some(it) if it.body.is_none() => (it.url.clone(), matches!(it.fetch, FetchStatus::Fetching)),
            _ => return,
        };
        if url.is_empty() || already {
            return;
        }
        if let Some(item) = self.sources.get_mut(key.0).and_then(|s| s.items.get_mut(key.1)) {
            item.fetch = FetchStatus::Fetching;
        }
        if let Some(tx) = &self.tx_events {
            spawn_fetch(&ctx.rt, url, tx.clone());
        }
    }

    /// Clamp `scroll` to the selected item's body line count and apply it.
    fn set_scroll(&mut self, scroll: u16) {
        let max_lines = self.selected().and_then(|it| it.body.as_deref()).map(|b| b.lines().count() as u16).unwrap_or(1);
        let max_scroll = max_lines.saturating_sub(1);
        self.view = View::Reader { scroll: scroll.min(max_scroll) };
    }

    fn on_key_list(&mut self, key: KeyEvent, ctx: &Ctx) -> bool {
        match key.code {
            KeyCode::Up => {
                self.move_sel(-1);
                true
            }
            KeyCode::Down => {
                self.move_sel(1);
                true
            }
            KeyCode::Char('[') => {
                self.switch_source(-1);
                true
            }
            KeyCode::Char(']') => {
                self.switch_source(1);
                true
            }
            KeyCode::Char('r') => {
                if let Some(tx) = &self.tx_refresh {
                    let _ = tx.try_send(());
                }
                true
            }
            KeyCode::Char('o') => {
                self.open_selected(ctx);
                true
            }
            KeyCode::Enter => {
                if self.selected().is_some() {
                    self.set_scroll(0);
                }
                true
            }
            _ => false,
        }
    }

    fn on_key_reader(&mut self, key: KeyEvent, scroll: u16, ctx: &Ctx) -> bool {
        match key.code {
            KeyCode::Up => {
                self.set_scroll(scroll.saturating_sub(1));
                true
            }
            KeyCode::Down => {
                self.set_scroll(scroll.saturating_add(1));
                true
            }
            KeyCode::Enter => {
                self.fetch_selected(ctx);
                true
            }
            KeyCode::PageUp => {
                let page = self.reader_height.get().saturating_sub(3).max(1);
                self.set_scroll(scroll.saturating_sub(page));
                true
            }
            KeyCode::PageDown => {
                let page = self.reader_height.get().saturating_sub(3).max(1);
                self.set_scroll(scroll.saturating_add(page));
                true
            }
            KeyCode::Backspace | KeyCode::Esc => {
                self.view = View::List;
                true
            }
            KeyCode::Char('o') => {
                self.open_selected(ctx);
                true
            }
            _ => false,
        }
    }
}

impl Module for News {
    fn id(&self) -> &'static str {
        "news"
    }
    fn title(&self) -> &'static str {
        "NEWS"
    }
    fn describe(&self) -> &'static str {
        "Hacker News and your RSS feeds, with a reader"
    }
    fn help(&self) -> &'static str {
        match self.view {
            View::List => "↑/↓ select   [ ] source   enter read   o browser   r refresh   1-9 tabs   q quit",
            View::Reader { .. } => {
                let can_fetch = self
                    .selected()
                    .map(|it| it.body.is_none() && !matches!(it.fetch, FetchStatus::Fetching))
                    .unwrap_or(false);
                if can_fetch {
                    "↑/↓ scroll   enter fetch article   o browser   esc/backspace back   1-9 tabs   q quit"
                } else {
                    "↑/↓ scroll   o browser   esc/backspace back   1-9 tabs   q quit"
                }
            }
        }
    }

    fn start(&mut self, ctx: &Ctx) {
        let (cfg, notice) = ctx.config.section::<NewsCfg>(self.id());
        if let Some(n) = notice {
            let _ = ctx.notify.send(Notice::Footer(n));
        }
        self.cfg = cfg;

        let mut sources = vec![Source::new("Hacker News")];
        let mut specs = vec![SourceSpec::Hn];
        for feed in &self.cfg.feeds {
            sources.push(Source::new(&host_name(feed)));
            specs.push(SourceSpec::Feed(feed.clone()));
        }
        self.sources = sources;

        let (tx, rx) = mpsc::channel::<NewsEvent>();
        self.rx = Some(rx);
        self.tx_events = Some(tx.clone());
        let limit = self.cfg.limit.max(1) as usize;
        let notify = ctx.notify.clone();
        self.tx_refresh = Some(spawn_refresh(&ctx.rt, specs, limit, tx, notify));
    }

    fn poll(&mut self, _ctx: &Ctx) -> usize {
        let mut n = 0;
        if let Some(rx) = &self.rx {
            while let Ok(ev) = rx.try_recv() {
                n += 1;
                match ev {
                    NewsEvent::Source { idx, items } => {
                        if let Some(s) = self.sources.get_mut(idx) {
                            s.err = None;
                            s.updated = Some(Local::now());
                            let len = items.len();
                            s.items = items;
                            if idx == self.sel.0 {
                                self.sel.1 = self.sel.1.min(len.saturating_sub(1));
                            }
                        }
                    }
                    NewsEvent::Error { idx, msg } => {
                        if let Some(s) = self.sources.get_mut(idx) {
                            s.err = Some(msg);
                        }
                    }
                    NewsEvent::Article { url, result } => apply_article(&mut self.sources, &url, result),
                }
            }
        }
        n
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &Ctx) -> bool {
        match self.view {
            View::List => self.on_key_list(key, ctx),
            View::Reader { scroll } => self.on_key_reader(key, scroll, ctx),
        }
    }

    fn draw(&self, f: &mut Frame, area: Rect, t: Theme) {
        match self.view {
            View::List => {
                let rows =
                    Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(2)]).split(area);
                draw_source_strip(f, rows[0], t, &self.sources, self.sel.0);
                draw_list(f, rows[1], t, &self.sources, self.sel);
                draw_footer(f, rows[2], t, &self.sources, self.sel);
            }
            View::Reader { scroll } => self.draw_reader(f, area, t, scroll),
        }
    }

    fn overview(&self, width: u16, height: u16, t: Theme) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(Span::styled(" NEWS", t.title))];
        if let Some(src) = self.sources.get(self.sel.0) {
            let w = (width as usize).saturating_sub(4);
            for it in src.items.iter().take(3) {
                lines.push(Line::from(truncate(&it.title, w)));
            }
        }
        lines.truncate(height.max(1) as usize);
        lines
    }

    fn overview_slot(&self) -> Slot {
        Slot::Right(2)
    }

    fn status(&self) -> String {
        let items: usize = self.sources.iter().map(|s| s.items.len()).sum();
        let err = self.sources.iter().filter(|s| s.err.is_some()).count();
        format!("news {} sources, {} items, err={}", self.sources.len(), items, err)
    }
}

impl News {
    fn draw_reader(&self, f: &mut Frame, area: Rect, t: Theme, scroll: u16) {
        let rows =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)]).split(area);
        let item = self.selected();
        let title = item.map(|it| it.title.as_str()).unwrap_or("");
        let meta = item.map(|it| it.meta.as_str()).unwrap_or("");
        f.render_widget(Paragraph::new(Line::from(Span::styled(title.to_string(), t.title))), rows[0]);
        f.render_widget(Paragraph::new(Line::from(Span::styled(meta.to_string(), t.frame))), rows[1]);
        self.reader_height.set(rows[2].height);
        let body = match item.and_then(|it| it.body.clone()) {
            Some(b) => b,
            None => match item.map(|it| &it.fetch) {
                Some(FetchStatus::Fetching) => "fetching…".to_string(),
                Some(FetchStatus::Failed(msg)) if msg == UNREADABLE => {
                    "no readable content — press o to open in browser".to_string()
                }
                Some(FetchStatus::Failed(msg)) => msg.clone(),
                _ => "press enter to fetch the article · o to open in browser".to_string(),
            },
        };
        let p = Paragraph::new(body).wrap(Wrap { trim: false }).scroll((scroll, 0));
        f.render_widget(p, rows[2]);
    }
}

fn draw_source_strip(f: &mut Frame, area: Rect, t: Theme, sources: &[Source], active: usize) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, s) in sources.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        let style = if i == active { t.tab_active } else { t.frame };
        spans.push(Span::styled(format!(" {} ", s.name), style));
    }
    let left_w: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let right = sources
        .get(active)
        .and_then(|s| s.updated)
        .map(|dt| format!("updated {}", dt.format("%H:%M")))
        .unwrap_or_default();
    let pad = (area.width as usize).saturating_sub(left_w + right.chars().count());
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(right, t.frame));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_list(f: &mut Frame, area: Rect, t: Theme, sources: &[Source], sel: (usize, usize)) {
    let mut items: Vec<ListItem> = Vec::new();
    let mut err_row = false;
    if let Some(src) = sources.get(sel.0) {
        if let Some(err) = &src.err {
            items.push(ListItem::new(format!("n/a: {err}")).style(t.warn));
            err_row = true;
        }
        let w = area.width.saturating_sub(2) as usize;
        for it in &src.items {
            items.push(ListItem::new(truncate(&it.title, w)));
        }
        if items.is_empty() {
            items.push(ListItem::new("(no items)").style(t.frame));
        }
    } else {
        items.push(ListItem::new("no sources configured").style(t.frame));
    }

    let mut state = ListState::default();
    if let Some(src) = sources.get(sel.0) {
        if !src.items.is_empty() {
            state.select(Some(sel.1 + if err_row { 1 } else { 0 }));
        }
    }
    let list = List::new(items).highlight_style(t.tab_active);
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_footer(f: &mut Frame, area: Rect, t: Theme, sources: &[Source], sel: (usize, usize)) {
    let (meta, url) = sources
        .get(sel.0)
        .and_then(|s| s.items.get(sel.1))
        .map(|it| (it.meta.clone(), it.url.clone()))
        .unwrap_or_default();
    let w = area.width as usize;
    let lines = vec![
        Line::from(Span::styled(meta, t.value)),
        Line::from(Span::styled(truncate(&url, w), t.frame)),
    ];
    f.render_widget(Paragraph::new(lines), area);
}

/// "host.example.com" from a feed URL, for the source tab label.
fn host_name(url: &str) -> String {
    let s = url.trim_start_matches("https://").trim_start_matches("http://");
    s.split('/').next().unwrap_or(s).to_string()
}

/// Strips HTML markup from feed/article content: `<br>`, `</p>`, `</li>`
/// become a newline, every other tag is dropped, named/numeric entities are
/// decoded, and runs of 3+ newlines collapse to 2.
fn strip_html(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let n = chars.len();
    let mut no_tags = String::with_capacity(input.len());
    let mut i = 0;
    while i < n {
        if chars[i] == '<' {
            if let Some(end) = (i..n).find(|&j| chars[j] == '>') {
                let tag: String = chars[i + 1..end].iter().collect();
                let tag = tag.trim().trim_end_matches('/').to_ascii_lowercase();
                if tag == "br" || tag == "/p" || tag == "/li" {
                    no_tags.push('\n');
                }
                i = end + 1;
                continue;
            }
        }
        no_tags.push(chars[i]);
        i += 1;
    }
    collapse_and_trim(&decode_entities(&no_tags))
}

fn decode_entities(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < n {
        if chars[i] == '&' {
            if let Some(semi_rel) = chars[i + 1..].iter().position(|&c| c == ';') {
                let semi = i + 1 + semi_rel;
                let ent: String = chars[i + 1..semi].iter().collect();
                let decoded = match ent.as_str() {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "#39" | "apos" => Some('\''),
                    "nbsp" => Some(' '),
                    _ => None,
                };
                if let Some(c) = decoded {
                    out.push(c);
                    i = semi + 1;
                    continue;
                }
                let numeric = if let Some(hex) = ent.strip_prefix('#').and_then(|r| r.strip_prefix(['x', 'X'])) {
                    u32::from_str_radix(hex, 16).ok()
                } else {
                    ent.strip_prefix('#').and_then(|r| r.parse::<u32>().ok())
                };
                if let Some(c) = numeric.and_then(char::from_u32) {
                    out.push(c);
                    i = semi + 1;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn collapse_and_trim(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut run = 0u32;
    for c in input.chars() {
        if c == '\r' {
            continue;
        }
        if c == '\n' {
            run += 1;
            if run <= 2 {
                out.push('\n');
            }
        } else {
            run = 0;
            out.push(c);
        }
    }
    out.trim().to_string()
}

/// `strip_html` the raw markup, `None` if nothing (or only whitespace) is left.
fn body_from_html(raw: Option<&str>) -> Option<String> {
    let stripped = strip_html(raw?);
    if stripped.is_empty() {
        None
    } else {
        Some(stripped)
    }
}

/// Sentinel error string for "fetched fine, but too little text to read" —
/// distinct from a real network/HTTP error, whose message is shown as-is.
const UNREADABLE: &str = "unreadable";
const MAX_FETCH_BYTES: usize = 1_000_000;
const ARTICLE_MIN_CHARS: usize = 200;

/// `article` is too short to be worth showing (boilerplate/JS-only page, paywall, etc).
fn is_unreadable(article: &str) -> bool {
    article.chars().count() < ARTICLE_MIN_CHARS
}

/// Case-insensitive `<tag ...>` search from byte offset `from`; `tag` must be
/// followed by whitespace, `/` or `>` (so `<article` doesn't match `<articlefoo`).
/// Returns `(tag_start, byte offset right after the closing '>')`.
fn find_open_tag(lower: &str, tag: &str, from: usize) -> Option<(usize, usize)> {
    let needle = format!("<{tag}");
    let mut at = from;
    loop {
        let rel = lower.get(at..)?.find(&needle)?;
        let start = at + rel;
        let after_name = start + needle.len();
        let boundary = matches!(lower[after_name..].chars().next(), None | Some('>' | '/' | ' ' | '\t' | '\n' | '\r'));
        if boundary {
            let gt = lower[after_name..].find('>')?;
            return Some((start, after_name + gt + 1));
        }
        at = after_name;
    }
}

/// Byte offset of the matching `</tag>`'s `<` from offset `from`, plus the
/// offset right after its closing `>`.
fn find_close_tag(lower: &str, tag: &str, from: usize) -> Option<(usize, usize)> {
    let needle = format!("</{tag}");
    let rel = lower.get(from..)?.find(&needle)?;
    let start = from + rel;
    let after_name = start + needle.len();
    let gt = lower[after_name..].find('>')?;
    Some((start, after_name + gt + 1))
}

/// Inner HTML of the first `<tag>...</tag>` (non-nested-aware: the first open
/// to the first matching close), or `None` if `tag` doesn't appear.
fn extract_container(html: &str, tag: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let (_, open_end) = find_open_tag(&lower, tag, 0)?;
    let (close_start, _) = find_close_tag(&lower, tag, open_end)?;
    if close_start < open_end {
        return None;
    }
    Some(html[open_end..close_start].to_string())
}

/// Removes every `<tag>...</tag>` block (tag and content) for each name in `tags`.
/// An open tag with no matching close is left in place and skipped over —
/// scanning continues past it so a later, properly closed block of the same
/// tag name still gets stripped.
// ponytail: rescans from the top after each removal (O(n) tags on a <=1MB doc) — fine at this size.
fn strip_blocks(html: &str, tags: &[&str]) -> String {
    let mut out = html.to_string();
    for tag in tags {
        let mut from = 0usize;
        loop {
            let lower = out.to_ascii_lowercase();
            let Some((open_start, open_end)) = find_open_tag(&lower, tag, from) else { break };
            match find_close_tag(&lower, tag, open_end) {
                Some((_, close_end)) => {
                    out.replace_range(open_start..close_end, "");
                    from = 0;
                }
                None => from = open_end,
            }
        }
    }
    out
}

/// Drops every remaining tag; `<p> <h1>-<h3> <li> <br> <div>` (open or close)
/// become a line break first.
fn tags_to_breaks_and_strip(input: &str) -> String {
    const BREAK_TAGS: [&str; 7] = ["p", "h1", "h2", "h3", "li", "br", "div"];
    let chars: Vec<char> = input.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < n {
        if chars[i] == '<' {
            if let Some(end) = (i..n).find(|&j| chars[j] == '>') {
                let raw: String = chars[i + 1..end].iter().collect();
                let name = raw.trim().trim_start_matches('/');
                let name = name.split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("").to_ascii_lowercase();
                if BREAK_TAGS.contains(&name.as_str()) {
                    out.push('\n');
                }
                i = end + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Collapses runs of whitespace within a line to one space, and drops blank
/// lines beyond a single one between paragraphs (also trims lead/trail).
fn collapse_ws(input: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut pending_blank = false;
    for raw in input.split('\n') {
        let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            if !lines.is_empty() {
                pending_blank = true;
            }
        } else {
            if pending_blank {
                lines.push(String::new());
                pending_blank = false;
            }
            lines.push(collapsed);
        }
    }
    lines.join("\n")
}

/// Cleans one candidate container's raw inner HTML into plain text: drops
/// script/style/nav/header/footer/aside/noscript blocks, turns paragraph/list
/// boundaries into line breaks, strips remaining tags, decodes entities,
/// collapses whitespace.
fn clean_container(raw: &str) -> String {
    let cleaned = strip_blocks(raw, &["script", "style", "nav", "header", "footer", "aside", "noscript"]);
    let text = tags_to_breaks_and_strip(&cleaned);
    collapse_ws(&decode_entities(&text))
}

/// Extracts readable article text from a full HTML page. Some sites wrap a
/// short teaser card in the first `<article>`, so the real content is missed
/// by taking that first match alone; instead this cleans the first
/// `<article>`, `<main>` and `<body>` each independently and keeps whichever
/// yields the most text, falling back to the whole input when none of the
/// three tags appear at all.
fn extract_article(html: &str) -> String {
    ["article", "main", "body"]
        .into_iter()
        .filter_map(|tag| extract_container(html, tag))
        .map(|raw| clean_container(&raw))
        .max_by_key(|text| text.chars().count())
        .unwrap_or_else(|| clean_container(html))
}

async fn fetch_article(url: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent("PipBoyCRT")
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(url).send().await.map_err(|e| e.to_string())?;
    let mut resp = resp.error_for_status().map_err(|e| e.to_string())?;
    let is_html = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| {
            let ct = ct.trim_start().to_ascii_lowercase();
            ct.starts_with("text/html") || ct.starts_with("application/xhtml+xml")
        });
    if !is_html {
        return Err(UNREADABLE.to_string());
    }
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        buf.extend_from_slice(&chunk);
        if buf.len() >= MAX_FETCH_BYTES {
            buf.truncate(MAX_FETCH_BYTES);
            break;
        }
    }
    let html = String::from_utf8_lossy(&buf);
    let article = extract_article(&html);
    if is_unreadable(&article) {
        return Err(UNREADABLE.to_string());
    }
    Ok(article)
}

/// Fire-and-forget tokio task: fetches one article and reports the result on `tx`.
fn spawn_fetch(rt: &tokio::runtime::Handle, url: String, tx: StdSender<NewsEvent>) {
    rt.spawn(async move {
        let result = fetch_article(&url).await;
        let _ = tx.send(NewsEvent::Article { url, result });
    });
}

#[derive(Deserialize)]
struct HnResp {
    hits: Vec<HnHit>,
}

#[derive(Deserialize)]
struct HnHit {
    title: Option<String>,
    url: Option<String>,
    points: Option<i64>,
    num_comments: Option<i64>,
    story_text: Option<String>,
    #[serde(rename = "objectID")]
    object_id: String,
}

fn parse_hn(json: &str, limit: usize) -> Result<Vec<Item>, String> {
    let r: HnResp = serde_json::from_str(json).map_err(|e| e.to_string())?;
    Ok(r.hits
        .into_iter()
        .take(limit)
        .map(|h| {
            let url = h.url.unwrap_or_else(|| format!("https://news.ycombinator.com/item?id={}", h.object_id));
            let title = h.title.unwrap_or_else(|| "(untitled)".to_string());
            let meta = format!("{} pts · {} comments", h.points.unwrap_or(0), h.num_comments.unwrap_or(0));
            let body = body_from_html(h.story_text.as_deref());
            Item { title, url, meta, body, fetch: FetchStatus::Idle }
        })
        .collect())
}

fn parse_feed(bytes: &[u8], limit: usize) -> Result<Vec<Item>, String> {
    let feed = feed_rs::parser::parse(bytes).map_err(|e| e.to_string())?;
    Ok(feed
        .entries
        .into_iter()
        .take(limit)
        .map(|e| {
            let title = e.title.map(|t| t.content).unwrap_or_else(|| "(untitled)".to_string());
            let url = e.links.first().map(|l| l.href.clone()).unwrap_or_default();
            let meta = e
                .published
                .or(e.updated)
                .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default();
            let raw_body =
                e.content.and_then(|c| c.body).or_else(|| e.summary.map(|s| s.content));
            let body = body_from_html(raw_body.as_deref());
            Item { title, url, meta, body, fetch: FetchStatus::Idle }
        })
        .collect())
}

async fn fetch_source(client: &reqwest::Client, spec: &SourceSpec, limit: usize) -> Result<Vec<Item>, String> {
    match spec {
        SourceSpec::Hn => {
            let url = format!("{HN_URL}&hitsPerPage={limit}");
            let body = client
                .get(&url)
                .send()
                .await
                .map_err(|e| e.to_string())?
                .error_for_status()
                .map_err(|e| e.to_string())?
                .text()
                .await
                .map_err(|e| e.to_string())?;
            parse_hn(&body, limit)
        }
        SourceSpec::Feed(url) => {
            let body = client
                .get(url)
                .send()
                .await
                .map_err(|e| e.to_string())?
                .error_for_status()
                .map_err(|e| e.to_string())?
                .bytes()
                .await
                .map_err(|e| e.to_string())?;
            parse_feed(body.as_ref(), limit)
        }
    }
}

/// Single tokio task: fetches every source in order (HN first, then feeds),
/// on start / every 10 min / on the returned channel; per-source error backs
/// off 1 -> 8 minutes independently of the others.
fn spawn_refresh(
    rt: &tokio::runtime::Handle,
    specs: Vec<SourceSpec>,
    limit: usize,
    tx: StdSender<NewsEvent>,
    notify: StdSender<Notice>,
) -> tokio::sync::mpsc::Sender<()> {
    let (refresh_tx, mut refresh_rx) = tokio::sync::mpsc::channel::<()>(1);
    rt.spawn(async move {
        let client = match reqwest::Client::builder().timeout(Duration::from_secs(20)).build() {
            Ok(c) => c,
            Err(e) => {
                let _ = notify.send(Notice::Footer(format!("news: http client: {e}")));
                return;
            }
        };
        let n = specs.len();
        let mut next_due = vec![Instant::now(); n];
        let mut backoff = vec![BACKOFF_MIN; n];
        loop {
            let now = Instant::now();
            for (i, spec) in specs.iter().enumerate() {
                if next_due[i] <= now {
                    let res = fetch_source(&client, spec, limit).await;
                    let ev = match res {
                        Ok(items) => {
                            backoff[i] = BACKOFF_MIN;
                            next_due[i] = Instant::now() + Duration::from_secs(REFRESH);
                            NewsEvent::Source { idx: i, items }
                        }
                        Err(msg) => {
                            next_due[i] = Instant::now() + Duration::from_secs(backoff[i]);
                            backoff[i] = (backoff[i] * 2).min(BACKOFF_MAX);
                            NewsEvent::Error { idx: i, msg }
                        }
                    };
                    if tx.send(ev).is_err() {
                        return;
                    }
                }
            }
            let now = Instant::now();
            let wait = next_due.iter().map(|d| d.saturating_duration_since(now)).min().unwrap_or(Duration::from_secs(REFRESH));
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                r = refresh_rx.recv() => {
                    if r.is_none() {
                        return;
                    }
                    for d in next_due.iter_mut() {
                        *d = Instant::now();
                    }
                }
            }
        }
    });
    refresh_tx
}

#[cfg(test)]
mod tests {
    use super::*;

    const HN_FIXTURE: &str = r#"{
      "hits": [
        {"title": "Rust is great", "url": "https://example.com/rust", "points": 312, "num_comments": 88, "objectID": "1001", "story_text": null},
        {"title": "Show HN: thing", "url": null, "points": 5, "num_comments": 0, "objectID": "1002", "story_text": "<p>hello &amp; world<br>line two</p>"}
      ]
    }"#;

    const RSS_FIXTURE: &str = r#"<?xml version="1.0"?>
    <rss version="2.0"><channel><title>Test Feed</title>
    <item><title>Item One</title><link>https://example.com/1</link><pubDate>Thu, 10 Sep 2026 09:00:00 GMT</pubDate><description>Some &lt;b&gt;summary&lt;/b&gt; text</description></item>
    <item><title>Item Two</title><link>https://example.com/2</link><pubDate>Thu, 10 Sep 2026 10:00:00 GMT</pubDate></item>
    </channel></rss>"#;

    #[test]
    fn parses_hn_fixture() {
        let items = parse_hn(HN_FIXTURE, 30).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "Rust is great");
        assert_eq!(items[0].url, "https://example.com/rust");
        assert_eq!(items[0].meta, "312 pts · 88 comments");
        assert_eq!(items[0].body, None);
        assert_eq!(items[1].url, "https://news.ycombinator.com/item?id=1002");
        assert_eq!(items[1].body.as_deref(), Some("hello & world\nline two"));
    }

    #[test]
    fn hn_fixture_respects_limit() {
        let items = parse_hn(HN_FIXTURE, 1).unwrap();
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn bad_hn_json_is_error_not_panic() {
        assert!(parse_hn("not json", 30).is_err());
    }

    #[test]
    fn parses_rss_fixture() {
        let items = parse_feed(RSS_FIXTURE.as_bytes(), 30).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "Item One");
        assert_eq!(items[0].url, "https://example.com/1");
        assert!(!items[0].meta.is_empty());
        // feed-rs itself unescapes/strips the RSS <description> HTML; strip_html is a no-op here.
        assert_eq!(items[0].body.as_deref(), Some("Some summary text"));
        assert_eq!(items[1].body, None);
    }

    #[test]
    fn bad_feed_is_error_not_panic() {
        assert!(parse_feed(b"not a feed", 30).is_err());
    }

    #[test]
    fn strip_html_tags_breaks_and_entities() {
        assert_eq!(strip_html("<p>a &amp; b<br>c</p>"), "a & b\nc");
    }

    #[test]
    fn strip_html_numeric_entities() {
        assert_eq!(strip_html("caf&#233; &#x2603;"), "café ☃");
        assert_eq!(strip_html("it&#39;s"), "it's");
    }

    #[test]
    fn strip_html_collapses_blank_runs_and_trims() {
        assert_eq!(strip_html("  a<br><br><br><br>b  "), "a\n\nb");
    }

    #[test]
    fn body_from_html_empty_is_none() {
        assert_eq!(body_from_html(Some("")), None);
        assert_eq!(body_from_html(Some("<p></p>")), None);
        assert_eq!(body_from_html(None), None);
    }

    #[test]
    fn is_unreadable_threshold() {
        assert!(is_unreadable(&"x".repeat(199)));
        assert!(!is_unreadable(&"x".repeat(200)));
    }

    #[test]
    fn extract_article_picks_longest_over_short_teaser_article() {
        // A short <article> teaser card (common on link-aggregator front pages)
        // must not win over a much longer <main>/<body> just for being first.
        let html = "<html><body>\
            <article><h3>Teaser</h3><p>read more</p></article>\
            <main><p>This is the real full article content with plenty of words to read today.</p></main>\
            </body></html>";
        let text = extract_article(html);
        assert!(text.contains("real full article content"));
        assert!(text.len() > "Teaser\n\nread more".len());
    }

    #[test]
    fn extract_article_nested_containers_take_longest_candidate() {
        // <article> nested inside <main> which is nested inside <body>: body's
        // candidate text is a superset and should win.
        let html = "<body><main><article><p>short bit</p></article><p>more text around it too</p></main></body>";
        let text = extract_article(html);
        assert!(text.contains("short bit"));
        assert!(text.contains("more text around it too"));
    }

    #[test]
    fn extract_article_unclosed_container_falls_back() {
        // An unclosed <article> has no matching close tag, so it yields no
        // candidate; <main> is used instead.
        let html = "<body><article><p>never closed<main><p>real content here today</p></main></body>";
        assert!(extract_article(html).contains("real content here today"));
    }

    #[test]
    fn extract_article_binary_input_does_not_panic() {
        let bytes: Vec<u8> = (0u16..=255).map(|b| b as u8).collect();
        let lossy = String::from_utf8_lossy(&bytes).into_owned();
        let article = extract_article(&lossy);
        // No panic is the main assertion; garbage bytes decode to something
        // readability can flag as unreadable, which is fine either way.
        let _ = is_unreadable(&article);
    }

    #[test]
    fn extract_article_falls_back_to_main() {
        let html = "<html><body><nav>menu</nav><main><p>main content lives here</p></main></body></html>";
        assert_eq!(extract_article(html), "main content lives here");
    }

    #[test]
    fn extract_article_falls_back_to_body_then_whole_input() {
        let html = "<html><body><p>just the body text</p></body></html>";
        assert_eq!(extract_article(html), "just the body text");

        let html_no_wrapper = "<p>bare fragment</p>";
        assert_eq!(extract_article(html_no_wrapper), "bare fragment");
    }

    #[test]
    fn extract_article_drops_script_style_nav_blocks() {
        let html = "<article><script>evil();</script><style>.x{}</style>\
            <nav>Home | About</nav><p>kept text</p></article>";
        assert_eq!(extract_article(html), "kept text");
    }

    #[test]
    fn extract_article_decodes_entities_including_numeric() {
        let html = "<article><p>Tom &amp; Jerry say &quot;caf&#233;&quot; &#x2603;</p></article>";
        assert_eq!(extract_article(html), "Tom & Jerry say \"café\" ☃");
    }

    #[test]
    fn extract_article_breaks_paragraphs_into_lines() {
        let html = "<article><p>first</p><p>second</p><h2>third</h2><li>fourth</li></article>";
        assert_eq!(extract_article(html), "first\n\nsecond\n\nthird\n\nfourth");
    }

    #[test]
    fn move_sel_bounds_on_empty_and_last() {
        let mut n = News::new();
        n.sources = vec![Source::new("A")];
        n.move_sel(-1);
        assert_eq!(n.sel, (0, 0));
        n.move_sel(1);
        assert_eq!(n.sel, (0, 0));
        n.sources[0].items = vec![Item::default(), Item::default(), Item::default()];
        n.move_sel(10);
        assert_eq!(n.sel.1, 2);
        n.move_sel(-10);
        assert_eq!(n.sel.1, 0);
    }

    #[test]
    fn switch_source_wraps() {
        let mut n = News::new();
        n.sources = vec![Source::new("A"), Source::new("B"), Source::new("C")];
        n.switch_source(-1);
        assert_eq!(n.sel.0, 2);
        n.switch_source(1);
        assert_eq!(n.sel.0, 0);
        n.switch_source(1);
        assert_eq!(n.sel.0, 1);
    }

    #[test]
    fn switch_source_empty_is_noop() {
        let mut n = News::new();
        n.switch_source(1);
        assert_eq!(n.sel, (0, 0));
    }

    #[test]
    fn draw_does_not_panic_on_empty_and_tiny() {
        let t = crate::style::Theme::new(crate::config::ThemeKind::Color);
        let news = News::new();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(1, 1)).unwrap();
        terminal.draw(|f| news.draw(f, f.area(), t)).unwrap();

        let mut news2 = News::new();
        news2.sources = vec![Source::new("Hacker News")];
        news2.sources[0].items = vec![Item {
            title: "A title".into(),
            url: "https://x".into(),
            meta: "1 pts".into(),
            body: None,
            ..Default::default()
        }];
        let mut terminal2 = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 10)).unwrap();
        terminal2.draw(|f| news2.draw(f, f.area(), t)).unwrap();

        // Reader view (empty and with an item) at degenerate sizes.
        let mut reader_empty = News::new();
        reader_empty.view = View::Reader { scroll: 0 };
        let mut terminal3 = ratatui::Terminal::new(ratatui::backend::TestBackend::new(1, 1)).unwrap();
        terminal3.draw(|f| reader_empty.draw(f, f.area(), t)).unwrap();

        let mut reader_item = News::new();
        reader_item.sources = vec![Source::new("Hacker News")];
        reader_item.sources[0].items = vec![Item {
            title: "A title".into(),
            url: "https://x".into(),
            meta: "1 pts".into(),
            body: Some("l1\nl2\nl3".into()),
            ..Default::default()
        }];
        reader_item.view = View::Reader { scroll: 0 };
        let mut terminal4 = ratatui::Terminal::new(ratatui::backend::TestBackend::new(3, 2)).unwrap();
        terminal4.draw(|f| reader_item.draw(f, f.area(), t)).unwrap();
    }

    #[test]
    fn status_and_overview_do_not_panic_when_empty() {
        let t = crate::style::Theme::new(crate::config::ThemeKind::Color);
        let n = News::new();
        assert_eq!(n.status(), "news 0 sources, 0 items, err=0");
        assert_eq!(n.overview(20, 4, t).len(), 1);
    }

    #[test]
    fn enter_without_body_still_opens_reader() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let mut n = News::new();
        n.sources = vec![Source::new("A")];
        n.sources[0].items = vec![Item {
            title: "no body".into(),
            url: "https://x".into(),
            meta: String::new(),
            body: None,
            ..Default::default()
        }];
        let key = KeyEvent::from(KeyCode::Enter);
        assert!(n.on_key(key, &ctx));
        assert!(matches!(n.view, View::Reader { scroll: 0 }));
    }

    #[test]
    fn reader_enter_is_noop_when_body_already_present() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let mut n = News::new();
        n.sources = vec![Source::new("A")];
        n.sources[0].items = vec![Item {
            title: "t".into(),
            url: "https://x".into(),
            meta: String::new(),
            body: Some("already here".into()),
            ..Default::default()
        }];
        n.view = View::Reader { scroll: 0 };
        assert!(n.on_key(KeyEvent::from(KeyCode::Enter), &ctx));
        assert!(matches!(n.sources[0].items[0].fetch, FetchStatus::Idle));
        assert_eq!(n.sources[0].items[0].body.as_deref(), Some("already here"));
    }

    #[test]
    fn reader_enter_is_noop_with_empty_url() {
        // Guards against spawning a fetch with nothing to fetch (no network call).
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let mut n = News::new();
        n.sources = vec![Source::new("A")];
        n.sources[0].items = vec![Item {
            title: "t".into(),
            url: String::new(),
            meta: String::new(),
            body: None,
            ..Default::default()
        }];
        n.view = View::Reader { scroll: 0 };
        assert!(n.on_key(KeyEvent::from(KeyCode::Enter), &ctx));
        assert!(matches!(n.sources[0].items[0].fetch, FetchStatus::Idle));
    }

    #[test]
    fn enter_with_body_switches_to_reader() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let mut n = News::new();
        n.sources = vec![Source::new("A")];
        n.sources[0].items = vec![Item {
            title: "has body".into(),
            url: "https://x".into(),
            meta: String::new(),
            body: Some("line1\nline2\nline3".into()),
            ..Default::default()
        }];
        let key = KeyEvent::from(KeyCode::Enter);
        assert!(n.on_key(key, &ctx));
        assert!(matches!(n.view, View::Reader { scroll: 0 }));
    }

    #[test]
    fn reader_scroll_never_exceeds_body_lines() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let mut n = News::new();
        n.sources = vec![Source::new("A")];
        n.sources[0].items = vec![Item {
            title: "t".into(),
            url: "https://x".into(),
            meta: String::new(),
            body: Some("l1\nl2\nl3".into()), // 3 lines -> max scroll 2
            ..Default::default()
        }];
        n.view = View::Reader { scroll: 0 };
        for _ in 0..10 {
            n.on_key(KeyEvent::from(KeyCode::Down), &ctx);
        }
        assert!(matches!(n.view, View::Reader { scroll: 2 }));
        for _ in 0..10 {
            n.on_key(KeyEvent::from(KeyCode::Up), &ctx);
        }
        assert!(matches!(n.view, View::Reader { scroll: 0 }));
    }

    #[test]
    fn reader_backspace_and_esc_return_to_list() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let mut n = News::new();
        n.sources = vec![Source::new("A")];
        n.sources[0].items = vec![Item {
            title: "t".into(),
            url: "https://x".into(),
            meta: String::new(),
            body: Some("l1".into()),
            ..Default::default()
        }];
        n.view = View::Reader { scroll: 0 };
        assert!(n.on_key(KeyEvent::from(KeyCode::Backspace), &ctx));
        assert!(n.view == View::List);
        n.view = View::Reader { scroll: 0 };
        assert!(n.on_key(KeyEvent::from(KeyCode::Esc), &ctx));
        assert!(n.view == View::List);
    }

    #[test]
    fn apply_article_only_updates_the_matching_url() {
        let mut sources = vec![Source::new("A"), Source::new("B")];
        sources[0].items = vec![
            Item { title: "one".into(), url: "https://one".into(), fetch: FetchStatus::Fetching, ..Default::default() },
            Item { title: "two".into(), url: "https://two".into(), fetch: FetchStatus::Fetching, ..Default::default() },
        ];
        sources[1].items =
            vec![Item { title: "three".into(), url: "https://three".into(), fetch: FetchStatus::Fetching, ..Default::default() }];

        apply_article(&mut sources, "https://two", Ok("body of two".to_string()));

        assert_eq!(sources[0].items[0].body, None);
        assert!(matches!(sources[0].items[0].fetch, FetchStatus::Fetching));
        assert_eq!(sources[0].items[1].body.as_deref(), Some("body of two"));
        assert!(matches!(sources[0].items[1].fetch, FetchStatus::Idle));
        assert_eq!(sources[1].items[0].body, None);
        assert!(matches!(sources[1].items[0].fetch, FetchStatus::Fetching));
    }

    #[test]
    fn apply_article_drops_result_when_url_no_longer_present() {
        // Simulates a refresh that replaced the items mid-fetch: the fetched
        // url is gone, so the stale completion must not touch anything.
        let mut sources = vec![Source::new("A")];
        sources[0].items =
            vec![Item { title: "new".into(), url: "https://new".into(), fetch: FetchStatus::Idle, ..Default::default() }];

        apply_article(&mut sources, "https://stale", Ok("stale body".to_string()));

        assert_eq!(sources[0].items[0].body, None);
        assert!(matches!(sources[0].items[0].fetch, FetchStatus::Idle));
    }

    #[test]
    fn help_differs_by_view() {
        let mut n = News::new();
        assert!(n.help().contains("enter read"));
        n.view = View::Reader { scroll: 0 };
        assert!(n.help().contains("scroll"));
    }

    #[test]
    fn reader_help_shows_fetch_hint_only_when_body_empty_and_idle() {
        let mut n = News::new();
        n.sources = vec![Source::new("A")];
        n.sources[0].items = vec![Item {
            title: "t".into(),
            url: "https://x".into(),
            meta: String::new(),
            body: None,
            fetch: FetchStatus::Idle,
        }];
        n.view = View::Reader { scroll: 0 };
        assert!(n.help().contains("enter fetch article"));

        n.sources[0].items[0].fetch = FetchStatus::Fetching;
        assert!(!n.help().contains("enter fetch article"));

        n.sources[0].items[0].fetch = FetchStatus::Idle;
        n.sources[0].items[0].body = Some("already here".into());
        assert!(!n.help().contains("enter fetch article"));
    }
}
