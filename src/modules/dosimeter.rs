//! DOSIMETER module: screen time as a radiation dose, 45 minutes on / 15 off.
//!
//! Signals come from a 5 s background thread (`GetLastInputInfo` for the idle
//! time, `OpenInputDesktop` for the locked workstation, optionally the
//! foreground process name). The state machine ([`Engine`]) is pure and clock
//! driven, so every transition is testable without Windows.
//!
//! Closed sessions and completed breaks are appended to `dosimeter.log` next
//! to the exe; the file is read back on start for today's totals and the strip.

use crate::module::{Ctx, Module, Notice, Slot};
use crate::style::Theme;
use crate::ui::{anim, bigfont};
use chrono::{DateTime, Duration as ChronoDur, Local, NaiveDateTime, NaiveTime, Timelike};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

/// Header alert text while the dose is critical.
const ALERT: &str = " ☢ RADS CRITICAL ◀ ";
/// Repeat interval of the Geiger burst while Overdue.
const NAG_MIN: i64 = 5;
/// `z` parks the alert for this long.
const SNOOZE_MIN: i64 = 5;
/// One strip cell is 10 minutes, so a day is 144 cells.
pub const STRIP: usize = 144;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DoseCfg {
    /// Minutes of continuous screen time before the dose is critical.
    pub work: u32,
    /// Minutes of prescribed break.
    pub rest: u32,
    /// A gap this long (minutes) closes the session.
    pub idle: u32,
    /// `HH:MM-HH:MM` windows with no sound.
    pub quiet: Vec<String>,
    /// Watch the foreground process (per-app minutes today, memory only).
    pub watch_foreground: bool,
    /// Foreground apps that count as rest even while you keep clicking.
    pub rest_apps: Vec<String>,
    /// Days of log kept for the daily sparkline.
    pub history: u32,
}

impl Default for DoseCfg {
    fn default() -> Self {
        Self {
            work: 45,
            rest: 15,
            idle: 3,
            quiet: vec!["22:00-07:00".into()],
            watch_foreground: false,
            rest_apps: vec!["vlc".into(), "mpv".into()],
            history: 30,
        }
    }
}

/// One closed session (screen time) or one completed break.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Session {
    pub start: DateTime<Local>,
    pub rest: bool,
    pub minutes: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum State {
    Away,
    Active { since: DateTime<Local> },
    Overdue { since: DateTime<Local> },
    Resting { since: DateTime<Local>, needed: u32 },
}

impl State {
    pub fn name(&self) -> &'static str {
        match self {
            State::Away => "away",
            State::Active { .. } => "active",
            State::Overdue { .. } => "overdue",
            State::Resting { .. } => "resting",
        }
    }
}

/// What the shell has to hear about after a step.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    /// The dose just went critical.
    Overdue,
    /// The prescribed break is done.
    RestDone,
    /// Input arrived after `n` minutes of a break that needed more.
    BreakCut(u32),
}

/// Snapshot published on the blackboard under `"dosimeter"`.
#[derive(Clone, Debug, PartialEq)]
pub struct DosimeterSnapshot {
    pub state: &'static str,
    pub session_min: u32,
    pub today_min: u32,
    pub sessions_today: u32,
    pub overdue: bool,
}

fn minutes(a: DateTime<Local>, b: DateTime<Local>) -> u32 {
    (b - a).num_minutes().max(0) as u32
}

/// `3h50m` / `42m`.
pub fn hm(min: u32) -> String {
    if min >= 60 {
        format!("{}h{:02}m", min / 60, min % 60)
    } else {
        format!("{min}m")
    }
}

/// 0–999: `work` minutes is 500, twice that is the pegged 999.
pub fn rads(session_min: u32, work: u32) -> u16 {
    let work = work.max(1) as f32;
    ((session_min as f32 / work) * 500.0).round().clamp(0.0, 999.0) as u16
}

fn parse_hm(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s.trim(), "%H:%M").ok()
}

/// Is `now` inside one of the `HH:MM-HH:MM` windows (wrapping over midnight)?
pub fn in_quiet(windows: &[String], now: NaiveTime) -> bool {
    windows.iter().any(|w| match w.split_once('-') {
        Some((a, b)) => match (parse_hm(a), parse_hm(b)) {
            (Some(a), Some(b)) if a <= b => now >= a && now < b,
            (Some(a), Some(b)) => now >= a || now < b,
            _ => false,
        },
        None => false,
    })
}

/// `C:\…\vlc.exe` → `vlc`.
pub fn app_name(raw: &str) -> String {
    raw.rsplit(['\\', '/']).next().unwrap_or(raw).trim_end_matches(".exe").to_lowercase()
}

/// Does the foreground app count as rest?
pub fn is_rest_app(app: Option<&str>, rest_apps: &[String]) -> bool {
    match app {
        None => false,
        Some(a) => rest_apps.iter().any(|r| !r.is_empty() && a.contains(&app_name(r))),
    }
}

// ── the state machine ────────────────────────────────────────────────────────

pub struct Engine {
    pub cfg: DoseCfg,
    pub state: State,
    /// Sessions of the last `history` days (loaded from the log + closed here).
    pub sessions: Vec<Session>,
    /// Last sample that counted as screen time — the end of the open session.
    last_active: DateTime<Local>,
    /// Minutes carried over a break, so a cut-short break keeps the counter.
    held: u32,
    /// Breaks started today (a cut-short one is never logged).
    pub breaks_today: u32,
    pub snooze_until: Option<DateTime<Local>>,
    last_nag: Option<DateTime<Local>>,
}

impl Engine {
    pub fn new(cfg: DoseCfg, now: DateTime<Local>) -> Self {
        Self {
            cfg,
            state: State::Away,
            sessions: Vec::new(),
            last_active: now,
            held: 0,
            breaks_today: 0,
            snooze_until: None,
            last_nag: None,
        }
    }

    /// One 5 s sample. `idle` = seconds since the last input.
    pub fn step(&mut self, at: DateTime<Local>, idle: u64, locked: bool, rest_app: bool) -> Vec<Event> {
        let gap = idle >= (self.cfg.idle.max(1) as u64) * 60;
        let input = !gap && !locked && !rest_app;
        let mut ev = Vec::new();
        match self.state {
            State::Away => {
                if input {
                    self.state = State::Active { since: at };
                }
            }
            State::Active { since } => {
                if !input {
                    self.close(since, false, self.last_active);
                    self.state = State::Away;
                }
            }
            State::Overdue { since } => {
                if !input {
                    self.held = minutes(since, self.last_active);
                    self.close(since, false, self.last_active);
                    self.breaks_today += 1;
                    self.state = State::Resting { since: at, needed: self.cfg.rest };
                }
            }
            State::Resting { since, needed } => {
                let done = minutes(since, at);
                if done >= needed {
                    self.sessions.push(Session { start: since, rest: true, minutes: done });
                    self.held = 0;
                    self.last_nag = None;
                    ev.push(Event::RestDone);
                    self.state = if input { State::Active { since: at } } else { State::Away };
                } else if input {
                    ev.push(Event::BreakCut(done));
                    // The counter survives the cut-short break: rewind the start.
                    self.state = State::Active { since: at - ChronoDur::minutes(self.held as i64) };
                }
            }
        }
        if let State::Active { since } = self.state {
            if minutes(since, at) >= self.cfg.work {
                self.state = State::Overdue { since };
                ev.push(Event::Overdue);
            }
        }
        if input {
            self.last_active = at;
        }
        ev
    }

    /// Close an open screen-time session; sub-minute ones are not logged.
    fn close(&mut self, since: DateTime<Local>, rest: bool, end: DateTime<Local>) {
        let m = minutes(since, end);
        if m >= 1 {
            self.sessions.push(Session { start: since, rest, minutes: m });
        }
    }

    /// The last closed session, for the log writer.
    pub fn last_closed(&self) -> Option<Session> {
        self.sessions.last().copied()
    }

    /// Minutes of the running session (a break keeps the held counter).
    pub fn session_min(&self, at: DateTime<Local>) -> u32 {
        match self.state {
            State::Away => 0,
            State::Active { since } | State::Overdue { since } => minutes(since, at),
            State::Resting { .. } => self.held,
        }
    }

    /// Minutes of the running break, and how many are still needed.
    pub fn rest_left(&self, at: DateTime<Local>) -> Option<(u32, u32)> {
        match self.state {
            State::Resting { since, needed } => {
                let done = minutes(since, at);
                Some((done, needed.saturating_sub(done)))
            }
            _ => None,
        }
    }

    pub fn overdue(&self) -> bool {
        matches!(self.state, State::Overdue { .. })
    }

    /// May the Geiger burst play now? Silent in quiet hours, while snoozed and
    /// for `NAG_MIN` minutes after the previous burst.
    pub fn may_sound(&mut self, at: DateTime<Local>) -> bool {
        if !self.overdue() || in_quiet(&self.cfg.quiet, at.time()) {
            return false;
        }
        if self.snooze_until.is_some_and(|s| at < s) {
            return false;
        }
        if self.last_nag.is_some_and(|n| at - n < ChronoDur::minutes(NAG_MIN)) {
            return false;
        }
        self.last_nag = Some(at);
        true
    }

    pub fn snooze(&mut self, at: DateTime<Local>) {
        self.snooze_until = Some(at + ChronoDur::minutes(SNOOZE_MIN));
    }

    /// Drop the running session without logging it (`r`).
    pub fn reset(&mut self, at: DateTime<Local>) {
        self.state = State::Away;
        self.held = 0;
        self.last_nag = None;
        self.last_active = at;
    }

    /// `(screen minutes, sessions, longest, breaks, completed breaks)` for today,
    /// the running session included.
    pub fn today(&self, at: DateTime<Local>) -> (u32, u32, u32, u32, u32) {
        let today = at.date_naive();
        let mut total = 0;
        let mut count = 0;
        let mut longest = 0;
        let mut done = 0;
        for s in self.sessions.iter().filter(|s| s.start.date_naive() == today) {
            if s.rest {
                done += 1;
            } else {
                total += s.minutes;
                count += 1;
                longest = longest.max(s.minutes);
            }
        }
        let open = self.session_min(at);
        if open > 0 && !matches!(self.state, State::Resting { .. }) {
            total += open;
            count += 1;
            longest = longest.max(open);
        }
        (total, count, longest, self.breaks_today.max(done), done)
    }

    /// Screen minutes per day for the last `days` days, oldest first.
    pub fn daily(&self, at: DateTime<Local>, days: i64) -> Vec<u64> {
        (0..days)
            .rev()
            .map(|d| {
                let day = (at - ChronoDur::days(d)).date_naive();
                self.sessions
                    .iter()
                    .filter(|s| !s.rest && s.start.date_naive() == day)
                    .map(|s| s.minutes as u64)
                    .sum()
            })
            .collect()
    }

    /// Today's 24-hour strip, one cell per 10 minutes.
    pub fn strip(&self, at: DateTime<Local>) -> [Cell; STRIP] {
        let mut cells = [Cell::Away; STRIP];
        let today = at.date_naive();
        let open = match self.state {
            State::Active { since } | State::Overdue { since } => {
                Some(Session { start: since, rest: false, minutes: minutes(since, at) })
            }
            State::Resting { since, needed: _ } => {
                Some(Session { start: since, rest: true, minutes: minutes(since, at) })
            }
            State::Away => None,
        };
        for s in self.sessions.iter().copied().chain(open) {
            if s.start.date_naive() != today {
                continue;
            }
            let from = s.start.hour() * 60 + s.start.minute();
            for m in 0..s.minutes {
                let idx = ((from + m) / 10) as usize;
                if idx >= STRIP {
                    break;
                }
                let c = if s.rest {
                    Cell::Rest
                } else if m >= self.cfg.work {
                    Cell::Overdue
                } else {
                    Cell::Active
                };
                if c != Cell::Active || cells[idx] == Cell::Away {
                    cells[idx] = c;
                }
            }
        }
        cells
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cell {
    Away,
    Active,
    Overdue,
    Rest,
}

// ── log ──────────────────────────────────────────────────────────────────────

fn log_path() -> PathBuf {
    let dir = std::env::current_exe().ok().and_then(|p| p.parent().map(std::path::Path::to_path_buf)).unwrap_or_default();
    dir.join("dosimeter.log")
}

/// `2026-09-11T14:02\tactive\t38`.
pub fn log_line(s: &Session) -> String {
    format!("{}\t{}\t{}", s.start.format("%Y-%m-%dT%H:%M"), if s.rest { "rest" } else { "active" }, s.minutes)
}

/// Parse the log; anything unreadable is skipped, never fatal.
pub fn parse_log(text: &str) -> Vec<Session> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let at = NaiveDateTime::parse_from_str(f.next()?.trim(), "%Y-%m-%dT%H:%M").ok()?;
            let rest = match f.next()? {
                "rest" => true,
                "active" => false,
                _ => return None,
            };
            let minutes: u32 = f.next()?.trim().parse().ok()?;
            let start = at.and_local_timezone(Local).single()?;
            Some(Session { start, rest, minutes })
        })
        .collect()
}

fn append_log(s: &Session) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log_path()) {
        let _ = writeln!(f, "{}", log_line(s));
    }
}

// ── Windows signals ──────────────────────────────────────────────────────────

mod signals {
    use sysinfo::{Pid, ProcessesToUpdate, System};

    /// Seconds since the last keyboard/mouse input (0 if the call fails).
    pub fn idle_secs() -> u64 {
        use windows_sys::Win32::System::SystemInformation::GetTickCount;
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
        let mut lii = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
        if unsafe { GetLastInputInfo(&mut lii) } == 0 {
            return 0;
        }
        let now = unsafe { GetTickCount() };
        (now.wrapping_sub(lii.dwTime) / 1000) as u64
    }

    /// Is the workstation locked? `OpenInputDesktop` returns null while the
    /// secure (Winlogon) desktop is in front — that is the cheapest reliable
    /// lock signal without a window message loop. The handle is closed at once.
    pub fn locked() -> bool {
        use windows_sys::Win32::System::StationsAndDesktops::{CloseDesktop, OpenInputDesktop};
        const GENERIC_READ: u32 = 0x8000_0000;
        let d = unsafe { OpenInputDesktop(0, 0, GENERIC_READ) };
        if d.is_null() {
            return true;
        }
        unsafe { CloseDesktop(d) };
        false
    }

    /// Process name of the foreground window (`vlc`), if it can be resolved.
    pub fn foreground_app(sys: &mut System) -> Option<String> {
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.is_null() {
            return None;
        }
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if pid == 0 {
            return None;
        }
        let pid = Pid::from_u32(pid);
        sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
        sys.process(pid).map(|p| super::app_name(&p.name().to_string_lossy()))
    }
}

#[derive(Clone, Debug)]
struct Sample {
    at: DateTime<Local>,
    idle: u64,
    locked: bool,
    app: Option<String>,
}

/// One thread, one sample every 5 s; the foreground app every 30 s.
fn spawn_signals(tx: Sender<Sample>, watch: bool) {
    std::thread::spawn(move || {
        let mut sys = sysinfo::System::new();
        let mut app = None;
        let mut n: u32 = 0;
        loop {
            if watch && n % 6 == 0 {
                app = signals::foreground_app(&mut sys);
            }
            let s = Sample { at: Local::now(), idle: signals::idle_secs(), locked: signals::locked(), app: app.clone() };
            if tx.send(s).is_err() {
                return;
            }
            n = n.wrapping_add(1);
            std::thread::sleep(Duration::from_secs(5));
        }
    });
}

// ── the module ───────────────────────────────────────────────────────────────

pub struct Dosimeter {
    eng: Engine,
    rx: Option<Receiver<Sample>>,
    /// `Local::now()` cached for the `&self` draw.
    now: DateTime<Local>,
    /// Minutes per foreground app today (memory only, never logged).
    apps: HashMap<String, u32>,
    last_app: Option<String>,
    last_idle: u64,
    alerting: bool,
    frame: u32,
}

impl Default for Dosimeter {
    fn default() -> Self {
        Self::new()
    }
}

impl Dosimeter {
    pub fn new() -> Self {
        let now = Local::now();
        Self {
            eng: Engine::new(DoseCfg::default(), now),
            rx: None,
            now,
            apps: HashMap::new(),
            last_app: None,
            last_idle: 0,
            alerting: false,
            frame: 0,
        }
    }

    fn rads(&self) -> u16 {
        rads(self.eng.session_min(self.now), self.eng.cfg.work)
    }

    fn level_style(&self, t: Theme) -> Style {
        match self.rads() {
            0..=249 => t.value,
            250..=499 => t.warn,
            _ => t.danger,
        }
    }

    fn dismiss(&mut self, ctx: &Ctx) {
        if self.alerting {
            self.alerting = false;
            let _ = ctx.notify.send(Notice::Alert(None));
            ctx.audio.stop_alarm();
        }
    }

    /// The text block under the big readout.
    fn info_lines(&self, width: u16, narrow: bool, t: Theme) -> Vec<Line<'static>> {
        let w = width.max(1) as usize;
        let mut out: Vec<Line<'static>> = Vec::new();
        let session = self.eng.session_min(self.now);
        if !narrow {
            out.push(self.dose_bar(w.saturating_sub(2).max(4), t));
        }
        let since = match self.eng.state {
            State::Active { since } | State::Overdue { since } => since.format("%H:%M").to_string(),
            _ => "--:--".into(),
        };
        out.push(Line::from(vec![
            Span::styled(" session ", t.frame),
            Span::styled(format!("{session} min"), self.level_style(t)),
            Span::styled(format!(" · {} since {since} · last input {} s ago", self.eng.state.name(), self.last_idle), t.frame),
        ]));
        if !narrow {
            let (total, count, longest, breaks, done) = self.eng.today(self.now);
            out.push(Line::from(vec![
                Span::styled(" today ", t.frame),
                Span::styled(hm(total), t.value),
                Span::styled(
                    format!(" in {count} sessions · longest {longest} min · breaks {breaks} ({done} complete)"),
                    t.frame,
                ),
            ]));
        }
        out.push(self.strip_line(w, t));
        if !narrow {
            out.push(Line::from(Span::styled(axis(w), t.frame)));
        }
        match self.eng.state {
            State::Overdue { .. } => out.push(Line::from(Span::styled(
                format!(" TAKE A BREAK · {} min", self.eng.cfg.rest),
                t.danger,
            ))),
            State::Resting { .. } => {
                if let Some((done, left)) = self.eng.rest_left(self.now) {
                    out.push(Line::from(vec![
                        Span::styled(" resting ", t.frame),
                        Span::styled(format!("{done} min"), t.value),
                        Span::styled(format!(" · {left} min to go"), t.frame),
                    ]));
                }
            }
            _ => {}
        }
        if !narrow && self.eng.cfg.watch_foreground && !self.apps.is_empty() {
            let mut top: Vec<(&String, &u32)> = self.apps.iter().collect();
            top.sort_by(|a, b| b.1.cmp(a.1));
            top.truncate(5);
            let text = top.iter().map(|(n, m)| format!("{n} {}", hm(**m / 12))).collect::<Vec<_>>().join(" · ");
            out.push(Line::from(vec![Span::styled(" today by app ", t.frame), Span::styled(text, t.value)]));
        }
        if !narrow && w >= 40 {
            let days = self.eng.daily(self.now, 7);
            out.push(Line::from(vec![
                Span::styled(" last 7 days ", t.frame),
                Span::styled(crate::ui::widgets::spark(&days, 7, 0), t.graph),
                Span::styled(format!(" max {}", hm(days.iter().copied().max().unwrap_or(0) as u32)), t.frame),
            ]));
        }
        out
    }

    /// Dose bar with ticks at `work` and 2×`work`.
    fn dose_bar(&self, w: usize, t: Theme) -> Line<'static> {
        let work = self.eng.cfg.work.max(1);
        let session = self.eng.session_min(self.now);
        let filled = ((session as usize * w) / (work as usize * 2)).min(w);
        let tick = w / 2;
        let mut spans = vec![Span::raw(" ")];
        let mut run = String::new();
        let mut style = self.level_style(t);
        for i in 0..w {
            let (ch, st) = if i == tick || i + 1 == w {
                ('┃', if i < filled { self.level_style(t) } else { t.frame })
            } else if i < filled {
                ('█', self.level_style(t))
            } else {
                ('░', t.frame)
            };
            if st != style && !run.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut run), style));
            }
            style = st;
            run.push(ch);
        }
        spans.push(Span::styled(run, style));
        spans.push(Span::styled(format!(" {} RADS", self.rads()), self.level_style(t)));
        Line::from(spans)
    }

    fn strip_line(&self, w: usize, t: Theme) -> Line<'static> {
        let cells = self.eng.strip(self.now);
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut run = String::new();
        let mut style = t.frame;
        for i in 0..w {
            let c = cells[(i * STRIP / w.max(1)).min(STRIP - 1)];
            let (ch, st) = match c {
                Cell::Away => (' ', t.frame),
                Cell::Active => ('█', t.graph),
                Cell::Overdue => ('█', t.danger),
                Cell::Rest => ('▒', t.value),
            };
            if st != style && !run.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut run), style));
            }
            style = st;
            run.push(ch);
        }
        spans.push(Span::styled(run, style));
        Line::from(spans)
    }
}

/// `00    06    12    18` scale under the strip.
fn axis(w: usize) -> String {
    let mut s = vec![b' '; w];
    for h in (0..24).step_by(6) {
        let col = h * 6 * w / STRIP;
        let label = format!("{h:02}");
        for (i, b) in label.bytes().enumerate() {
            if col + i < w {
                s[col + i] = b;
            }
        }
    }
    String::from_utf8_lossy(&s).into_owned()
}

impl Module for Dosimeter {
    fn id(&self) -> &'static str {
        "dosimeter"
    }
    fn title(&self) -> &'static str {
        "DOSIMETER"
    }
    fn help(&self) -> &'static str {
        "z snooze 5 min   r reset session   1-9 tabs   q quit"
    }
    fn describe(&self) -> &'static str {
        "Screen time as a radiation dose — 45 minutes on, 15 off"
    }

    fn start(&mut self, ctx: &Ctx) {
        let (cfg, notice) = ctx.config.section::<DoseCfg>(self.id());
        if let Some(n) = notice {
            let _ = ctx.notify.send(Notice::Footer(n));
        }
        let watch = cfg.watch_foreground || !cfg.rest_apps.is_empty();
        self.now = Local::now();
        let history = cfg.history.max(1) as i64;
        self.eng = Engine::new(cfg, self.now);
        // Today's totals and the strip come from the log; older days feed the sparkline.
        let text = std::fs::read_to_string(log_path()).unwrap_or_default();
        let cutoff = (self.now - ChronoDur::days(history)).date_naive();
        self.eng.sessions = parse_log(&text).into_iter().filter(|s| s.start.date_naive() >= cutoff).collect();
        self.eng.breaks_today = self
            .eng
            .sessions
            .iter()
            .filter(|s| s.rest && s.start.date_naive() == self.now.date_naive())
            .count() as u32;
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        spawn_signals(tx, watch);
    }

    fn poll(&mut self, ctx: &Ctx) -> usize {
        let mut n = 0;
        let mut samples = Vec::new();
        if let Some(rx) = &self.rx {
            while let Ok(s) = rx.try_recv() {
                samples.push(s);
            }
        }
        for s in samples {
            n += 1;
            self.last_idle = s.idle;
            self.now = s.at;
            let rest_app = is_rest_app(s.app.as_deref(), &self.eng.cfg.rest_apps);
            if self.eng.cfg.watch_foreground && s.idle < 60 {
                if let Some(a) = s.app.as_ref() {
                    // One sample is 5 s: `apps` counts samples, the view divides by 12.
                    *self.apps.entry(a.clone()).or_insert(0) += 1;
                    self.last_app = Some(a.clone());
                }
            }
            let before = self.eng.sessions.len();
            let events = self.eng.step(s.at, s.idle, s.locked, rest_app);
            for new in self.eng.sessions[before.min(self.eng.sessions.len())..].to_vec() {
                append_log(&new);
            }
            for e in events {
                match e {
                    Event::Overdue => {
                        let _ = ctx.notify.send(Notice::Footer(format!(
                            "☢ {} min at the screen — take {}",
                            self.eng.cfg.work, self.eng.cfg.rest
                        )));
                        let _ = ctx.notify.send(Notice::Activate("dosimeter"));
                        let _ = ctx.notify.send(Notice::Alert(Some(ALERT)));
                        self.alerting = true;
                    }
                    Event::RestDone => {
                        let _ = ctx.notify.send(Notice::Footer("rest complete".into()));
                        let _ = ctx.notify.send(Notice::Alert(None));
                        self.alerting = false;
                    }
                    Event::BreakCut(m) => {
                        let _ = ctx.notify.send(Notice::Footer(format!("break cut short after {m} min")));
                    }
                }
            }
            if self.eng.may_sound(s.at) {
                ctx.audio.geiger();
            }
        }
        if n > 0 {
            let (today_min, sessions_today, _, _, _) = self.eng.today(self.now);
            ctx.board.publish(
                self.id(),
                DosimeterSnapshot {
                    state: self.eng.state.name(),
                    session_min: self.eng.session_min(self.now),
                    today_min,
                    sessions_today,
                    overdue: self.eng.overdue(),
                },
            );
        }
        n
    }

    fn tick(&mut self, _ctx: &Ctx) {
        self.frame = self.frame.wrapping_add(1);
        if self.rx.is_none() {
            self.now = Local::now();
        }
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &Ctx) -> bool {
        if self.alerting {
            self.dismiss(ctx);
            return true;
        }
        match key.code {
            KeyCode::Char('z') => {
                self.eng.snooze(Local::now());
                let _ = ctx.notify.send(Notice::Footer(format!("snoozed {SNOOZE_MIN} min")));
                ctx.audio.stop_alarm();
            }
            KeyCode::Char('r') => {
                self.eng.reset(Local::now());
                let _ = ctx.notify.send(Notice::Footer("session reset".into()));
            }
            _ => return false,
        }
        true
    }

    fn draw(&self, f: &mut Frame, area: Rect, t: Theme) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let narrow = area.width < 80;
        let mut lines = self.info_lines(area.width, narrow, t);
        lines.truncate(area.height as usize);
        let big_h = area.height.saturating_sub(lines.len() as u16).min(14);
        let txt = self.rads().to_string();
        let big_w = if narrow { area.width } else { area.width / 2 };
        let scale = bigfont::fit_scale(big_w.saturating_sub(2), big_h, bigfont::text_cols(&txt));
        if let Some(scale) = scale {
            let scale = scale.min(3);
            let h = bigfont::ROWS as u16 * scale + 1;
            let y0 = area.y + big_h.saturating_sub(h) / 2;
            let clip = Rect { height: big_h, ..area };
            {
                let buf = f.buffer_mut();
                bigfont::blit(buf, clip, area.x as i32 + 3, y0 as i32 + 1, &txt, scale, t.frame, true);
                bigfont::blit(buf, clip, area.x as i32 + 2, y0 as i32, &txt, scale, self.level_style(t), false);
            }
            if big_h >= 2 {
                let r = Rect { x: area.x + 1, y: area.y, width: area.width.min(6), height: 1 };
                f.render_widget(Paragraph::new(Line::from(Span::styled("RADS", t.frame))), r);
            }
            let tw = area.width.saturating_sub(big_w);
            if !narrow && tw >= 24 && big_h >= 6 {
                // The trefoil only pulses while the dose is critical.
                let frame = if self.eng.overdue() { 40 + self.frame % 24 } else { 30 };
                let r = Rect { x: area.x + big_w, y: area.y, width: tw, height: big_h };
                f.render_widget(Paragraph::new(anim::trefoil(tw, big_h, frame, t)), r);
            }
        } else if lines.len() < area.height as usize {
            lines.insert(0, Line::from(Span::styled(format!(" RADS {}", self.rads()), self.level_style(t))));
        }
        let y = area.y + big_h;
        let r = Rect { x: area.x, y, width: area.width, height: area.height.saturating_sub(big_h) };
        if r.height > 0 {
            f.render_widget(Paragraph::new(lines), r);
        }
    }

    fn header(&self, width: u16, t: Theme) -> Vec<Span<'static>> {
        let m = self.eng.session_min(self.now);
        let work = self.eng.cfg.work.max(1);
        let text = format!("☢ {m}m");
        if width < text.chars().count() as u16 {
            return vec![];
        }
        if self.eng.overdue() {
            vec![Span::styled(text, t.danger)]
        } else if m * 100 >= work * 80 {
            vec![Span::styled(text, t.warn)]
        } else {
            vec![]
        }
    }

    fn overview(&self, _width: u16, _height: u16, t: Theme) -> Vec<Line<'static>> {
        let (today_min, _, _, _, _) = self.eng.today(self.now);
        let mut out = vec![
            Line::from(Span::styled(" DOSIMETER", t.title)),
            Line::from(vec![
                Span::styled("  RADS ", t.frame),
                Span::styled(self.rads().to_string(), self.level_style(t)),
                Span::styled(
                    format!(" · session {} min · today {}", self.eng.session_min(self.now), hm(today_min)),
                    t.frame,
                ),
            ]),
        ];
        if self.eng.overdue() {
            out.push(Line::from(Span::styled("  ☢ take a break", t.danger)));
        }
        out
    }

    fn overview_slot(&self) -> Slot {
        Slot::Right(9)
    }

    fn wants_fast_frames(&self, active: bool) -> bool {
        active && self.eng.overdue()
    }

    fn status(&self) -> String {
        let (today_min, _, _, _, _) = self.eng.today(self.now);
        format!(
            "dosimeter {} session={} today={}",
            self.eng.state.name(),
            self.eng.session_min(self.now),
            today_min
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemeKind;
    use chrono::TimeZone;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn at(h: u32, m: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 9, 11, h, m, 0).single().expect("local time")
    }

    fn engine() -> Engine {
        Engine::new(DoseCfg::default(), at(9, 0))
    }

    /// Feed `mins` minutes of samples one minute apart (12 real samples/minute
    /// would only slow the test down).
    fn run(e: &mut Engine, from: DateTime<Local>, mins: i64, idle: u64, locked: bool) -> Vec<Event> {
        let mut ev = Vec::new();
        for i in 1..=mins {
            ev.extend(e.step(from + ChronoDur::minutes(i), idle, locked, false));
        }
        ev
    }

    #[test]
    fn away_to_active_to_overdue_to_resting_to_away() {
        let mut e = engine();
        assert_eq!(e.state, State::Away);
        e.step(at(9, 0), 0, false, false);
        assert_eq!(e.state, State::Active { since: at(9, 0) });
        let ev = run(&mut e, at(9, 0), 44, 0, false);
        assert!(ev.is_empty(), "no alert before work minutes");
        let ev = run(&mut e, at(9, 44), 1, 0, false);
        assert_eq!(ev, vec![Event::Overdue]);
        assert_eq!(e.state, State::Overdue { since: at(9, 0) });
        assert_eq!(e.session_min(at(9, 45)), 45);
        // Walk away: the session is closed and logged, the break starts.
        e.step(at(9, 50), 5 * 60, false, false);
        assert_eq!(e.state, State::Resting { since: at(9, 50), needed: 15 });
        assert_eq!(e.sessions.last().map(|s| (s.rest, s.minutes)), Some((false, 45)));
        let ev = run(&mut e, at(9, 50), 15, 5 * 60, false);
        assert_eq!(ev, vec![Event::RestDone]);
        assert_eq!(e.state, State::Away);
        assert_eq!(e.sessions.last().map(|s| (s.rest, s.minutes)), Some((true, 15)));
        assert_eq!(e.session_min(at(10, 10)), 0, "counter reset");
    }

    #[test]
    fn short_gap_keeps_the_session_and_lock_ends_it() {
        let mut e = engine();
        e.step(at(9, 0), 0, false, false);
        e.step(at(9, 2), 100, false, false); // 1 min 40 s idle < 3 min
        assert_eq!(e.state, State::Active { since: at(9, 0) });
        e.step(at(9, 10), 0, false, false);
        e.step(at(9, 11), 0, true, false); // locked
        assert_eq!(e.state, State::Away);
        assert_eq!(e.sessions.last().map(|s| s.minutes), Some(10), "session ends at the last input");
    }

    #[test]
    fn rest_app_counts_as_rest() {
        let mut e = engine();
        e.step(at(9, 0), 0, false, false);
        e.step(at(9, 5), 0, false, true);
        assert_eq!(e.state, State::Away, "vlc in the foreground is not screen work");
        assert!(is_rest_app(Some("vlc"), &["vlc".into(), "mpv".into()]));
        assert!(is_rest_app(Some("mpv"), &["mpv".into()]));
        assert!(!is_rest_app(Some("code"), &["vlc".into()]));
        assert!(!is_rest_app(None, &["vlc".into()]));
        assert!(!is_rest_app(Some("code"), &[]));
        assert_eq!(app_name("C:\\Program Files\\VideoLAN\\VLC.exe"), "vlc");
        assert_eq!(app_name("mpv.exe"), "mpv");
    }

    #[test]
    fn cut_short_break_keeps_the_counter() {
        let mut e = engine();
        e.step(at(9, 0), 0, false, false);
        run(&mut e, at(9, 0), 45, 0, false);
        e.step(at(9, 50), 5 * 60, false, false);
        assert!(matches!(e.state, State::Resting { .. }));
        let ev = e.step(at(9, 55), 0, false, false);
        assert_eq!(ev, vec![Event::BreakCut(5), Event::Overdue], "back after 5 min, still critical");
        assert_eq!(e.session_min(at(9, 55)), 45, "the previous session length is kept");
        assert_eq!(e.breaks_today, 1);
        let (_, _, _, breaks, done) = e.today(at(9, 55));
        assert_eq!((breaks, done), (1, 0), "a cut-short break is not complete");
    }

    #[test]
    fn snooze_and_quiet_hours_silence_the_burst() {
        let mut e = engine();
        e.step(at(9, 0), 0, false, false);
        run(&mut e, at(9, 0), 45, 0, false);
        assert!(e.may_sound(at(9, 45)));
        assert!(!e.may_sound(at(9, 47)), "at most one burst per 5 min");
        assert!(e.may_sound(at(9, 51)));
        e.snooze(at(9, 52));
        assert!(!e.may_sound(at(9, 56)));
        assert!(e.may_sound(at(9, 58)), "snooze over");
        assert!(in_quiet(&["22:00-07:00".into()], NaiveTime::from_hms_opt(23, 30, 0).unwrap()));
        assert!(in_quiet(&["22:00-07:00".into()], NaiveTime::from_hms_opt(3, 0, 0).unwrap()));
        assert!(!in_quiet(&["22:00-07:00".into()], NaiveTime::from_hms_opt(12, 0, 0).unwrap()));
        assert!(in_quiet(&["09:00-10:00".into()], NaiveTime::from_hms_opt(9, 30, 0).unwrap()));
        assert!(!in_quiet(&["nonsense".into(), "x-y".into()], NaiveTime::from_hms_opt(9, 30, 0).unwrap()));
        e.cfg.quiet = vec!["00:00-23:59".into()];
        e.last_nag = None;
        assert!(!e.may_sound(at(10, 30)), "quiet hours");
    }

    #[test]
    fn rads_scale_and_clamp() {
        assert_eq!(rads(0, 45), 0);
        assert_eq!(rads(45, 45), 500);
        assert_eq!(rads(90, 45), 999, "twice the limit pegs the needle");
        assert_eq!(rads(200, 45), 999);
        assert_eq!(rads(22, 45), 244);
        assert_eq!(rads(10, 0), 999, "a zero work limit must not divide by zero");
    }

    #[test]
    fn strip_marks_active_overdue_and_rest() {
        let mut e = engine();
        e.sessions = vec![
            Session { start: at(9, 0), rest: false, minutes: 60 },
            Session { start: at(10, 0), rest: true, minutes: 20 },
        ];
        let s = e.strip(at(11, 0));
        assert_eq!(s[0], Cell::Away, "00:00 is empty");
        assert_eq!(s[54], Cell::Active, "09:00");
        assert_eq!(s[56], Cell::Active, "09:20, still under the limit");
        assert_eq!(s[59], Cell::Overdue, "09:50, past 45 min");
        assert_eq!(s[60], Cell::Rest, "10:00");
        assert_eq!(s[62], Cell::Away, "10:20, break over");
        assert_eq!(s.len(), STRIP);
        let (total, count, longest, _, done) = e.today(at(11, 0));
        assert_eq!((total, count, longest, done), (60, 1, 60, 1));
    }

    #[test]
    fn log_roundtrip_skips_corrupt_lines() {
        let s = Session { start: at(14, 2), rest: false, minutes: 38 };
        assert_eq!(log_line(&s), "2026-09-11T14:02\tactive\t38");
        let text = "2026-09-11T14:02\tactive\t38\n\
                    2026-09-11T14:40\trest\t15\n\
                    garbage\n\
                    2026-09-11T14:02\tsleep\t9\n\
                    2026-13-99T99:99\tactive\t1\n\
                    2026-09-11T15:00\tactive\tmany\n\
                    2026-09-11T15:00\tactive\n";
        let out = parse_log(text);
        assert_eq!(out.len(), 2, "only the two good lines");
        assert_eq!(out[0], s);
        assert_eq!(out[1].rest, true);
        assert_eq!(out[1].minutes, 15);
        assert_eq!(parse_log(""), vec![]);
    }

    #[test]
    fn daily_totals_feed_the_sparkline() {
        let mut e = engine();
        e.sessions = vec![
            Session { start: at(9, 0) - ChronoDur::days(1), rest: false, minutes: 30 },
            Session { start: at(9, 0), rest: false, minutes: 60 },
            Session { start: at(10, 0), rest: true, minutes: 15 },
        ];
        let d = e.daily(at(11, 0), 7);
        assert_eq!(d.len(), 7);
        assert_eq!(d[6], 60, "today, breaks excluded");
        assert_eq!(d[5], 30, "yesterday");
        assert_eq!(d[0], 0);
    }

    #[test]
    fn draw_survives_every_size_and_state() {
        let t = Theme::new(ThemeKind::Color);
        let mut d = Dosimeter::new();
        d.now = at(14, 40);
        d.apps.insert("code".into(), 120);
        d.eng.sessions = vec![Session { start: at(9, 0), rest: false, minutes: 60 }];
        let states = [
            State::Away,
            State::Active { since: at(14, 2) },
            State::Overdue { since: at(13, 55) },
            State::Resting { since: at(14, 35), needed: 15 },
        ];
        for st in states {
            d.eng.state = st;
            for (w, h) in [(1u16, 1u16), (40, 12), (80, 24), (120, 40), (2, 3)] {
                let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
                term.draw(|f| d.draw(f, f.area(), t)).unwrap();
                let _ = d.header(w, t);
                let _ = d.overview(w, h, t);
                let _ = d.status();
            }
        }
        d.eng.cfg.watch_foreground = true;
        d.eng.state = State::Overdue { since: at(13, 55) };
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| d.draw(f, f.area(), t)).unwrap();
        let text: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("RADS"), "the readout label is there");
        assert!(text.contains("TAKE A BREAK"), "the overdue line is there");
        assert!(d.wants_fast_frames(true) && !d.wants_fast_frames(false));
        assert!(!d.header(120, t).is_empty(), "overdue shows in the header");
        d.eng.state = State::Away;
        assert!(d.header(120, t).is_empty(), "no header noise when away");
        assert!(d.header(2, t).is_empty(), "no header in 2 columns");
    }

    #[test]
    fn signals_do_not_panic() {
        let idle = signals::idle_secs();
        assert!(idle < 60 * 60 * 24 * 365, "idle seconds are sane: {idle}");
        let _ = signals::locked();
    }
}
