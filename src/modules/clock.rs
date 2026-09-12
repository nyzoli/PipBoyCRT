//! CLOCK modul: óra, világidő, holdfázis és a küldetés-időzítő.
//!
//! A napkelte/napnyugta a WEATHER modultól jön a közös táblán (`Blackboard`);
//! mivel a `draw`/`overview` nem kap `Ctx`-et, a `poll` gyorsítótárazza.
use crate::clock::{moon_phase, ClockState, ClockView, Timer, TimerState};
use crate::module::{Ctx, Module, Notice, Slot};
use crate::style::Theme;
use crate::ui::widgets::big_digits;
use crate::weather::WeatherSnapshot;
use chrono::{DateTime, Datelike, Local};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::Frame;
use serde::Deserialize;
use std::time::Instant;

/// A fejléc villogó szövege a lejárat után.
const ALERT: &str = " ▶ TIMER DONE ◀ ";

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ClockCfg {
    pub zones: Vec<String>,
    pub timer_minutes: u32,
    /// Induló nézet: `normal` / `digital` / `analog`.
    pub view: ClockView,
}

impl Default for ClockCfg {
    fn default() -> Self {
        Self {
            zones: vec!["America/New_York".into(), "Asia/Tokyo".into()],
            timer_minutes: 25,
            view: ClockView::Normal,
        }
    }
}

pub struct Clock {
    pub state: ClockState,
    /// (napkelte, napnyugta) a WEATHER tábláról, `poll`-ban frissítve.
    pub sun: Option<(String, String)>,
    /// A gyorsítótárazott pillanatkép `fetched_at`-je (olcsó változás-kulcs).
    sun_at: Option<DateTime<Local>>,
    /// Az aktuális nézet (`v`).
    pub view: ClockView,
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock {
    pub fn new() -> Self {
        Self {
            state: ClockState { zones: Vec::new(), timer: Timer::new(ClockCfg::default().timer_minutes) },
            sun: None,
            sun_at: None,
            view: ClockView::Normal,
        }
    }

    /// A napkelte/napnyugta párja, `--:--` ha még nincs időjárás.
    pub fn sun_times(&self) -> (&str, &str) {
        match &self.sun {
            Some((sr, ss)) => (sr.as_str(), ss.as_str()),
            None => ("--:--", "--:--"),
        }
    }
}

impl Module for Clock {
    fn id(&self) -> &'static str {
        "clock"
    }
    fn title(&self) -> &'static str {
        "CLOCK"
    }
    fn describe(&self) -> &'static str {
        "World clocks, sun and moon, quest timer, big and analog dials"
    }
    fn manual(&self) -> &'static str {
        "\
CLOCK is the timekeeping panel: your local time, world clocks
from [clock], sun and moon, and a quest timer.

  v     cycle the views: normal, big digital, analog dial
  enter start or pause the quest timer
  x     reset it
  [ ]   give or take 5 minutes
  1-9   jump to a tab

When the timer runs out the header blinks and a radiation
alarm goes off; any key on this tab quiets it. Sunrise and
sunset are borrowed from WEATHER, so they follow the same
coordinates - no second place to configure, no excuses."
    }
    fn help(&self) -> &'static str {
        "v view   enter start/pause   x reset   [ ] ±5 min   1-9 tabs   q quit"
    }

    fn start(&mut self, ctx: &Ctx) {
        let (cfg, notice) = ctx.config.section::<ClockCfg>(self.id());
        if let Some(n) = notice {
            let _ = ctx.notify.send(Notice::Footer(n));
        }
        self.view = cfg.view;
        let (state, zone_notice) = ClockState::new(&cfg);
        self.state = state;
        if let Some(n) = zone_notice {
            let _ = ctx.notify.send(Notice::Footer(n));
        }
    }

    /// Nincs saját forrás: csak a WEATHER pillanatkép napkelte/napnyugta adata.
    /// A teljes pillanatképet csak akkor másoljuk le, ha a `weather.at`
    /// változás-kulcs eltér a gyorsítótárazottól (nem képkockánként).
    fn poll(&mut self, ctx: &Ctx) -> usize {
        let at = ctx.board.get::<DateTime<Local>>("weather.at");
        if at.is_none() || at == self.sun_at {
            return 0;
        }
        if let Some(w) = ctx.board.get::<WeatherSnapshot>("weather") {
            self.sun = Some((w.sunrise, w.sunset));
            self.sun_at = at;
        }
        0
    }

    fn tick(&mut self, ctx: &Ctx) {
        if self.state.timer.tick(Instant::now()) {
            let _ = ctx.notify.send(Notice::Activate(self.id()));
            let _ = ctx.notify.send(Notice::Alert(Some(ALERT)));
            ctx.audio.alarm();
        }
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &Ctx) -> bool {
        // Lejárat után bármely billentyű elnémítja a riasztást (a fül ilyenkor aktív).
        if self.state.timer.fired() {
            self.state.timer.dismiss();
            let _ = ctx.notify.send(Notice::Alert(None));
            ctx.audio.stop_alarm();
            return true;
        }
        match key.code {
            KeyCode::Char('v') => self.view = self.view.next(),
            KeyCode::Enter => self.state.timer.toggle(Instant::now()),
            KeyCode::Char('x') => self.state.timer.reset(),
            KeyCode::Char('[') => self.state.timer.adjust(-5),
            KeyCode::Char(']') => self.state.timer.adjust(5),
            _ => return false,
        }
        true
    }

    fn draw(&self, f: &mut Frame, area: Rect, t: Theme) {
        crate::ui::clock::draw(f, self, area, t);
    }

    fn overview(&self, _width: u16, height: u16, t: Theme) -> Vec<Line<'static>> {
        let local = Local::now();
        let compact = height < 22;
        let tm = &self.state.timer;
        let mut ck: Vec<Line> = vec![Line::from(Span::styled(" CLOCK", t.title))];
        let (sr, ss) = self.sun_times();
        let moon_line = || {
            let m = moon_phase(local.date_naive());
            Line::from(vec![
                Span::styled("  moon ", t.frame),
                Span::styled(format!("{} {}% {}", m.glyph, m.pct, m.name), t.value),
                Span::styled(format!(" · sunrise {sr} · sunset {ss}"), t.frame),
            ])
        };
        if compact {
            ck.push(Line::from(vec![
                Span::styled(format!("  {}", local.format("%H:%M:%S")), t.value),
                Span::styled(
                    format!(
                        " · {} · {} · week {}",
                        local.format("%A"),
                        local.format("%Y-%m-%d"),
                        local.iso_week().week()
                    ),
                    t.frame,
                ),
            ]));
            match tm.state {
                TimerState::Running { .. } | TimerState::Paused => {
                    let mmss = format!("{:02}:{:02}", tm.remaining.as_secs() / 60, tm.remaining.as_secs() % 60);
                    let line = if matches!(tm.state, TimerState::Paused) {
                        format!("  TIMER paused {mmss}")
                    } else {
                        format!("  TIMER {mmss} remaining")
                    };
                    ck.push(Line::from(Span::styled(line, t.warn)));
                }
                _ => ck.push(moon_line()),
            }
            return ck;
        }
        ck.extend(
            big_digits(&local.format("%H:%M:%S").to_string())
                .iter()
                .map(|r| Line::from(Span::styled(format!("  {r}"), t.value))),
        );
        ck.push(Line::from(Span::styled(
            format!("  {} · {} · week {}", local.format("%A"), local.format("%Y-%m-%d"), local.iso_week().week()),
            t.frame,
        )));
        match tm.state {
            TimerState::Running { .. } | TimerState::Paused => {
                let paused = matches!(tm.state, TimerState::Paused);
                let label = if paused { "QUEST TIMER · paused" } else { "QUEST TIMER" };
                ck.push(Line::from(Span::styled(format!("  {label}"), if paused { t.warn } else { t.title })));
                let mmss = format!("{:02}:{:02}", tm.remaining.as_secs() / 60, tm.remaining.as_secs() % 60);
                ck.extend(big_digits(&mmss).iter().map(|r| Line::from(Span::styled(format!("  {r}"), t.warn))));
            }
            _ => ck.push(moon_line()),
        }
        ck
    }

    fn overview_slot(&self) -> Slot {
        Slot::Left(1)
    }

    /// A lejárat animációja bármelyik fülön fut.
    fn wants_fast_frames(&self, _active: bool) -> bool {
        self.state.timer.fired()
    }

    fn status(&self) -> String {
        format!(
            "timer {:?} {:02}:{:02} zones={} sun={:?}",
            self.state.timer.state,
            self.state.timer.remaining.as_secs() / 60,
            self.state.timer.remaining.as_secs() % 60,
            self.state.zones.len(),
            self.sun
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::ModuleConfig;
    use crate::shell::test_ctx;
    use ratatui::crossterm::event::KeyModifiers;
    use std::time::Duration;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn config_defaults_and_section() {
        let (cfg, notice) = ModuleConfig(toml::Table::new()).section::<ClockCfg>("clock");
        assert_eq!(cfg.zones, vec!["America/New_York", "Asia/Tokyo"]);
        assert_eq!(cfg.timer_minutes, 25);
        assert!(notice.is_none());
        let table: toml::Table = "[clock]\ntimer_minutes = 10\n".parse().unwrap();
        let (cfg, _) = ModuleConfig(table).section::<ClockCfg>("clock");
        assert_eq!(cfg.timer_minutes, 10);
        assert_eq!(cfg.zones.len(), 2, "a hiányzó kulcs alapértéket kap");
    }

    #[test]
    fn expiry_activates_the_tab_alerts_and_sounds() {
        let (ctx, notices) = test_ctx(toml::Table::new());
        let mut m = Clock::new();
        m.state.timer = Timer::new(1);
        let t0 = Instant::now();
        m.state.timer.state =
            TimerState::Running { started: t0 - Duration::from_secs(61), at_start: Duration::from_secs(60) };
        m.tick(&ctx);
        assert_eq!(notices.try_recv().unwrap(), Notice::Activate("clock"));
        assert_eq!(notices.try_recv().unwrap(), Notice::Alert(Some(ALERT)));
        assert!(m.state.timer.fired());
        assert!(m.wants_fast_frames(false), "az animáció bármelyik fülön fut");
        m.tick(&ctx);
        assert!(notices.try_recv().is_err(), "pontosan egyszer jelez");
    }

    #[test]
    fn any_key_after_expiry_dismisses_and_stops_the_alarm() {
        let (ctx, notices) = test_ctx(toml::Table::new());
        let mut m = Clock::new();
        m.state.timer.state = TimerState::Fired { since: Instant::now() };
        assert!(m.on_key(key(KeyCode::Char('q')), &ctx), "a lejárt időzítő megeszi a billentyűt");
        assert_eq!(notices.try_recv().unwrap(), Notice::Alert(None));
        assert!(matches!(m.state.timer.state, TimerState::Idle));
        assert!(!m.on_key(key(KeyCode::Char('q')), &ctx), "utána már a héjhoz jut");
    }

    #[test]
    fn v_cycles_the_three_views_and_comes_back() {
        let (ctx, _n) = test_ctx(toml::Table::new());
        let mut m = Clock::new();
        assert_eq!(m.view, ClockView::Normal);
        for want in [ClockView::Digital, ClockView::Analog, ClockView::Normal] {
            assert!(m.on_key(key(KeyCode::Char('v')), &ctx));
            assert_eq!(m.view, want);
        }
        // A configbol indulo nezet, es az idozito minden nezetben mukodik.
        let table: toml::Table = "[clock]
view = \"analog\"
".parse().unwrap();
        let (cfg, notice) = ModuleConfig(table).section::<ClockCfg>("clock");
        assert_eq!(cfg.view, ClockView::Analog);
        assert!(notice.is_none());
        m.view = ClockView::Digital;
        assert!(m.on_key(key(KeyCode::Enter), &ctx));
        assert!(matches!(m.state.timer.state, TimerState::Running { .. }));
        assert_eq!(m.view, ClockView::Digital, "az idozito nem valt nezetet");
    }

    #[test]
    fn timer_keys_and_sun_from_the_blackboard() {
        let (ctx, _n) = test_ctx(toml::Table::new());
        let mut m = Clock::new();
        assert!(m.on_key(key(KeyCode::Char(']')), &ctx));
        assert_eq!(m.state.timer.length.as_secs(), 30 * 60);
        assert!(m.on_key(key(KeyCode::Enter), &ctx));
        assert!(matches!(m.state.timer.state, TimerState::Running { .. }));
        assert!(m.on_key(key(KeyCode::Char('x')), &ctx));
        assert!(matches!(m.state.timer.state, TimerState::Idle));

        assert_eq!(m.sun_times(), ("--:--", "--:--"));
        let at = chrono::Local::now();
        ctx.board.publish("weather.at", at);
        ctx.board.publish(
            "weather",
            WeatherSnapshot {
                place: "X".into(),
                fetched_at: at,
                temp: 0.0,
                feels: 0.0,
                humidity: 0,
                wind_kmh: 0.0,
                wind_dir: 0,
                pressure: 0.0,
                code: 0,
                uv: None,
                is_day: true,
                clouds: 0,
                precip_mm: 0.0,
                sunrise: "06:12".into(),
                sunset: "19:04".into(),
                hourly: vec![],
                daily: vec![],
                air: None,
            },
        );
        m.poll(&ctx);
        assert_eq!(m.sun_times(), ("06:12", "19:04"));

        // Változatlan kulcs → nincs újraolvasás (a kézzel átírt gyorsítótár megmarad).
        m.sun = Some(("XX:XX".into(), "YY:YY".into()));
        m.poll(&ctx);
        assert_eq!(m.sun_times(), ("XX:XX", "YY:YY"), "azonos fetched_at → nincs másolás");
        ctx.board.publish("weather.at", at + chrono::Duration::seconds(1));
        m.poll(&ctx);
        assert_eq!(m.sun_times(), ("06:12", "19:04"), "új kulcs → újraolvasás");
    }
}
