//! WIFI module: WlanAPI scan, a channel-congestion chart per band, and joining
//! a network from the tab (`c`, with a `PASSWORD>` prompt for secured ones).
use crate::module::{Ctx, Module, Notice, Slot};
use crate::style::Theme;
use crate::ui::widgets::truncate;
use crate::wifi::{self, Band, Bss, WifiCmd, WifiEvent, WifiSnapshot};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Chart, Dataset, GraphType, Paragraph};
use ratatui::Frame;
use serde::Deserialize;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

/// Sample points per curve on the channel chart.
const SAMPLES: usize = 96;
/// At most this many curves are drawn; a crowded band stays readable.
const MAX_CURVES: usize = 24;
/// Network rows on the narrow (stacked) layout.
const NARROW_ROWS: u16 = 8;
/// Longest WPA passphrase (a 64-character value would be a raw PSK, not one we
/// can put in a `passPhrase` profile).
const PASS_MAX_CHARS: usize = 63;
/// Bytes reserved for the passphrase up front. A `push` past the capacity would
/// reallocate and leave the plaintext behind in a buffer nothing can wipe, so
/// the buffer is sized once for the longest passphrase and never grows.
const PASS_CAPACITY: usize = 4 * PASS_MAX_CHARS + 1;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WifiCfg {
    /// Seconds between scans.
    pub interval: u64,
}

impl Default for WifiCfg {
    fn default() -> Self {
        Self { interval: 15 }
    }
}

/// What the tab says while it has nothing (or something bad) to report.
#[derive(Debug, Default, PartialEq)]
enum State {
    #[default]
    Scanning,
    NoAdapter,
    Error(String),
    Ready,
}

/// The open `PASSWORD>` prompt. It pins down *which* network the passphrase is
/// for: the scan list is re-sorted by signal every 15 s, so by the time Enter is
/// pressed `sel` may well point at a different network than `c` did.
struct Prompt {
    ssid: String,
    ssid_raw: Vec<u8>,
    auth: wifi::ProfileAuth,
    input: String,
}

impl Prompt {
    fn new(n: &Bss, auth: wifi::ProfileAuth) -> Self {
        Self {
            ssid: n.ssid.clone(),
            ssid_raw: n.ssid_raw.clone(),
            auth,
            input: String::with_capacity(PASS_CAPACITY),
        }
    }
}

/// One key of the `PASSWORD>` prompt. Pure, so the state machine is testable
/// without a `Ctx`; the caller turns `Submit` into a connect command.
#[derive(Debug, PartialEq)]
enum Action {
    Typing,
    Cancel,
    Submit,
}

fn prompt_key(input: &mut String, code: KeyCode) -> Action {
    match code {
        KeyCode::Enter => Action::Submit,
        KeyCode::Esc => {
            wifi::zero(input);
            Action::Cancel
        }
        KeyCode::Backspace => {
            input.pop();
            Action::Typing
        }
        // Past the WPA limit — or past the reserved capacity, which a `push`
        // would answer by reallocating and stranding a copy of the plaintext.
        KeyCode::Char(c)
            if input.chars().count() < PASS_MAX_CHARS && input.len() + c.len_utf8() <= input.capacity() =>
        {
            input.push(c);
            Action::Typing
        }
        _ => Action::Typing,
    }
}

/// Phone-style signal bars: the reached ones from `▂▃▅▇`, the rest as `·`.
fn bars(rssi_dbm: i32) -> String {
    let n = match rssi_dbm {
        r if r >= -55 => 4,
        r if r >= -67 => 3,
        r if r >= -78 => 2,
        r if r >= -90 => 1,
        _ => 0,
    };
    "▂▃▅▇".chars().enumerate().map(|(i, c)| if i < n { c } else { '·' }).collect()
}

#[derive(Default)]
pub struct Wifi {
    cfg: WifiCfg,
    rx: Option<Receiver<WifiEvent>>,
    tx: Option<Sender<WifiCmd>>,
    /// Every BSS of the last scan — the chart's input.
    all: Vec<Bss>,
    /// One row per SSID, strongest first — the list's input.
    nets: Vec<Bss>,
    sel: usize,
    band: Band,
    /// `Some` while the `PASSWORD>` prompt owns the keyboard.
    prompt: Option<Prompt>,
    state: State,
}

/// Wipe the passphrase of a command that will never be delivered.
fn drop_cmd(cmd: WifiCmd) {
    if let WifiCmd::Connect { mut password, .. } = cmd {
        wifi::zero(&mut password);
    }
}

impl Wifi {
    pub fn new() -> Self {
        Self::default()
    }

    fn send(&self, cmd: WifiCmd) {
        // An undelivered `Connect` still owns the plaintext passphrase, whether
        // the thread was never started or has already exited.
        let Some(tx) = &self.tx else { return drop_cmd(cmd) };
        if let Err(mpsc::SendError(cmd)) = tx.send(cmd) {
            drop_cmd(cmd);
        }
    }

    fn snapshot(&self) -> WifiSnapshot {
        WifiSnapshot {
            networks: self.nets.clone(),
            connected: self.nets.iter().find(|n| n.connected).map(|n| n.ssid.clone()),
        }
    }

    fn event(&mut self, ev: WifiEvent, ctx: &Ctx) {
        match ev {
            WifiEvent::Scan(all) => {
                self.nets = wifi::networks(&all);
                self.all = all;
                self.sel = self.sel.min(self.nets.len().saturating_sub(1));
                self.state = State::Ready;
            }
            WifiEvent::NoAdapter => self.state = State::NoAdapter,
            WifiEvent::Error(e) => self.state = State::Error(e),
            // Not "connected to": WlanConnect only queues the request and the
            // handshake result never comes back. The next scan's `*` decides.
            WifiEvent::Joining(ssid) => {
                let _ = ctx.notify.send(Notice::Footer(format!("joining {ssid}… — * confirms")));
            }
            WifiEvent::ConnectFailed(code) => {
                let _ = ctx.notify.send(Notice::Footer(format!("connect failed: {code}")));
            }
        }
    }

    /// `c`: an open network is joined straight away, a secured one opens the
    /// `PASSWORD>` prompt. An 802.1X network is refused here, before anyone can
    /// type a domain password into what would become a PSK profile.
    fn start_connect(&mut self, ctx: &Ctx) {
        let Some(n) = self.nets.get(self.sel) else { return };
        let auth = wifi::profile_auth(n.secured, &n.auth);
        if auth == wifi::ProfileAuth::Enterprise {
            let _ = ctx
                .notify
                .send(Notice::Footer("enterprise networks are not supported — connect from Windows".into()));
            return;
        }
        if n.secured {
            self.prompt = Some(Prompt::new(n, auth));
            return;
        }
        let (ssid, ssid_raw) = (n.ssid.clone(), n.ssid_raw.clone());
        let _ = ctx.notify.send(Notice::Footer(format!("joining {ssid}…")));
        self.send(WifiCmd::Connect { ssid, ssid_raw, auth, password: String::new() });
    }

    fn apply_prompt_key(&mut self, code: KeyCode, ctx: &Ctx) -> bool {
        let Some(p) = self.prompt.as_mut() else { return false };
        match prompt_key(&mut p.input, code) {
            Action::Typing => {}
            Action::Cancel => self.prompt = None,
            Action::Submit if p.input.is_empty() => {
                // Stay in the prompt: an empty PSK profile would just fail later.
                let _ = ctx.notify.send(Notice::Footer("password required".into()));
            }
            Action::Submit => {
                // The target was captured when the prompt opened; the list has
                // very likely been re-sorted since, so `sel` is not to be
                // trusted. Taking the whole `Prompt` moves the buffer rather
                // than copying it, and the scan thread wipes it after use.
                let Some(mut p) = self.prompt.take() else { return true };
                let password = std::mem::take(&mut p.input);
                let _ = ctx.notify.send(Notice::Footer(format!("joining {}…", p.ssid)));
                self.send(WifiCmd::Connect { ssid: p.ssid, ssid_raw: p.ssid_raw, auth: p.auth, password });
            }
        }
        true
    }

    fn draw_list(&self, f: &mut Frame, area: Rect, t: Theme) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let mut lines = vec![Line::from(Span::styled(" NETWORKS", t.title))];
        // Row budget: the ` NETWORKS` header and the prompt/detail line below.
        let room = (area.height as usize).saturating_sub(2).max(1);
        let first = self.sel.saturating_sub(room.saturating_sub(1));
        for (i, n) in self.nets.iter().enumerate().skip(first).take(room) {
            lines.push(row(n, i == self.sel, area.width as usize, t));
        }
        if self.nets.is_empty() {
            let (text, style) = match &self.state {
                State::NoAdapter => ("no wireless adapter", t.warn),
                State::Error(e) => (e.as_str(), t.warn),
                _ => ("scanning…", t.frame),
            };
            lines.push(Line::from(Span::styled(format!("   {text}"), style)));
        }
        if let Some(p) = &self.prompt {
            // Naming the target makes it visible that the list re-sorting under
            // the prompt does not move the passphrase to another network.
            let room = (area.width as usize).saturating_sub(" PASSWORD for > ".len() + 8).max(4);
            lines.push(Line::from(vec![
                Span::styled(format!(" PASSWORD for {}> ", truncate(&p.ssid, room)), t.warn),
                Span::styled("*".repeat(p.input.chars().count()), t.value),
            ]));
        } else if let Some(n) = self.nets.get(self.sel) {
            lines.push(Line::from(Span::styled(
                format!("   {} · {} MHz · {} MHz wide · {}", n.bssid, n.freq_khz / 1000, n.width_mhz, n.auth),
                t.frame,
            )));
        }
        f.render_widget(Paragraph::new(lines), area);
    }

    fn draw_chart(&self, f: &mut Frame, area: Rect, t: Theme) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let [head, body, foot] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)]).areas(area);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!(" CHANNELS {}", self.band.label()), t.title),
                Span::styled("   [b] band", t.frame),
            ])),
            head,
        );
        f.render_widget(Paragraph::new(self.summary(t)), foot);
        if body.height == 0 || body.width == 0 {
            return;
        }

        let channels = self.band.chart_channels();
        let (lo, hi) = match (channels.first(), channels.last()) {
            (Some(a), Some(b)) => (*a as f64, *b as f64),
            _ => return,
        };
        let selected = self.nets.get(self.sel).map(|n| n.ssid.as_str());
        let mut in_band: Vec<&Bss> = self.all.iter().filter(|b| b.band == self.band).collect();
        in_band.sort_by_key(|b| -b.rssi_dbm);
        in_band.truncate(MAX_CURVES);

        // The selected network's curve is built last so it is drawn on top.
        let mut curves: Vec<(bool, Vec<(f64, f64)>)> = in_band
            .iter()
            .map(|b| {
                let points = (0..=SAMPLES)
                    .map(|i| {
                        let x = lo + (hi - lo) * i as f64 / SAMPLES as f64;
                        (x, wifi::bss_load(b, x as f32) as f64)
                    })
                    .collect();
                (Some(b.ssid.as_str()) == selected, points)
            })
            .collect();
        curves.sort_by_key(|(sel, _)| *sel);
        let ymax = curves.iter().flat_map(|(_, p)| p.iter().map(|(_, y)| *y)).fold(0.0_f64, f64::max).max(1.0);

        let best = wifi::best_channel(self.band, &self.all);
        let marker = [(best as f64, 0.0), (best as f64, ymax)];
        let mut datasets = vec![Dataset::default()
            .data(&marker)
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(t.value)];
        datasets.extend(curves.iter().map(|(sel, p)| {
            Dataset::default()
                .data(p)
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(if *sel { t.title } else { t.graph })
        }));

        let chart = Chart::new(datasets)
            .x_axis(
                Axis::default().style(t.frame).bounds([lo, hi]).labels([
                    Line::from(Span::styled(format!("ch {lo:.0}"), t.frame)),
                    Line::from(Span::styled(format!("best {best}"), t.value)),
                    Line::from(Span::styled(format!("ch {hi:.0}"), t.frame)),
                ]),
            )
            .y_axis(Axis::default().style(t.frame).bounds([0.0, ymax]));
        f.render_widget(chart, body);
    }

    /// `best channel 2.4 GHz: 11 · 5 GHz: 100 · strongest: <ssid> (-52 dBm)`
    fn summary(&self, t: Theme) -> Line<'static> {
        if let State::Error(e) = &self.state {
            return Line::from(Span::styled(format!(" {e}"), t.warn));
        }
        if self.state == State::NoAdapter {
            return Line::from(Span::styled(" no wireless adapter", t.warn));
        }
        let mut spans = vec![
            Span::styled(" best channel ", t.frame),
            Span::raw("2.4 GHz: "),
            Span::styled(wifi::best_channel(Band::G2_4, &self.all).to_string(), t.value),
            Span::styled(" · ", t.frame),
            Span::raw("5 GHz: "),
            Span::styled(wifi::best_channel(Band::G5, &self.all).to_string(), t.value),
        ];
        match wifi::best_network(&self.all) {
            Some((ssid, dbm)) => {
                spans.push(Span::styled(" · ", t.frame));
                spans.push(Span::raw("strongest: "));
                spans.push(Span::styled(format!("{ssid} ({dbm} dBm)"), t.value));
            }
            None => spans.push(Span::styled(" · no networks", t.frame)),
        }
        Line::from(spans)
    }
}

/// One list row: `*# SSID              5 ch  36  -52 dBm ▇▅▃·`
fn row(n: &Bss, selected: bool, width: usize, t: Theme) -> Line<'static> {
    let flags = format!("{}{}", if n.connected { '*' } else { ' ' }, if n.secured { '#' } else { ' ' });
    let tail = format!("{:>3} ch{:>4} {:>4} dBm {}", n.band.short(), n.channel, n.rssi_dbm, bars(n.rssi_dbm));
    // " " + flags + " " + ssid + " " + tail
    let ssid_room = width.saturating_sub(tail.chars().count() + 5).max(1);
    let ssid = truncate(&n.ssid, ssid_room);
    Line::from(vec![
        Span::styled(format!(" {flags} "), if n.connected { t.value } else { t.frame }),
        Span::styled(format!("{ssid:<ssid_room$}"), if selected { t.title } else { t.text }),
        Span::styled(format!(" {tail}"), if selected { t.value } else { t.frame }),
    ])
}

impl Module for Wifi {
    fn id(&self) -> &'static str {
        "wifi"
    }
    fn title(&self) -> &'static str {
        "WIFI"
    }
    fn describe(&self) -> &'static str {
        "Nearby networks, channel congestion, connect"
    }
    fn help(&self) -> &'static str {
        if self.prompt.is_some() {
            "type password · enter connect · esc cancel"
        } else {
            "↑/↓ network   b band   c connect   r rescan   1-9 tabs   q quit"
        }
    }

    fn start(&mut self, ctx: &Ctx) {
        let (cfg, notice) = ctx.config.section::<WifiCfg>(self.id());
        if let Some(n) = notice {
            let _ = ctx.notify.send(Notice::Footer(n));
        }
        self.cfg = cfg;
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        // A scan already takes ~4 s, so anything under 5 s would just spin.
        self.tx = Some(wifi::spawn(Duration::from_secs(self.cfg.interval.max(5)), tx));
    }

    fn poll(&mut self, ctx: &Ctx) -> usize {
        let mut events = Vec::new();
        if let Some(rx) = &self.rx {
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
            }
        }
        let n = events.len();
        for ev in events {
            self.event(ev, ctx);
        }
        if n > 0 {
            ctx.board.publish(self.id(), self.snapshot());
        }
        n
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &Ctx) -> bool {
        // Ctrl+C must still quit while the password prompt owns every other key.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        if self.prompt.is_some() {
            return self.apply_prompt_key(key.code, ctx);
        }
        match key.code {
            KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down => self.sel = (self.sel + 1).min(self.nets.len().saturating_sub(1)),
            KeyCode::Char('b') => self.band = self.band.next(),
            KeyCode::Char('r') => {
                self.send(WifiCmd::Rescan);
                let _ = ctx.notify.send(Notice::Footer("scanning…".into()));
            }
            KeyCode::Char('c') => self.start_connect(ctx),
            _ => return false,
        }
        true
    }

    fn draw(&self, f: &mut Frame, area: Rect, t: Theme) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let (list, chart) = if area.width >= 100 {
            let [l, r] = Layout::horizontal([Constraint::Length(40), Constraint::Min(0)]).areas(area);
            (l, r)
        } else {
            let rows = (NARROW_ROWS + 1 + u16::from(self.prompt.is_some())).min(area.height);
            let [top, bottom] = Layout::vertical([Constraint::Length(rows), Constraint::Min(0)]).areas(area);
            (top, bottom)
        };
        self.draw_list(f, list, t);
        self.draw_chart(f, chart, t);
    }

    fn overview(&self, _width: u16, _height: u16, t: Theme) -> Vec<Line<'static>> {
        let connected = self.nets.iter().find(|n| n.connected);
        let band = connected.map(|n| n.band).unwrap_or(self.band);
        let second = match connected {
            Some(n) => Line::from(vec![
                Span::raw(format!("   {} ", truncate(&n.ssid, 20))),
                Span::styled(format!("{} dBm", n.rssi_dbm), t.value),
            ]),
            None if self.state == State::NoAdapter => Line::from(Span::styled("   no wireless adapter", t.warn)),
            None => Line::from(Span::styled("   not connected", t.frame)),
        };
        vec![
            Line::from(Span::styled(" WIFI", t.title)),
            second,
            Line::from(vec![
                Span::raw("   best ch "),
                Span::styled(wifi::best_channel(band, &self.all).to_string(), t.value),
                Span::styled(format!(" ({})", band.label()), t.frame),
            ]),
        ]
    }

    fn overview_slot(&self) -> Slot {
        Slot::Right(4)
    }

    fn status(&self) -> String {
        let connected = self.nets.iter().find(|n| n.connected).map(|n| n.ssid.as_str()).unwrap_or("none");
        format!("wifi {} networks, connected={connected}", self.nets.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::ModuleConfig;
    use crate::config::ThemeKind;
    use crate::shell::test_ctx;

    fn bss(ssid: &str, ch: u16, band: Band, rssi: i32, secured: bool) -> Bss {
        Bss {
            ssid: ssid.into(),
            ssid_raw: ssid.as_bytes().to_vec(),
            bssid: "aa:bb:cc:dd:ee:ff".into(),
            rssi_dbm: rssi,
            freq_khz: 0,
            channel: ch,
            band,
            width_mhz: 20,
            secured,
            auth: if secured { "WPA2-PSK".into() } else { "open".into() },
            connected: false,
        }
    }

    fn wifi_with_data() -> (Wifi, Receiver<WifiCmd>) {
        let mut m = Wifi::new();
        let all = vec![
            bss("Vault 111", 6, Band::G2_4, -45, true),
            bss("Vault 111", 36, Band::G5, -52, true),
            bss("Open Diner", 11, Band::G2_4, -70, false),
        ];
        m.nets = wifi::networks(&all);
        m.all = all;
        m.state = State::Ready;
        let (tx, rx) = mpsc::channel();
        m.tx = Some(tx);
        (m, rx)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn config_defaults_to_15_seconds() {
        let (cfg, notice) = ModuleConfig(toml::Table::new()).section::<WifiCfg>("wifi");
        assert_eq!(cfg.interval, 15);
        assert!(notice.is_none());
        let table: toml::Table = "[wifi]\ninterval = 30\n".parse().unwrap();
        let (cfg, _) = ModuleConfig(table).section::<WifiCfg>("wifi");
        assert_eq!(cfg.interval, 30);
    }

    #[test]
    fn password_prompt_state_machine() {
        let mut input = String::with_capacity(PASS_CAPACITY);
        assert_eq!(prompt_key(&mut input, KeyCode::Char('p')), Action::Typing);
        assert_eq!(prompt_key(&mut input, KeyCode::Char('w')), Action::Typing);
        assert_eq!(prompt_key(&mut input, KeyCode::Backspace), Action::Typing);
        assert_eq!(input, "p");
        assert_eq!(prompt_key(&mut input, KeyCode::Char('q')), Action::Typing);
        assert_eq!(prompt_key(&mut input, KeyCode::Enter), Action::Submit);
        assert_eq!(input, "pq", "Submit leaves the buffer for the caller to take");
        assert_eq!(prompt_key(&mut input, KeyCode::Esc), Action::Cancel);
        assert!(input.is_empty(), "Esc wipes the typed passphrase");
        // Esc must scrub the whole allocation, not just the live bytes.
        assert!(wifi::raw_buffer(&input).iter().all(|b| *b == 0), "the plaintext is gone from the buffer");
        assert!(wifi::raw_buffer(&input).len() >= PASS_CAPACITY);
    }

    #[test]
    fn the_passphrase_never_outgrows_its_buffer() {
        let mut input = String::with_capacity(PASS_CAPACITY);
        let cap = input.capacity();
        for _ in 0..200 {
            prompt_key(&mut input, KeyCode::Char('x'));
        }
        assert_eq!(input.chars().count(), PASS_MAX_CHARS, "the WPA limit stops the input");
        assert_eq!(input.capacity(), cap, "no reallocation, so no un-wipeable copy is left behind");
    }

    #[test]
    fn c_on_an_open_network_connects_and_on_a_secured_one_prompts() {
        let (ctx, _n) = test_ctx(toml::Table::new());
        let (mut m, cmds) = wifi_with_data();
        // Row 0 is the strongest, secured "Vault 111".
        assert!(m.on_key(key(KeyCode::Char('c')), &ctx));
        assert!(m.prompt.is_some(), "a secured network asks for the passphrase");
        assert!(cmds.try_recv().is_err(), "nothing is sent before Enter");

        assert!(m.on_key(key(KeyCode::Char('s')), &ctx), "the prompt eats plain keys");
        assert!(m.on_key(key(KeyCode::Enter), &ctx));
        assert!(m.prompt.is_none());
        match cmds.try_recv() {
            Ok(WifiCmd::Connect { ssid, auth, password, .. }) => {
                assert_eq!(ssid, "Vault 111");
                assert_eq!(auth, wifi::ProfileAuth::Wpa2Psk);
                assert_eq!(password, "s");
            }
            _ => panic!("expected a Connect command"),
        }

        assert!(m.on_key(key(KeyCode::Down), &ctx));
        assert!(m.on_key(key(KeyCode::Char('c')), &ctx));
        assert!(m.prompt.is_none(), "an open network needs no passphrase");
        match cmds.try_recv() {
            Ok(WifiCmd::Connect { ssid, auth, password, .. }) => {
                assert_eq!(ssid, "Open Diner");
                assert_eq!(auth, wifi::ProfileAuth::Open);
                assert!(password.is_empty());
            }
            _ => panic!("expected a Connect command"),
        }
    }

    #[test]
    fn the_passphrase_goes_to_the_network_the_prompt_was_opened_for() {
        let (ctx, _n) = test_ctx(toml::Table::new());
        let (mut m, cmds) = wifi_with_data();
        assert_eq!(m.nets[0].ssid, "Vault 111");
        m.on_key(key(KeyCode::Char('c')), &ctx);
        assert!(m.prompt.is_some());

        // A scan lands while the prompt is open and re-sorts the list: row 0 is
        // now a different network entirely.
        let all = vec![
            bss("Red Rocket", 1, Band::G2_4, -30, true),
            bss("Vault 111", 6, Band::G2_4, -45, true),
        ];
        m.event(WifiEvent::Scan(all), &ctx);
        assert_eq!(m.nets[m.sel].ssid, "Red Rocket", "the selection now points elsewhere");

        m.on_key(key(KeyCode::Char('s')), &ctx);
        m.on_key(key(KeyCode::Enter), &ctx);
        match cmds.try_recv() {
            Ok(WifiCmd::Connect { ssid, ssid_raw, password, .. }) => {
                assert_eq!(ssid, "Vault 111", "the passphrase must not follow the selection");
                assert_eq!(ssid_raw.as_slice(), b"Vault 111");
                assert_eq!(password, "s");
            }
            _ => panic!("expected a Connect command"),
        }
    }

    #[test]
    fn an_empty_passphrase_keeps_the_prompt_open() {
        let (ctx, _n) = test_ctx(toml::Table::new());
        let (mut m, cmds) = wifi_with_data();
        m.on_key(key(KeyCode::Char('c')), &ctx);
        assert!(m.on_key(key(KeyCode::Enter), &ctx));
        assert!(m.prompt.is_some(), "Enter on an empty passphrase stays in the prompt");
        assert!(cmds.try_recv().is_err(), "and sends nothing");
    }

    #[test]
    fn an_enterprise_network_is_refused_before_the_prompt_opens() {
        let (ctx, _n) = test_ctx(toml::Table::new());
        let (mut m, cmds) = wifi_with_data();
        m.nets[0].auth = "WPA3-ENT".into();
        assert!(m.on_key(key(KeyCode::Char('c')), &ctx));
        assert!(m.prompt.is_none(), "no passphrase is asked for a network that has none");
        assert!(cmds.try_recv().is_err(), "and nothing is sent");
    }

    #[test]
    fn ctrl_c_is_never_consumed_not_even_by_the_prompt() {
        let (ctx, _n) = test_ctx(toml::Table::new());
        let (mut m, _cmds) = wifi_with_data();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!m.on_key(ctrl_c, &ctx));
        m.prompt = Some(Prompt::new(&m.nets[0].clone(), wifi::ProfileAuth::Wpa2Psk));
        assert!(!m.on_key(ctrl_c, &ctx), "the shell must still be able to quit");
    }

    #[test]
    fn navigation_band_and_rescan() {
        let (ctx, _n) = test_ctx(toml::Table::new());
        let (mut m, cmds) = wifi_with_data();
        assert!(m.on_key(key(KeyCode::Up), &ctx));
        assert_eq!(m.sel, 0, "no wrapping past the top");
        for _ in 0..5 {
            m.on_key(key(KeyCode::Down), &ctx);
        }
        assert_eq!(m.sel, m.nets.len() - 1, "clamped to the last row");
        assert!(m.on_key(key(KeyCode::Char('b')), &ctx));
        assert_eq!(m.band, Band::G5);
        assert!(m.on_key(key(KeyCode::Char('r')), &ctx));
        assert!(matches!(cmds.try_recv(), Ok(WifiCmd::Rescan)));
        assert!(!m.on_key(key(KeyCode::Char('z')), &ctx), "other keys stay free");
    }

    #[test]
    fn status_and_overview_survive_an_empty_scan() {
        let t = Theme::new(ThemeKind::Color);
        let m = Wifi::new();
        assert_eq!(m.status(), "wifi 0 networks, connected=none");
        assert_eq!(m.overview(20, 3, t).len(), 3);
        let (mut m, _c) = wifi_with_data();
        m.nets[0].connected = true;
        assert_eq!(m.status(), "wifi 2 networks, connected=Vault 111");
        assert_eq!(m.overview(20, 3, t).len(), 3);
        assert_eq!(m.snapshot().connected.as_deref(), Some("Vault 111"));
    }

    #[test]
    fn signal_bars_scale_with_the_signal() {
        assert_eq!(bars(-40), "▂▃▅▇");
        assert_eq!(bars(-70), "▂▃··");
        assert_eq!(bars(-120), "····");
    }

    #[test]
    fn draw_never_panics_on_tiny_areas() {
        let t = Theme::new(ThemeKind::Color);
        let (mut m, _c) = wifi_with_data();
        for (w, h) in [(1u16, 1u16), (40, 12), (3, 2), (120, 40), (100, 12)] {
            let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            term.draw(|f| m.draw(f, f.area(), t)).unwrap();
        }
        // The same, with a prompt open, no data at all and each band selected.
        m.prompt = Some(Prompt::new(&m.nets[0].clone(), wifi::ProfileAuth::Wpa2Psk));
        m.prompt.as_mut().unwrap().input.push_str("secret");
        m.state = State::NoAdapter;
        m.nets.clear();
        m.all.clear();
        for band in [Band::G2_4, Band::G5, Band::G6] {
            m.band = band;
            for (w, h) in [(1u16, 1u16), (40, 12), (120, 40)] {
                let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
                term.draw(|f| m.draw(f, f.area(), t)).unwrap();
            }
        }
    }
}
