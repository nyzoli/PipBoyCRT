//! GLOBE module: braille world map with the day/night terminator, your
//! configured location, the subsolar point and the live ISS position.
//!
//! Land data: `crate::ui::landmask` (Natural Earth, public domain).
//! ISS data: <https://api.wheretheiss.at/> (no API key).

use crate::module::{Ctx, Module, Notice, Slot};
use crate::style::Theme;
use crate::ui::landmask;
use crate::ui::widgets::truncate;
use chrono::{Datelike, Timelike, Utc};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Points};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde::Deserialize;
use std::cell::RefCell;
use std::sync::mpsc::{self, Receiver, Sender as StdSender};
use std::time::Duration;
use stream_download::http::reqwest;

const ISS_URL: &str = "https://api.wheretheiss.at/v1/satellites/25544";
const REFRESH: u64 = 30;
const TRAIL: usize = 30;
/// Sun elevation still counted as daylight (refraction + solar radius).
const HORIZON: f64 = -0.833;

// ---------------------------------------------------------------- solar math

/// The point where the sun stands straight overhead (degrees).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Subsolar {
    pub lat: f64,
    pub lon: f64,
}

/// Subsolar point for a UTC instant: declination from the day-of-year
/// approximation, longitude from the hour angle corrected by the equation of
/// time. Good to about a degree, which is a fraction of a braille dot here.
pub fn subsolar(now: chrono::DateTime<Utc>) -> Subsolar {
    let hours = now.hour() as f64 + now.minute() as f64 / 60.0 + now.second() as f64 / 3600.0;
    let n = now.ordinal() as f64 + hours / 24.0;
    let decl = -23.44 * ((360.0 / 365.0) * (n + 10.0)).to_radians().cos();
    let b = ((360.0 / 365.0) * (n - 81.0)).to_radians();
    let eot = 9.87 * (2.0 * b).sin() - 7.53 * b.cos() - 1.5 * b.sin();
    Subsolar { lat: decl, lon: wrap_lon(-15.0 * (hours - 12.0 + eot / 60.0)) }
}

/// Sun elevation above the horizon at `lat`/`lon` in degrees (spherical law of
/// cosines).
pub fn sun_elevation(lat: f64, lon: f64, s: Subsolar) -> f64 {
    let (la, de) = (lat.to_radians(), s.lat.to_radians());
    let h = (lon - s.lon).to_radians();
    (la.sin() * de.sin() + la.cos() * de.cos() * h.cos()).clamp(-1.0, 1.0).asin().to_degrees()
}

/// Latitude where the terminator crosses `lon` (elevation 0). With the sun on
/// the equator `tan(decl)` is ~0 and the quotient saturates at ±90°, which is
/// exactly the near-vertical line that should be drawn there.
pub fn terminator_lat(lon: f64, s: Subsolar) -> f64 {
    (-(lon - s.lon).to_radians().cos() / s.lat.to_radians().tan()).atan().to_degrees()
}

fn wrap_lon(lon: f64) -> f64 {
    (lon + 180.0).rem_euclid(360.0) - 180.0
}

// ------------------------------------------------------------- dot ↔ lat/lon

/// Longitude of braille dot column `dx` out of `cols` (dot centre).
pub fn dot_lon(dx: usize, cols: usize) -> f64 {
    -180.0 + 360.0 * (dx as f64 + 0.5) / cols.max(1) as f64
}

/// Latitude of braille dot row `dy` out of `rows` (dot centre).
pub fn dot_lat(dy: usize, rows: usize) -> f64 {
    90.0 - 180.0 * (dy as f64 + 0.5) / rows.max(1) as f64
}

/// Inverse of [`dot_lon`] (round-trip test only).
#[cfg(test)]
pub fn lon_dot(lon: f64, cols: usize) -> usize {
    (((lon + 180.0) / 360.0 * cols.max(1) as f64) as usize).min(cols.saturating_sub(1))
}

/// Inverse of [`dot_lat`] (round-trip test only).
#[cfg(test)]
pub fn lat_dot(lat: f64, rows: usize) -> usize {
    (((90.0 - lat) / 180.0 * rows.max(1) as f64) as usize).min(rows.saturating_sub(1))
}

// ------------------------------------------------------------------ the data

#[derive(Clone, Copy, Debug)]
struct Iss {
    lat: f64,
    lon: f64,
    alt_km: f64,
    vel_kmh: f64,
}

#[derive(Deserialize)]
struct IssResp {
    latitude: f64,
    longitude: f64,
    altitude: f64,
    velocity: f64,
}

enum Event {
    Pos(Iss),
    Error(String),
}

/// The home marker follows WEATHER's `[weather]` section (config only — the
/// modules stay independent).
#[derive(Deserialize, Clone)]
#[serde(default)]
struct HomeCfg {
    name: String,
    lat: f64,
    lon: f64,
}

impl Default for HomeCfg {
    fn default() -> Self {
        Self { name: "Budapest".into(), lat: 47.4979, lon: 19.0402 }
    }
}

pub struct Globe {
    home: HomeCfg,
    iss: Option<Iss>,
    trail: Vec<(f64, f64)>,
    err: Option<String>,
    show_trail: bool,
    show_night: bool,
    /// Land dots for the last canvas size; recomputed only when it changes.
    dots: RefCell<((usize, usize), Vec<(f64, f64)>)>,
    rx: Option<Receiver<Event>>,
    tx_refresh: Option<tokio::sync::mpsc::Sender<()>>,
}

impl Default for Globe {
    fn default() -> Self {
        Self::new()
    }
}

impl Globe {
    pub fn new() -> Self {
        Self {
            home: HomeCfg::default(),
            iss: None,
            trail: Vec::new(),
            err: None,
            show_trail: true,
            show_night: true,
            dots: RefCell::new(((0, 0), Vec::new())),
            rx: None,
            tx_refresh: None,
        }
    }

    fn title_line(&self, width: u16, s: Subsolar, t: Theme) -> Line<'static> {
        let mut text = format!(
            " GLOBE · {} {} · sun over {}",
            self.home.name,
            fmt_ll(self.home.lat, self.home.lon),
            fmt_ll(s.lat, s.lon)
        );
        // A failed fetch must not leave a stale position on screen for hours.
        match (&self.err, self.iss) {
            (Some(_), _) => text.push_str(" · ISS n/a"),
            (None, Some(i)) => text.push_str(&format!(
                " · ISS {} · {:.0} km · {} km/h",
                fmt_ll(i.lat, i.lon),
                i.alt_km,
                thousands(i.vel_kmh)
            )),
            (None, None) => text.push_str(" · ISS n/a"),
        }
        Line::from(Span::styled(truncate(&text, width as usize), t.title))
    }
}

/// `47.5°N 19.0°E`
fn fmt_ll(lat: f64, lon: f64) -> String {
    let lon = wrap_lon(lon);
    format!(
        "{:.1}°{} {:.1}°{}",
        lat.abs(),
        if lat >= 0.0 { 'N' } else { 'S' },
        lon.abs(),
        if lon >= 0.0 { 'E' } else { 'W' }
    )
}

/// `27 600` — space-grouped thousands.
fn thousands(v: f64) -> String {
    let s = (v.round().max(0.0) as u64).to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

fn fg(s: Style) -> Color {
    s.fg.unwrap_or(Color::Reset)
}

impl Module for Globe {
    fn id(&self) -> &'static str {
        "globe"
    }
    fn title(&self) -> &'static str {
        "GLOBE"
    }
    fn describe(&self) -> &'static str {
        "World map with day/night, your location and the ISS"
    }
    fn help(&self) -> &'static str {
        "i ISS trail   n night shading   r refresh ISS   1-9 tabs   q quit"
    }

    fn start(&mut self, ctx: &Ctx) {
        let (home, notice) = ctx.config.section::<HomeCfg>("weather");
        if let Some(n) = notice {
            let _ = ctx.notify.send(Notice::Footer(n));
        }
        self.home = home;

        let (tx, rx) = mpsc::channel::<Event>();
        self.rx = Some(rx);
        self.tx_refresh = Some(spawn_iss(&ctx.rt, tx));
    }

    fn poll(&mut self, _ctx: &Ctx) -> usize {
        let mut n = 0;
        if let Some(rx) = &self.rx {
            while let Ok(ev) = rx.try_recv() {
                n += 1;
                match ev {
                    Event::Pos(p) => {
                        self.trail.push((p.lon, p.lat));
                        if self.trail.len() > TRAIL {
                            self.trail.remove(0);
                        }
                        self.iss = Some(p);
                        self.err = None;
                    }
                    Event::Error(m) => self.err = Some(m),
                }
            }
        }
        n
    }

    fn on_key(&mut self, key: KeyEvent, _ctx: &Ctx) -> bool {
        match key.code {
            KeyCode::Char('i') => {
                self.show_trail = !self.show_trail;
                true
            }
            KeyCode::Char('n') => {
                self.show_night = !self.show_night;
                true
            }
            KeyCode::Char('r') => {
                if let Some(tx) = &self.tx_refresh {
                    let _ = tx.try_send(());
                }
                true
            }
            _ => false,
        }
    }

    fn draw(&self, f: &mut Frame, area: Rect, t: Theme) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let s = subsolar(Utc::now());
        let [head, map] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
        f.render_widget(Paragraph::new(self.title_line(head.width, s, t)), head);
        if map.width == 0 || map.height == 0 {
            return;
        }

        let (cols, rows) = (map.width as usize * 2, map.height as usize * 4);
        let (mut day, mut night) = (Vec::new(), Vec::new());
        {
            let mut cache = self.dots.borrow_mut();
            if cache.0 != (cols, rows) {
                let mut v = Vec::new();
                for dy in 0..rows {
                    let lat = dot_lat(dy, rows);
                    for dx in 0..cols {
                        let lon = dot_lon(dx, cols);
                        if landmask::is_land(lat, lon) {
                            v.push((lon, lat));
                        }
                    }
                }
                *cache = ((cols, rows), v);
            }
            for &(lon, lat) in &cache.1 {
                if !self.show_night || sun_elevation(lat, lon, s) >= HORIZON {
                    day.push((lon, lat));
                } else {
                    night.push((lon, lat));
                }
            }
        }
        let term: Vec<(f64, f64)> = if self.show_night {
            (0..cols).map(|dx| dot_lon(dx, cols)).map(|lon| (lon, terminator_lat(lon, s))).collect()
        } else {
            Vec::new()
        };

        let (lit, dim, hi) = (fg(t.value), fg(t.frame), fg(t.title));
        let (home, iss, trail) = (self.home.clone(), self.iss, self.trail.clone());
        let show_trail = self.show_trail;
        let name_fits = map.width >= 60;

        let canvas = Canvas::default()
            .marker(Marker::Braille)
            .x_bounds([-180.0, 180.0])
            .y_bounds([-90.0, 90.0])
            .paint(move |ctx| {
                ctx.draw(&Points { coords: &night, color: dim });
                ctx.draw(&Points { coords: &day, color: lit });
                ctx.draw(&Points { coords: &term, color: hi });
                if show_trail && !trail.is_empty() {
                    ctx.draw(&Points { coords: &trail, color: dim });
                }
                ctx.layer();
                ctx.print(s.lon, s.lat, Line::from(Span::styled("☼", t.title)));
                let label =
                    if name_fits { format!("⌂ {}", home.name) } else { "⌂".to_string() };
                ctx.print(
                    home.lon,
                    home.lat,
                    Line::from(Span::styled(label, t.warn.add_modifier(Modifier::BOLD))),
                );
                if let Some(i) = iss {
                    ctx.print(i.lon, i.lat, Line::from(Span::styled("✦", t.danger)));
                }
            });
        f.render_widget(canvas, map);
    }

    fn overview(&self, _w: u16, _h: u16, t: Theme) -> Vec<Line<'static>> {
        let s = subsolar(Utc::now());
        let iss = match self.iss {
            Some(i) => fmt_ll(i.lat, i.lon),
            None => "n/a".to_string(),
        };
        vec![
            Line::from(Span::styled(" GLOBE", t.title)),
            Line::from(format!(" sun {} · ISS {iss}", fmt_ll(s.lat, s.lon))),
        ]
    }

    fn overview_slot(&self) -> Slot {
        Slot::Right(8)
    }

    fn status(&self) -> String {
        let s = subsolar(Utc::now());
        let iss = match self.iss {
            Some(i) => format!("{:.1},{:.1}", i.lat, i.lon),
            None => "n/a".to_string(),
        };
        format!("globe sun={:.1},{:.1} iss={iss}", s.lat, s.lon)
    }
}

/// Polls the ISS position every 30 s; a message on the returned channel forces
/// an immediate fetch.
fn spawn_iss(rt: &tokio::runtime::Handle, tx: StdSender<Event>) -> tokio::sync::mpsc::Sender<()> {
    let (refresh_tx, mut refresh_rx) = tokio::sync::mpsc::channel::<()>(1);
    rt.spawn(async move {
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent("PipBoyCRT")
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(Event::Error(e.to_string()));
                return;
            }
        };
        loop {
            let ev = match fetch_iss(&client).await {
                Ok(p) => Event::Pos(p),
                Err(e) => Event::Error(e),
            };
            if tx.send(ev).is_err() {
                return;
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(REFRESH)) => {}
                r = refresh_rx.recv() => {
                    if r.is_none() {
                        return;
                    }
                }
            }
        }
    });
    refresh_tx
}

async fn fetch_iss(client: &reqwest::Client) -> Result<Iss, String> {
    let body = client
        .get(ISS_URL)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    let r: IssResp = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    Ok(Iss { lat: r.latitude, lon: r.longitude, alt_km: r.altitude, vel_kmh: r.velocity })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn utc(y: i32, m: u32, d: u32, h: u32) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    #[test]
    fn landmask_spot_checks() {
        assert!(landmask::is_land(47.5, 19.0), "Budapest");
        assert!(!landmask::is_land(0.0, -30.0), "mid-Atlantic");
        assert!(landmask::is_land(23.0, 10.0), "Sahara");
        assert!(!landmask::is_land(0.0, -150.0), "Pacific");
        // Degenerate input must not panic or index out of bounds.
        assert!(!landmask::is_land(90.0, 180.0));
        assert!(!landmask::is_land(f64::NAN, f64::INFINITY));
        let _ = landmask::is_land(-90.0, -180.0);
        assert_eq!(landmask::LAND.len(), landmask::W * landmask::H / 8);
    }

    #[test]
    fn subsolar_declination_follows_the_season() {
        assert!(subsolar(utc(2026, 3, 20, 12)).lat.abs() < 1.0, "equinox ≈ 0");
        assert!((subsolar(utc(2026, 6, 21, 12)).lat - 23.4).abs() < 1.0, "June ≈ +23.4");
        assert!((subsolar(utc(2026, 12, 21, 12)).lat + 23.4).abs() < 1.0, "December ≈ -23.4");
    }

    #[test]
    fn subsolar_longitude_tracks_utc() {
        assert!(subsolar(utc(2026, 3, 20, 12)).lon.abs() < 4.0);
        // Six hours later the sun stands ~90° further west.
        let s = subsolar(utc(2026, 3, 20, 18));
        assert!((s.lon + 90.0).abs() < 4.0, "got {}", s.lon);
        assert!((-180.0..=180.0).contains(&subsolar(utc(2026, 3, 20, 0)).lon));
    }

    #[test]
    fn elevation_peaks_under_the_sun_and_is_negative_on_the_far_side() {
        let s = Subsolar { lat: 0.0, lon: 0.0 };
        assert!((sun_elevation(0.0, 0.0, s) - 90.0).abs() < 1e-6);
        assert!(sun_elevation(0.0, 180.0, s) < 0.0);
        assert!(sun_elevation(10.0, 150.0, s) < 0.0);
        assert!(sun_elevation(-80.0, 175.0, s) < 0.0);
    }

    #[test]
    fn terminator_crosses_the_equator_a_quarter_turn_from_the_sun() {
        let s = Subsolar { lat: 15.0, lon: 30.0 };
        assert!(terminator_lat(120.0, s).abs() < 1e-9);
        assert!(terminator_lat(-60.0, s).abs() < 1e-9);
        // A point on the terminator has zero elevation.
        let lat = terminator_lat(95.0, s);
        assert!(sun_elevation(lat, 95.0, s).abs() < 1e-6);
        // Zero declination must not panic; it saturates at the pole.
        assert!(terminator_lat(0.0, Subsolar { lat: 0.0, lon: 90.0 }).abs() <= 90.0);
    }

    #[test]
    fn dot_and_coordinate_round_trip() {
        let (cols, rows) = (240, 160);
        for dx in [0, 1, 120, 239] {
            assert_eq!(lon_dot(dot_lon(dx, cols), cols), dx);
        }
        for dy in [0, 1, 80, 159] {
            assert_eq!(lat_dot(dot_lat(dy, rows), rows), dy);
        }
        assert!((dot_lon(0, cols) + 180.0).abs() < 1.0);
        assert!((dot_lat(0, rows) - 90.0).abs() < 1.0);
        // A 1×1 canvas must not divide by zero.
        assert!(dot_lon(0, 0).is_finite());
        assert!(dot_lat(0, 0).is_finite());
        assert_eq!(lon_dot(0.0, 0), 0);
        assert_eq!(lat_dot(0.0, 0), 0);
    }

    #[test]
    fn formatting() {
        assert_eq!(fmt_ll(47.5, 19.0), "47.5°N 19.0°E");
        assert_eq!(fmt_ll(-3.25, -12.4), "3.2°S 12.4°W");
        assert_eq!(thousands(27_600.4), "27 600");
        assert_eq!(thousands(7.0), "7");
        assert_eq!(thousands(-5.0), "0");
    }

    #[test]
    fn draw_does_not_panic_and_shows_the_title() {
        let t = Theme::new(crate::config::ThemeKind::Color);
        let mut g = Globe::new();
        for (w, h) in [(1, 1), (40, 12), (120, 40), (2, 1)] {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| g.draw(f, f.area(), t)).unwrap();
        }
        g.iss = Some(Iss { lat: 51.2, lon: 10.4, alt_km: 420.0, vel_kmh: 27_600.0 });
        g.trail = vec![(1.0, 2.0), (3.0, 4.0)];
        g.show_night = false;
        g.show_trail = false;
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| g.draw(f, f.area(), t)).unwrap();
        g.show_night = true;
        g.show_trail = true;
        term.draw(|f| g.draw(f, f.area(), t)).unwrap();
        let text: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("GLOBE"), "title missing");
        assert!(g.status().starts_with("globe sun="));
        assert_eq!(g.overview(30, 2, t).len(), 2);
    }
}
