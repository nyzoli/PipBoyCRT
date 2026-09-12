//! STAT modul: rendszerállapot (CPU/GPU/MEM/DISK/NET), sparkline-előzményekkel.
use crate::module::{Ctx, Module, Slot};
use crate::stat::{self, fmt_uptime, StatSnapshot, HIST};
use crate::style::Theme;
use crate::ui::widgets::{bits_rate, bytes, gauge, spark};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::Frame;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

/// How often the foreign sources of the S.P.E.C.I.A.L. sheet are re-read
/// (blackboard clones and a file read: never per frame).
const SHEET_TTL: Duration = Duration::from_secs(10);

/// Which screen the STAT tab shows; `s` toggles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StatView {
    #[default]
    Stats,
    Special,
}

#[derive(Default)]
pub struct Stat {
    pub snap: Option<StatSnapshot>,
    pub cpu_hist: VecDeque<u64>,
    pub gpu_hist: VecDeque<u64>,
    pub rx_hist: VecDeque<u64>,
    pub tx_hist: VecDeque<u64>,
    rx: Option<Receiver<StatSnapshot>>,
    pub view: StatView,
    /// (Wi-Fi networks in range, connected) from the WIFI blackboard entry.
    pub wifi_seen: Option<(usize, bool)>,
    /// WMO code from the WEATHER blackboard entry.
    pub weather_code: Option<u8>,
    /// Count of tracks in the `Favorite tracks` note of `notes.md`.
    pub favorites: Option<usize>,
    notes_path: PathBuf,
    sheet_at: Option<Instant>,
}

/// The `[notes]` section as STAT needs it: just the file name, same default
/// as the NOTES module. Modules never reference each other, so this tiny
/// config-reading/path-resolving logic is duplicated (also in RADIO) rather
/// than calling into `modules::notes`.
#[derive(Default, serde::Deserialize)]
#[serde(default)]
struct NotesFileCfg {
    file: String,
}

impl Stat {
    pub fn new() -> Self {
        Self::default()
    }

    /// Re-read the S.P.E.C.I.A.L. sources (blackboard + the `Favorite tracks`
    /// note) at most every [`SHEET_TTL`]; the view itself only reads the
    /// cached values.
    fn refresh_sheet(&mut self, ctx: &Ctx) {
        if self.sheet_at.is_some_and(|t| t.elapsed() < SHEET_TTL) {
            return;
        }
        self.sheet_at = Some(Instant::now());
        self.wifi_seen = ctx
            .board
            .get::<crate::wifi::WifiSnapshot>("wifi")
            .map(|w| (w.networks.len(), w.connected.is_some()));
        self.weather_code = ctx.board.get::<crate::weather::WeatherSnapshot>("weather").map(|w| w.code);
        self.favorites =
            std::fs::read_to_string(&self.notes_path).ok().map(|s| crate::radio::count_favorites(&s));
    }
}

fn push(h: &mut VecDeque<u64>, v: u64, cap: usize) {
    if h.len() == cap {
        h.pop_front();
    }
    h.push_back(v);
}

fn hist(h: &VecDeque<u64>) -> Vec<u64> {
    h.iter().copied().collect()
}

impl Module for Stat {
    fn id(&self) -> &'static str {
        "stat"
    }
    fn title(&self) -> &'static str {
        "STAT"
    }
    fn describe(&self) -> &'static str {
        "CPU, memory, disks, network, GPU, battery, S.P.E.C.I.A.L. sheet"
    }
    fn manual(&self) -> &'static str {
        "\
STAT is the vitals panel: CPU, memory, disks, network traffic,
the GPU and, on a laptop, the battery that also rides along in
the header. The graphs feed themselves; nothing to press.

  s     the S.P.E.C.I.A.L. character sheet, and back again

The sheet grades your machine on the seven Vault-Tec virtues:
Endurance is uptime and battery, Agility is the CPU load and
the process count, Luck is whatever was left over. A low score
is the hardware's fault, not a comment on your character.

  ←/→   change tab            1-9  jump to a tab
  space play/pause the radio  +/-  volume     m  mute"
    }
    fn help(&self) -> &'static str {
        match self.view {
            StatView::Stats => "s special   ←/→ tab   1-9 jump   space play/pause   +/- volume   m mute   q quit",
            StatView::Special => "s stats   ←/→ tab   1-9 jump   space play/pause   +/- volume   m mute   q quit",
        }
    }

    fn start(&mut self, ctx: &Ctx) {
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(std::path::Path::to_path_buf)).unwrap_or_default();
        let (cfg, notice) = ctx.config.section::<NotesFileCfg>("notes");
        if let Some(n) = notice {
            let _ = ctx.notify.send(crate::module::Notice::Footer(n));
        }
        let file = if cfg.file.is_empty() { "notes.md" } else { &cfg.file };
        self.notes_path = exe_dir.join(file);
        let (tx, rx) = mpsc::channel();
        stat::spawn(tx);
        self.rx = Some(rx);
    }

    fn on_key(&mut self, key: KeyEvent, _ctx: &Ctx) -> bool {
        if key.code != KeyCode::Char('s') || key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            return false;
        }
        self.view = match self.view {
            StatView::Stats => StatView::Special,
            StatView::Special => StatView::Stats,
        };
        true
    }

    fn poll(&mut self, ctx: &Ctx) -> usize {
        if self.view == StatView::Special {
            self.refresh_sheet(ctx);
        }
        let mut n = 0;
        let Some(rx) = &self.rx else { return 0 };
        while let Ok(s) = rx.try_recv() {
            push(&mut self.cpu_hist, s.cpu_total as u64, HIST);
            push(&mut self.gpu_hist, s.gpu.as_ref().map(|g| g.util as u64).unwrap_or(0), HIST);
            push(&mut self.rx_hist, s.rx_bps, HIST);
            push(&mut self.tx_hist, s.tx_bps, HIST);
            self.snap = Some(s);
            n += 1;
        }
        if n > 0 {
            if let Some(s) = &self.snap {
                ctx.board.publish(self.id(), s.clone());
            }
        }
        n
    }

    fn draw(&self, f: &mut Frame, area: Rect, t: Theme) {
        match self.view {
            StatView::Stats => crate::ui::stat::draw(f, self, area, t),
            StatView::Special => crate::ui::special::draw(f, self, area, t),
        }
    }

    fn header(&self, _width: u16, t: Theme) -> Vec<Span<'static>> {
        let Some(b) = self.snap.as_ref().and_then(|s| s.battery.as_ref()) else { return vec![] };
        let style = if b.pct <= 15 && !b.charging { t.danger } else { t.value };
        vec![Span::styled(format!("BAT {}%{}", b.pct, if b.charging { "⇡" } else { "" }), style)]
    }

    fn overview(&self, width: u16, _height: u16, t: Theme) -> Vec<Line<'static>> {
        let lw = width.saturating_sub(12) as usize;
        let mut st = vec![Line::from(Span::styled(" STAT", t.title))];
        let Some(s) = &self.snap else {
            st.push(Line::from(Span::styled("   collecting…", t.frame)));
            return st;
        };
        let mem_pct = if s.mem_total > 0 { s.mem_used as f32 / s.mem_total as f32 } else { 0.0 };
        st.push(Line::from(vec![
            Span::styled("   CPU ", t.frame),
            Span::styled(spark(&hist(&self.cpu_hist), lw.min(30), 100), t.graph),
            Span::styled(format!(" {:3.0}%", s.cpu_total), t.load(s.cpu_total)),
        ]));
        st.push(Line::from(vec![
            Span::styled("   MEM ", t.frame),
            Span::styled(gauge(mem_pct, 10), t.graph),
            Span::styled(format!(" {:3.0}% {}", mem_pct * 100.0, bytes(s.mem_used)), t.load(mem_pct * 100.0)),
        ]));
        match &s.gpu {
            Some(g) => st.push(Line::from(vec![
                Span::styled("   GPU ", t.frame),
                Span::styled(spark(&hist(&self.gpu_hist), lw.min(30), 100), t.graph),
                Span::styled(format!(" {:3.0}%", g.util), t.load(g.util)),
            ])),
            None => st.push(Line::from(vec![Span::styled("   GPU ", t.frame), Span::styled("n/a", t.frame)])),
        }
        if let Some(d) = s.disks.first() {
            let pct = d.used as f32 / d.total.max(1) as f32;
            st.push(Line::from(vec![
                Span::styled(format!("   DSK {:<3}", d.mount), t.frame),
                Span::styled(gauge(pct, 10), t.graph),
                Span::styled(format!(" {:3.0}%", pct * 100.0), t.value),
            ]));
        }
        st.push(Line::from(vec![
            Span::styled("   NET ", t.frame),
            Span::raw(format!("↓ {}  ↑ {}", bits_rate(s.rx_bps), bits_rate(s.tx_bps))),
        ]));
        st.push(Line::from(Span::styled(
            format!("   UP {} · PROCS {}", fmt_uptime(s.uptime_s), s.procs),
            t.frame,
        )));
        st
    }

    fn overview_slot(&self) -> Slot {
        Slot::Left(0)
    }

    fn status(&self) -> String {
        match &self.snap {
            None => "stat collecting…".into(),
            Some(s) => format!(
                "cpu={:.0}% mem={} gpu={:?} bat={:?}",
                s.cpu_total,
                bytes(s.mem_used),
                s.gpu.as_ref().map(|g| g.util),
                s.battery.as_ref().map(|b| b.pct)
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::test_ctx;

    #[test]
    fn history_is_bounded_and_keeps_the_newest() {
        let (ctx, _rx) = test_ctx(toml::Table::new());
        let (tx, rx) = mpsc::channel();
        let mut m = Stat::new();
        m.rx = Some(rx);
        for i in 0..(HIST + 5) {
            tx.send(StatSnapshot { cpu_total: i as f32, ..Default::default() }).unwrap();
        }
        assert_eq!(m.poll(&ctx), HIST + 5);
        assert_eq!(m.cpu_hist.len(), HIST);
        assert_eq!(*m.cpu_hist.back().unwrap(), (HIST + 4) as u64);
        assert!(ctx.board.get::<StatSnapshot>("stat").is_some(), "a pillanatkép a táblára kerül");
    }

    #[test]
    fn s_toggles_the_special_sheet_and_the_help() {
        let (ctx, _rx) = test_ctx(toml::Table::new());
        let mut m = Stat::new();
        let key = |c| KeyEvent::from(KeyCode::Char(c));
        assert_eq!(m.view, StatView::Stats);
        assert!(m.help().starts_with("s special"));
        assert!(m.on_key(key('s'), &ctx));
        assert_eq!(m.view, StatView::Special);
        assert!(m.help().starts_with("s stats"));
        assert!(m.on_key(key('s'), &ctx));
        assert_eq!(m.view, StatView::Stats);
        assert!(!m.on_key(key('x'), &ctx), "other keys fall through to the shell");
        assert_eq!(m.view, StatView::Stats);
    }

    #[test]
    fn the_sheet_sources_are_cached_from_the_blackboard() {
        let (ctx, _rx) = test_ctx(toml::Table::new());
        let mut m = Stat::new();
        ctx.board.publish("wifi", crate::wifi::WifiSnapshot { networks: Vec::new(), connected: Some("VAULT".into()) });
        m.poll(&ctx);
        assert!(m.wifi_seen.is_none(), "the sheet sources are only read while the sheet is shown");
        m.view = StatView::Special;
        m.poll(&ctx);
        assert_eq!(m.wifi_seen, Some((0, true)));
        assert!(m.weather_code.is_none(), "no weather on the board → ?");

        // Within the TTL a new publish is not picked up (no per-frame clone).
        ctx.board.publish("wifi", crate::wifi::WifiSnapshot { networks: Vec::new(), connected: None });
        m.poll(&ctx);
        assert_eq!(m.wifi_seen, Some((0, true)));
    }
}
