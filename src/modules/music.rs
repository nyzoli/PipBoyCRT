//! MUSIC module: a folder browser over local files on the shell's shared mixer.
//!
//! Its `Player` connects to `ctx.audio.mixer()` — the same mixer RADIO uses —
//! but only one of the two is ever audible: starting playback claims the
//! `module::AudioFocus` token and the other module pauses itself. The volume
//! here is the module's own (it does not touch `ctx.audio.set_volume`, which
//! owns the alarm). One directory level is read at a time on a background
//! thread (the library may live on a slow network share), playback runs on
//! another one. The decoded samples pass through `ui::vu::Vu`, the same VU
//! tap RADIO uses.

use crate::module::{next_focus_seq, AudioFocus, Ctx, Module, Notice, Slot, AUDIO_FOCUS};
use crate::style::Theme;
use crate::ui::vu::{Vu, BANDS, VU_BLOCK};
use crate::ui::widgets::{gauge, spectrum, truncate, vu_bar};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use rodio::mixer::Mixer;
use rodio::{Decoder, Player, Source};
use serde::Deserialize;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// Extensions we offer to rodio. Codecs enabled in `Cargo.toml`:
/// `symphonia-mp3`, `symphonia-aac` + `symphonia-isomp4` (m4a),
/// `symphonia-flac`, `symphonia-wav` + `symphonia-pcm`.
const EXTS: [&str; 5] = ["mp3", "aac", "m4a", "flac", "wav"];
const DEFAULT_VOLUME: u8 = 70;
/// `Player::empty()` is only trusted after this long, so a freshly appended
/// track is never mistaken for a finished one.
const START_GRACE: Duration = Duration::from_millis(600);
/// Width of the spectrum column on wide layouts (2 cells per band + slack).
const SPECTRUM_W: u16 = 34;
/// From this width on the VU gets its own column instead of a bar in the footer.
const WIDE: u16 = 100;

// ---- library ---------------------------------------------------------------

/// One row of the browser: a subfolder or a playable file of the current
/// directory. `label` is the folder name, or `artist – title` for a file
/// (cached at read time so `draw` doesn't reformat every row every frame).
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub path: PathBuf,
    pub is_dir: bool,
    pub label: String,
}

/// `artist – title`, or just the title when there is no artist part.
fn make_label(artist: &str, title: &str) -> String {
    if artist.is_empty() {
        title.to_string()
    } else {
        format!("{artist} – {title}")
    }
}

/// `"Artist - Title.mp3"` → `("Artist", "Title")`; anything without a `" - "`
/// (or `" – "`) separator is all title. No ID3: the file name is the metadata.
pub fn split_name(file_name: &str) -> (String, String) {
    let stem = Path::new(file_name).file_stem().and_then(|s| s.to_str()).unwrap_or(file_name).trim();
    match stem.split_once(" - ").or_else(|| stem.split_once(" – ")) {
        Some((a, t)) if !a.trim().is_empty() && !t.trim().is_empty() => (a.trim().to_string(), t.trim().to_string()),
        _ => (String::new(), stem.to_string()),
    }
}

/// `true` for the extensions we have a decoder for (case-insensitive).
pub fn is_music(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Subfolders first, then files; each group by name, case-insensitively.
pub fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
            .then_with(|| a.label.cmp(&b.label))
    });
}

/// One `read_dir` level, never recursive: the tree may be thousands of albums
/// on a network share. An unreadable directory (ACL, offline share, gone) is
/// an `Err` the UI can show — never a silently empty list. Directory
/// junctions/symlinks are not followed (`file_type()` reports the link itself,
/// unlike `Path::is_dir()`).
pub fn read_folder(dir: &Path) -> Result<Vec<Entry>, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for e in rd.flatten() {
        let is_dir = e.file_type().map(|f| f.is_dir()).unwrap_or(false);
        let path = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if is_dir {
            out.push(Entry { path, is_dir: true, label: name });
        } else if is_music(&path) {
            let (artist, title) = split_name(&name);
            out.push(Entry { path, is_dir: false, label: make_label(&artist, &title) });
        }
    }
    sort_entries(&mut out);
    Ok(out)
}

/// The folder to go up to, or `None` at (or outside) the root.
pub fn parent_of(root: &Path, cwd: &Path) -> Option<PathBuf> {
    if cwd == root {
        return None;
    }
    let p = cwd.parent()?;
    p.starts_with(root).then(|| p.to_path_buf())
}

/// `♫ Library › Music › Artist › Album`: `cwd` shown from two components
/// above the root — on a share like `\\nas\Media\Library\Music` the share
/// itself is the path prefix, so those two are what name the library —
/// truncated from the left with `…` when it doesn't fit `max` columns.
pub fn breadcrumb(root: &Path, cwd: &Path, max: usize) -> String {
    if max < 3 {
        return String::new();
    }
    let base = root.parent().and_then(Path::parent).or_else(|| root.parent()).unwrap_or(root);
    let rel = cwd.strip_prefix(base).unwrap_or(cwd);
    let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    let s = parts.join(" › ");
    let budget = max - 2; // "♫ "
    let n = s.chars().count();
    if n <= budget {
        return format!("♫ {s}");
    }
    let tail: String = s.chars().skip(n - (budget - 1)).collect();
    format!("♫ …{tail}")
}

/// Which track to play next. `delta` is `+1`/`-1`; shuffle ignores it and
/// picks any index. `None` for an empty playlist.
pub fn advance(len: usize, current: Option<usize>, delta: isize, shuffle: bool, seed: &mut u64) -> Option<usize> {
    if len == 0 {
        return None;
    }
    if shuffle {
        if len < 2 {
            return Some(0);
        }
        loop {
            let idx = (xorshift(seed) % len as u64) as usize;
            if Some(idx) != current {
                return Some(idx);
            }
        }
    }
    let cur = current.unwrap_or(0) as isize;
    Some((cur + delta).rem_euclid(len as isize) as usize)
}

/// After this many playback errors in a row, MUSIC gives up advancing
/// instead of churning through an entirely unplayable folder forever.
const MAX_CONSECUTIVE_FAILURES: u32 = 5;

/// Whether `fail_count` consecutive playback errors mean "give up".
fn too_many_failures(fail_count: u32) -> bool {
    fail_count >= MAX_CONSECUTIVE_FAILURES
}

/// xorshift64: enough randomness for shuffle, no `rand` dependency.
fn xorshift(s: &mut u64) -> u64 {
    let mut x = *s;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *s = x;
    x
}

// ---- playback thread -------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum MusicCmd {
    Play(PathBuf),
    Pause,
    Resume,
    Volume(u8),
}

/// Sent by `run` when `ctx.audio.mixer()` was `None` at spawn time; matched
/// verbatim in `poll` to distinguish "no audio device" from a per-track error.
const NO_AUDIO_ERR: &str = "no audio device";

#[derive(Debug, Clone, PartialEq)]
pub enum MusicEvent {
    Started(Option<Duration>),
    Pos(Duration),
    Level(f32),
    Spectrum([u8; BANDS]),
    Ended,
    Error(String),
}

/// Snapshot on `ctx.board` under `"music"`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MusicSnapshot {
    pub playing: bool,
    pub title: String,
}

fn spawn(mixer: Option<Mixer>, tx: Sender<MusicEvent>) -> Sender<MusicCmd> {
    let (ctx, crx) = mpsc::channel();
    std::thread::spawn(move || run(mixer, tx, crx));
    ctx
}

fn open(path: &Path) -> Result<(Decoder<std::io::BufReader<File>>, Option<Duration>), String> {
    let f = File::open(path).map_err(|e| e.to_string())?;
    let dec = Decoder::try_from(f).map_err(|e| format!("decoder: {e}"))?;
    let total = dec.total_duration();
    Ok((dec, total))
}

fn run(mixer: Option<Mixer>, tx: Sender<MusicEvent>, crx: Receiver<MusicCmd>) {
    let Some(mixer) = mixer else {
        let _ = tx.send(MusicEvent::Error(NO_AUDIO_ERR.into()));
        return;
    };
    let player = Player::connect_new(&mixer);
    player.set_volume(DEFAULT_VOLUME as f32 / 100.0);
    // `Some(started)` while a track is loaded; the instant guards `empty()`.
    let mut active: Option<Instant> = None;
    loop {
        match crx.recv_timeout(Duration::from_millis(250)) {
            Ok(MusicCmd::Play(path)) => {
                player.clear();
                match open(&path) {
                    Ok((src, total)) => {
                        player.append(Vu::new(src, tx.clone(), VU_BLOCK, MusicEvent::Level, MusicEvent::Spectrum));
                        player.play();
                        active = Some(Instant::now());
                        let _ = tx.send(MusicEvent::Started(total));
                    }
                    Err(e) => {
                        active = None;
                        let _ = tx.send(MusicEvent::Error(e));
                    }
                }
            }
            Ok(MusicCmd::Pause) => player.pause(),
            Ok(MusicCmd::Resume) => player.play(),
            Ok(MusicCmd::Volume(v)) => player.set_volume(v as f32 / 100.0),
            Err(RecvTimeoutError::Timeout) => {
                let Some(started) = active else { continue };
                if started.elapsed() > START_GRACE && player.empty() {
                    active = None;
                    let _ = tx.send(MusicEvent::Ended);
                } else if !player.is_paused() {
                    let _ = tx.send(MusicEvent::Pos(player.get_pos()));
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

// ---- module ----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct MusicCfg {
    /// Library root, relative to the executable (absolute is honoured too).
    pub dir: String,
    pub shuffle: bool,
}

impl Default for MusicCfg {
    fn default() -> Self {
        Self { dir: "music".to_string(), shuffle: false }
    }
}

pub struct Music {
    /// Library root from `[music] dir`; navigation never goes above it.
    root: PathBuf,
    /// The folder the browser is showing.
    cwd: PathBuf,
    entries: Vec<Entry>,
    selected: usize,
    /// Cursor position per folder, so going back lands where you left.
    sel_memory: HashMap<PathBuf, usize>,
    /// The playable files of the folder a track was started from; browsing
    /// elsewhere does not touch it.
    playlist: Vec<Entry>,
    current: Option<usize>,
    playing: bool,
    volume: u8,
    shuffle: bool,
    elapsed: Duration,
    total: Option<Duration>,
    level: f32,
    spectrum: [u8; BANDS],
    error: Option<String>,
    /// Why the last `read_dir` of `cwd` failed; `None` when it succeeded.
    ls_error: Option<String>,
    loading: bool,
    /// Highest `AudioFocus::seq` this module published or already reacted to.
    focus_seq: u64,
    seed: u64,
    /// Consecutive per-track playback failures; reset on a successful start.
    fail_count: u32,
    /// `true` once `run` reported no audio device — the now-playing line
    /// says so instead of looking paused.
    no_audio: bool,
    list_state: RefCell<ListState>,
    rx: Option<Receiver<MusicEvent>>,
    tx: Option<Sender<MusicCmd>>,
    ls_rx: Option<Receiver<(PathBuf, Result<Vec<Entry>, String>)>>,
}

impl Default for Music {
    fn default() -> Self {
        Self {
            root: PathBuf::from("music"),
            cwd: PathBuf::from("music"),
            entries: Vec::new(),
            selected: 0,
            sel_memory: HashMap::new(),
            playlist: Vec::new(),
            current: None,
            playing: false,
            volume: DEFAULT_VOLUME,
            shuffle: false,
            elapsed: Duration::ZERO,
            total: None,
            level: 0.0,
            spectrum: [0; BANDS],
            error: None,
            ls_error: None,
            loading: false,
            focus_seq: 0,
            fail_count: 0,
            no_audio: false,
            // Nonzero seed; xorshift stays stuck at 0 forever otherwise.
            seed: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(1)
                | 1,
            list_state: RefCell::new(ListState::default()),
            rx: None,
            tx: None,
            ls_rx: None,
        }
    }
}

impl Music {
    pub fn new() -> Self {
        Self::default()
    }

    fn send(&self, cmd: MusicCmd) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(cmd);
        }
    }

    /// `artist – title` of the playing track, empty when nothing is loaded.
    fn now_title(&self) -> String {
        self.current.and_then(|i| self.playlist.get(i)).map(|t| t.label.clone()).unwrap_or_default()
    }

    fn silence(&mut self) {
        self.level = 0.0;
        self.spectrum = [0; BANDS];
    }

    /// Claim the audio focus: RADIO pauses itself on its next `poll`.
    fn take_focus(&mut self, ctx: &Ctx) {
        self.focus_seq = next_focus_seq();
        ctx.board.publish(AUDIO_FOCUS, AudioFocus { owner: self.id(), seq: self.focus_seq });
    }

    fn pause(&mut self) {
        self.playing = false;
        self.silence();
        self.send(MusicCmd::Pause);
    }

    /// RADIO took the focus: pause so the two never sound at once.
    /// `true` when this poll actually paused us.
    fn yield_focus(&mut self, ctx: &Ctx) -> bool {
        // ponytail: a 2-word clone per poll; a separate seq key to gate it is not worth the second publish.
        let Some(f) = ctx.board.get::<AudioFocus>(AUDIO_FOCUS) else { return false };
        if f.owner == self.id() || f.seq <= self.focus_seq {
            return false;
        }
        self.focus_seq = f.seq;
        if !self.playing {
            return false;
        }
        self.pause();
        let _ = ctx.notify.send(Notice::Footer("music paused — radio is playing".to_string()));
        true
    }

    fn play(&mut self, idx: usize, ctx: &Ctx) {
        let Some(path) = self.playlist.get(idx).map(|tr| tr.path.clone()) else { return };
        self.take_focus(ctx);
        self.current = Some(idx);
        self.playing = true;
        self.elapsed = Duration::ZERO;
        self.total = None;
        self.error = None;
        self.send(MusicCmd::Play(path));
    }

    fn step(&mut self, delta: isize, ctx: &Ctx) {
        if let Some(i) = advance(self.playlist.len(), self.current, delta, self.shuffle, &mut self.seed) {
            self.play(i, ctx);
        }
    }

    /// Enter: descend into a folder, or make the current folder the playlist
    /// and start the selected file.
    fn activate(&mut self, ctx: &Ctx) {
        let Some(e) = self.entries.get(self.selected) else { return };
        if e.is_dir {
            let dir = e.path.clone();
            self.go(dir);
            return;
        }
        let path = e.path.clone();
        self.playlist = self.entries.iter().filter(|e| !e.is_dir).cloned().collect();
        let idx = self.playlist.iter().position(|t| t.path == path).unwrap_or(0);
        self.play(idx, ctx);
    }

    /// Space: pause/resume, or start the selection when nothing is loaded.
    fn toggle(&mut self, ctx: &Ctx) {
        match (self.current, self.playing) {
            (Some(_), true) => self.pause(),
            (Some(_), false) => {
                self.take_focus(ctx);
                self.playing = true;
                self.send(MusicCmd::Resume);
            }
            (None, _) => self.activate(ctx),
        }
    }

    /// Show `dir`, remembering where the cursor stood in the old folder.
    fn go(&mut self, dir: PathBuf) {
        self.sel_memory.insert(self.cwd.clone(), self.selected);
        self.cwd = dir;
        self.selected = self.sel_memory.get(&self.cwd).copied().unwrap_or(0);
        self.reload();
    }

    /// Read `cwd` on a background thread; a network share can take seconds.
    fn reload(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.ls_rx = Some(rx);
        self.loading = true;
        self.ls_error = None;
        self.entries.clear();
        let dir = self.cwd.clone();
        std::thread::spawn(move || {
            let res = read_folder(&dir);
            let _ = tx.send((dir, res));
        });
    }

    /// The bottom block: now playing, progress, volume/shuffle (+ the level
    /// bar when the VU has no column of its own).
    fn footer(&self, width: u16, rows: u16, vu: bool, t: Theme) -> Vec<Line<'static>> {
        let title = if self.no_audio { NO_AUDIO_ERR.to_string() } else { self.now_title() };
        let icon = if self.no_audio || self.current.is_none() {
            "■"
        } else if self.playing {
            "▶"
        } else {
            "‖"
        };
        let pos = match self.total {
            Some(tot) => format!("{} / {}", mmss(self.elapsed), mmss(tot)),
            None if self.current.is_some() => mmss(self.elapsed),
            None => String::new(),
        };
        let marks = format!("vol {}%{}", self.volume, if self.shuffle { "  shuffle" } else { "" });
        if rows < 3 {
            let budget = (width as usize).saturating_sub(marks.chars().count() + pos.chars().count() + 6);
            return vec![Line::from(vec![
                Span::styled(format!("{icon} {} ", truncate(&title, budget)), t.nowplaying),
                Span::styled(format!("{pos}  "), t.value),
                Span::styled(marks, t.frame),
            ])];
        }
        let bar_w = (width as usize).saturating_sub(pos.chars().count() + 4).clamp(1, 60);
        let ratio = match self.total {
            Some(tot) if !tot.is_zero() => self.elapsed.as_secs_f32() / tot.as_secs_f32(),
            _ => 0.0,
        };
        let mut last = Line::from(Span::styled(format!("{marks}  "), t.frame));
        if vu {
            let vu_w = (width as usize).saturating_sub(marks.chars().count() + 4).min(40);
            if vu_w > 0 {
                last.spans.extend(vu_bar((self.level * 300.0).min(100.0) as u8, vu_w, t).spans);
            }
        }
        vec![
            Line::from(Span::styled(
                format!("{icon} {}", truncate(&title, (width as usize).saturating_sub(2))),
                t.nowplaying,
            )),
            Line::from(vec![Span::styled(gauge(ratio, bar_w), t.graph), Span::styled(format!(" {pos}"), t.value)]),
            last,
        ]
    }
}

fn mmss(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}", s / 60, s % 60)
}

impl Module for Music {
    fn id(&self) -> &'static str {
        "music"
    }
    fn title(&self) -> &'static str {
        "MUSIC"
    }
    fn describe(&self) -> &'static str {
        "Your music folder on the same mixer, with a VU"
    }
    fn help(&self) -> &'static str {
        "↑/↓ select   enter open/play   ←/bksp up   space pause   n/p next/prev   s shuffle   +/- volume   r reread"
    }

    fn start(&mut self, ctx: &Ctx) {
        let (cfg, notice) = ctx.config.section::<MusicCfg>(self.id());
        if let Some(n) = notice {
            let _ = ctx.notify.send(Notice::Footer(n));
        }
        self.shuffle = cfg.shuffle;
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default();
        let p = Path::new(&cfg.dir);
        self.root = if p.is_absolute() { p.to_path_buf() } else { exe_dir.join(p) };
        self.cwd = self.root.clone();
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.tx = Some(spawn(ctx.audio.mixer(), tx));
        self.send(MusicCmd::Volume(self.volume));
        self.reload();
    }

    fn poll(&mut self, ctx: &Ctx) -> usize {
        let mut n = usize::from(self.yield_focus(ctx));
        if let Some(rx) = &self.ls_rx {
            if let Ok((dir, res)) = rx.try_recv() {
                // An answer for a folder we already left changes nothing but
                // the spinner — which stops either way, so it can never stick.
                self.loading = false;
                if dir == self.cwd {
                    match res {
                        Ok(entries) => {
                            self.entries = entries;
                            self.selected = self.selected.min(self.entries.len().saturating_sub(1));
                        }
                        Err(e) => {
                            self.entries.clear();
                            let _ = ctx.notify.send(Notice::Footer(format!("music: {e}")));
                            self.ls_error = Some(e);
                        }
                    }
                }
                n += 1;
            }
        }
        let mut events = Vec::new();
        if let Some(rx) = &self.rx {
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
                n += 1;
            }
        }
        for ev in events {
            match ev {
                MusicEvent::Started(total) => {
                    self.total = total;
                    self.elapsed = Duration::ZERO;
                    self.fail_count = 0;
                }
                MusicEvent::Pos(p) => self.elapsed = p,
                MusicEvent::Level(l) => self.level = l,
                MusicEvent::Spectrum(b) => self.spectrum = b,
                MusicEvent::Ended => self.step(1, ctx),
                MusicEvent::Error(e) if e == NO_AUDIO_ERR => {
                    self.no_audio = true;
                    self.playing = false;
                    self.error = Some(e);
                }
                MusicEvent::Error(e) => {
                    self.fail_count += 1;
                    if too_many_failures(self.fail_count) {
                        self.playing = false;
                        self.current = None;
                        self.silence();
                        let _ = ctx.notify.send(Notice::Footer(
                            "music: too many unplayable files — check [music] dir".to_string(),
                        ));
                    } else {
                        let _ = ctx.notify.send(Notice::Footer(format!("music: {e}")));
                        self.step(1, ctx);
                    }
                    self.error = Some(e);
                }
            }
        }
        // ponytail: unconditional publish at 4–20 fps; a dirty flag if it ever shows up in a profile.
        ctx.board.publish("music", MusicSnapshot { playing: self.playing, title: self.now_title() });
        n
    }

    /// Tab-local only: the active module wins over RADIO's global `space`/`+`/`-`.
    fn on_key(&mut self, key: KeyEvent, ctx: &Ctx) -> bool {
        match key.code {
            // While LOADING… `entries` is empty: clamping now would throw away
            // the cursor position restored for this folder.
            KeyCode::Up | KeyCode::Down if self.loading => {}
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(self.entries.len().saturating_sub(1)),
            KeyCode::Enter => self.activate(ctx),
            // `←` stays free for tab switching at the root.
            KeyCode::Left | KeyCode::Backspace => match parent_of(&self.root, &self.cwd) {
                Some(up) => self.go(up),
                None => return key.code == KeyCode::Backspace,
            },
            KeyCode::Char(' ') => self.toggle(ctx),
            KeyCode::Char('n') => self.step(1, ctx),
            KeyCode::Char('p') => self.step(-1, ctx),
            KeyCode::Char('s') => self.shuffle = !self.shuffle,
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.volume = (self.volume + 5).min(100);
                self.send(MusicCmd::Volume(self.volume));
            }
            KeyCode::Char('-') => {
                self.volume = self.volume.saturating_sub(5);
                self.send(MusicCmd::Volume(self.volume));
            }
            KeyCode::Char('r') => self.reload(),
            _ => return false,
        }
        true
    }

    fn draw(&self, f: &mut Frame, area: Rect, t: Theme) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let crumb = breadcrumb(&self.root, &self.cwd, area.width as usize);
        let want = if area.width < 80 { 1 } else { 3 };
        let foot = want.min(area.height.saturating_sub(2));
        let [head, body, foot_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0), Constraint::Length(foot)])
            .areas(area);
        f.render_widget(Paragraph::new(Line::from(Span::styled(crumb, t.title))), head);

        // A wide window gets the RADIO-style spectrum column; a narrow one a
        // level bar in the footer.
        let wide = area.width >= WIDE;
        let [list_area, vu_area] = if wide {
            Layout::horizontal([Constraint::Min(0), Constraint::Length(SPECTRUM_W)]).areas(body)
        } else {
            [body, Rect::new(body.x, body.y, 0, 0)]
        };

        if self.entries.is_empty() {
            let msg = match (self.loading, &self.ls_error, &self.error) {
                (true, _, _) => "LOADING…".to_string(),
                (_, Some(e), _) => format!("cannot read folder: {e}"),
                (_, None, Some(e)) => e.clone(),
                (_, None, None) => format!("no music in {} — set [music] dir in config.toml", self.cwd.display()),
            };
            let centred = Paragraph::new(msg)
                .wrap(Wrap { trim: true })
                .style(t.frame)
                .alignment(ratatui::layout::Alignment::Center);
            f.render_widget(centred, list_area);
        } else {
            let now = self.current.and_then(|i| self.playlist.get(i)).map(|e| e.path.as_path());
            let items: Vec<ListItem> = self
                .entries
                .iter()
                .map(|e| {
                    let playing = !e.is_dir && now == Some(e.path.as_path());
                    let style = if playing {
                        t.nowplaying
                    } else if e.is_dir {
                        t.title
                    } else {
                        t.text
                    };
                    let mark = if playing { "▶ " } else { "  " };
                    let label = if e.is_dir { format!("[DIR] {}", e.label) } else { e.label.clone() };
                    ListItem::new(Line::from(Span::styled(format!("{mark}{label}"), style)))
                })
                .collect();
            let mut state = self.list_state.borrow_mut();
            state.select(Some(self.selected.min(self.entries.len() - 1)));
            f.render_stateful_widget(List::new(items).highlight_style(t.tab_active), list_area, &mut state);
        }

        if wide && vu_area.height > 1 {
            let [sh, sb] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(vu_area);
            f.render_widget(Paragraph::new(Line::from(Span::styled(" SPECTRUM ", t.title))), sh);
            f.render_widget(Paragraph::new(spectrum(&self.spectrum, sb, t)), sb);
        }
        if foot > 0 {
            f.render_widget(Paragraph::new(self.footer(foot_area.width, foot, !wide, t)), foot_area);
        }
    }

    /// 20 fps only on this tab, and only while something is actually playing.
    fn wants_fast_frames(&self, active: bool) -> bool {
        active && self.playing
    }

    /// `♫ <title>` while a track is playing; nothing under 6 columns.
    fn header(&self, width: u16, t: Theme) -> Vec<Span<'static>> {
        if !self.playing || width < 6 {
            return vec![];
        }
        let title = truncate(&self.now_title(), (width as usize).saturating_sub(2));
        if title.is_empty() {
            return vec![];
        }
        vec![Span::styled(format!("♫ {title}"), t.nowplaying)]
    }

    fn overview(&self, width: u16, _height: u16, t: Theme) -> Vec<Line<'static>> {
        let second = match self.current {
            None => {
                let n = self.entries.iter().filter(|e| !e.is_dir).count();
                Span::styled(format!("   ■ idle · {n} tracks here"), t.frame)
            }
            Some(_) => {
                let icon = if self.playing { "▶" } else { "‖" };
                let el = mmss(self.elapsed);
                let budget = (width as usize).saturating_sub(el.chars().count() + 8);
                Span::styled(format!("   {icon} {} {el}", truncate(&self.now_title(), budget)), t.nowplaying)
            }
        };
        let mut lines = vec![Line::from(Span::styled(" MUSIC", t.title)), Line::from(second)];
        if self.playing && self.current.is_some() {
            // Same VU line as the RADIO block.
            let vu_w = width.saturating_sub(18).min(30) as usize;
            let mut line = vu_bar((self.level * 300.0).min(100.0) as u8, vu_w, t);
            line.spans.insert(0, Span::raw("   "));
            line.spans.push(Span::styled(format!("  VOL {}%", self.volume), t.frame));
            lines.push(line);
        }
        lines
    }

    fn overview_slot(&self) -> Slot {
        Slot::Right(7)
    }

    fn status(&self) -> String {
        let playing = if self.playing && self.current.is_some() { self.now_title() } else { "none".to_string() };
        format!("music {} tracks, playing={playing}", self.playlist.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemeKind;
    use crate::shell::test_ctx;
    use ratatui::crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn dir(name: &str) -> Entry {
        Entry { path: PathBuf::from("/root").join(name), is_dir: true, label: name.to_string() }
    }

    fn file(name: &str) -> Entry {
        let (artist, title) = split_name(name);
        Entry { path: PathBuf::from("/root").join(name), is_dir: false, label: make_label(&artist, &title) }
    }

    /// A browser sitting in `/root` with the given entries.
    fn fake(entries: Vec<Entry>) -> Music {
        let mut m = Music::new();
        m.root = PathBuf::from("/root");
        m.cwd = m.root.clone();
        m.entries = entries;
        m
    }

    #[test]
    fn file_name_splits_into_artist_and_title() {
        assert_eq!(split_name("Boards of Canada - Roygbiv.mp3"), ("Boards of Canada".into(), "Roygbiv".into()));
        assert_eq!(split_name("Arca – Nonbinary.flac"), ("Arca".into(), "Nonbinary".into()));
        assert_eq!(split_name("track01.mp3"), (String::new(), "track01".into()));
        // A bare dash without spaces is part of the title, not a separator.
        assert_eq!(split_name("lo-fi.mp3"), (String::new(), "lo-fi".into()));
        assert_eq!(split_name(" - x.mp3"), (String::new(), "- x".into()));
        assert_eq!(split_name(""), (String::new(), String::new()));
    }

    #[test]
    fn extension_filter_is_case_insensitive() {
        for ok in ["a.mp3", "a.AAC", "d/b.m4a", "c.flac", "c.WAV"] {
            assert!(is_music(Path::new(ok)), "{ok}");
        }
        for no in ["a.ogg", "a.txt", "cover.jpg", "noext", "a.mp3.bak"] {
            assert!(!is_music(Path::new(no)), "{no}");
        }
    }

    #[test]
    fn folders_sort_first_then_names_case_insensitively() {
        let mut v = vec![file("zz - b.mp3"), dir("beta"), file("AA - a.mp3"), dir("Alpha"), dir("alpha2")];
        sort_entries(&mut v);
        let labels: Vec<&str> = v.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, vec!["Alpha", "alpha2", "beta", "AA – a", "zz – b"]);
        assert!(v[..3].iter().all(|e| e.is_dir));
    }

    #[test]
    fn breadcrumb_keeps_the_tail_when_it_does_not_fit() {
        let root = PathBuf::from("/nas/Media/Library/Music");
        let cwd = root.join("Artist").join("Album");
        let full = breadcrumb(&root, &cwd, 80);
        assert_eq!(full, "♫ Library › Music › Artist › Album");
        assert_eq!(breadcrumb(&root, &root, 80), "♫ Library › Music");
        assert_eq!(breadcrumb(&root, &cwd, full.chars().count()), full, "an exact fit is not truncated");
        for w in [3usize, 10, 20, 30] {
            let s = breadcrumb(&root, &cwd, w);
            assert!(s.chars().count() <= w, "w={w} → {s:?}");
            assert!(s.starts_with("♫ …"), "truncates from the left: {s:?}");
            assert!(full.ends_with(s.trim_start_matches("♫ …")), "keeps the tail: {s:?}");
        }
        assert!(breadcrumb(&root, &cwd, 2).is_empty(), "no room at all");
    }

    #[test]
    fn parent_navigation_stops_at_the_root() {
        let root = Path::new("/root");
        assert_eq!(parent_of(root, root), None);
        assert_eq!(parent_of(root, Path::new("/root/a")), Some(PathBuf::from("/root")));
        assert_eq!(parent_of(root, Path::new("/root/a/b")), Some(PathBuf::from("/root/a")));
        assert_eq!(parent_of(root, Path::new("/elsewhere/a")), None, "never escapes the root");
    }

    #[test]
    fn left_is_not_consumed_at_the_root_so_tabs_still_switch() {
        let (ctx, _n) = test_ctx(toml::Table::new());
        let mut m = fake(vec![dir("a")]);
        assert!(!m.on_key(key(KeyCode::Left), &ctx), "root: left belongs to the tab bar");
        assert!(m.on_key(key(KeyCode::Backspace), &ctx), "backspace is always ours");
        m.cwd = m.root.join("a");
        assert!(m.on_key(key(KeyCode::Left), &ctx), "below the root left goes up");
        assert_eq!(m.cwd, m.root);
    }

    #[test]
    fn the_cursor_of_each_folder_is_remembered() {
        let (ctx, _n) = test_ctx(toml::Table::new());
        let mut m = fake(vec![dir("a"), dir("b"), file("x - 1.mp3")]);
        m.on_key(key(KeyCode::Down), &ctx); // b
        assert_eq!(m.selected, 1);
        m.on_key(key(KeyCode::Enter), &ctx); // enter /root/b
        assert_eq!(m.cwd, m.root.join("b"));
        assert_eq!(m.selected, 0);
        assert!(m.loading, "the new folder is read on a thread");
        m.entries = vec![dir("inner")];
        m.on_key(key(KeyCode::Backspace), &ctx);
        assert_eq!(m.cwd, m.root);
        assert_eq!(m.selected, 1, "back at the folder we left, on the same row");
    }

    #[test]
    fn playing_a_file_makes_the_current_folder_the_playlist() {
        let (ctx, _n) = test_ctx(toml::Table::new());
        let mut m = fake(vec![dir("a"), file("x - 1.mp3"), file("y - 2.mp3")]);
        for _ in 0..2 {
            m.on_key(key(KeyCode::Down), &ctx);
        }
        assert!(m.on_key(key(KeyCode::Enter), &ctx));
        assert_eq!(m.playlist.iter().map(|e| e.label.as_str()).collect::<Vec<_>>(), vec!["x – 1", "y – 2"]);
        assert_eq!(m.current, Some(1), "the folder becomes the playlist, the selection plays");
        assert!(m.playing);
        // Browsing elsewhere leaves the playlist alone.
        m.entries = vec![file("z - 3.mp3")];
        assert_eq!(m.playlist.len(), 2);
        assert_eq!(m.status(), "music 2 tracks, playing=y – 2");
    }

    #[test]
    fn advance_handles_empty_single_and_wrap() {
        let mut s = 12345u64;
        assert_eq!(advance(0, None, 1, false, &mut s), None);
        assert_eq!(advance(0, Some(3), -1, true, &mut s), None);
        assert_eq!(advance(1, Some(0), 1, false, &mut s), Some(0));
        assert_eq!(advance(1, Some(0), -1, false, &mut s), Some(0));
        assert_eq!(advance(3, Some(2), 1, false, &mut s), Some(0), "wraps forward");
        assert_eq!(advance(3, Some(0), -1, false, &mut s), Some(2), "wraps backward");
        assert_eq!(advance(3, None, 1, false, &mut s), Some(1));
        for _ in 0..100 {
            let idx = advance(7, Some(0), 1, true, &mut s).unwrap();
            assert!(idx < 7, "shuffle stays in bounds");
            assert_ne!(idx, 0, "shuffle never re-picks the current track");
        }
        // A single-track playlist has no other index to pick.
        assert_eq!(advance(1, Some(0), 1, true, &mut s), Some(0));
    }

    #[test]
    fn gives_up_after_five_consecutive_failures() {
        for n in 0..5 {
            assert!(!too_many_failures(n), "n={n}");
        }
        assert!(too_many_failures(5));
        assert!(too_many_failures(6));
    }

    #[test]
    fn keys_move_the_cursor_and_toggle_shuffle_in_bounds() {
        let (ctx, _n) = test_ctx(toml::Table::new());
        let mut m = fake(vec![file("a - 1.mp3"), file("b - 2.mp3")]);
        m.on_key(key(KeyCode::Up), &ctx);
        assert_eq!(m.selected, 0);
        for _ in 0..10 {
            m.on_key(key(KeyCode::Down), &ctx);
        }
        assert_eq!(m.selected, 1);
        assert!(m.on_key(key(KeyCode::Char('s')), &ctx));
        assert!(m.shuffle);
        for _ in 0..10 {
            m.on_key(key(KeyCode::Char('+')), &ctx);
        }
        assert_eq!(m.volume, 100);
        m.on_key(key(KeyCode::Char('-')), &ctx);
        assert_eq!(m.volume, 95);
        assert!(!m.on_key(key(KeyCode::Esc), &ctx), "esc does nothing here");
        // Space with nothing loaded starts the selected track.
        assert!(m.on_key(key(KeyCode::Char(' ')), &ctx));
        assert_eq!(m.current, Some(1));
        assert!(m.playing);
        assert!(m.wants_fast_frames(true), "20 fps while playing on this tab");
        assert!(!m.wants_fast_frames(false), "…but not from another tab");
        m.on_key(key(KeyCode::Char(' ')), &ctx);
        assert!(!m.playing, "second space pauses");
        assert!(!m.wants_fast_frames(true));
    }

    #[test]
    fn header_fits_the_budget_and_only_shows_while_playing() {
        let t = Theme::new(ThemeKind::Color);
        let mut m = fake(vec![file(&format!("{} - {}.mp3", "A".repeat(40), "B".repeat(40)))]);
        m.playlist = m.entries.clone();
        let text = |m: &Music, w: u16| -> String { m.header(w, t).iter().map(|s| s.content.to_string()).collect() };
        assert_eq!(text(&m, 80), "", "nothing plays yet");
        m.current = Some(0);
        m.playing = true;
        for w in [0u16, 5, 6, 10, 40, 200] {
            let s = text(&m, w);
            assert!(s.chars().count() <= w as usize, "w={w} → {s:?}");
        }
        assert_eq!(text(&m, 5), "", "under 6 columns there is no header");
        assert!(text(&m, 20).starts_with("♫ "));
        assert!(text(&m, 20).contains('…'), "long titles are ellipsized");
        m.playing = false;
        assert_eq!(text(&m, 80), "", "paused shows nothing");
    }

    #[test]
    fn draws_tiny_areas_without_panicking() {
        let t = Theme::new(ThemeKind::Color);
        let mut loading = Music::new();
        loading.loading = true;
        let mut browsing = fake(vec![dir("Some Artist"), file("a - 1.mp3"), file("no-artist.flac")]);
        browsing.playlist = browsing.entries[1..].to_vec();
        browsing.current = Some(0);
        browsing.playing = true;
        browsing.level = 0.4;
        browsing.spectrum = [42; BANDS];
        browsing.total = Some(Duration::from_secs(245));
        browsing.elapsed = Duration::from_secs(83);
        let cases = [Music::new(), loading, browsing];
        for m in &cases {
            for (w, h) in [(1u16, 1u16), (40, 12), (80, 24), (2, 3), (120, 40)] {
                let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
                term.draw(|f| m.draw(f, f.area(), t)).unwrap();
                // OVERVIEW / header use the same data at the same sizes.
                let _ = m.overview(w, h, t);
                let _ = m.header(w, t);
            }
        }
        assert!(cases[0].status().starts_with("music 0 tracks, playing=none"));
        assert_eq!(cases[2].status(), "music 2 tracks, playing=a – 1");
    }

    #[test]
    fn no_audio_device_shows_on_the_now_playing_line_instead_of_looking_paused() {
        let t = Theme::new(ThemeKind::Color);
        let mut m = fake(vec![file("a - 1.mp3")]);
        m.playlist = m.entries.clone();
        m.current = Some(0);
        m.playing = true; // a bogus "playing" state a lost Play command could leave behind
        m.no_audio = true;
        let line = m.footer(80, 3, false, t)[0].spans.iter().map(|s| s.content.to_string()).collect::<String>();
        assert!(line.contains("no audio device"), "{line:?}");
        assert!(line.starts_with("■"), "shows idle, not the ‖ paused icon: {line:?}");
    }

    #[test]
    fn an_unreadable_folder_is_an_error_not_an_empty_listing() {
        let missing = std::env::temp_dir().join("pipboy-no-such-folder-42");
        let _ = std::fs::remove_dir_all(&missing);
        let e = read_folder(&missing).unwrap_err();
        assert!(!e.is_empty(), "the OS message is passed through: {e:?}");
        assert!(read_folder(&std::env::temp_dir()).is_ok(), "a readable folder still lists");

        // poll reports it in the footer and remembers it for draw.
        let (ctx, n) = test_ctx(toml::Table::new());
        let mut m = fake(Vec::new());
        let (tx, rx) = mpsc::channel();
        m.ls_rx = Some(rx);
        m.loading = true;
        tx.send((m.cwd.clone(), Err(e.clone()))).unwrap();
        assert_eq!(m.poll(&ctx), 1);
        assert!(!m.loading, "the spinner never sticks");
        assert_eq!(n.try_recv().unwrap(), Notice::Footer(format!("music: {e}")));
        assert_eq!(m.ls_error.as_deref(), Some(e.as_str()));

        // …and the list area says so instead of "no music in <dir>".
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(70, 10)).unwrap();
        term.draw(|f| m.draw(f, f.area(), Theme::new(ThemeKind::Color))).unwrap();
        let text: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("cannot read folder:"), "{text}");
        assert!(!text.contains("no music in"), "{text}");
    }

    #[test]
    fn arrows_do_nothing_while_the_folder_is_still_loading() {
        let (ctx, _n) = test_ctx(toml::Table::new());
        let mut m = fake(Vec::new());
        m.selected = 4; // restored from sel_memory for this folder
        m.loading = true;
        assert!(m.on_key(key(KeyCode::Down), &ctx), "consumed, but not acted on");
        assert!(m.on_key(key(KeyCode::Up), &ctx));
        assert_eq!(m.selected, 4, "the restored cursor survives LOADING…");
    }

    #[test]
    fn the_radio_taking_the_audio_focus_pauses_music() {
        let (ctx, n) = test_ctx(toml::Table::new());
        let mut m = fake(vec![file("a - 1.mp3")]);
        assert!(m.on_key(key(KeyCode::Char(' ')), &ctx), "space starts the selected track");
        assert!(m.playing);
        let mine = ctx.board.get::<AudioFocus>(AUDIO_FOCUS).expect("playing claims the focus");
        assert_eq!(mine.owner, "music");

        assert_eq!(m.poll(&ctx), 0);
        assert!(m.playing, "my own focus does nothing");
        ctx.board.publish(AUDIO_FOCUS, AudioFocus { owner: "radio", seq: mine.seq.saturating_sub(1) });
        assert_eq!(m.poll(&ctx), 0);
        assert!(m.playing, "a stale seq does nothing");

        ctx.board.publish(AUDIO_FOCUS, AudioFocus { owner: "radio", seq: next_focus_seq() });
        assert_eq!(m.poll(&ctx), 1);
        assert!(!m.playing, "the radio is audible now");
        assert_eq!(m.current, Some(0), "the track stays loaded, just paused");
        assert_eq!(n.try_recv().unwrap(), Notice::Footer("music paused — radio is playing".to_string()));
        assert_eq!(m.poll(&ctx), 0, "and it is not repeated every frame");
        assert!(n.try_recv().is_err());
    }
}
