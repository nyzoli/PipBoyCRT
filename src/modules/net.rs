//! NET modul: ping-célok, Wi-Fi/interfész adatok, traceroute és SPEEDTEST.
use crate::module::{Ctx, Module, Notice, Slot};
use crate::net::{self, IfaceInfo, NetCmd, NetEvent, WifiInfo};
use crate::style::Theme;
use chrono::{DateTime, Local};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::Frame;
use serde::Deserialize;
use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use stream_download::http::reqwest;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NetCfg {
    pub targets: Vec<String>,
    /// How many SPEEDTEST results `speedtest.log` keeps (and loads at start).
    pub speedtest_history: usize,
}

impl Default for NetCfg {
    fn default() -> Self {
        Self { targets: vec!["gateway".into(), "1.1.1.1".into(), "8.8.8.8".into()], speedtest_history: 20 }
    }
}

// ---- SPEEDTEST: Cloudflare down/up, history log next to the executable ----

const SPEED_DOWN_URL: &str = "https://speed.cloudflare.com/__down?bytes=50000000";
const SPEED_UP_URL: &str = "https://speed.cloudflare.com/__up";
const SPEED_PHASE_BUDGET: Duration = Duration::from_secs(8);
const SPEED_RAMP_UP: Duration = Duration::from_millis(300);
const SPEED_UPLOAD_CHUNK: usize = 512_000;
const SPEED_UPLOAD_TOTAL: u64 = 20_000_000;
const SPEED_LOG_FILE: &str = "speedtest.log";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeedPhase {
    Latency,
    Download,
    Upload,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SpeedResult {
    pub down_mbps: f32,
    pub up_mbps: f32,
    pub ping_ms: Option<u32>,
    pub at: DateTime<Local>,
}

#[derive(Clone, Debug)]
pub enum SpeedEvent {
    Progress { phase: SpeedPhase, frac: f32, mbps_so_far: f32 },
    Done(SpeedResult),
    Error(String),
}

/// Ctx-free measurement state, so transitions are unit-testable without a
/// running app: `Idle -> Running(phase) -> Done/Error`, cancel back to `Idle`.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum SpeedState {
    #[default]
    Idle,
    Running {
        phase: SpeedPhase,
        frac: f32,
        mbps_so_far: f32,
    },
    Done(SpeedResult),
    Error(String),
}

impl SpeedState {
    pub fn start(&mut self) {
        *self = SpeedState::Running { phase: SpeedPhase::Latency, frac: 0.0, mbps_so_far: 0.0 };
    }

    pub fn cancel(&mut self) {
        *self = SpeedState::Idle;
    }

    pub fn is_running(&self) -> bool {
        matches!(self, SpeedState::Running { .. })
    }

    pub fn apply(&mut self, ev: SpeedEvent) {
        *self = match ev {
            SpeedEvent::Progress { phase, frac, mbps_so_far } => SpeedState::Running { phase, frac, mbps_so_far },
            SpeedEvent::Done(r) => SpeedState::Done(r),
            SpeedEvent::Error(e) => SpeedState::Error(e),
        };
    }
}

/// Mbps for a transfer, excluding both the bytes and the time seen during the
/// first `ramp_up` — TCP is still growing its window then, so counting it
/// undersells the real rate. Only excludes once the transfer has run at least
/// `3 * ramp_up`: shorter than that, subtracting `ramp_up` from the elapsed
/// time leaves too small (or negative-ish) a window, which can undercount to
/// 0.0 Mbps even though real data moved — the raw total is more honest there.
pub fn mbps(total_bytes: u64, total_elapsed: Duration, bytes_at_ramp: u64, ramp_up: Duration) -> f32 {
    let (bytes, elapsed) = if total_elapsed >= ramp_up * 3 {
        (total_bytes.saturating_sub(bytes_at_ramp), total_elapsed - ramp_up)
    } else {
        (total_bytes, total_elapsed)
    };
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return 0.0;
    }
    ((bytes as f64 * 8.0) / secs / 1_000_000.0) as f32
}

/// One history line: ISO local time, down Mbps, up Mbps, ping ms (empty if unknown).
pub fn format_log_line(r: &SpeedResult) -> String {
    format!(
        "{}\t{:.1}\t{:.1}\t{}\n",
        r.at.to_rfc3339(),
        r.down_mbps,
        r.up_mbps,
        r.ping_ms.map(|p| p.to_string()).unwrap_or_default()
    )
}

pub fn parse_log_line(line: &str) -> Option<SpeedResult> {
    let mut parts = line.trim_end().split('\t');
    let at = DateTime::parse_from_rfc3339(parts.next()?).ok()?.with_timezone(&Local);
    let down_mbps = parts.next()?.parse().ok()?;
    let up_mbps = parts.next()?.parse().ok()?;
    let ping_ms = match parts.next() {
        Some("") | None => None,
        Some(s) => s.parse().ok(),
    };
    Some(SpeedResult { down_mbps, up_mbps, ping_ms, at })
}

/// Parses `speedtest.log` content, keeping only the last `max` entries.
pub fn load_speed_history(content: &str, max: usize) -> Vec<SpeedResult> {
    let mut all: Vec<SpeedResult> = content.lines().filter_map(parse_log_line).collect();
    let start = all.len().saturating_sub(max);
    all.split_off(start)
}

fn speed_log_path(exe_dir: &Path) -> PathBuf {
    exe_dir.join(SPEED_LOG_FILE)
}

fn append_speed_log(path: &Path, r: &SpeedResult) {
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(format_log_line(r).as_bytes());
    }
}

/// Download phase: `GET`s the Cloudflare payload in chunks, up to
/// `SPEED_PHASE_BUDGET` or end of body, reporting progress; checks `cancel`
/// between chunks. `Ok(None)` means cancelled mid-transfer.
async fn speed_download(client: &reqwest::Client, cancel: &AtomicBool, tx: &Sender<SpeedEvent>) -> Result<Option<f32>, String> {
    let mut resp = client.get(SPEED_DOWN_URL).send().await.map_err(|e| e.to_string())?;
    resp = resp.error_for_status().map_err(|e| e.to_string())?;
    let start = Instant::now();
    let mut total = 0u64;
    let mut bytes_at_ramp = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let elapsed = start.elapsed();
        if elapsed >= SPEED_PHASE_BUDGET {
            break;
        }
        match resp.chunk().await.map_err(|e| e.to_string())? {
            Some(chunk) => {
                total += chunk.len() as u64;
                let elapsed = start.elapsed();
                if elapsed < SPEED_RAMP_UP {
                    bytes_at_ramp = total;
                }
                let frac = (elapsed.as_secs_f32() / SPEED_PHASE_BUDGET.as_secs_f32()).min(1.0);
                let mbps_so_far = mbps(total, elapsed, bytes_at_ramp, SPEED_RAMP_UP);
                let _ = tx.send(SpeedEvent::Progress { phase: SpeedPhase::Download, frac, mbps_so_far });
            }
            None => break,
        }
    }
    Ok(Some(mbps(total, start.elapsed(), bytes_at_ramp, SPEED_RAMP_UP)))
}

/// Upload phase: since a single `reqwest` POST can't be capped mid-flight,
/// sends the 20 MB zero body as ~512 KB requests in a loop until
/// `SPEED_PHASE_BUDGET` elapses or the total is sent; checks `cancel` between
/// requests, and each individual POST is itself capped to the budget
/// remaining so a slow uplink can't overrun the phase by one whole chunk.
/// `Ok(None)` means cancelled mid-transfer.
async fn speed_upload(client: &reqwest::Client, cancel: &AtomicBool, tx: &Sender<SpeedEvent>) -> Result<Option<f32>, String> {
    // ponytail: `bytes` isn't a direct dependency (only pulled in transitively
    // by reqwest, and reqwest doesn't re-export `Bytes`), so `Body::from(Bytes)`
    // isn't reachable without adding it just for a free clone here. Each POST
    // still deep-copies the chunk via `Vec::clone`. Add `bytes` as a direct
    // dep if this measurably matters.
    let chunk = vec![0u8; SPEED_UPLOAD_CHUNK];
    let start = Instant::now();
    let mut total = 0u64;
    let mut bytes_at_ramp = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let elapsed = start.elapsed();
        if elapsed >= SPEED_PHASE_BUDGET || total >= SPEED_UPLOAD_TOTAL {
            break;
        }
        let remaining = SPEED_PHASE_BUDGET - elapsed;
        let post = client.post(SPEED_UP_URL).body(chunk.clone()).send();
        let resp = match tokio::time::timeout(remaining, post).await {
            Ok(r) => r.map_err(|e| e.to_string())?,
            Err(_) => break, // stalled mid-POST: drop the partial body, end the phase
        };
        resp.error_for_status().map_err(|e| e.to_string())?;
        total += SPEED_UPLOAD_CHUNK as u64;
        let elapsed = start.elapsed();
        if elapsed < SPEED_RAMP_UP {
            bytes_at_ramp = total;
        }
        let frac = (elapsed.as_secs_f32() / SPEED_PHASE_BUDGET.as_secs_f32()).min(1.0);
        let mbps_so_far = mbps(total, elapsed, bytes_at_ramp, SPEED_RAMP_UP);
        let _ = tx.send(SpeedEvent::Progress { phase: SpeedPhase::Upload, frac, mbps_so_far });
    }
    Ok(Some(mbps(total, start.elapsed(), bytes_at_ramp, SPEED_RAMP_UP)))
}

/// Full measurement, spawned on `ctx.rt`: latency is reported instantly from
/// the ping stats the caller already has, then download, then upload. Silent
/// early return on cancel (no `Done`/`Error`) — a cancelled test just stops.
async fn run_speed_test(ping_ms: Option<u32>, cancel: Arc<AtomicBool>, log_path: PathBuf, tx: Sender<SpeedEvent>) {
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    let _ = tx.send(SpeedEvent::Progress { phase: SpeedPhase::Latency, frac: 1.0, mbps_so_far: 0.0 });

    let client = match reqwest::Client::builder()
        .user_agent("PipBoyCRT")
        .connect_timeout(Duration::from_secs(5))
        .read_timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(SpeedEvent::Error(e.to_string()));
            return;
        }
    };

    let down_mbps = match speed_download(&client, &cancel, &tx).await {
        Ok(Some(v)) => v,
        Ok(None) => return,
        Err(e) => {
            let _ = tx.send(SpeedEvent::Error(e));
            return;
        }
    };

    let up_mbps = match speed_upload(&client, &cancel, &tx).await {
        Ok(Some(v)) => v,
        Ok(None) => return,
        Err(e) => {
            let _ = tx.send(SpeedEvent::Error(e));
            return;
        }
    };

    let result = SpeedResult { down_mbps, up_mbps, ping_ms, at: Local::now() };
    append_speed_log(&log_path, &result);
    let _ = tx.send(SpeedEvent::Done(result));
}

pub struct Target {
    pub label: String,
    pub addr: Option<String>,
}

#[derive(Default)]
pub struct NetState {
    pub targets: Vec<Target>,
    pub hist: Vec<VecDeque<Option<u32>>>,
    pub wifi: Option<WifiInfo>,
    pub iface: Option<IfaceInfo>,
    pub trace: Vec<String>,
    pub tracing: bool,
    pub log: Option<String>,
    pub speed: SpeedState,
    pub speed_history: Vec<SpeedResult>,
}

impl NetState {
    pub fn new(cfg: &NetCfg) -> Self {
        Self {
            targets: cfg.targets.iter().map(|t| Target { label: t.clone(), addr: None }).collect(),
            hist: cfg.targets.iter().map(|_| VecDeque::new()).collect(),
            ..Self::default()
        }
    }

    /// Az utolsó `net::WINDOW` minta statisztikája.
    pub fn stats(&self, target: usize) -> Option<net::Stats> {
        let h = self.hist.get(target)?;
        net::stats(h.iter().rev().take(net::WINDOW).rev().copied())
    }
}

#[derive(Default)]
pub struct Net {
    pub state: NetState,
    rx: Option<Receiver<NetEvent>>,
    tx: Option<Sender<NetCmd>>,
    speed_rx: Option<Receiver<SpeedEvent>>,
    speed_cancel: Option<Arc<AtomicBool>>,
    speed_log_path: PathBuf,
    speed_history_max: usize,
}

impl Net {
    pub fn new() -> Self {
        Self::default()
    }

    fn send(&self, cmd: NetCmd) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(cmd);
        }
    }

    /// `s`: start a measurement, or cancel one already running.
    fn toggle_speedtest(&mut self, ctx: &Ctx) {
        if self.state.speed.is_running() {
            self.cancel_speedtest();
            return;
        }
        let ping_ms = self.state.stats(0).map(|s| s.avg);
        let cancel = Arc::new(AtomicBool::new(false));
        self.speed_cancel = Some(cancel.clone());
        let (tx, rx) = mpsc::channel();
        self.speed_rx = Some(rx);
        self.state.speed.start();
        let log_path = self.speed_log_path.clone();
        ctx.rt.spawn(run_speed_test(ping_ms, cancel, log_path, tx));
    }

    fn cancel_speedtest(&mut self) {
        if let Some(c) = &self.speed_cancel {
            c.store(true, Ordering::Relaxed);
        }
        self.speed_cancel = None;
        self.speed_rx = None;
        self.state.speed.cancel();
    }

    fn event(&mut self, ev: NetEvent) {
        match ev {
            NetEvent::Resolved { target, addr } => {
                if let Some(t) = self.state.targets.get_mut(target) {
                    t.addr = addr;
                }
            }
            NetEvent::Ping { target, rtt } => {
                if let Some(h) = self.state.hist.get_mut(target) {
                    if h.len() == net::HIST {
                        h.pop_front();
                    }
                    h.push_back(rtt);
                }
            }
            NetEvent::Wifi(w) => self.state.wifi = w,
            NetEvent::Iface(i) => self.state.iface = i,
            NetEvent::TraceLine(l) => self.state.trace.push(l),
            NetEvent::TraceDone => self.state.tracing = false,
            NetEvent::Log(s) => self.state.log = Some(s),
        }
    }
}

impl Module for Net {
    fn id(&self) -> &'static str {
        "net"
    }
    fn title(&self) -> &'static str {
        "NET"
    }
    fn describe(&self) -> &'static str {
        "Ping, traceroute and a Cloudflare speed test"
    }
    fn manual(&self) -> &'static str {
        "\
NET keeps an eye on the wires: a running ping to your gateway
and to the public resolvers, as sparklines with packet loss.

  t     traceroute on and off - every hop between you and
        the outside world, guilty parties included
  s     start a Cloudflare speed test, down then up
  esc   cancel a speed test that is still running

Earlier runs are kept, so you can prove to yourself that the
line really did used to be faster.

  1-9   jump to a tab          space  play/pause the radio
  +/-   volume"
    }
    fn help(&self) -> &'static str {
        "t traceroute   s speedtest   1-9 tabs   space play/pause   +/- volume   q quit"
    }

    fn start(&mut self, ctx: &Ctx) {
        let (cfg, notice) = ctx.config.section::<NetCfg>(self.id());
        if let Some(n) = notice {
            let _ = ctx.notify.send(Notice::Footer(n));
        }
        self.speed_history_max = cfg.speedtest_history;
        self.state = NetState::new(&cfg);
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default();
        self.speed_log_path = speed_log_path(&exe_dir);
        let content = std::fs::read_to_string(&self.speed_log_path).unwrap_or_default();
        self.state.speed_history = load_speed_history(&content, self.speed_history_max);
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.tx = Some(net::spawn(cfg, tx));
    }

    fn poll(&mut self, _ctx: &Ctx) -> usize {
        let mut n = 0;
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

        let mut speed_events = Vec::new();
        if let Some(rx) = &self.speed_rx {
            while let Ok(ev) = rx.try_recv() {
                speed_events.push(ev);
                n += 1;
            }
        }
        for ev in speed_events {
            if let SpeedEvent::Done(r) = &ev {
                self.state.speed_history.push(r.clone());
                if self.state.speed_history.len() > self.speed_history_max {
                    self.state.speed_history.remove(0);
                }
            }
            self.state.speed.apply(ev);
        }
        n
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &Ctx) -> bool {
        match key.code {
            KeyCode::Char('t') => {
                if self.state.tracing || !self.state.trace.is_empty() {
                    self.state.tracing = false;
                    self.state.trace.clear();
                    self.send(NetCmd::TraceStop);
                } else {
                    self.state.tracing = true;
                    self.state.log = None;
                    self.send(NetCmd::TraceStart);
                }
                true
            }
            KeyCode::Char('s') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.toggle_speedtest(ctx);
                true
            }
            KeyCode::Esc if self.state.speed.is_running() => {
                self.cancel_speedtest();
                true
            }
            _ => false,
        }
    }

    fn draw(&self, f: &mut Frame, area: Rect, t: Theme) {
        crate::ui::net::draw(f, self, area, t);
    }

    fn overview(&self, width: u16, height: u16, t: Theme) -> Vec<Line<'static>> {
        let mut parts: Vec<String> = Vec::new();
        let mut worst_loss = 0u8;
        for (i, tg) in self.state.targets.iter().enumerate() {
            let label = if tg.label == "gateway" { "gw".to_string() } else { tg.label.clone() };
            match self.state.stats(i) {
                Some(s) => {
                    worst_loss = worst_loss.max(s.loss_pct);
                    parts.push(format!("{label} {} ms", s.avg));
                }
                None => parts.push(format!("{label} n/a")),
            }
        }
        let mut lines = vec![Line::from(vec![
            Span::styled(" NET ", t.title),
            Span::raw(parts.join(" · ")),
            Span::styled(
                format!(" · loss {worst_loss}%"),
                if worst_loss >= 50 {
                    t.danger
                } else if worst_loss > 0 {
                    t.warn
                } else {
                    t.frame
                },
            ),
        ])];
        if height > 1 {
            if let Some(r) = self.state.speed_history.last() {
                let text = format!(" ↓ {:.1} Mbps  ↑ {:.1} Mbps", r.down_mbps, r.up_mbps);
                lines.push(Line::from(Span::raw(crate::ui::widgets::truncate(&text, width as usize))));
            }
        }
        lines
    }

    fn overview_slot(&self) -> Slot {
        Slot::Left(2)
    }

    fn wants_fast_frames(&self, active: bool) -> bool {
        active && self.state.speed.is_running()
    }

    fn status(&self) -> String {
        let parts: Vec<String> = self
            .state
            .targets
            .iter()
            .enumerate()
            .map(|(i, tg)| match self.state.stats(i) {
                Some(s) => format!("{} {}ms/{}%", tg.label, s.avg, s.loss_pct),
                None => format!("{} n/a", tg.label),
            })
            .collect();
        let mut s = format!("{} wifi={}", parts.join(" "), self.state.wifi.as_ref().map(|w| w.ssid.as_str()).unwrap_or("-"));
        if let SpeedState::Done(r) = &self.state.speed {
            s.push_str(&format!(" speed={:.1}/{:.1}", r.down_mbps, r.up_mbps));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::ModuleConfig;
    use crate::shell::test_ctx;
    use chrono::TimeZone;

    fn net() -> (Net, Receiver<NetCmd>) {
        let mut m = Net::new();
        m.state = NetState::new(&NetCfg::default());
        let (tx, rx) = mpsc::channel();
        m.tx = Some(tx);
        (m, rx)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn config_defaults_and_section() {
        let (cfg, notice) = ModuleConfig(toml::Table::new()).section::<NetCfg>("net");
        assert_eq!(cfg.targets, vec!["gateway", "1.1.1.1", "8.8.8.8"]);
        assert!(notice.is_none());
        let table: toml::Table = "[net]\ntargets = [\"1.1.1.1\"]\n".parse().unwrap();
        let (cfg, _) = ModuleConfig(table).section::<NetCfg>("net");
        assert_eq!(cfg.targets, vec!["1.1.1.1"]);
    }

    #[test]
    fn t_toggles_trace_and_clears_it() {
        let (ctx, _n) = test_ctx(toml::Table::new());
        let (mut m, cmds) = net();
        assert!(m.on_key(key(KeyCode::Char('t')), &ctx));
        assert_eq!(cmds.try_recv().unwrap(), NetCmd::TraceStart);
        assert!(m.state.tracing);
        m.event(NetEvent::TraceLine("1  <1 ms  192.168.1.1".into()));
        assert!(m.on_key(key(KeyCode::Char('t')), &ctx));
        assert_eq!(cmds.try_recv().unwrap(), NetCmd::TraceStop);
        assert!(m.state.trace.is_empty() && !m.state.tracing);
        assert!(!m.on_key(key(KeyCode::Char('x')), &ctx), "más kulcsot nem fogyaszt el");
    }

    #[test]
    fn ping_history_is_bounded() {
        let (ctx, _n) = test_ctx(toml::Table::new());
        let (mut m, _cmds) = net();
        for i in 0..(net::HIST + 10) {
            m.event(NetEvent::Ping { target: 0, rtt: Some(i as u32) });
        }
        assert_eq!(m.state.hist[0].len(), net::HIST);
        m.event(NetEvent::Ping { target: 99, rtt: Some(1) });
        assert_eq!(m.poll(&ctx), 0, "nincs forrás-csatorna, nincs esemény");
    }

    #[test]
    fn esc_cancels_a_running_test_but_is_otherwise_unconsumed() {
        let (ctx, _n) = test_ctx(toml::Table::new());
        let (mut m, _cmds) = net();
        assert!(!m.on_key(key(KeyCode::Esc), &ctx), "esc idle-ben nem fogyaszt el semmit");
        m.state.speed.start();
        assert!(m.on_key(key(KeyCode::Esc), &ctx));
        assert_eq!(m.state.speed, SpeedState::Idle);
    }

    #[test]
    fn ctrl_s_does_not_start_a_speedtest() {
        let (ctx, _n) = test_ctx(toml::Table::new());
        let (mut m, _cmds) = net();
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(!m.on_key(ctrl_s, &ctx), "ctrl+s nem indíthat speedtestet");
        assert_eq!(m.state.speed, SpeedState::Idle);
        assert!(m.on_key(key(KeyCode::Char('s')), &ctx), "sima s indítja");
        assert!(m.state.speed.is_running());
    }

    fn sample_result() -> SpeedResult {
        SpeedResult {
            down_mbps: 245.3,
            up_mbps: 41.2,
            ping_ms: Some(12),
            at: chrono::Local.with_ymd_and_hms(2026, 9, 10, 18, 40, 0).unwrap(),
        }
    }

    #[test]
    fn mbps_excludes_ramp_up_bytes_and_time_when_more_data_follows() {
        let v = mbps(50_000_000, Duration::from_secs(5), 1_000_000, Duration::from_millis(300));
        let expected = (49_000_000f64 * 8.0 / 4.7 / 1_000_000.0) as f32;
        assert!((v - expected).abs() < 0.01, "{v} vs {expected}");
    }

    #[test]
    fn mbps_uses_raw_total_when_transfer_finishes_within_ramp_up() {
        // Finished at 200ms, still under the 300ms ramp-up window: no exclusion.
        let v = mbps(1_000_000, Duration::from_millis(200), 1_000_000, Duration::from_millis(300));
        let expected = (1_000_000f64 * 8.0 / 0.2 / 1_000_000.0) as f32;
        assert!((v - expected).abs() < 0.01, "{v} vs {expected}");
    }

    #[test]
    fn mbps_uses_full_window_when_elapsed_is_past_ramp_up_but_under_3x() {
        // Regression: elapsed (350ms) is past the 300ms ramp-up but under the
        // 3x guard (900ms), and every byte arrived during the ramp window.
        // The old `> ramp_up` check would subtract all bytes and the ramp
        // time, yielding 0.0 Mbps despite 1MB having moved.
        let v = mbps(1_000_000, Duration::from_millis(350), 1_000_000, Duration::from_millis(300));
        assert!(v > 0.0, "expected a real rate, got {v}");
        let expected = (1_000_000f64 * 8.0 / 0.35 / 1_000_000.0) as f32;
        assert!((v - expected).abs() < 0.01, "{v} vs {expected}");
    }

    #[test]
    fn mbps_zero_elapsed_is_zero_not_a_panic() {
        assert_eq!(mbps(1_000, Duration::ZERO, 0, Duration::from_millis(300)), 0.0);
    }

    #[test]
    fn log_line_format_and_parse_round_trip() {
        let r = sample_result();
        let line = format_log_line(&r);
        assert!(line.ends_with('\n'));
        assert_eq!(parse_log_line(line.trim_end()), Some(r));
    }

    #[test]
    fn log_line_without_ping_round_trips_to_none() {
        let mut r = sample_result();
        r.ping_ms = None;
        let line = format_log_line(&r);
        assert_eq!(parse_log_line(line.trim_end()).unwrap().ping_ms, None);
    }

    #[test]
    fn parse_log_line_rejects_garbage() {
        assert_eq!(parse_log_line("not a log line"), None);
        assert_eq!(parse_log_line(""), None);
    }

    #[test]
    fn speed_history_load_keeps_only_the_last_max_entries_in_order() {
        let mut content = String::new();
        let mut expected_last = String::new();
        for i in 0..25u32 {
            let r = SpeedResult {
                down_mbps: i as f32,
                up_mbps: i as f32,
                ping_ms: Some(i),
                at: chrono::Local.with_ymd_and_hms(2026, 9, 10, 12, 0, 0).unwrap() + chrono::Duration::minutes(i as i64),
            };
            content.push_str(&format_log_line(&r));
            if i == 24 {
                expected_last = format!("{i}");
            }
        }
        let hist = load_speed_history(&content, 20);
        assert_eq!(hist.len(), 20);
        assert_eq!(hist[0].ping_ms, Some(5), "az első 5 sort levágja");
        assert_eq!(hist.last().unwrap().ping_ms, Some(24));
        assert_eq!(format!("{}", hist.last().unwrap().ping_ms.unwrap()), expected_last);
    }

    #[test]
    fn speed_history_load_empty_when_max_zero() {
        let r = sample_result();
        let content = format_log_line(&r);
        assert!(load_speed_history(&content, 0).is_empty());
    }

    #[test]
    fn speed_state_idle_running_done_and_error() {
        let mut s = SpeedState::default();
        assert_eq!(s, SpeedState::Idle);
        assert!(!s.is_running());

        s.start();
        assert!(matches!(s, SpeedState::Running { phase: SpeedPhase::Latency, .. }));
        assert!(s.is_running());

        s.apply(SpeedEvent::Progress { phase: SpeedPhase::Download, frac: 0.5, mbps_so_far: 100.0 });
        assert!(matches!(s, SpeedState::Running { phase: SpeedPhase::Download, frac, .. } if frac == 0.5));

        let result = sample_result();
        s.apply(SpeedEvent::Done(result.clone()));
        assert_eq!(s, SpeedState::Done(result));
        assert!(!s.is_running());

        let mut err = SpeedState::default();
        err.start();
        err.apply(SpeedEvent::Error("boom".into()));
        assert_eq!(err, SpeedState::Error("boom".into()));
        assert!(!err.is_running());
    }

    #[test]
    fn speed_state_cancel_returns_to_idle_from_any_state() {
        let mut s = SpeedState::default();
        s.start();
        s.cancel();
        assert_eq!(s, SpeedState::Idle);

        s.start();
        s.apply(SpeedEvent::Done(sample_result()));
        s.cancel();
        assert_eq!(s, SpeedState::Idle);
    }
}
