//! WEATHER modul: Open-Meteo pillanatkép, órás és napi előrejelzés.
use crate::module::{Ctx, Module, Notice, Slot};
use crate::style::Theme;
use crate::ui::widgets::wmo;
use crate::weather::{self, WeatherSnapshot};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::Frame;
use serde::Deserialize;
use std::sync::mpsc::{self, Receiver};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WeatherCfg {
    pub name: String,
    pub lat: f64,
    pub lon: f64,
}

impl Default for WeatherCfg {
    fn default() -> Self {
        Self { name: "Budapest".into(), lat: 47.4979, lon: 19.0402 }
    }
}

#[derive(Default)]
pub struct Weather {
    pub snap: Option<WeatherSnapshot>,
    pub err: Option<String>,
    rx: Option<Receiver<Result<WeatherSnapshot, String>>>,
    refresh: Option<tokio::sync::mpsc::Sender<()>>,
}

impl Weather {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Module for Weather {
    fn id(&self) -> &'static str {
        "weather"
    }
    fn title(&self) -> &'static str {
        "WEATHER"
    }
    fn describe(&self) -> &'static str {
        "Open-Meteo forecast, air quality and pollen for your location"
    }
    fn help(&self) -> &'static str {
        "←/→ tab   r refresh   space play/pause   +/- volume   m mute   q quit"
    }

    fn start(&mut self, ctx: &Ctx) {
        let (cfg, notice) = ctx.config.section::<WeatherCfg>(self.id());
        if let Some(n) = notice {
            let _ = ctx.notify.send(Notice::Footer(n));
        }
        let (tx, rx) = mpsc::channel();
        self.refresh = Some(weather::spawn(&ctx.rt, cfg, tx));
        self.rx = Some(rx);
    }

    fn poll(&mut self, ctx: &Ctx) -> usize {
        let mut n = 0;
        let Some(rx) = &self.rx else { return 0 };
        while let Ok(res) = rx.try_recv() {
            n += 1;
            match res {
                Ok(w) => {
                    // Olcsó változás-kulcs a fogyasztóknak (lásd clock.rs): így nem
                    // kell képkockánként lemásolniuk a teljes pillanatképet.
                    ctx.board.publish("weather.at", w.fetched_at);
                    ctx.board.publish(self.id(), w.clone());
                    self.snap = Some(w);
                    self.err = None;
                }
                Err(e) => self.err = Some(e),
            }
        }
        n
    }

    fn on_key(&mut self, key: KeyEvent, _ctx: &Ctx) -> bool {
        if key.code != KeyCode::Char('r') {
            return false;
        }
        if let Some(tx) = &self.refresh {
            let _ = tx.try_send(());
        }
        true
    }

    fn draw(&self, f: &mut Frame, area: Rect, t: Theme) {
        crate::ui::weather::draw(f, self, area, t);
    }

    fn overview(&self, width: u16, _height: u16, t: Theme) -> Vec<Line<'static>> {
        let mut we = vec![Line::from(Span::styled(" WEATHER", t.title))];
        let Some(w) = &self.snap else {
            we.push(Line::from(Span::styled(
                format!("   {}", self.err.as_deref().unwrap_or("fetching…")),
                t.frame,
            )));
            return we;
        };
        let (icon, _, text) = wmo(w.code);
        we.push(Line::from(vec![
            Span::styled(format!("  {}", icon[0]), t.graph),
            Span::styled(format!("  {:.0}°C ", w.temp), t.value),
            Span::raw(text),
        ]));
        we.push(Line::from(vec![
            Span::styled(format!("  {}", icon[1]), t.graph),
            Span::raw(format!(
                "  feels {:.0}°  ↑{:.0}°  ↓{:.0}°",
                w.feels,
                w.daily.first().map(|d| d.tmax).unwrap_or(0.0),
                w.daily.first().map(|d| d.tmin).unwrap_or(0.0)
            )),
        ]));
        if let Some(a) = &w.air {
            we.push(Line::from(vec![
                Span::styled(format!("  {}", icon[2]), t.graph),
                Span::styled(
                    format!(
                        "  {}",
                        crate::ui::widgets::truncate(
                            &crate::ui::weather::air_summary(a),
                            (width as usize).saturating_sub(4)
                        )
                    ),
                    t.frame,
                ),
            ]));
        }
        let days: Vec<String> = w
            .daily
            .iter()
            .skip(1)
            .take(3)
            .map(|d| format!("{} {} {:.0}/{:.0}", d.date.format("%a"), wmo(d.code).1, d.tmax, d.tmin))
            .collect();
        we.push(Line::from(vec![
            Span::styled(format!("  {}", icon[2]), t.graph),
            Span::styled(format!("  {}", days.join("  ")), t.frame),
        ]));
        we
    }

    fn overview_slot(&self) -> Slot {
        Slot::Right(0)
    }

    fn status(&self) -> String {
        match (&self.snap, &self.err) {
            (Some(w), e) => format!(
                "{} {:.1}°C code={} err={}",
                w.place,
                w.temp,
                w.code,
                e.as_deref().unwrap_or("-")
            ),
            (None, Some(e)) => format!("offline: {e}"),
            (None, None) => "fetching…".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::test_ctx;

    fn snapshot(temp: f32) -> WeatherSnapshot {
        WeatherSnapshot {
            place: "X".into(),
            fetched_at: chrono::Local::now(),
            temp,
            feels: temp,
            humidity: 0,
            wind_kmh: 0.0,
            wind_dir: 0,
            pressure: 0.0,
            code: 0,
            uv: None,
            is_day: true,
            clouds: 0,
            precip_mm: 0.0,
            sunrise: "06:00".into(),
            sunset: "18:00".into(),
            hourly: vec![],
            daily: vec![],
            air: None,
        }
    }

    #[test]
    fn error_keeps_the_old_snapshot_and_publishes_to_the_board() {
        let (ctx, _rx) = test_ctx(toml::Table::new());
        let (tx, rx) = mpsc::channel();
        let mut m = Weather::new();
        m.rx = Some(rx);
        tx.send(Ok(snapshot(21.0))).unwrap();
        tx.send(Err("offline".into())).unwrap();
        assert_eq!(m.poll(&ctx), 2);
        assert_eq!(m.snap.as_ref().unwrap().temp, 21.0);
        assert_eq!(m.err.as_deref(), Some("offline"));
        assert_eq!(ctx.board.get::<WeatherSnapshot>("weather").unwrap().sunrise, "06:00");
        tx.send(Ok(snapshot(22.0))).unwrap();
        m.poll(&ctx);
        assert!(m.err.is_none());
        assert_eq!(m.snap.as_ref().unwrap().temp, 22.0);
    }

    #[test]
    fn overview_shows_the_air_line_when_present() {
        let t = Theme::new(crate::config::ThemeKind::Color);
        let mut m = Weather::new();
        let mut w = snapshot(20.0);
        assert!(!lines_text(&m.overview(40, 10, t)).contains("AIR"));
        w.air = Some(crate::weather::AirSnapshot {
            aqi: 32,
            pollen: vec![("grass", 45.0)],
            ..Default::default()
        });
        m.snap = Some(w);
        let txt = lines_text(&m.overview(40, 10, t));
        assert!(txt.contains("AIR fair"), "{txt}");
        assert!(txt.contains("pollen moderate (grass)"), "{txt}");
    }

    fn lines_text(ls: &[Line<'static>]) -> String {
        ls.iter().flat_map(|l| l.spans.iter()).map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn config_section_defaults_to_budapest() {
        let table: toml::Table = "[weather]\nlat = 1.5\n".parse().unwrap();
        let (cfg, notice) = crate::module::ModuleConfig(table).section::<WeatherCfg>("weather");
        assert_eq!(cfg.name, "Budapest");
        assert_eq!(cfg.lat, 1.5);
        assert!(notice.is_none());
    }
}
