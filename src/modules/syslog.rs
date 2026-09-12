//! SYSLOG module: recent Windows Event Log errors and warnings.
//!
//! Everything comes from one fixed PowerShell one-liner (`Get-WinEvent …
//! | ConvertTo-Json`) run as a child process on a background thread — no user
//! input is ever interpolated into the script, only the two validated integers
//! `hours` and `max`. `ConvertTo-Json` returns an **array** for several events,
//! a **bare object** for exactly one and **nothing at all** for none, so all
//! three shapes are accepted. Event messages are untrusted text, so everything
//! that reaches the screen goes through [`sanitize`].

use crate::module::{Ctx, Module, Notice, Slot};
use crate::style::Theme;
use crate::ui::widgets::truncate;
use chrono::{DateTime, Local, NaiveDateTime};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use serde::Deserialize;
use std::cell::Cell;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// Shortest accepted `interval`: reading the event log is not free.
const MIN_INTERVAL: u64 = 60;
/// A PowerShell call still running after this is killed.
const CMD_TIMEOUT: Duration = Duration::from_secs(30);
const NOT_FOUND: &str = "powershell not found — Windows PowerShell or pwsh is required";
/// Interpreters tried in order; Windows PowerShell is always present.
const SHELLS: [&str; 2] = ["powershell.exe", "pwsh"];
/// Accepted `hours` / `max` ranges (a typo cannot ask for a year of log).
const HOURS: (u32, u32) = (1, 168);
const MAX_EVENTS: (u32, u32) = (10, 500);
/// Largest amount of child stdout kept in memory.
const MAX_STDOUT: u64 = 8 << 20; // 8 MiB
/// Largest event message kept; nothing legitimate needs more.
const MAX_MSG: usize = 4 << 10; // 4 KiB
/// List column widths. `MM-DD HH:MM:SS` is 14 columns.
const TIME_W: usize = 14;
const TIME_NARROW_W: usize = 5; // HH:MM
const LVL_W: usize = 4;
const SRC_W: usize = 22;
/// Below this width the list drops the SOURCE column and the date/seconds.
const NARROW: u16 = 80;

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct SyslogCfg {
    /// How far back to look, in hours (clamped to [`HOURS`]).
    pub hours: u32,
    /// Most events fetched per refresh (clamped to [`MAX_EVENTS`]).
    pub max: u32,
    /// Refresh period in seconds, clamped to at least [`MIN_INTERVAL`].
    pub interval: u64,
    /// `"error"` = critical + error only, anything else = with warnings.
    pub levels: String,
}

impl Default for SyslogCfg {
    fn default() -> Self {
        Self { hours: 24, max: 200, interval: 300, levels: "warn".into() }
    }
}

/// Which levels the list shows; `l` cycles it at runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Filter {
    /// Critical + error (levels 0–2).
    Errors,
    /// … plus warnings (level 3).
    Warn,
}

impl Filter {
    fn from_cfg(s: &str) -> Self {
        if s.trim().eq_ignore_ascii_case("error") { Filter::Errors } else { Filter::Warn }
    }
    fn next(self) -> Self {
        match self {
            Filter::Errors => Filter::Warn,
            Filter::Warn => Filter::Errors,
        }
    }
    /// Label shown in the title line so the user always sees which filter is on.
    fn label(self) -> &'static str {
        match self {
            Filter::Errors => "showing errors",
            Filter::Warn => "showing errors+warnings",
        }
    }
    fn keeps(self, level: u8) -> bool {
        match self {
            Filter::Errors => level <= 2,
            Filter::Warn => true,
        }
    }
}

// ---- the PowerShell source ---------------------------------------------------

/// The one fixed script. Only the two validated integers are formatted in;
/// nothing else ever reaches the command line.
fn script(hours: u32, max: u32) -> String {
    let hours = hours.clamp(HOURS.0, HOURS.1);
    let max = max.clamp(MAX_EVENTS.0, MAX_EVENTS.1);
    format!(
        "[Console]::OutputEncoding=[Text.Encoding]::UTF8; \
         Get-WinEvent -FilterHashtable @{{LogName=@('System','Application'); Level=@(1,2,3); \
         StartTime=(Get-Date).AddHours(-{hours})}} -MaxEvents {max} -ErrorAction SilentlyContinue | \
         Select-Object @{{n='t';e={{$_.TimeCreated.ToString('s')}}}},@{{n='l';e={{$_.Level}}}},\
@{{n='log';e={{$_.LogName}}}},@{{n='src';e={{$_.ProviderName}}}},@{{n='id';e={{$_.Id}}}},\
@{{n='msg';e={{$_.Message}}}} | ConvertTo-Json -Compress -Depth 2"
    )
}

// ---- wire + display types ----------------------------------------------------

#[derive(Deserialize)]
struct Wire {
    #[serde(default)]
    t: Option<String>,
    #[serde(default)]
    l: Option<u8>,
    #[serde(default)]
    log: Option<String>,
    #[serde(default)]
    src: Option<String>,
    #[serde(default)]
    id: Option<u32>,
    #[serde(default)]
    msg: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Event {
    /// Local `MM-DD HH:MM:SS`.
    time: String,
    /// 1 = critical, 2 = error, 3 = warning.
    level: u8,
    log: String,
    source: String,
    id: u32,
    /// Sanitized, whitespace-collapsed, capped at [`MAX_MSG`].
    message: String,
    /// Parsed timestamp, kept for sorting and the "last hour" header badge.
    at: Option<NaiveDateTime>,
}

impl From<Wire> for Event {
    fn from(w: Wire) -> Self {
        let (time, at) = format_time(w.t.as_deref().unwrap_or(""));
        Event {
            time,
            level: w.l.unwrap_or(0),
            log: truncate(&sanitize(w.log.as_deref().unwrap_or("")), 32),
            source: sanitize(w.src.as_deref().unwrap_or("")),
            id: w.id.unwrap_or(0),
            message: sanitize_message(w.msg.as_deref().unwrap_or("")),
            at,
        }
    }
}

/// `2026-09-11T10:15:00` (PowerShell's `ToString('s')`, already local) →
/// `09-11 10:15:00`; anything else is shown as-is, trimmed.
fn format_time(raw: &str) -> (String, Option<NaiveDateTime>) {
    match NaiveDateTime::parse_from_str(raw.trim(), "%Y-%m-%dT%H:%M:%S") {
        Ok(dt) => (dt.format("%m-%d %H:%M:%S").to_string(), Some(dt)),
        Err(_) => (truncate(&sanitize(raw.trim()), TIME_W), None),
    }
}

/// `CRIT` / `ERR` / `WARN`, padded to [`LVL_W`].
fn level_tag(level: u8, t: Theme) -> Span<'static> {
    let (text, style) = match level {
        0 | 1 => ("CRIT", t.danger.add_modifier(Modifier::BOLD)),
        2 => ("ERR", t.danger),
        3 => ("WARN", t.warn),
        _ => ("INFO", t.frame),
    };
    Span::styled(pad(text, LVL_W), style)
}

// ---- sanitising untrusted event text ----------------------------------------

/// Drops C0/C1 control characters (keeping `\n` and `\t`) and ANSI `ESC[…` /
/// `ESC]…` sequences: an event message is attacker-controlled text that would
/// otherwise be able to repaint the terminal.
/// ponytail: same routine as MAIL's — duplicated rather than hoisted into a
/// shared module, to keep this tab a single self-contained file.
fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            match it.peek() {
                // CSI: runs to a final byte in 0x40..=0x7e.
                Some('[') => {
                    it.next();
                    for c in it.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC: runs to BEL or ST (ESC \).
                Some(']') => {
                    it.next();
                    while let Some(c) = it.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' {
                            it.next();
                            break;
                        }
                    }
                }
                // Two-character escape.
                Some(_) => {
                    it.next();
                }
                None => {}
            }
            continue;
        }
        let code = c as u32;
        if c == '\n' || c == '\t' {
            out.push(c);
        } else if code >= 0x20
            && code != 0x7f
            && !(0x80..=0x9f).contains(&code)
            && !matches!(code, 0x200b..=0x200d | 0x202d | 0x202e | 0x2066..=0x2069 | 0xfeff)
        {
            out.push(c);
        }
    }
    out
}

/// Sanitized + whitespace-collapsed + capped: event messages are multi-line
/// blobs with trailing blank lines, and both the one-line rows and the wrapped
/// detail view read better as a single paragraph.
fn sanitize_message(s: &str) -> String {
    let mut out: String = sanitize(s).split_whitespace().collect::<Vec<_>>().join(" ");
    if out.len() > MAX_MSG {
        let mut end = MAX_MSG;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out.push('…');
    }
    out
}

fn first_line(s: &str) -> String {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_string()
}

// ---- JSON parsing ------------------------------------------------------------

/// `ConvertTo-Json` emits an array for many events, a bare object for exactly
/// one and nothing at all for none — all three are valid results here.
fn parse_events(stdout: &str) -> Result<Vec<Event>, String> {
    let s = stdout.trim();
    if s.is_empty() {
        return Ok(Vec::new());
    }
    let v: serde_json::Value = serde_json::from_str(s).map_err(|e| first_line(&e.to_string()))?;
    let wires: Vec<Wire> = match &v {
        serde_json::Value::Array(_) => serde_json::from_value(v).map_err(|e| first_line(&e.to_string()))?,
        serde_json::Value::Object(_) => {
            vec![serde_json::from_value(v).map_err(|e| first_line(&e.to_string()))?]
        }
        serde_json::Value::Null => Vec::new(),
        _ => return Err("unexpected JSON from Get-WinEvent".to_string()),
    };
    let mut events: Vec<Event> = wires.into_iter().map(Event::from).collect();
    // Newest first; entries without a parsable timestamp sink to the bottom.
    events.sort_by(|a, b| b.at.cmp(&a.at));
    Ok(events)
}

// ---- running PowerShell ------------------------------------------------------

enum RunErr {
    NotFound,
    Failed(String),
}

/// Drains a child pipe on its own thread, so a large output cannot fill the
/// pipe buffer and deadlock the timeout loop below.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(r) = pipe {
            let _ = r.take(MAX_STDOUT).read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    })
}

/// Runs `<exe> -NoProfile -NonInteractive -Command <script>` with no shell and
/// no stdin, killing it after [`CMD_TIMEOUT`].
fn run_ps(exe: &str, script: &str) -> Result<String, RunErr> {
    let mut child = match Command::new(exe)
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(RunErr::NotFound),
        Err(e) => return Err(RunErr::Failed(format!("{exe}: {e}"))),
    };
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    let deadline = Instant::now() + CMD_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(RunErr::Failed(e.to_string())),
        }
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    let Some(status) = status else {
        return Err(RunErr::Failed(format!("{exe} timed out after {}s", CMD_TIMEOUT.as_secs())));
    };
    // No matching events at all = empty stdout and exit 0, which is a result,
    // not a failure.
    if status.success() || stdout.trim_start().starts_with(['{', '[']) {
        return Ok(stdout);
    }
    let msg = sanitize(&first_line(&stderr));
    Err(RunErr::Failed(if msg.is_empty() { format!("{exe} exited with {status}") } else { msg }))
}

/// Windows PowerShell first (always installed), pwsh as the fallback.
fn capture(script: &str) -> Result<String, String> {
    for exe in SHELLS {
        match run_ps(exe, script) {
            Ok(s) => return Ok(s),
            Err(RunErr::NotFound) => continue,
            Err(RunErr::Failed(e)) => return Err(e),
        }
    }
    Err(NOT_FOUND.to_string())
}

// ---- background thread -------------------------------------------------------

enum SysCmd {
    Refresh,
}

enum SysEvent {
    Events(Vec<Event>),
    Error(String),
}

fn run(cfg: SyslogCfg, tx: Sender<SysEvent>, crx: Receiver<SysCmd>) {
    let interval = Duration::from_secs(cfg.interval.max(MIN_INTERVAL));
    let script = script(cfg.hours, cfg.max);
    let mut next = Instant::now();
    loop {
        if Instant::now() >= next {
            let ev = match capture(&script).and_then(|o| parse_events(&o)) {
                Ok(v) => SysEvent::Events(v),
                Err(e) => SysEvent::Error(e),
            };
            if tx.send(ev).is_err() {
                return;
            }
            next = Instant::now() + interval;
        }
        let wait = next.saturating_duration_since(Instant::now());
        match crx.recv_timeout(wait) {
            Ok(SysCmd::Refresh) => next = Instant::now(),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

// ---- the module --------------------------------------------------------------

/// Open event: the full message, scrolled.
struct Detail {
    ev: Event,
    scroll: u16,
}

pub struct Syslog {
    cfg: SyslogCfg,
    filter: Filter,
    /// Newest first, every fetched level; `filter` decides what is listed.
    events: Vec<Event>,
    /// Index into the *filtered* list.
    sel: usize,
    /// `Some` = detail view, `None` = list view.
    detail: Option<Detail>,
    err: Option<String>,
    loading: bool,
    updated: Option<DateTime<Local>>,
    /// Size last given to the detail body, for paging and the scroll limit.
    body_height: Cell<u16>,
    body_width: Cell<u16>,
    rx: Option<Receiver<SysEvent>>,
    tx: Option<Sender<SysCmd>>,
}

impl Syslog {
    pub fn new() -> Self {
        let cfg = SyslogCfg::default();
        Self {
            filter: Filter::from_cfg(&cfg.levels),
            cfg,
            events: Vec::new(),
            sel: 0,
            detail: None,
            err: None,
            loading: false,
            updated: None,
            body_height: Cell::new(10),
            body_width: Cell::new(80),
            rx: None,
            tx: None,
        }
    }

    /// `(critical, error, warning)` over everything fetched, filter included.
    fn counts(&self) -> (usize, usize, usize) {
        let mut c = (0, 0, 0);
        for e in &self.events {
            match e.level {
                0 | 1 => c.0 += 1,
                2 => c.1 += 1,
                3 => c.2 += 1,
                _ => {}
            }
        }
        c
    }

    fn visible(&self) -> impl Iterator<Item = &Event> {
        let f = self.filter;
        self.events.iter().filter(move |e| f.keeps(e.level))
    }

    fn visible_count(&self) -> usize {
        self.visible().count()
    }

    fn move_sel(&mut self, delta: i32) {
        let n = self.visible_count();
        if n == 0 {
            self.sel = 0;
            return;
        }
        self.sel = (self.sel as i32 + delta).clamp(0, n as i32 - 1) as usize;
    }

    fn open_selected(&mut self) {
        let ev = self.visible().nth(self.sel).cloned();
        if let Some(ev) = ev {
            self.detail = Some(Detail { ev, scroll: 0 });
        }
    }

    fn scroll_detail(&mut self, delta: i32) {
        let width = self.body_width.get();
        let height = self.body_height.get();
        let Some(d) = &mut self.detail else { return };
        let max = wrapped_rows(&d.ev.message, width).saturating_sub(height);
        d.scroll = (d.scroll as i32 + delta).clamp(0, max as i32) as u16;
    }

    /// Critical/error events from the last hour, for the header badge.
    fn recent_errors(&self) -> usize {
        let cutoff = Local::now().naive_local() - chrono::Duration::hours(1);
        self.events.iter().filter(|e| e.level <= 2 && e.at.is_some_and(|a| a >= cutoff)).count()
    }

    fn title_line(&self, width: u16, t: Theme) -> Line<'static> {
        let (crit, err, warn) = self.counts();
        let mut head = format!(
            "SYSLOG · last {} h · {crit} critical · {err} errors · {warn} warnings",
            self.cfg.hours
        );
        if let Some(u) = self.updated {
            head.push_str(&format!(" · updated {}", u.format("%H:%M")));
        }
        head.push_str(" · ");
        head.push_str(self.filter.label());
        let mut spans = vec![Span::styled(truncate(&head, width as usize), t.title)];
        if let Some(e) = &self.err {
            let rest = (width as usize).saturating_sub(head.chars().count());
            if rest > 3 {
                spans.push(Span::styled(truncate(&format!(" · {e}"), rest), t.warn));
            }
        }
        Line::from(spans)
    }

    fn draw_list(&self, f: &mut Frame, area: Rect, t: Theme) {
        let rows =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)]).split(area);
        f.render_widget(Paragraph::new(self.title_line(rows[0].width, t)), rows[0]);
        f.render_widget(Paragraph::new(header_line(rows[1].width, t)), rows[1]);
        let visible: Vec<&Event> = self.visible().collect();
        if visible.is_empty() && (self.loading || self.err.is_none()) {
            // Centred status instead of an empty list.
            let (msg, style) = if self.loading {
                ("LOADING\u{2026}".to_string(), t.title)
            } else {
                (format!("no events in the last {} h", self.cfg.hours), t.frame)
            };
            if rows[2].height > 0 {
                let line = Rect {
                    x: rows[2].x,
                    y: rows[2].y + rows[2].height / 2,
                    width: rows[2].width,
                    height: 1,
                };
                f.render_widget(
                    Paragraph::new(Line::from(Span::styled(truncate(&msg, rows[2].width as usize), style)))
                        .alignment(Alignment::Center),
                    line,
                );
            }
            return;
        }
        let items: Vec<ListItem> = if visible.is_empty() {
            let msg = match &self.err {
                Some(e) => format!("n/a: {e}"),
                None => "(no events)".to_string(),
            };
            vec![ListItem::new(truncate(&msg, rows[2].width as usize)).style(t.frame)]
        } else {
            visible.iter().map(|e| ListItem::new(row_line(e, rows[2].width, t))).collect()
        };
        let mut state = ListState::default();
        if !visible.is_empty() {
            state.select(Some(self.sel.min(visible.len() - 1)));
        }
        f.render_stateful_widget(List::new(items).highlight_style(t.tab_active), rows[2], &mut state);
    }

    fn draw_detail(&self, f: &mut Frame, area: Rect, t: Theme, d: &Detail) {
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(area);
        let w = area.width as usize;
        let e = &d.ev;
        f.render_widget(Paragraph::new(Line::from(Span::styled(truncate(&e.time, w), t.title))), rows[0]);
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::raw("level  "), level_tag(e.level, t)])),
            rows[1],
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(truncate(&format!("log    {}", e.log), w), t.value))),
            rows[2],
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(truncate(&format!("source {}", e.source), w), t.value))),
            rows[3],
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(truncate(&format!("event  {}", e.id), w), t.frame))),
            rows[4],
        );
        self.body_height.set(rows[5].height);
        self.body_width.set(rows[5].width);
        let (body, style) = if e.message.trim().is_empty() {
            ("(no message)", t.frame)
        } else {
            (e.message.as_str(), t.text)
        };
        let max_scroll = wrapped_rows(body, rows[5].width).saturating_sub(rows[5].height);
        let p = Paragraph::new(body)
            .style(style)
            .wrap(Wrap { trim: false })
            .scroll((d.scroll.min(max_scroll), 0));
        f.render_widget(p, rows[5]);
    }
}

/// `TIME LVL [SOURCE] MESSAGE`; below [`NARROW`] the date and the SOURCE
/// column go and the time shrinks to `HH:MM`.
fn header_line(width: u16, t: Theme) -> Line<'static> {
    let narrow = width < NARROW;
    let mut s = pad("TIME", if narrow { TIME_NARROW_W } else { TIME_W });
    s.push(' ');
    s.push_str(&pad("LVL", LVL_W));
    s.push(' ');
    if !narrow {
        s.push_str(&pad("SOURCE", SRC_W));
        s.push(' ');
    }
    s.push_str("MESSAGE");
    Line::from(Span::styled(truncate(&s, width as usize), t.title.add_modifier(Modifier::UNDERLINED)))
}

fn row_line(e: &Event, width: u16, t: Theme) -> Line<'static> {
    let narrow = width < NARROW;
    let time_w = if narrow { TIME_NARROW_W } else { TIME_W };
    // `MM-DD HH:MM:SS` → `HH:MM` in the narrow layout.
    let time: String = if narrow { e.time.chars().skip(6).take(5).collect() } else { e.time.clone() };
    let mut fixed = time_w + 1 + LVL_W + 1;
    if !narrow {
        fixed += SRC_W + 1;
    }
    let msg_w = (width as usize).saturating_sub(fixed).max(6);
    let mut spans = vec![Span::styled(pad(&time, time_w), t.frame), Span::raw(" "), level_tag(e.level, t), Span::raw(" ")];
    if !narrow {
        spans.push(Span::styled(pad(&e.source, SRC_W), t.value));
        spans.push(Span::raw(" "));
    }
    // The message is already whitespace-collapsed, so this is its first line.
    spans.push(Span::styled(pad(&e.message, msg_w), t.text));
    Line::from(spans)
}

/// Wrapped-row count for a body shown at `width` columns, for the scroll limit.
/// ponytail: character count, not full Unicode display width — same trade-off
/// as MAIL's reader.
fn wrapped_rows(body: &str, width: u16) -> u16 {
    let width = (width as usize).max(1);
    let rows: usize = body
        .lines()
        .map(|l| {
            let w = l.chars().count();
            if w == 0 { 1 } else { w.div_ceil(width) }
        })
        .sum();
    rows.clamp(1, u16::MAX as usize) as u16
}

/// Truncates to `n` characters, then pads to exactly `n` with spaces.
fn pad(s: &str, n: usize) -> String {
    let t = truncate(s, n);
    let fill = n.saturating_sub(t.chars().count());
    format!("{t}{}", " ".repeat(fill))
}

impl Module for Syslog {
    fn id(&self) -> &'static str {
        "syslog"
    }
    fn title(&self) -> &'static str {
        "SYSLOG"
    }
    fn describe(&self) -> &'static str {
        "Windows event-log errors and warnings"
    }
    fn manual(&self) -> &'static str {
        "\
SYSLOG is the last 24 hours of Windows event-log errors and
warnings in one list, with the full details one key away.

  ↑/↓   pick an event    PgUp/PgDn  page through the list
  enter open the details
  l     level filter: errors only, or errors and warnings
  r     refresh now
  esc / bksp             back to the list
  ↑/↓ PgUp/PgDn          scroll the open event

Most of these have been failing quietly for months and the
machine still boots. That is either reassuring or deeply
unsettling, and Vault-Tec declines to say which."
    }
    fn help(&self) -> &'static str {
        match self.detail {
            None => "↑/↓ select   enter details   l level   r refresh   1-9 tabs   q quit",
            Some(_) => "↑/↓ scroll   esc back   q quit",
        }
    }

    fn start(&mut self, ctx: &Ctx) {
        self.loading = true;
        let (cfg, notice) = ctx.config.section::<SyslogCfg>(self.id());
        if let Some(n) = notice {
            let _ = ctx.notify.send(Notice::Footer(n));
        }
        self.cfg = cfg;
        // Validated once, here: the worker and the title both use the clamped
        // values, so what is shown is what was asked for.
        self.cfg.hours = self.cfg.hours.clamp(HOURS.0, HOURS.1);
        self.cfg.max = self.cfg.max.clamp(MAX_EVENTS.0, MAX_EVENTS.1);
        self.filter = Filter::from_cfg(&self.cfg.levels);
        let (tx_ev, rx_ev) = mpsc::channel();
        let (tx_cmd, rx_cmd) = mpsc::channel();
        self.rx = Some(rx_ev);
        self.tx = Some(tx_cmd);
        let cfg = self.cfg.clone();
        std::thread::spawn(move || run(cfg, tx_ev, rx_cmd));
    }

    fn poll(&mut self, _ctx: &Ctx) -> usize {
        let mut n = 0;
        // Taken out for the duration of the loop: the arms below need
        // `&mut self`, which cannot coexist with a borrow of `self.rx`.
        let Some(rx) = self.rx.take() else { return 0 };
        while let Ok(ev) = rx.try_recv() {
            n += 1;
            match ev {
                SysEvent::Events(v) => {
                    self.loading = false;
                    self.err = None;
                    self.updated = Some(Local::now());
                    self.events = v;
                    let max = self.visible_count().saturating_sub(1);
                    self.sel = self.sel.min(max);
                }
                SysEvent::Error(e) => {
                    self.err = Some(e);
                    self.loading = false;
                }
            }
        }
        self.rx = Some(rx);
        n
    }

    fn on_key(&mut self, key: KeyEvent, _ctx: &Ctx) -> bool {
        if self.detail.is_some() {
            let page = self.body_height.get().saturating_sub(2).max(1) as i32;
            match key.code {
                KeyCode::Up => self.scroll_detail(-1),
                KeyCode::Down => self.scroll_detail(1),
                KeyCode::PageUp => self.scroll_detail(-page),
                KeyCode::PageDown => self.scroll_detail(page),
                KeyCode::Esc | KeyCode::Backspace => self.detail = None,
                _ => return false,
            }
            return true;
        }
        let page = self.body_height.get().max(1) as i32;
        match key.code {
            KeyCode::Up => self.move_sel(-1),
            KeyCode::Down => self.move_sel(1),
            KeyCode::PageUp => self.move_sel(-page),
            KeyCode::PageDown => self.move_sel(page),
            KeyCode::Char('l') => {
                self.filter = self.filter.next();
                self.sel = 0;
            }
            KeyCode::Char('r') => {
                self.loading = true;
                if let Some(tx) = &self.tx {
                    let _ = tx.send(SysCmd::Refresh);
                }
            }
            KeyCode::Enter => self.open_selected(),
            _ => return false,
        }
        true
    }

    fn draw(&self, f: &mut Frame, area: Rect, t: Theme) {
        // The list area doubles as the page size for PgUp/PgDn.
        self.body_height.set(area.height.saturating_sub(2));
        match &self.detail {
            Some(d) => self.draw_detail(f, area, t, d),
            None => self.draw_list(f, area, t),
        }
    }

    fn header(&self, width: u16, t: Theme) -> Vec<Span<'static>> {
        let n = self.recent_errors();
        if n == 0 {
            return vec![];
        }
        let s = format!("⚠ {n}");
        if s.chars().count() > width as usize {
            return vec![];
        }
        vec![Span::styled(s, t.danger)]
    }

    fn overview(&self, width: u16, height: u16, t: Theme) -> Vec<Line<'static>> {
        let w = (width as usize).saturating_sub(1);
        let (crit, err, warn) = self.counts();
        let mut lines = vec![
            Line::from(Span::styled(" SYSLOG", t.title)),
            Line::from(format!(" {} errors · {warn} warnings ({} h)", crit + err, self.cfg.hours)),
        ];
        if let Some(e) = self.events.iter().find(|e| e.level <= 2) {
            lines.push(Line::from(truncate(&format!(" {} – {}", e.source, e.message), w)));
        } else if let Some(e) = &self.err {
            lines.push(Line::from(Span::styled(truncate(&format!(" {e}"), w), t.warn)));
        }
        lines.truncate(height.max(1) as usize);
        lines
    }

    fn overview_slot(&self) -> Slot {
        Slot::Left(4)
    }

    fn status(&self) -> String {
        let (crit, err, warn) = self.counts();
        let state = match &self.err {
            Some(e) => format!("error: {e}"),
            None => "ok".to_string(),
        };
        format!("syslog {} events, {} err, {warn} warn, {state}", self.events.len(), crit + err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemeKind;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    const ARRAY: &str = r#"[
      {"t":"2020-09-11T10:15:00","l":2,"log":"System","src":"Service Control Manager","id":7000,
       "msg":"The Fusion service failed to start.\r\n\r\nCheck the reactor."},
      {"t":"2020-09-11T09:00:00","l":1,"log":"Application","src":"Vault-Tec","id":13,"msg":"Critical fault"},
      {"t":"2020-09-11T11:30:00","l":3,"log":"System","src":"DCOM","id":10016,"msg":null}
    ]"#;

    const SINGLE: &str =
        r#"{"t":"2020-09-11T10:15:00","l":2,"log":"System","src":"Service Control Manager","id":7000,"msg":"only one"}"#;

    fn ev(level: u8, source: &str, message: &str) -> Event {
        Event {
            time: "09-11 10:15:00".into(),
            level,
            log: "System".into(),
            source: source.into(),
            id: 7000,
            message: message.into(),
            at: NaiveDateTime::parse_from_str("2020-09-11T10:15:00", "%Y-%m-%dT%H:%M:%S").ok(),
        }
    }

    fn plain(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn screen(term: &Terminal<TestBackend>, w: u16, h: u16) -> String {
        (0..h)
            .map(|y| {
                (0..w).map(|x| term.backend().buffer()[(x, y)].symbol().to_string()).collect::<String>() + "\n"
            })
            .collect()
    }

    #[test]
    fn parses_a_json_array_newest_first() {
        let v = parse_events(ARRAY).unwrap();
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].time, "09-11 11:30:00", "newest first");
        assert_eq!(v[0].level, 3);
        assert_eq!(v[0].message, "", "a null message is empty, not a panic");
        assert_eq!(v[1].source, "Service Control Manager");
        assert_eq!(v[1].id, 7000);
        assert_eq!(v[1].log, "System");
        assert_eq!(v[1].message, "The Fusion service failed to start. Check the reactor.");
        assert_eq!(v[2].level, 1);
    }

    #[test]
    fn parses_a_bare_object_when_there_is_exactly_one_event() {
        let v = parse_events(SINGLE).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].message, "only one");
        assert_eq!(v[0].level, 2);
    }

    #[test]
    fn empty_output_is_no_events_and_malformed_output_is_an_error() {
        assert!(parse_events("").unwrap().is_empty(), "no matching events at all");
        assert!(parse_events("   \r\n").unwrap().is_empty());
        assert!(parse_events("null").unwrap().is_empty());
        assert!(parse_events("Get-WinEvent : No events were found.").is_err());
        assert!(parse_events("{").is_err());
        assert!(parse_events("\"just a string\"").is_err());
        assert!(parse_events(r#"[{"t":"x","l":"nope"}]"#).is_err());
    }

    #[test]
    fn level_tags_and_time_formatting() {
        let t = Theme::new(ThemeKind::Color);
        assert_eq!(level_tag(1, t).content.trim(), "CRIT");
        assert_eq!(level_tag(2, t).content.trim(), "ERR");
        assert_eq!(level_tag(3, t).content.trim(), "WARN");
        assert_eq!(level_tag(4, t).content.trim(), "INFO");
        assert_eq!(level_tag(2, t).content.chars().count(), LVL_W);
        assert_eq!(level_tag(1, t).style, t.danger.add_modifier(Modifier::BOLD));
        assert_eq!(level_tag(3, t).style, t.warn);

        let (s, at) = format_time("2020-09-11T10:15:00");
        assert_eq!(s, "09-11 10:15:00");
        assert!(at.is_some());
        let (s, at) = format_time("not a date");
        assert_eq!(s, "not a date");
        assert!(at.is_none());
        assert_eq!(format_time("").0, "");
    }

    #[test]
    fn sanitize_drops_escapes_and_collapses_whitespace() {
        assert_eq!(sanitize("a\x1b[31mb\x07c"), "abc");
        assert_eq!(sanitize("t\x1b]0;pwn\x07x"), "tx");
        assert_eq!(sanitize("a\u{9b}31mb"), "a31mb");
        assert_eq!(sanitize("Árvíztűrő"), "Árvíztűrő");
        assert_eq!(sanitize("a\u{202e}b\u{200b}c"), "abc");
        assert_eq!(sanitize_message("line1\r\n\r\n  line2\tend  "), "line1 line2 end");
        assert_eq!(sanitize_message("\x1b[2Jevil"), "evil");
        let long = sanitize_message(&"a".repeat(MAX_MSG + 500));
        assert!(long.ends_with('…'));
        assert!(long.len() <= MAX_MSG + 3);
    }

    #[test]
    fn filter_cycles_and_selects_levels() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        assert_eq!(Filter::from_cfg("error"), Filter::Errors);
        assert_eq!(Filter::from_cfg("ERROR"), Filter::Errors);
        assert_eq!(Filter::from_cfg("warn"), Filter::Warn);
        assert_eq!(Filter::from_cfg("nonsense"), Filter::Warn);
        assert!(Filter::Errors.keeps(2) && !Filter::Errors.keeps(3));
        assert!(Filter::Warn.keeps(3));

        let mut m = Syslog::new();
        m.events = parse_events(ARRAY).unwrap();
        assert_eq!(m.visible_count(), 3);
        m.sel = 2;
        assert!(m.on_key(KeyEvent::from(KeyCode::Char('l')), &ctx));
        assert_eq!(m.filter, Filter::Errors);
        assert_eq!(m.sel, 0, "selection resets when the list shrinks");
        assert_eq!(m.visible_count(), 2, "warnings hidden");
        m.on_key(KeyEvent::from(KeyCode::Char('l')), &ctx);
        assert_eq!(m.filter, Filter::Warn);
        let t = Theme::new(crate::config::ThemeKind::Color);
        let shown: String = m.title_line(120, t).spans.iter().map(|s| s.content.to_string()).collect();
        assert!(shown.contains("showing errors+warnings"), "{shown}");
        assert_eq!(m.counts(), (1, 1, 1));
        assert!(m.status().contains("3 events, 2 err, 1 warn, ok"));
        m.err = Some("boom".into());
        assert!(m.status().ends_with("error: boom"));
    }

    #[test]
    fn rows_and_header_fill_the_width_at_each_breakpoint() {
        let t = Theme::new(ThemeKind::Color);
        let e = ev(2, "Service Control Manager", "The Fusion service failed to start");
        for w in [40u16, 79, 80, 120] {
            let row = plain(&row_line(&e, w, t));
            let head = plain(&header_line(w, t));
            assert_eq!(row.chars().count(), w as usize, "w={w}: {row:?}");
            assert!(head.chars().count() <= w as usize, "w={w}: {head:?}");
            assert!(row.contains("ERR"), "w={w}: {row:?}");
            if w < NARROW {
                assert!(row.starts_with("10:15"), "w={w}: short time: {row:?}");
                assert!(!row.contains("Service Control"), "w={w}: source hidden: {row:?}");
                assert!(!head.contains("SOURCE"), "w={w}: {head:?}");
            } else {
                assert!(row.starts_with("09-11 10:15:00"), "w={w}: full time: {row:?}");
                assert!(row.contains("Service Control Man"), "w={w}: {row:?}");
                assert!(head.contains("SOURCE") && head.contains("MESSAGE"), "w={w}: {head:?}");
            }
        }
        // Unicode and tiny widths must not panic.
        let e = ev(1, "Vault-Tec Árvíztűrő", "Árvíztűrő tükörfúrógép hibát jelzett a reaktorban");
        for w in [0u16, 1, 12, 40, 200] {
            plain(&row_line(&e, w, t));
            plain(&header_line(w, t));
        }
    }

    #[test]
    fn script_is_fixed_and_clamps_its_two_numbers() {
        let s = script(24, 200);
        assert!(s.starts_with("[Console]::OutputEncoding=[Text.Encoding]::UTF8;"));
        assert!(s.contains("LogName=@('System','Application'); Level=@(1,2,3)"));
        assert!(s.contains("StartTime=(Get-Date).AddHours(-24)"));
        assert!(s.contains("-MaxEvents 200"));
        assert!(s.ends_with("ConvertTo-Json -Compress -Depth 2"));
        assert!(script(0, 0).contains("AddHours(-1)"));
        assert!(script(0, 0).contains("-MaxEvents 10"));
        assert!(script(9999, 9999).contains("AddHours(-168)"));
        assert!(script(9999, 9999).contains("-MaxEvents 500"));
    }

    #[test]
    fn config_defaults_and_interval_floor() {
        let cfg = SyslogCfg::default();
        assert_eq!((cfg.hours, cfg.max, cfg.interval), (24, 200, 300));
        assert_eq!(cfg.levels, "warn");
        let table: toml::Table = toml::from_str("[syslog]\nhours = 6\ninterval = 5\nlevels = \"error\"\n").unwrap();
        let (parsed, notice) = crate::module::ModuleConfig(table).section::<SyslogCfg>("syslog");
        assert!(notice.is_none());
        assert_eq!(parsed.hours, 6);
        assert_eq!(parsed.max, 200, "missing keys keep their defaults");
        assert_eq!(parsed.interval.max(MIN_INTERVAL), MIN_INTERVAL);
        assert_eq!(Filter::from_cfg(&parsed.levels), Filter::Errors);
    }

    #[test]
    fn selection_detail_and_scroll_stay_in_bounds() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let mut m = Syslog::new();
        m.move_sel(1);
        assert_eq!(m.sel, 0, "nothing to select yet");
        m.events = parse_events(ARRAY).unwrap();
        m.move_sel(10);
        assert_eq!(m.sel, 2);
        m.move_sel(-10);
        assert_eq!(m.sel, 0);

        assert!(m.on_key(KeyEvent::from(KeyCode::Enter), &ctx));
        assert_eq!(m.detail.as_ref().unwrap().ev.time, "09-11 11:30:00");
        assert!(m.help().contains("scroll"));
        m.detail = Some(Detail { ev: ev(2, "src", &"x".repeat(300)), scroll: 0 });
        m.body_width.set(40);
        m.body_height.set(2); // 8 wrapped rows, 2 visible
        for _ in 0..50 {
            m.on_key(KeyEvent::from(KeyCode::Down), &ctx);
        }
        assert_eq!(m.detail.as_ref().unwrap().scroll, 6);
        for _ in 0..50 {
            m.on_key(KeyEvent::from(KeyCode::Up), &ctx);
        }
        assert_eq!(m.detail.as_ref().unwrap().scroll, 0);
        assert!(m.on_key(KeyEvent::from(KeyCode::Esc), &ctx));
        assert!(m.detail.is_none());
        assert!(m.help().contains("enter details"));
    }

    #[test]
    fn header_badge_counts_only_the_last_hour_and_overview_lists_the_newest_error() {
        let t = Theme::new(ThemeKind::Color);
        let mut m = Syslog::new();
        assert!(m.header(20, t).is_empty());
        m.events = parse_events(ARRAY).unwrap();
        assert!(m.header(20, t).is_empty(), "2020 events are not in the last hour");
        let now = Local::now().naive_local();
        m.events.insert(0, Event { at: Some(now), ..ev(2, "Now", "just happened") });
        assert_eq!(m.header(20, t).len(), 1);
        assert!(m.header(1, t).is_empty(), "dropped when it does not fit");

        let ov = m.overview(60, 3, t);
        assert_eq!(ov.len(), 3);
        assert!(plain(&ov[1]).contains("3 errors · 1 warnings (24 h)"));
        assert!(plain(&ov[2]).contains("Now – just happened"));
        assert_eq!(m.overview(30, 1, t).len(), 1);
        assert_eq!(m.overview_slot(), Slot::Left(4));
    }

    #[test]
    fn poll_replaces_the_list_and_reports_errors() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let (tx, rx) = mpsc::channel();
        let mut m = Syslog::new();
        m.rx = Some(rx);
        m.loading = true;
        m.sel = 5;
        tx.send(SysEvent::Events(parse_events(ARRAY).unwrap())).unwrap();
        assert_eq!(m.poll(&ctx), 1);
        assert!(!m.loading);
        assert_eq!(m.events.len(), 3);
        assert_eq!(m.sel, 2, "selection clamped to the new list");
        assert!(m.updated.is_some());
        tx.send(SysEvent::Error(NOT_FOUND.into())).unwrap();
        m.poll(&ctx);
        assert_eq!(m.err.as_deref(), Some(NOT_FOUND));
    }

    #[test]
    fn draw_does_not_panic_in_any_state_at_tiny_sizes() {
        let t = Theme::new(ThemeKind::Color);
        let states = || {
            let empty = Syslog::new();
            let mut loading = Syslog::new();
            loading.loading = true;
            let mut errored = Syslog::new();
            errored.err = Some(NOT_FOUND.to_string());
            let mut list = Syslog::new();
            list.events = parse_events(ARRAY).unwrap();
            list.updated = Some(Local::now());
            list.sel = 2;
            let mut detail = Syslog::new();
            detail.detail = Some(Detail { ev: ev(1, "Vault-Tec", "Árvíztűrő hiba\nmásodik sor"), scroll: 3 });
            vec![empty, loading, errored, list, detail]
        };
        for (w, h) in [(40u16, 12u16), (1, 1), (120, 40)] {
            for m in states() {
                let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
                term.draw(|f| m.draw(f, f.area(), t)).unwrap();
            }
        }
    }

    #[test]
    fn list_shows_loading_then_the_empty_notice_then_rows() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let (tx, rx) = mpsc::channel();
        let t = Theme::new(ThemeKind::Color);
        let mut m = Syslog::new();
        m.rx = Some(rx);
        m.loading = true;
        let mut term = Terminal::new(TestBackend::new(60, 12)).unwrap();
        term.draw(|f| m.draw(f, f.area(), t)).unwrap();
        assert!(screen(&term, 60, 12).contains("LOADING"));

        tx.send(SysEvent::Events(Vec::new())).unwrap();
        m.poll(&ctx);
        term.draw(|f| m.draw(f, f.area(), t)).unwrap();
        assert!(screen(&term, 60, 12).contains("no events in the last 24 h"));

        tx.send(SysEvent::Events(parse_events(ARRAY).unwrap())).unwrap();
        m.poll(&ctx);
        term.draw(|f| m.draw(f, f.area(), t)).unwrap();
        let s = screen(&term, 60, 12);
        assert!(s.contains("SYSLOG · last 24 h · 1 critical · 1 errors · 1 warnings"), "{s}");
        assert!(s.contains("WARN") && s.contains("ERR") && s.contains("CRIT"), "{s}");
        assert!(s.contains("TIME"), "{s}");
    }
}
