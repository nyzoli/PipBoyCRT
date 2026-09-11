//! CLOCK logika: időzítő állapotgép, holdfázis, időzónák.
use crate::modules::clock::ClockCfg;
use chrono::{NaiveDate, NaiveDateTime};
use chrono_tz::Tz;
use std::time::{Duration, Instant};

/// A CLOCK fül nézetei; a `v` billentyű léptet köztük.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClockView {
    /// A megszokott fül: nagy óra, világidő, nap/hold, időzítő.
    #[default]
    Normal,
    /// Teljes képernyős, árnyékolt blokk-font.
    Digital,
    /// Teljes képernyős analóg számlap.
    Analog,
}

impl ClockView {
    pub fn next(self) -> Self {
        match self {
            Self::Normal => Self::Digital,
            Self::Digital => Self::Analog,
            Self::Analog => Self::Normal,
        }
    }
}

const MIN_MINUTES: u64 = 5;
const MAX_MINUTES: u64 = 120;
pub const ANIM_FPS: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TimerState {
    Idle,
    Running { started: Instant, at_start: Duration },
    Paused,
    Fired { since: Instant },
}

#[derive(Debug, Clone)]
pub struct Timer {
    pub length: Duration,
    pub remaining: Duration,
    pub state: TimerState,
}

impl Timer {
    pub fn new(minutes: u32) -> Self {
        // A configból 1 perc is lehet (teszthez is); a [ ] léptetés 5–120 között mozog.
        let len = Duration::from_secs((minutes as u64).clamp(1, MAX_MINUTES) * 60);
        Self { length: len, remaining: len, state: TimerState::Idle }
    }

    /// Enter: Idle/Paused → Running, Running → Paused. Fired állapotban nem csinál semmit.
    pub fn toggle(&mut self, now: Instant) {
        self.state = match self.state {
            TimerState::Idle | TimerState::Paused => TimerState::Running { started: now, at_start: self.remaining },
            TimerState::Running { .. } => { self.tick(now); TimerState::Paused }
            TimerState::Fired { .. } => return,
        };
    }

    /// x: vissza a beállított hosszra, Idle.
    pub fn reset(&mut self) {
        self.remaining = self.length;
        self.state = TimerState::Idle;
    }

    /// [ / ]: ±perc, csak Idle-ben, 5–120 perc között.
    pub fn adjust(&mut self, minutes: i64) {
        if self.state != TimerState::Idle { return; }
        let cur = (self.length.as_secs() / 60) as i64;
        let new = (cur + minutes).clamp(MIN_MINUTES as i64, MAX_MINUTES as i64) as u64;
        self.length = Duration::from_secs(new * 60);
        self.remaining = self.length;
    }

    /// Frissíti a hátralévő időt; `true` pontosan egyszer, a lejárat pillanatában.
    pub fn tick(&mut self, now: Instant) -> bool {
        if let TimerState::Running { started, at_start } = self.state {
            let elapsed = now.saturating_duration_since(started);
            self.remaining = at_start.saturating_sub(elapsed);
            if self.remaining.is_zero() {
                self.state = TimerState::Fired { since: now };
                return true;
            }
        }
        false
    }

    /// Bármely billentyű lejárat után: Idle, a hossz marad.
    pub fn dismiss(&mut self) { self.reset(); }

    /// Animációs képkocka a lejárat óta (10 kép/s), egyébként 0.
    pub fn frame(&self, now: Instant) -> u32 {
        match self.state {
            TimerState::Fired { since } => (now.saturating_duration_since(since).as_millis() as u64 * ANIM_FPS / 1000) as u32,
            _ => 0,
        }
    }

    pub fn fired(&self) -> bool { matches!(self.state, TimerState::Fired { .. }) }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MoonPhase { pub pct: u8, pub glyph: &'static str, pub name: &'static str, pub waxing: bool }

/// Holdfázis a szinodikus hónapból; referencia újhold: 2000-01-06 18:14 UTC.
pub fn moon_phase(date: NaiveDate) -> MoonPhase {
    const SYNODIC: f64 = 29.530588;
    let reference = NaiveDate::from_ymd_opt(2000, 1, 6).unwrap().and_hms_opt(18, 14, 0).unwrap();
    let noon: NaiveDateTime = date.and_hms_opt(12, 0, 0).unwrap();
    let days = (noon - reference).num_seconds() as f64 / 86400.0;
    let p = (days / SYNODIC).rem_euclid(1.0);
    let pct = ((1.0 - (2.0 * std::f64::consts::PI * p).cos()) / 2.0 * 100.0).round() as u8;
    let (glyph, name) = match p {
        x if x < 0.0625 => ("○", "new moon"),
        x if x < 0.1875 => ("◔", "waxing crescent"),
        x if x < 0.3125 => ("◑", "first quarter"),
        x if x < 0.4375 => ("◕", "waxing gibbous"),
        x if x < 0.5625 => ("●", "full moon"),
        x if x < 0.6875 => ("◕", "waning gibbous"),
        x if x < 0.8125 => ("◐", "last quarter"),
        x if x < 0.9375 => ("◔", "waning crescent"),
        _ => ("○", "new moon"),
    };
    MoonPhase { pct, glyph, name, waxing: p < 0.5 }
}

pub struct ClockState {
    /// (rövid név a városból, zóna)
    pub zones: Vec<(String, Tz)>,
    pub timer: Timer,
}

impl ClockState {
    /// A hibás zónanevek kimaradnak; a második érték a láblécnek szánt figyelmeztetés.
    pub fn new(cfg: &ClockCfg) -> (Self, Option<String>) {
        let mut zones = Vec::new();
        let mut bad = Vec::new();
        for z in &cfg.zones {
            match z.parse::<Tz>() {
                Ok(tz) => zones.push((z.rsplit('/').next().unwrap_or(z).replace('_', " "), tz)),
                Err(_) => bad.push(z.clone()),
            }
        }
        let notice = (!bad.is_empty()).then(|| format!("unknown time zone: {}", bad.join(", ")));
        (Self { zones, timer: Timer::new(cfg.timer_minutes) }, notice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate { NaiveDate::from_ymd_opt(y, m, day).unwrap() }

    #[test]
    fn moon_phase_known_dates() {
        assert!(moon_phase(d(2026, 1, 18)).pct <= 5, "new moon");
        assert!(moon_phase(d(2026, 2, 1)).pct >= 95, "full moon");
        assert_eq!(moon_phase(d(2026, 2, 1)).name, "full moon");
        assert!(moon_phase(d(2026, 1, 25)).waxing);
        assert!(!moon_phase(d(2026, 2, 8)).waxing);
    }

    #[test]
    fn timer_state_machine() {
        let t0 = Instant::now();
        let mut t = Timer::new(1);
        assert_eq!(t.remaining, Duration::from_secs(60));
        t.toggle(t0);
        assert!(matches!(t.state, TimerState::Running { .. }));
        assert!(!t.tick(t0 + Duration::from_secs(30)));
        assert_eq!(t.remaining, Duration::from_secs(30));
        t.toggle(t0 + Duration::from_secs(30));
        assert!(matches!(t.state, TimerState::Paused));
        assert!(!t.tick(t0 + Duration::from_secs(100)));
        assert_eq!(t.remaining, Duration::from_secs(30));
        t.toggle(t0 + Duration::from_secs(100));
        assert!(t.tick(t0 + Duration::from_secs(131)), "fires exactly once");
        assert!(matches!(t.state, TimerState::Fired { .. }));
        assert!(!t.tick(t0 + Duration::from_secs(140)));
        assert_eq!(t.frame(t0 + Duration::from_secs(131) + Duration::from_millis(1250)), 12);
        t.dismiss();
        assert!(matches!(t.state, TimerState::Idle));
        assert_eq!(t.remaining, Duration::from_secs(60));
    }

    #[test]
    fn timer_adjust_only_when_idle_and_bounded() {
        let mut t = Timer::new(5);
        t.adjust(-5);
        assert_eq!(t.length, Duration::from_secs(5 * 60), "lower bound 5 min");
        for _ in 0..40 { t.adjust(5); }
        assert_eq!(t.length, Duration::from_secs(120 * 60), "upper bound 120 min");
        t.toggle(Instant::now());
        t.adjust(-5);
        assert_eq!(t.length, Duration::from_secs(120 * 60), "no change while running");
    }

    #[test]
    fn zones_parse_and_skip_invalid() {
        let cfg = ClockCfg { zones: vec!["Asia/Tokyo".into(), "Mars/Olympus".into()], ..ClockCfg::default() };
        let (s, notice) = ClockState::new(&cfg);
        assert_eq!(s.zones.len(), 1);
        assert_eq!(s.zones[0].0, "Tokyo");
        assert!(notice.unwrap().contains("Mars/Olympus"));
    }
}
