//! RADIO modul: internetes rádió lejátszás, VU-mérő és spektrum.
//!
//! A lejátszó a héj közös mixerére csatlakozik (`ctx.audio.mixer()`); a
//! hangerő/némítás a saját playerre és a riasztásra (`ctx.audio.set_volume`)
//! egyszerre érvényes. A `RadioCmd` a modul belügye.
use crate::module::{next_focus_seq, AudioFocus, Ctx, Module, Notice, Slot, AUDIO_FOCUS};
use crate::radio::{self, codec_label, RadioCmd, RadioEvent, RadioStatus};
use crate::style::Theme;
use crate::ui::vu::BANDS;
use crate::ui::widgets::{truncate, vu_bar};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::Frame;
use serde::Deserialize;
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;

const LOG_LEN: usize = 3;
/// A fejlécben az állomásnév felső határa (a `header()` nem kap szélességet).
const HEADER_NAME_W: usize = 24;

/// The `[notes]` section as RADIO needs it: just the file name, same default
/// as the NOTES module (task F: favourites are saved into a note in it, not
/// their own file). Modules never reference each other, so the tiny
/// `[notes]`-reading/path-resolving logic is duplicated here rather than
/// calling into `modules::notes`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct NotesFileCfg {
    file: String,
}

fn resolve_notes_path(file: &str, exe_dir: &Path) -> PathBuf {
    let file = if file.is_empty() { "notes.md" } else { file };
    let p = Path::new(file);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        exe_dir.join(p)
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Station {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RadioCfg {
    pub station: Vec<Station>,
}

impl Default for RadioCfg {
    fn default() -> Self {
        let s = |name: &str, url: &str| Station { name: name.into(), url: url.into() };
        Self {
            station: vec![
                s("Radio Paradise", "https://stream.radioparadise.com/mp3-128"),
                s("Radio Paradise Mellow", "https://stream.radioparadise.com/mellow-128"),
                s("Radio Paradise Rock", "https://stream.radioparadise.com/rock-128"),
                s("Radio Paradise Global", "https://stream.radioparadise.com/global-128"),
                s("SomaFM Groove Salad", "https://ice1.somafm.com/groovesalad-128-mp3"),
                s("SomaFM Drone Zone", "https://ice1.somafm.com/dronezone-128-mp3"),
                s("SomaFM Secret Agent", "https://ice1.somafm.com/secretagent-128-mp3"),
                s("SomaFM Lush", "https://ice1.somafm.com/lush-128-mp3"),
                s("SomaFM Space Station", "https://ice1.somafm.com/spacestation-128-mp3"),
                s("SomaFM Underground 80s", "https://ice1.somafm.com/u80s-128-mp3"),
                s("SomaFM Indie Pop Rocks", "https://ice1.somafm.com/indiepop-128-mp3"),
                s("SomaFM Boot Liquor", "https://ice1.somafm.com/bootliquor-128-mp3"),
                s("SomaFM DEF CON Radio", "https://ice1.somafm.com/defcon-128-mp3"),
                s("SomaFM Fluid", "https://ice1.somafm.com/fluid-128-mp3"),
                s("SomaFM Left Coast 70s", "https://ice1.somafm.com/seventies-128-mp3"),
                s("SomaFM Sonic Universe", "https://ice1.somafm.com/sonicuniverse-128-mp3"),
                s("SomaFM Vaporwaves", "https://ice1.somafm.com/vaporwaves-128-mp3"),
                s("SomaFM Metal Detector", "https://ice1.somafm.com/metal-128-mp3"),
                s("Nightride FM", "https://stream.nightride.fm/nightride.mp3"),
                s("Nightride Chillsynth", "https://stream.nightride.fm/chillsynth.mp3"),
                s("Nightride Datawave", "https://stream.nightride.fm/datawave.mp3"),
                s("Nightride Spacesynth", "https://stream.nightride.fm/spacesynth.mp3"),
                s("Nightride Darksynth", "https://stream.nightride.fm/darksynth.mp3"),
                s("Nightride Horrorsynth", "https://stream.nightride.fm/horrorsynth.mp3"),
                s("Nightride EBSM", "https://stream.nightride.fm/ebsm.mp3"),
                s("FIP", "https://icecast.radiofrance.fr/fip-midfi.mp3"),
                s("FIP Jazz", "https://icecast.radiofrance.fr/fipjazz-midfi.mp3"),
                s("FIP Electro", "https://icecast.radiofrance.fr/fipelectro-midfi.mp3"),
                s("FIP Rock", "https://icecast.radiofrance.fr/fiprock-midfi.mp3"),
                s("FIP Groove", "https://icecast.radiofrance.fr/fipgroove-midfi.mp3"),
                s("FIP World", "https://icecast.radiofrance.fr/fipworld-midfi.mp3"),
                s("France Musique", "https://icecast.radiofrance.fr/francemusique-midfi.mp3"),
                s("Radio Swiss Jazz", "https://stream.srg-ssr.ch/m/rsj/mp3_128"),
                s("Radio Swiss Classic", "https://stream.srg-ssr.ch/m/rsc_de/mp3_128"),
                s("Radio Swiss Pop", "https://stream.srg-ssr.ch/m/rsp/mp3_128"),
                s("KEXP Seattle", "https://kexp-mp3-128.streamguys1.com/kexp128.mp3"),
                s("WFMU", "https://stream0.wfmu.org/freeform-128k"),
            ],
        }
    }
}

pub struct RadioState {
    pub status: RadioStatus,
    /// Az éppen hangolt állomás indexe a `stations`-ben.
    pub current: Option<usize>,
    /// A listakurzor.
    pub selected: usize,
    pub title: String,
    pub format: String,
    pub volume: u8,
    pub muted: bool,
    pub level: f32,
    pub spectrum: [u8; BANDS],
    pub log: VecDeque<String>,
    pub since: Option<Instant>,
    /// `true` a `*`-gal elmentett `title` alatt, amíg az a cím szól (F/R2).
    pub favorited: bool,
}

impl Default for RadioState {
    fn default() -> Self {
        Self {
            status: RadioStatus::Stopped,
            current: None,
            selected: 0,
            title: String::new(),
            format: String::new(),
            volume: 70,
            muted: false,
            level: 0.0,
            spectrum: [0; BANDS],
            log: VecDeque::new(),
            since: None,
            favorited: false,
        }
    }
}

impl RadioState {
    pub fn log_line(&mut self, s: String) {
        if self.log.len() == LOG_LEN {
            self.log.pop_front();
        }
        self.log.push_back(format!("{} {s}", chrono::Local::now().format("%H:%M:%S")));
    }
}

/// One saved-favourite entry: a markdown list item, readable straight in the
/// NOTES tab. `None` when there is no track title yet (only the station name
/// is known).
pub fn favorite_line(now: chrono::DateTime<chrono::Local>, station: &str, title: &str) -> Option<String> {
    let title = title.trim();
    if title.is_empty() {
        return None;
    }
    Some(format!("- {} \u{b7} {station} \u{b7} {title}", now.format("%Y-%m-%d %H:%M")))
}

/// The title (last `\u{b7}`-separated segment) of a saved entry line, used to
/// detect "same title as last saved" without a separate index.
pub fn last_saved_title(entry: &str) -> Option<&str> {
    entry.rsplit(" \u{b7} ").next()
}

/// Write `content` to `path` via a same-directory `<file>.tmp` + rename, so a
/// reader (or the NOTES file watcher) never observes a half-written file.
fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path)
}

#[derive(Default)]
pub struct Radio {
    pub stations: Vec<Station>,
    pub state: RadioState,
    rx: Option<Receiver<RadioEvent>>,
    tx: Option<Sender<RadioCmd>>,
    notes_path: PathBuf,
    /// Highest `AudioFocus::seq` this module published or already reacted to.
    focus_seq: u64,
}

impl Radio {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn current_name(&self) -> &str {
        self.state.current.and_then(|i| self.stations.get(i)).map(|s| s.name.as_str()).unwrap_or("")
    }

    fn send(&self, cmd: RadioCmd) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(cmd);
        }
    }

    /// Claim the audio focus: MUSIC pauses itself on its next `poll`.
    fn take_focus(&mut self, ctx: &Ctx) {
        self.focus_seq = next_focus_seq();
        ctx.board.publish(AUDIO_FOCUS, AudioFocus { owner: self.id(), seq: self.focus_seq });
    }

    /// Pause the stream but keep the station selected (the `space` pause).
    fn pause(&mut self) {
        self.state.status = RadioStatus::Paused;
        self.state.level = 0.0;
        self.state.spectrum = [0; BANDS];
        self.send(RadioCmd::Pause);
    }

    /// MUSIC took the focus: pause so the two never sound at once.
    /// `true` when this poll actually paused us.
    fn yield_focus(&mut self, ctx: &Ctx) -> bool {
        // ponytail: a 2-word clone per poll; a separate seq key to gate it is not worth the second publish.
        let Some(f) = ctx.board.get::<AudioFocus>(AUDIO_FOCUS) else { return false };
        if f.owner == self.id() || f.seq <= self.focus_seq {
            return false;
        }
        self.focus_seq = f.seq;
        if self.state.status != RadioStatus::Playing {
            return false;
        }
        self.pause();
        let _ = ctx.notify.send(Notice::Footer("radio paused — music is playing".to_string()));
        true
    }

    fn tune(&mut self, idx: usize, ctx: &Ctx) {
        let Some(st) = self.stations.get(idx).cloned() else { return };
        self.take_focus(ctx);
        self.state.current = Some(idx);
        self.state.status = RadioStatus::Connecting;
        self.state.title.clear();
        self.state.favorited = false;
        self.state.log_line(format!("tuning: {}", st.name));
        self.send(RadioCmd::Tune(st));
    }

    /// `*`: append the current ICY title as a line in the `Favorite tracks`
    /// note of `notes.md` (task F). Ctx-free formatting/dup-detection lives in
    /// `favorite_line`/`last_saved_title`/`crate::radio::{add_favorite,
    /// last_favorite}`; this only does the I/O and footer reporting. If the
    /// NOTES editor is open on the same file when this writes, the editor's
    /// next save overwrites it — see the README note on this.
    fn save_favorite(&mut self, ctx: &Ctx) {
        let Some(line) = favorite_line(chrono::Local::now(), self.current_name(), &self.state.title) else {
            let _ = ctx.notify.send(Notice::Footer("no track title to save".to_string()));
            return;
        };
        if self.state.favorited {
            let _ = ctx.notify.send(Notice::Footer("already saved".to_string()));
            return;
        }
        let existing = fs::read_to_string(&self.notes_path).unwrap_or_default();
        let title = self.state.title.trim();
        if crate::radio::last_favorite(&existing).and_then(last_saved_title) == Some(title) {
            self.state.favorited = true;
            let _ = ctx.notify.send(Notice::Footer("already saved".to_string()));
            return;
        }
        let updated = crate::radio::add_favorite(&existing, &line);
        match write_atomic(&self.notes_path, &updated) {
            Ok(()) => {
                self.state.favorited = true;
                let _ = ctx.notify.send(Notice::Footer(format!("saved to notes: {title}")));
            }
            Err(e) => {
                let _ = ctx.notify.send(Notice::Footer(format!("favorites: {e}")));
            }
        }
    }

    fn toggle_play(&mut self, ctx: &Ctx) {
        match self.state.status {
            RadioStatus::Playing => self.pause(),
            RadioStatus::Paused => {
                self.take_focus(ctx);
                self.state.status = RadioStatus::Playing;
                self.send(RadioCmd::Play);
            }
            _ => self.tune(self.state.current.unwrap_or(self.state.selected), ctx),
        }
    }

    /// A hangerő a saját lejátszóra és a közös riasztás-playerre is érvényes.
    fn apply_volume(&self, ctx: &Ctx) {
        let v = if self.state.muted { 0.0 } else { self.state.volume as f32 / 100.0 };
        ctx.audio.set_volume(v);
    }

    fn event(&mut self, ev: RadioEvent) {
        let r = &mut self.state;
        match ev {
            RadioEvent::Connected { name, bitrate, content_type } => {
                r.status = RadioStatus::Playing;
                r.since = Some(Instant::now());
                r.title.clear();
                r.favorited = false;
                r.format = match bitrate {
                    Some(b) => format!("{} {b}k", codec_label(&content_type)),
                    None => codec_label(&content_type).to_string(),
                };
                r.log_line(format!("connected: {}", name.unwrap_or_else(|| "?".into())));
            }
            RadioEvent::Title(t) => {
                r.log_line(format!("title: {t}"));
                r.title = t;
                r.favorited = false;
            }
            RadioEvent::Level(l) => r.level = l,
            RadioEvent::Spectrum(b) => r.spectrum = b,
            RadioEvent::Log(s) => r.log_line(s),
            RadioEvent::Error(e) => {
                r.log_line(format!("error: {e}"));
                r.status = RadioStatus::Error(e);
                r.since = None;
                r.level = 0.0;
                r.spectrum = [0; BANDS];
            }
        }
    }
}

impl Module for Radio {
    fn id(&self) -> &'static str {
        "radio"
    }
    fn title(&self) -> &'static str {
        "RADIO"
    }
    fn describe(&self) -> &'static str {
        "Internet radio with ICY titles and a spectrum VU"
    }
    fn manual(&self) -> &'static str {
        "\
RADIO plays the internet stations listed in [[radio.station]]
in config.toml, with ICY track titles and a spectrum VU meter.

  ↑/↓   pick a station
  enter tune in to the selected one
  space plays or pauses
  +/-   the volume knob
  m     mutes, for when the boss walks in
  *     saves the current song to your notes, because
        \"what was that track?\" has ruined enough evenings

space, +, - and m answer from any tab, not just this one.
RADIO and MUSIC share one mixer, so whichever you start
politely pauses the other. No fighting over the speakers."
    }
    fn help(&self) -> &'static str {
        "↑/↓ station   enter tune   space play/pause   +/- volume   m mute   * save track   q quit"
    }

    fn start(&mut self, ctx: &Ctx) {
        let (cfg, notice) = ctx.config.section::<RadioCfg>(self.id());
        if let Some(n) = notice {
            let _ = ctx.notify.send(Notice::Footer(n));
        }
        self.stations = cfg.station;
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default();
        let (notes_cfg, notes_notice) = ctx.config.section::<NotesFileCfg>("notes");
        if let Some(n) = notes_notice {
            let _ = ctx.notify.send(Notice::Footer(n));
        }
        self.notes_path = resolve_notes_path(&notes_cfg.file, &exe_dir);
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.tx = Some(radio::spawn(ctx.rt.clone(), ctx.audio.mixer(), tx));
    }

    fn poll(&mut self, ctx: &Ctx) -> usize {
        let mut n = usize::from(self.yield_focus(ctx));
        let mut events = Vec::new();
        if let Some(rx) = &self.rx {
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
                n += 1;
            }
        }
        for ev in events {
            self.event(ev);
        }
        n
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &Ctx) -> bool {
        match key.code {
            KeyCode::Up => self.state.selected = self.state.selected.saturating_sub(1),
            KeyCode::Down => {
                self.state.selected = (self.state.selected + 1).min(self.stations.len().saturating_sub(1))
            }
            KeyCode::Enter => self.tune(self.state.selected, ctx),
            // Tab-local (not on_global_key) so `*` stays free for other modules.
            KeyCode::Char('*') => self.save_favorite(ctx),
            _ => return false,
        }
        true
    }

    fn on_global_key(&mut self, key: KeyEvent, ctx: &Ctx) -> bool {
        match key.code {
            KeyCode::Char(' ') => self.toggle_play(ctx),
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.state.volume = (self.state.volume + 5).min(100);
                self.send(RadioCmd::Volume(self.state.volume));
                self.apply_volume(ctx);
            }
            KeyCode::Char('-') => {
                self.state.volume = self.state.volume.saturating_sub(5);
                self.send(RadioCmd::Volume(self.state.volume));
                self.apply_volume(ctx);
            }
            KeyCode::Char('m') => {
                self.state.muted = !self.state.muted;
                self.send(RadioCmd::Mute(self.state.muted));
                self.apply_volume(ctx);
            }
            _ => return false,
        }
        true
    }

    fn draw(&self, f: &mut Frame, area: Rect, t: Theme) {
        crate::ui::radio::draw(f, self, area, t);
    }

    /// `♪ <név> ▶ hh:mm:ss` a kapott szélességbe zsugorítva: előbb az eltelt idő
    /// esik ki, aztán a név rövidül `…`-tal; 6 oszlop alatt nincs fejléc.
    fn header(&self, width: u16, t: Theme) -> Vec<Span<'static>> {
        let r = &self.state;
        let width = width as usize;
        if width < 6 {
            return vec![];
        }
        let mut spans = Vec::new();
        let (style, icon, with_elapsed) = match &r.status {
            RadioStatus::Playing => (t.nowplaying, "▶", true),
            RadioStatus::Paused => (t.nowplaying, "‖", true),
            RadioStatus::Connecting => (t.nowplaying, "…", false),
            RadioStatus::Error(_) => (t.danger, "✕", false),
            RadioStatus::Stopped => (t.nowplaying, "", false),
        };
        // A némítás-jelző ` M` a csoport része, ezért előre lefoglalja a helyét.
        let budget = width - if r.muted { 2 } else { 0 };
        if !matches!(r.status, RadioStatus::Stopped) {
            let el = r.since.map(|s| s.elapsed().as_secs()).unwrap_or(0);
            let mut elapsed = if with_elapsed {
                format!(" {:02}:{:02}:{:02}", el / 3600, el / 60 % 60, el % 60)
            } else {
                String::new()
            };
            // "♪ " + név + " " + ikon [+ eltelt idő]
            let fixed = 3 + icon.chars().count();
            let mut name_w = self.current_name().chars().count().min(HEADER_NAME_W);
            if fixed + name_w + elapsed.chars().count() > budget {
                elapsed.clear();
            }
            name_w = name_w.min(budget.saturating_sub(fixed + elapsed.chars().count()));
            if name_w == 0 {
                return if r.muted { vec![Span::styled(" M", t.warn)] } else { vec![] };
            }
            let name = truncate(self.current_name(), name_w);
            spans.push(Span::styled(format!("♪ {name} {icon}{elapsed}"), style));
        }
        if r.muted {
            spans.push(Span::styled(" M", t.warn));
        }
        spans
    }

    fn overview(&self, width: u16, _height: u16, t: Theme) -> Vec<Line<'static>> {
        let r = &self.state;
        let name = self.current_name().to_string();
        let mut ra = vec![Line::from(Span::styled(" RADIO", t.title))];
        match &r.status {
            RadioStatus::Stopped => ra.push(Line::from(Span::styled(
                format!("   ■ radio idle · {} stations", self.stations.len()),
                t.frame,
            ))),
            RadioStatus::Error(e) => ra.push(Line::from(Span::styled(format!("   ✕ {name}: {e}"), t.danger))),
            RadioStatus::Connecting => ra.push(Line::from(Span::styled(format!("   … {name}"), t.nowplaying))),
            _ => {
                let icon = if r.status == RadioStatus::Playing { "▶" } else { "‖" };
                ra.push(Line::from(Span::styled(format!("   {icon} {name}"), t.nowplaying)));
                ra.push(Line::from(Span::styled(format!("   {}", r.title), t.value)));
                let vu_w = width.saturating_sub(18).min(30) as usize;
                let mut line = vu_bar((r.level * 300.0).min(100.0) as u8, vu_w, t);
                line.spans.insert(0, Span::raw("   "));
                line.spans.push(Span::styled(
                    format!("  VOL {}%{}", r.volume, if r.muted { " M" } else { "" }),
                    t.frame,
                ));
                ra.push(line);
            }
        }
        ra
    }

    fn overview_slot(&self) -> Slot {
        Slot::Right(1)
    }

    /// Csak szóló rádiónál kell 20 kép/s (VU + spektrum), és csak ha látszik.
    fn wants_fast_frames(&self, active: bool) -> bool {
        active && self.state.status == RadioStatus::Playing
    }

    fn status(&self) -> String {
        format!(
            "{:?} \"{}\" vol={}{} title=\"{}\"",
            self.state.status,
            self.current_name(),
            self.state.volume,
            if self.state.muted { " muted" } else { "" },
            self.state.title
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::ModuleConfig;
    use crate::shell::test_ctx;
    use ratatui::crossterm::event::KeyModifiers;

    fn radio() -> (Radio, Receiver<RadioCmd>) {
        let mut m = Radio::new();
        m.stations = RadioCfg::default().station;
        let (tx, rx) = mpsc::channel();
        m.tx = Some(tx);
        (m, rx)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn config_defaults_and_station_section() {
        let (cfg, notice) = ModuleConfig(toml::Table::new()).section::<RadioCfg>("radio");
        assert_eq!(cfg.station.len(), 37);
        assert!(notice.is_none());
        let table: toml::Table = "[[radio.station]]\nname = \"X\"\nurl = \"http://x/s\"\n".parse().unwrap();
        let (cfg, _) = ModuleConfig(table).section::<RadioCfg>("radio");
        assert_eq!(cfg.station, vec![Station { name: "X".into(), url: "http://x/s".into() }]);
    }

    #[test]
    fn space_when_stopped_tunes_the_selected_station() {
        let (ctx, _n) = test_ctx(toml::Table::new());
        let (mut m, cmds) = radio();
        m.state.selected = 1;
        let st = m.stations[1].clone();
        assert!(m.on_global_key(key(KeyCode::Char(' ')), &ctx));
        assert_eq!(cmds.try_recv().unwrap(), RadioCmd::Tune(st));
        assert_eq!(m.state.status, RadioStatus::Connecting);
        assert_eq!(m.state.current, Some(1));
    }

    #[test]
    fn space_toggles_pause_when_playing() {
        let (ctx, _n) = test_ctx(toml::Table::new());
        let (mut m, cmds) = radio();
        m.event(RadioEvent::Connected { name: None, bitrate: Some(128), content_type: "audio/mpeg".into() });
        assert_eq!(m.state.status, RadioStatus::Playing);
        assert_eq!(m.state.format, "mp3 128k");
        m.on_global_key(key(KeyCode::Char(' ')), &ctx);
        assert_eq!(cmds.try_recv().unwrap(), RadioCmd::Pause);
        assert_eq!(m.state.status, RadioStatus::Paused);
        m.on_global_key(key(KeyCode::Char(' ')), &ctx);
        assert_eq!(cmds.try_recv().unwrap(), RadioCmd::Play);
        assert_eq!(m.state.status, RadioStatus::Playing);
    }

    #[test]
    fn volume_steps_and_caps() {
        let (ctx, _n) = test_ctx(toml::Table::new());
        let (mut m, cmds) = radio();
        for _ in 0..10 {
            m.on_global_key(key(KeyCode::Char('+')), &ctx);
        }
        assert_eq!(m.state.volume, 100);
        m.on_global_key(key(KeyCode::Char('-')), &ctx);
        assert_eq!(m.state.volume, 95);
        let last = cmds.try_iter().last().unwrap();
        assert_eq!(last, RadioCmd::Volume(95));
        m.on_global_key(key(KeyCode::Char('m')), &ctx);
        assert!(m.state.muted);
        assert_eq!(cmds.try_recv().unwrap(), RadioCmd::Mute(true));
    }

    #[test]
    fn fast_frames_only_when_playing_and_visible() {
        let (mut m, _cmds) = radio();
        assert!(!m.wants_fast_frames(true));
        m.event(RadioEvent::Connected { name: None, bitrate: None, content_type: "audio/mpeg".into() });
        assert!(!m.wants_fast_frames(false), "szól, de más fül aktív");
        assert!(m.wants_fast_frames(true));
    }

    #[test]
    fn list_cursor_stays_in_bounds() {
        let (ctx, _n) = test_ctx(toml::Table::new());
        let (mut m, _cmds) = radio();
        m.on_key(key(KeyCode::Up), &ctx);
        assert_eq!(m.state.selected, 0);
        for _ in 0..100 {
            m.on_key(key(KeyCode::Down), &ctx);
        }
        assert_eq!(m.state.selected, m.stations.len() - 1);
        assert!(!m.on_key(key(KeyCode::Char('q')), &ctx), "idegen kulcs nem fogy el");
    }

    #[test]
    fn header_truncates_a_long_station_name() {
        let (mut m, _cmds) = radio();
        m.stations[0].name = "A".repeat(60);
        m.state.current = Some(0);
        m.event(RadioEvent::Connected { name: None, bitrate: None, content_type: "audio/mpeg".into() });
        let t = Theme::new(crate::config::ThemeKind::Color);
        let text: String = m.header(200, t).iter().map(|s| s.content.as_ref()).collect();
        assert!(text.chars().count() <= HEADER_NAME_W + 16, "fejléc: {text}");
        assert!(text.contains('…'));
    }

    /// A szerződés szerinti zsugorodás: teljes → idő nélkül → rövidített név → semmi.
    #[test]
    fn header_shrinks_to_the_budget() {
        let (mut m, _cmds) = radio();
        m.stations[0].name = "Radio Paradise".into();
        m.state.current = Some(0);
        m.event(RadioEvent::Connected { name: None, bitrate: None, content_type: "audio/mpeg".into() });
        let t = Theme::new(crate::config::ThemeKind::Color);
        let text = |w: u16, m: &Radio| -> String { m.header(w, t).iter().map(|s| s.content.to_string()).collect() };

        let full = text(80, &m);
        assert_eq!(full, "♪ Radio Paradise ▶ 00:00:00");
        for w in [5u16, 6, 7, 12, 18, 20, 26, 27, 40] {
            let s = text(w, &m);
            assert!(s.chars().count() <= w as usize, "w={w} → {s:?}");
        }
        assert_eq!(text(5, &m), "", "6 oszlop alatt nincs fejléc");
        assert_eq!(text(20, &m), "♪ Radio Paradise ▶", "az eltelt idő esik ki előbb");
        assert!(text(12, &m).contains('…'), "aztán a név rövidül");

        m.state.muted = true;
        for w in [6u16, 10, 20, 30] {
            let s = text(w, &m);
            assert!(s.chars().count() <= w as usize, "némán w={w} → {s:?}");
            assert!(s.ends_with(" M"), "a némítás-jelző marad: {s:?}");
        }
    }

    fn local(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> chrono::DateTime<chrono::Local> {
        use chrono::TimeZone;
        chrono::Local.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn favorite_line_formats_as_a_markdown_list_item() {
        let line = favorite_line(local(2026, 9, 10, 21, 5), "Radio Paradise", "Boards of Canada – Roygbiv");
        assert_eq!(line, Some("- 2026-09-10 21:05 \u{b7} Radio Paradise \u{b7} Boards of Canada – Roygbiv".to_string()));
    }

    #[test]
    fn favorite_line_none_when_title_empty_or_blank() {
        assert_eq!(favorite_line(local(2026, 9, 10, 21, 5), "Radio Paradise", ""), None);
        assert_eq!(favorite_line(local(2026, 9, 10, 21, 5), "Radio Paradise", "   "), None);
    }

    #[test]
    fn last_saved_title_reads_the_last_segment_of_an_entry() {
        assert_eq!(last_saved_title(""), Some(""));
        let entry = "- 2026-09-10 21:05 \u{b7} Radio Paradise \u{b7} Boards of Canada – Roygbiv";
        assert_eq!(last_saved_title(entry), Some("Boards of Canada – Roygbiv"));
    }

    /// End-to-end `*` flow against a real temp notes file: no title, first
    /// save (creating the `Favorite tracks` note), duplicate (same title,
    /// same session), and a title change clears the star.
    #[test]
    fn star_key_saves_appends_dedups_and_clears_on_title_change() {
        let dir = std::env::temp_dir().join("pipboy-test-favorites");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("notes-{}.md", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let (ctx, n) = test_ctx(toml::Table::new());
        let (mut m, _cmds) = radio();
        m.notes_path = path.clone();
        m.state.current = Some(0);

        // No track title yet: only the station name is known.
        assert!(m.on_key(key(KeyCode::Char('*')), &ctx));
        assert_eq!(n.try_recv().unwrap(), Notice::Footer("no track title to save".to_string()));
        assert!(!path.exists());

        // A title arrives, `*` saves it into the `Favorite tracks` note.
        m.event(RadioEvent::Title("Boards of Canada – Roygbiv".to_string()));
        assert!(!m.state.favorited);
        assert!(m.on_key(key(KeyCode::Char('*')), &ctx));
        assert_eq!(n.try_recv().unwrap(), Notice::Footer("saved to notes: Boards of Canada – Roygbiv".to_string()));
        assert!(m.state.favorited);
        let content = std::fs::read_to_string(&path).unwrap();
        let notes = crate::modules::notes::parse_notes(&content);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].title, radio::FAVORITES_NOTE);
        assert_eq!(notes[0].body.len(), 1);
        assert_eq!(last_saved_title(&notes[0].body[0]), Some("Boards of Canada – Roygbiv"));
        assert!(notes[0].body[0].starts_with(&format!("- {} ", chrono::Local::now().format("%Y-%m-%d %H:%M"))));

        // Same title again: no duplicate line appended.
        assert!(m.on_key(key(KeyCode::Char('*')), &ctx));
        assert_eq!(n.try_recv().unwrap(), Notice::Footer("already saved".to_string()));
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(crate::radio::count_favorites(&content), 1);

        // The title changes: the star clears and the new title can be saved.
        m.event(RadioEvent::Title("Next Track".to_string()));
        assert!(!m.state.favorited);
        assert!(m.on_key(key(KeyCode::Char('*')), &ctx));
        assert_eq!(n.try_recv().unwrap(), Notice::Footer("saved to notes: Next Track".to_string()));
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(crate::radio::count_favorites(&content), 2);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn music_taking_the_audio_focus_pauses_the_radio() {
        let (ctx, n) = test_ctx(toml::Table::new());
        let (mut m, cmds) = radio();
        assert!(m.on_global_key(key(KeyCode::Char(' ')), &ctx));
        m.event(RadioEvent::Connected { name: None, bitrate: None, content_type: "audio/mpeg".into() });
        assert_eq!(m.state.status, RadioStatus::Playing);
        let mine = ctx.board.get::<AudioFocus>(AUDIO_FOCUS).expect("tuning claims the focus");
        assert_eq!(mine.owner, "radio");

        assert_eq!(m.poll(&ctx), 0);
        assert_eq!(m.state.status, RadioStatus::Playing, "my own focus does nothing");
        ctx.board.publish(AUDIO_FOCUS, AudioFocus { owner: "music", seq: mine.seq.saturating_sub(1) });
        assert_eq!(m.poll(&ctx), 0);
        assert_eq!(m.state.status, RadioStatus::Playing, "a stale seq does nothing");

        ctx.board.publish(AUDIO_FOCUS, AudioFocus { owner: "music", seq: next_focus_seq() });
        assert_eq!(m.poll(&ctx), 1);
        assert_eq!(m.state.status, RadioStatus::Paused, "same as the space pause");
        assert_eq!(m.state.current, Some(0), "the station stays selected");
        assert_eq!(cmds.try_iter().last(), Some(RadioCmd::Pause));
        assert_eq!(n.try_recv().unwrap(), Notice::Footer("radio paused — music is playing".to_string()));
        assert_eq!(m.poll(&ctx), 0, "and it is not repeated every frame");
        assert!(n.try_recv().is_err());
    }
}
