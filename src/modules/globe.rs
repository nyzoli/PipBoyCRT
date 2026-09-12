//! GLOBE module: braille world map with the day/night terminator, your
//! configured location, the subsolar point and the live ISS position.
//!
//! Land data: `crate::ui::landmask` (Natural Earth, public domain).
//! ISS data: <https://api.wheretheiss.at/> (no API key).
//!
//! Arcs: WASTELAND publishes the public remote addresses of this machine's
//! open connections on the blackboard ([`CONNECTIONS`]); the country of each
//! comes from <https://get.geojs.io/> (HTTPS, no key, batched), is cached in
//! `geo.json` next to the executable for 30 days, and lands on the country's
//! centroid (`crate::ui::countries`). Those remote addresses are the only
//! thing that leaves the machine for this; the home country is the centroid
//! nearest to `[weather]`, found offline. `[globe] arcs = false` keeps every
//! address here: no lookup, no arcs.

use crate::module::{ConnSnapshot, Ctx, Module, Notice, RemoteConn, Slot, CONNECTIONS, CONNECTIONS_AT};
use crate::style::Theme;
use crate::ui::countries;
use crate::ui::landmask;
use crate::ui::widgets::truncate;
use chrono::{Datelike, Timelike, Utc};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Line as CLine, Points};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender as StdSender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use stream_download::http::reqwest;

const ISS_URL: &str = "https://api.wheretheiss.at/v1/satellites/25544";
const REFRESH: u64 = 30;
const TRAIL: usize = 30;
/// geojs country lookup, batched: `?ip=a,b,c`; without `ip` it answers for
/// the caller — that is how the home country is found.
const GEO_URL: &str = "https://get.geojs.io/v1/ip/country.json";
/// A centroid further than this from `[weather]` is not "home": no country
/// is then left out of the arcs.
const HOME_RADIUS_KM: f64 = 1500.0;
/// Addresses per geojs request, and the shortest gap between two requests.
const GEO_BATCH: usize = 50;
const GEO_GAP: Duration = Duration::from_secs(3);
/// After a failed request geojs is left alone for this long.
const GEO_BACKOFF: Duration = Duration::from_secs(60);
/// A cached country answer is trusted this long (seconds).
const GEO_TTL: u64 = 30 * 24 * 3600;
/// Bézier segments per arc; the canvas draws each as a clipped line.
const ARC_SEGMENTS: usize = 32;
/// How far the arc's control point is lifted north, as a fraction of the
/// end-to-end distance.
const ARC_LIFT: f64 = 0.35;
/// Progress of the travelling dot per tick (20 fps → about 2.5 s per trip).
const DOT_STEP: f64 = 0.02;
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

// ------------------------------------------------------------------- arcs

/// Quadratic Bézier from `from` to `to` (both `(lon, lat)`), control point
/// at the midpoint lifted north by [`ARC_LIFT`] of the distance — a
/// ballistic bulge. `to` is unwrapped first so the arc takes the short way
/// round: its longitude may leave ±180 and the caller draws it twice.
pub fn arc_control(from: (f64, f64), to: (f64, f64)) -> ((f64, f64), (f64, f64)) {
    let mut to = to;
    if to.0 - from.0 > 180.0 {
        to.0 -= 360.0;
    } else if to.0 - from.0 < -180.0 {
        to.0 += 360.0;
    }
    let dist = ((to.0 - from.0).powi(2) + (to.1 - from.1).powi(2)).sqrt();
    let ctrl = ((from.0 + to.0) / 2.0, ((from.1 + to.1) / 2.0 + ARC_LIFT * dist).min(89.0));
    (ctrl, to)
}

/// Point of the Bézier `from`→`ctrl`→`to` at `t` in 0..=1.
pub fn arc_at(from: (f64, f64), ctrl: (f64, f64), to: (f64, f64), t: f64) -> (f64, f64) {
    let t = t.clamp(0.0, 1.0);
    let u = 1.0 - t;
    (
        u * u * from.0 + 2.0 * u * t * ctrl.0 + t * t * to.0,
        u * u * from.1 + 2.0 * u * t * ctrl.1 + t * t * to.1,
    )
}

/// `n + 1` points along the arc, parameter strictly increasing from 0 to 1.
pub fn arc_points(from: (f64, f64), to: (f64, f64), n: usize) -> Vec<(f64, f64)> {
    let n = n.max(1);
    let (ctrl, to) = arc_control(from, to);
    (0..=n).map(|i| arc_at(from, ctrl, to, i as f64 / n as f64)).collect()
}

/// One arc on the map: every connection into one country, aggregated.
#[derive(Clone, Debug, PartialEq)]
pub struct ArcInfo {
    pub code: String,
    pub name: String,
    /// `(lon, lat)` of the country centroid.
    pub to: (f64, f64),
    pub established: bool,
    pub rate: u64,
    pub count: usize,
}

/// One arc per country the remotes fall into, the home country left out,
/// addresses without a known country skipped. Sorted by code, so the map is
/// stable between scans. `count` shows up in the label (`US·3`).
pub fn build_arcs(remotes: &[RemoteConn], geo: &HashMap<IpAddr, Geo>, home_code: &str) -> Vec<ArcInfo> {
    let mut by_code: BTreeMap<&str, ArcInfo> = BTreeMap::new();
    for r in remotes {
        let Some(g) = geo.get(&r.ip) else { continue };
        if g.country.is_empty() || g.country.eq_ignore_ascii_case(home_code) {
            continue;
        }
        let Some((lat, lon)) = countries::centroid(&g.country) else { continue };
        let e = by_code.entry(g.country.as_str()).or_insert_with(|| ArcInfo {
            code: g.country.clone(),
            name: g.name.clone(),
            to: (lon, lat),
            established: false,
            rate: 0,
            count: 0,
        });
        e.established |= r.established;
        e.rate = e.rate.saturating_add(r.rate);
        e.count += 1;
    }
    by_code.into_values().collect()
}

// -------------------------------------------------------------------- geo

/// What geojs said about one address; `country` empty = it did not know
/// (anycast, reserved), which is remembered too so it is not asked again.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Geo {
    pub country: String,
    pub name: String,
    /// Unix seconds of the answer; the cache drops it after [`GEO_TTL`].
    pub fetched: u64,
}

/// One row of geojs's country answer: every field a string, absent ones
/// empty. `ip` is echoed back, so a batch answer needs no ordering.
#[derive(Deserialize, Default)]
#[serde(default)]
struct GeoRow {
    ip: String,
    country: String,
    name: String,
}

/// A geojs answer is untrusted text that ends up printed on the map: drop
/// escape sequences (CSI to its final byte, OSC to BEL/ST, two-byte ones)
/// and every control character, C1 included.
fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            match it.next() {
                Some('[') => {
                    for c in it.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = it.next() {
                        if c == '\x07' || (c == '\x1b' && it.next().is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        if !c.is_control() && !('\u{80}'..='\u{9f}').contains(&c) {
            out.push(c);
        }
    }
    out
}

/// Parses a geojs country answer (a JSON array, or one object) into
/// `(ip, code, name)`; rows with an unparsable `ip` are dropped, rows without
/// a country are kept with an empty code.
pub fn parse_country(body: &str) -> Result<Vec<(IpAddr, String, String)>, String> {
    let rows: Vec<GeoRow> = match serde_json::from_str::<Vec<GeoRow>>(body) {
        Ok(v) => v,
        Err(_) => vec![serde_json::from_str::<GeoRow>(body).map_err(|e| e.to_string())?],
    };
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let ip = r.ip.parse::<IpAddr>().ok()?;
            let code = sanitize(&r.country).trim().to_ascii_uppercase();
            let code = if code.len() == 2 && code.bytes().all(|b| b.is_ascii_uppercase()) { code } else { String::new() };
            Some((ip, code, truncate(sanitize(&r.name).trim(), 40)))
        })
        .collect())
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `geo.json` next to the executable.
fn geo_path() -> PathBuf {
    let dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default();
    dir.join("geo.json")
}

/// Loads the cache, dropping entries older than [`GEO_TTL`] at `now`. A
/// missing or corrupt file is an empty cache.
pub fn load_geo(path: &Path, now: u64) -> HashMap<IpAddr, Geo> {
    let Ok(text) = std::fs::read_to_string(path) else { return HashMap::new() };
    let raw: HashMap<String, Geo> = serde_json::from_str(&text).unwrap_or_default();
    raw.into_iter()
        .filter(|(_, g)| now.saturating_sub(g.fetched) < GEO_TTL)
        .filter_map(|(ip, g)| Some((ip.parse().ok()?, g)))
        .collect()
}

/// `<path>.tmp` + rename, so a crash mid-write cannot leave half a file;
/// a leftover `.tmp` from such a crash is removed first. Entries past
/// [`GEO_TTL`] at `now` are not written.
fn save_geo(path: &Path, geo: &HashMap<IpAddr, Geo>, now: u64) -> std::io::Result<()> {
    let raw: BTreeMap<String, &Geo> = geo
        .iter()
        .filter(|(_, g)| now.saturating_sub(g.fetched) < GEO_TTL)
        .map(|(ip, g)| (ip.to_string(), g))
        .collect();
    let text = serde_json::to_string_pretty(&raw).unwrap_or_else(|_| "{}".to_string());
    let tmp = path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
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
    /// Country answers (cache load or a fresh batch).
    Geo(Vec<(IpAddr, Geo)>),
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct GlobeCfg {
    /// `false`: no address ever leaves the machine for the map — no geojs
    /// lookups, no arcs, `c` does nothing.
    pub arcs: bool,
}

impl Default for GlobeCfg {
    fn default() -> Self {
        Self { arcs: true }
    }
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
    cfg: GlobeCfg,
    iss: Option<Iss>,
    trail: Vec<(f64, f64)>,
    err: Option<String>,
    show_trail: bool,
    show_night: bool,
    /// `c`: the arc layer for this session (only meaningful with `cfg.arcs`).
    show_arcs: bool,
    /// Land dots split into day and night for the last canvas size and
    /// minute; the sun moves a quarter degree a minute, the split is ~11k
    /// elevations, so neither is redone per frame.
    dots: RefCell<((usize, usize, i64), Vec<(f64, f64)>, Vec<(f64, f64)>)>,
    /// Whether the last thing rendered was this tab (`draw`) rather than its
    /// OVERVIEW block: fast frames are only worth it for the tab itself.
    drawn: Cell<bool>,
    rx: Option<Receiver<Event>>,
    tx_refresh: Option<tokio::sync::mpsc::Sender<()>>,
    /// Addresses to look up, towards the geojs task.
    tx_geo: Option<tokio::sync::mpsc::Sender<Vec<IpAddr>>>,
    /// `taken` of the WASTELAND snapshot the arcs were built from.
    snap_at: Option<Instant>,
    remotes: Vec<RemoteConn>,
    geo: HashMap<IpAddr, Geo>,
    home_code: String,
    arcs: Vec<ArcInfo>,
    /// Index of the busiest arc (the one with the travelling dot) and the
    /// dot's progress along it.
    busiest: Option<usize>,
    dot_t: f64,
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
            cfg: GlobeCfg::default(),
            iss: None,
            trail: Vec::new(),
            err: None,
            show_trail: true,
            show_night: true,
            show_arcs: true,
            dots: RefCell::new(((0, 0, 0), Vec::new(), Vec::new())),
            drawn: Cell::new(false),
            rx: None,
            tx_refresh: None,
            tx_geo: None,
            snap_at: None,
            remotes: Vec::new(),
            geo: HashMap::new(),
            home_code: String::new(),
            arcs: Vec::new(),
            busiest: None,
            dot_t: 0.0,
        }
    }

    fn arcs_on(&self) -> bool {
        self.cfg.arcs && self.show_arcs
    }

    /// Rebuilds the arcs from the latest remotes + known countries and asks
    /// the geojs task about the addresses still unknown — unless `c` has
    /// hidden the layer, which pauses the lookups too.
    fn rebuild_arcs(&mut self) {
        if self.arcs_on() {
            let unknown: Vec<IpAddr> =
                self.remotes.iter().map(|r| r.ip).filter(|ip| !self.geo.contains_key(ip)).collect();
            if !unknown.is_empty() {
                if let Some(tx) = &self.tx_geo {
                    // A full channel means a request is already queued; the
                    // next snapshot asks again.
                    let _ = tx.try_send(unknown);
                }
            }
        }
        self.arcs = build_arcs(&self.remotes, &self.geo, &self.home_code);
        let busiest = self
            .arcs
            .iter()
            .enumerate()
            .filter(|(_, a)| a.rate > 0)
            .max_by_key(|(_, a)| a.rate)
            .map(|(i, _)| i);
        if busiest != self.busiest {
            self.dot_t = 0.0;
        }
        self.busiest = busiest;
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
        if self.arcs_on() && self.snap_at.is_some() {
            text.push_str(&format!(" · {} links / {} countries", self.remotes.len(), self.arcs.len()));
        }
        Line::from(Span::styled(truncate(&text, width as usize), t.title))
    }
}

/// Cell of a `(lon, lat)` on a `w`×`h` map — the same truncating cast the
/// canvas uses to place a `print`ed label, so the collision map is exact.
fn cell(lon: f64, lat: f64, w: u16, h: u16) -> (u16, u16) {
    let x = ((lon + 180.0) * (w.max(1) - 1) as f64 / 360.0) as u16;
    let y = ((90.0 - lat) * (h.max(1) - 1) as f64 / 180.0) as u16;
    (x.min(w.saturating_sub(1)), y.min(h.saturating_sub(1)))
}

/// The country whose centroid is within [`HOME_RADIUS_KM`] of home, empty
/// when none is (a big country's centroid can be further than that from its
/// own coast: then no country is left out of the arcs).
fn home_country(lat: f64, lon: f64) -> String {
    match countries::nearest(lat, lon) {
        Some((code, km)) if km <= HOME_RADIUS_KM => code.to_string(),
        _ => String::new(),
    }
}

/// Labels for the arc ends: the country code, or its name from 100 columns,
/// with `·N` when more than one address is behind the arc. A label that
/// would sit on the home label's row and columns, or on an already placed
/// one, is left out — the arc still shows where it goes.
pub fn arc_labels(
    arcs: &[ArcInfo],
    home: (f64, f64),
    home_label: &str,
    w: u16,
    h: u16,
) -> Vec<(f64, f64, String)> {
    let span = |lon: f64, lat: f64, len: usize| {
        let (x, y) = cell(lon, lat, w, h);
        (y, x, x.saturating_add(len as u16))
    };
    let mut taken = vec![span(home.0, home.1, home_label.chars().count())];
    let mut out = Vec::new();
    for a in arcs {
        let mut text = if w >= 100 && !a.name.is_empty() { a.name.clone() } else { a.code.clone() };
        if a.count > 1 {
            text.push_str(&format!("·{}", a.count));
        }
        let (y, x0, x1) = span(a.to.0, a.to.1, text.chars().count());
        if x1 > w || taken.iter().any(|&(ty, tx0, tx1)| ty == y && x0 < tx1 && tx0 < x1) {
            continue;
        }
        taken.push((y, x0, x1));
        out.push((a.to.0, a.to.1, text));
    }
    out
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
        "World map with day/night, your location, the ISS and your links"
    }
    fn manual(&self) -> &'static str {
        if self.cfg.arcs {
            "\
GLOBE is a braille world map with the day/night terminator,
the subsolar point, your location from [weather], the
International Space Station exactly where it is right now,
and an arc to every country this machine has a connection to.

  i     show or hide the ISS trail, its last 30 positions
  n     show or hide the night shading and the terminator
  c     show or hide the connection arcs
  r     fetch the ISS position now
  1-9   jump to a tab

The arcs come from WASTELAND's connection table: the public
remote addresses are sent to geojs.io over HTTPS, which answers
with a country and nothing more; answers are kept in geo.json
next to the executable for 30 days. Nothing else leaves the
machine for this. Bright arcs are established, dim ones closing;
the busiest carries a travelling dot. c hides the arcs and
pauses the lookups; [globe] arcs = false switches the feature
off entirely.

The ISS position comes from a public API and updates on its
own, so r is only for when you cannot wait. Waving at the
station is permitted; being seen back is not part of the deal."
        } else {
            "\
GLOBE is a braille world map with the day/night terminator,
the subsolar point, your location from [weather] and the
International Space Station exactly where it is right now.

  i     show or hide the ISS trail, its last 30 positions
  n     show or hide the night shading and the terminator
  r     fetch the ISS position now
  1-9   jump to a tab

Connection arcs are off ([globe] arcs = false): no address of
yours is sent to geojs.io, and c does nothing.

The position comes from a public API and updates on its own,
so r is only for when you cannot wait. Waving at the station
is permitted; being seen back is not part of the contract."
        }
    }
    fn help(&self) -> &'static str {
        if self.cfg.arcs {
            "i ISS trail   n night   c arcs   r refresh ISS   1-9 tabs   q quit"
        } else {
            "i ISS trail   n night shading   r refresh ISS   1-9 tabs   q quit"
        }
    }

    fn start(&mut self, ctx: &Ctx) {
        let (home, notice) = ctx.config.section::<HomeCfg>("weather");
        if let Some(n) = notice {
            let _ = ctx.notify.send(Notice::Footer(n));
        }
        self.home = home;
        self.home_code = home_country(self.home.lat, self.home.lon);
        let (cfg, notice) = ctx.config.section::<GlobeCfg>(self.id());
        if let Some(n) = notice {
            let _ = ctx.notify.send(Notice::Footer(n));
        }
        self.cfg = cfg;

        let (tx, rx) = mpsc::channel::<Event>();
        self.rx = Some(rx);
        self.tx_refresh = Some(spawn_iss(&ctx.rt, tx.clone()));
        if self.cfg.arcs {
            self.tx_geo = Some(spawn_geo(&ctx.rt, tx, geo_path()));
        }
    }

    fn poll(&mut self, ctx: &Ctx) -> usize {
        let mut n = 0;
        let mut dirty = false;
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
                    Event::Geo(rows) => {
                        self.geo.extend(rows);
                        dirty = true;
                    }
                }
            }
        }
        if self.cfg.arcs {
            // Cheap change key first; the snapshot is cloned only when new.
            let at = ctx.board.get::<Instant>(CONNECTIONS_AT);
            if at.is_some() && at != self.snap_at {
                if let Some(s) = ctx.board.get::<ConnSnapshot>(CONNECTIONS) {
                    self.snap_at = Some(s.taken);
                    self.remotes = s.remotes;
                    dirty = true;
                }
            }
        }
        if dirty {
            self.rebuild_arcs();
        }
        n
    }

    fn tick(&mut self, _ctx: &Ctx) {
        if self.arcs_on() && self.busiest.is_some() {
            self.dot_t += DOT_STEP;
            if self.dot_t > 1.0 {
                self.dot_t = 0.0;
            }
        }
    }

    fn wants_fast_frames(&self, active: bool) -> bool {
        active && self.drawn.get() && self.arcs_on() && self.busiest.is_some()
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
            KeyCode::Char('c') if self.cfg.arcs && !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.show_arcs = !self.show_arcs;
                if self.show_arcs {
                    // Lookups paused while hidden: catch up on what came in.
                    self.rebuild_arcs();
                }
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
        self.drawn.set(true);
        if area.width == 0 || area.height == 0 {
            return;
        }
        let now = Utc::now();
        let s = subsolar(now);
        let [head, map] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
        f.render_widget(Paragraph::new(self.title_line(head.width, s, t)), head);
        if map.width == 0 || map.height == 0 {
            return;
        }

        let (cols, rows) = (map.width as usize * 2, map.height as usize * 4);
        let key = (cols, rows, if self.show_night { now.timestamp() / 60 } else { -1 });
        if self.dots.borrow().0 != key {
            let (mut day, mut night) = (Vec::new(), Vec::new());
            for dy in 0..rows {
                let lat = dot_lat(dy, rows);
                for dx in 0..cols {
                    let lon = dot_lon(dx, cols);
                    if !landmask::is_land(lat, lon) {
                        continue;
                    }
                    if !self.show_night || sun_elevation(lat, lon, s) >= HORIZON {
                        day.push((lon, lat));
                    } else {
                        night.push((lon, lat));
                    }
                }
            }
            *self.dots.borrow_mut() = (key, day, night);
        }
        let dots = self.dots.borrow();
        let (day, night) = (&dots.1, &dots.2);
        let term: Vec<(f64, f64)> = if self.show_night {
            (0..cols).map(|dx| dot_lon(dx, cols)).map(|lon| (lon, terminator_lat(lon, s))).collect()
        } else {
            Vec::new()
        };

        let (lit, dim, hi) = (fg(t.value), fg(t.frame), fg(t.title));
        let (home, iss, trail) = (self.home.clone(), self.iss, self.trail.clone());
        let show_trail = self.show_trail;
        let name_fits = map.width >= 60;
        let arcs = if self.arcs_on() { self.arcs.clone() } else { Vec::new() };
        let (busiest, dot_t) = (self.busiest, self.dot_t);
        let from = (home.lon, home.lat);
        let home_label = if name_fits { format!("⌂ {}", home.name) } else { "⌂".to_string() };
        let labels =
            if name_fits { arc_labels(&arcs, from, &home_label, map.width, map.height) } else { Vec::new() };

        let canvas = Canvas::default()
            .marker(Marker::Braille)
            .x_bounds([-180.0, 180.0])
            .y_bounds([-90.0, 90.0])
            .paint(move |ctx| {
                ctx.draw(&Points { coords: night, color: dim });
                ctx.draw(&Points { coords: day, color: lit });
                ctx.draw(&Points { coords: &term, color: hi });
                if show_trail && !trail.is_empty() {
                    ctx.draw(&Points { coords: &trail, color: dim });
                }
                // Dim arcs first so an established one is never painted over.
                let (closing, open): (Vec<&ArcInfo>, Vec<&ArcInfo>) = arcs.iter().partition(|a| !a.established);
                for a in closing.into_iter().chain(open) {
                    let color = if a.established { lit } else { dim };
                    let pts = arc_points(from, a.to, ARC_SEGMENTS);
                    // Past ±180 the arc continues on the other edge of the map.
                    let wraps = pts.iter().any(|p| !(-180.0..=180.0).contains(&p.0));
                    for shift in [0.0, -360.0, 360.0] {
                        if shift != 0.0 && !wraps {
                            break;
                        }
                        for w in pts.windows(2) {
                            ctx.draw(&CLine { x1: w[0].0 + shift, y1: w[0].1, x2: w[1].0 + shift, y2: w[1].1, color });
                        }
                    }
                }
                ctx.layer();
                ctx.print(s.lon, s.lat, Line::from(Span::styled("☼", t.title)));
                for (lon, lat, text) in &labels {
                    ctx.print(*lon, *lat, Line::from(Span::styled(text.clone(), t.text)));
                }
                if let Some(a) = busiest.and_then(|i| arcs.get(i)) {
                    let (ctrl, to) = arc_control(from, a.to);
                    let (x, y) = arc_at(from, ctrl, to, dot_t);
                    ctx.print(wrap_lon(x), y, Line::from(Span::styled("●", t.warn)));
                }
                ctx.print(
                    home.lon,
                    home.lat,
                    Line::from(Span::styled(home_label.clone(), t.warn.add_modifier(Modifier::BOLD))),
                );
                if let Some(i) = iss {
                    ctx.print(i.lon, i.lat, Line::from(Span::styled("✦", t.danger)));
                }
            });
        f.render_widget(canvas, map);
    }

    fn overview(&self, _w: u16, _h: u16, t: Theme) -> Vec<Line<'static>> {
        self.drawn.set(false);
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
        format!("globe sun={:.1},{:.1} iss={iss} arcs={}/{}", s.lat, s.lon, self.arcs.len(), self.remotes.len())
    }
}

/// Answers "which country is this address in" for the module: loads the
/// cache, then serves batches of unknown addresses from the channel — at
/// most [`GEO_BATCH`] per request, one request per [`GEO_GAP`],
/// [`GEO_BACKOFF`] after a failure. Every answer goes back as [`Event::Geo`]
/// and into `geo.json`; an address the answer skipped is remembered as
/// unknown so it is asked once per [`GEO_TTL`], not once per scan. The file
/// is a few KB, so it is read and written right here on the runtime, like
/// the ISS fetch's own I/O.
fn spawn_geo(
    rt: &tokio::runtime::Handle,
    tx: StdSender<Event>,
    path: PathBuf,
) -> tokio::sync::mpsc::Sender<Vec<IpAddr>> {
    let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel::<Vec<IpAddr>>(1);
    rt.spawn(async move {
        let Ok(client) = reqwest::Client::builder().timeout(Duration::from_secs(15)).user_agent("PipBoyCRT").build()
        else {
            return;
        };
        let mut cache = load_geo(&path, unix_now());
        if !cache.is_empty() && tx.send(Event::Geo(cache.iter().map(|(ip, g)| (*ip, g.clone())).collect())).is_err() {
            return;
        }
        let mut next_ok = Instant::now();
        loop {
            let Some(ips) = ask_rx.recv().await else { return };
            let mut unknown: Vec<IpAddr> = ips.into_iter().filter(|ip| !cache.contains_key(ip)).collect();
            unknown.sort();
            unknown.dedup();
            unknown.truncate(GEO_BATCH);
            if unknown.is_empty() {
                continue;
            }
            tokio::time::sleep_until(tokio::time::Instant::from_std(next_ok)).await;
            let list = unknown.iter().map(ToString::to_string).collect::<Vec<_>>().join(",");
            match fetch_country(&client, &list).await {
                Ok(rows) => {
                    let now = unix_now();
                    let mut fresh: Vec<(IpAddr, Geo)> = rows
                        .into_iter()
                        .filter(|(ip, _, _)| unknown.contains(ip))
                        .map(|(ip, country, name)| (ip, Geo { country, name, fetched: now }))
                        .collect();
                    // Whatever the answer left out is "unknown" until the TTL runs out.
                    for ip in &unknown {
                        if !fresh.iter().any(|(f, _)| f == ip) {
                            fresh.push((*ip, Geo { fetched: now, ..Geo::default() }));
                        }
                    }
                    if !fresh.is_empty() {
                        cache.extend(fresh.iter().cloned());
                        let _ = save_geo(&path, &cache, now);
                        if tx.send(Event::Geo(fresh)).is_err() {
                            return;
                        }
                    }
                    next_ok = Instant::now() + GEO_GAP;
                }
                Err(_) => next_ok = Instant::now() + GEO_BACKOFF,
            }
        }
    });
    ask_tx
}

/// `ips` is the comma-separated list for `?ip=`.
async fn fetch_country(client: &reqwest::Client, ips: &str) -> Result<Vec<(IpAddr, String, String)>, String> {
    // Addresses are digits, dots and colons: nothing to escape in a query.
    let body = client
        .get(format!("{GEO_URL}?ip={ips}"))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    parse_country(&body)
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
    fn arc_ends_are_fixed_and_the_middle_bulges_north() {
        let (from, to) = ((19.0, 47.5), (-95.7, 37.1));
        let pts = arc_points(from, to, 32);
        assert_eq!(pts.len(), 33);
        assert_eq!(pts[0], from);
        assert!((pts[32].0 - to.0).abs() < 1e-9 && (pts[32].1 - to.1).abs() < 1e-9);
        // Monotone along the way: longitude never turns back.
        assert!(pts.windows(2).all(|w| w[1].0 < w[0].0), "{pts:?}");
        let mid = pts[16];
        assert!(mid.1 > (from.1 + to.1) / 2.0 + 5.0, "lifted: {mid:?}");
        assert!(mid.1 <= 89.0);
        // Degenerate: zero segments still yields both ends; same point stays put.
        assert_eq!(arc_points(from, to, 0).len(), 2);
        assert!(arc_points(from, from, 4)
            .iter()
            .all(|p| (p.0 - from.0).abs() < 1e-9 && (p.1 - from.1).abs() < 1e-9));
        // Across the antimeridian the short way: Tokyo → Los Angeles goes east.
        let (ctrl, to) = arc_control((139.7, 35.7), (-118.2, 34.1));
        assert!(to.0 > 180.0, "unwrapped: {to:?}");
        assert!(ctrl.0 > 139.7);
        let (_, to) = arc_control((-118.2, 34.1), (139.7, 35.7));
        assert!(to.0 < -180.0, "{to:?}");
        assert_eq!(arc_at(from, ctrl, to, -1.0), from, "t is clamped");
    }

    #[test]
    fn geojs_country_answers_parse_as_seen_in_the_wild() {
        let body = r#"[{"country":"US","country_3":"USA","ip":"8.8.8.8","name":"United States"},{"country":"","country_3":"","ip":"1.1.1.1","name":""},{"ip":"2606:4700::1111","country":"us"},{"ip":"not an ip","country":"DE","name":"Germany"}]"#;
        let rows = parse_country(body).unwrap();
        assert_eq!(rows.len(), 3, "the unparsable ip is dropped: {rows:?}");
        assert_eq!(rows[0], ("8.8.8.8".parse().unwrap(), "US".into(), "United States".into()));
        assert_eq!(rows[1], ("1.1.1.1".parse().unwrap(), String::new(), String::new()), "anycast: no country, kept");
        assert_eq!(
            rows[2],
            ("2606:4700::1111".parse().unwrap(), "US".into(), String::new()),
            "missing name, upper-cased code"
        );
        // One object instead of an array is accepted too.
        let me = parse_country(r#"{"country":"HU","ip":"5.6.7.8","name":"Hungary"}"#).unwrap();
        assert_eq!(me[0].1, "HU");
        // Untrusted text: escapes and controls never reach the map.
        let evil = parse_country(
            "[{\"ip\":\"1.2.3.4\",\"country\":\"D\\u001b[31mE\",\"name\":\"Ger\\u001b[2Jmany\\u001b]0;pwn\\u0007\\u0007\\u0085!\"}]",
        )
        .unwrap();
        assert_eq!(evil[0].1, "DE");
        assert_eq!(evil[0].2, "Germany!");
        assert_eq!(sanitize("a\x1b"), "a");
        assert_eq!(sanitize("a\x1b]8;;http://x\x1b\\b"), "ab");
        assert_eq!(sanitize("Côte d'Ivoire"), "Côte d'Ivoire");
        assert!(parse_country("nope").is_err());
        assert!(parse_country("[]").unwrap().is_empty());
        assert_eq!(parse_country(r#"[{"ip":"1.2.3.4","country":"USA"}]"#).unwrap()[0].1, "", "3 letters is not a code");
    }

    #[test]
    fn geo_cache_round_trips_and_expires() {
        let dir = std::env::temp_dir().join("pipboy-test-geo");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("geo.json");
        let now = 2_000_000_000u64;
        let mut m = HashMap::new();
        let (a, b): (IpAddr, IpAddr) = ("8.8.8.8".parse().unwrap(), "1.1.1.1".parse().unwrap());
        m.insert(a, Geo { country: "US".into(), name: "United States".into(), fetched: now - 100 });
        m.insert(b, Geo { country: String::new(), name: String::new(), fetched: now - GEO_TTL - 1 });
        std::fs::write(p.with_extension("json.tmp"), "stale").unwrap();
        save_geo(&p, &m, now).unwrap();
        assert!(!p.with_extension("json.tmp").exists(), "the stale tmp is gone");
        assert!(!std::fs::read_to_string(&p).unwrap().contains("1.1.1.1"), "expired entries are not written");
        let back = load_geo(&p, now);
        assert_eq!(back.len(), 1, "the 30-day-old one is gone: {back:?}");
        assert_eq!(back[&a].country, "US");
        assert!(load_geo(&p, now + GEO_TTL).is_empty(), "everything expires");
        std::fs::write(&p, "{ not json").unwrap();
        assert!(load_geo(&p, now).is_empty(), "corrupt file = empty cache");
        assert!(load_geo(&dir.join("missing.json"), now).is_empty());
    }

    fn remote(ip: &str, established: bool, rate: u64) -> RemoteConn {
        RemoteConn { ip: ip.parse().unwrap(), established, rate }
    }

    #[test]
    fn home_country_is_the_nearest_centroid_within_reason() {
        assert_eq!(home_country(47.4979, 19.0402), "HU");
        assert_eq!(home_country(35.68, 139.69), "JP");
        assert_eq!(home_country(0.0, -30.0), "", "mid-Atlantic: nothing within 1500 km");
        assert_eq!(home_country(f64::NAN, 1.0), "");
    }

    fn geo(code: &str, name: &str) -> Geo {
        Geo { country: code.into(), name: name.into(), fetched: 0 }
    }

    #[test]
    fn arcs_aggregate_per_country_and_skip_home_and_unknown() {
        let remotes = vec![
            remote("8.8.8.8", true, 100),
            remote("8.8.4.4", false, 250),
            remote("1.1.1.1", true, 9),
            remote("5.6.7.8", true, 1),
            remote("9.9.9.9", true, 1),
            remote("7.7.7.7", true, 1),
        ];
        let mut g = HashMap::new();
        g.insert("8.8.8.8".parse().unwrap(), geo("US", "United States"));
        g.insert("8.8.4.4".parse().unwrap(), geo("US", "United States"));
        g.insert("1.1.1.1".parse().unwrap(), geo("", ""));
        g.insert("5.6.7.8".parse().unwrap(), geo("HU", "Hungary"));
        g.insert("9.9.9.9".parse().unwrap(), geo("XX", "Nowhere"));
        let arcs = build_arcs(&remotes, &g, "hu");
        assert_eq!(arcs.len(), 1, "unknown, home and unmapped codes make no arc: {arcs:?}");
        assert_eq!((arcs[0].code.as_str(), arcs[0].count, arcs[0].rate, arcs[0].established), ("US", 2, 350, true));
        assert!(arcs[0].to.0 < -90.0, "centroid lon: {:?}", arcs[0].to);
        // Without a home code Hungary is an arc too; order is by code.
        let codes: Vec<String> = build_arcs(&remotes, &g, "").into_iter().map(|a| a.code).collect();
        assert_eq!(codes, ["HU", "US"]);
    }

    #[test]
    fn labels_avoid_the_home_label_and_each_other() {
        let mk = |code: &str, name: &str, to: (f64, f64)| ArcInfo {
            code: code.into(),
            name: name.into(),
            to,
            established: true,
            rate: 0,
            count: 1,
        };
        let home = (19.0, 47.5);
        let arcs = vec![
            mk("US", "United States", (-95.7, 37.1)),
            mk("AT", "Austria", (14.5, 47.5)),
            mk("DE", "Germany", (10.4, 51.1)),
            mk("CA", "Canada", (-106.3, 56.1)),
        ];
        let l = arc_labels(&arcs, home, "⌂ Budapest", 80, 20);
        let texts: Vec<&str> = l.iter().map(|(_, _, s)| s.as_str()).collect();
        assert!(texts.contains(&"US") && texts.contains(&"CA"), "{texts:?}");
        assert!(!texts.contains(&"AT"), "Austria sits on the home label's row and columns: {texts:?}");
        let wide = arc_labels(&arcs, home, "⌂ Budapest", 160, 40);
        assert!(wide.iter().any(|(_, _, s)| s == "United States"), "names from 100 columns: {wide:?}");
        // Two arcs landing on the same cell: only one label.
        let dup = vec![mk("US", "", (-95.7, 37.1)), mk("UM", "", (-95.7, 37.1))];
        assert_eq!(arc_labels(&dup, home, "⌂", 80, 20).len(), 1);
        // More than one address behind an arc shows in the label.
        let mut many = mk("US", "United States", (-95.7, 37.1));
        many.count = 3;
        assert_eq!(arc_labels(&[many.clone()], home, "⌂", 80, 20)[0].2, "US·3");
        assert_eq!(arc_labels(&[many], home, "⌂", 120, 20)[0].2, "United States·3");
        // A 1×1 map: everything lands on the home cell, nothing fits.
        assert_eq!(arc_labels(&arcs, home, "⌂", 1, 1).len(), 0);
        // The collision map uses the canvas's own placement: (0,0) is the
        // top-left cell and 180°E the last column, both truncated, not rounded.
        assert_eq!(cell(-180.0, 90.0, 80, 20), (0, 0));
        assert_eq!(cell(180.0, -90.0, 80, 20), (79, 19));
        assert_eq!(cell(-179.0, 89.0, 80, 20), (0, 0), "truncating");
    }

    #[test]
    fn snapshot_on_the_board_becomes_arcs_and_a_status() {
        use crate::module::{ConnSnapshot, CONNECTIONS, CONNECTIONS_AT};
        let t = Theme::new(crate::config::ThemeKind::Color);
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let mut g = Globe::new();
        g.geo.insert("8.8.8.8".parse().unwrap(), geo("US", "United States"));
        g.geo.insert("1.2.3.4".parse().unwrap(), geo("JP", "Japan"));
        g.home_code = "HU".into();
        let snap = ConnSnapshot {
            taken: Instant::now(),
            remotes: vec![remote("8.8.8.8", true, 5000), remote("1.2.3.4", false, 0), remote("5.5.5.5", true, 1)],
        };
        ctx.board.publish(CONNECTIONS_AT, snap.taken);
        ctx.board.publish(CONNECTIONS, snap);
        g.poll(&ctx);
        assert_eq!(g.arcs.len(), 2, "{:?}", g.arcs);
        assert_eq!(g.busiest, Some(1), "JP < US by code; US has the traffic");
        assert!(!g.wants_fast_frames(true), "nothing drawn yet");
        g.tick(&ctx);
        assert!(g.dot_t > 0.0);
        // Mid-arc, well clear of the home label that is printed on top of it.
        for _ in 0..24 {
            g.tick(&ctx);
        }
        assert!((0.4..0.6).contains(&g.dot_t), "{}", g.dot_t);

        let screen = |g: &Globe, w, h| {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| g.draw(f, f.area(), t)).unwrap();
            term.backend().buffer().content().iter().map(|c| c.symbol()).collect::<String>()
        };
        let text = screen(&g, 120, 40);
        assert!(text.contains("3 links / 2 countries"), "{text}");
        assert!(text.contains("United States") && text.contains("Japan"), "labels at 120 columns");
        assert!(text.contains('●'), "the travelling dot");
        assert!(g.wants_fast_frames(true), "the tab was drawn with a moving dot");
        g.overview(30, 2, t);
        assert!(!g.wants_fast_frames(true), "OVERVIEW never earns fast frames");
        screen(&g, 120, 40);
        assert!(g.wants_fast_frames(true));
        let text = screen(&g, 80, 24);
        assert!(text.contains("US") && text.contains("JP"), "codes at 80 columns");
        let text = screen(&g, 50, 14);
        assert!(!text.contains("JP"), "no labels under 60 columns: {text}");

        // `c` hides the layer for the session; the config switch removes it.
        assert!(g.on_key(KeyEvent::from(KeyCode::Char('c')), &ctx));
        assert!(!g.arcs_on() && !g.wants_fast_frames(true));
        assert!(!screen(&g, 120, 40).contains("links /"));
        g.show_arcs = true;
        g.cfg.arcs = false;
        assert!(!g.on_key(KeyEvent::from(KeyCode::Char('c')), &ctx), "c is a no-op without arcs");
        assert!(!screen(&g, 120, 40).contains("Japan"));
        assert!(g.manual().contains("arcs = false") && !g.help().contains("c arcs"));
        g.cfg.arcs = true;
        assert!(g.help().contains("c arcs") && g.manual().contains("geojs.io"));
        assert!(g.status().ends_with("arcs=2/3"), "{}", g.status());

        // An unchanged change key means the snapshot is not even cloned.
        let n = g.remotes.len();
        ctx.board.publish(CONNECTIONS, ConnSnapshot { taken: Instant::now(), remotes: vec![] });
        g.poll(&ctx);
        assert_eq!(g.remotes.len(), n, "same CONNECTIONS_AT, snapshot ignored");
        let snap = ConnSnapshot { taken: Instant::now() + Duration::from_secs(1), remotes: vec![] };
        ctx.board.publish(CONNECTIONS_AT, snap.taken);
        ctx.board.publish(CONNECTIONS, snap);
        g.poll(&ctx);
        assert!(g.arcs.is_empty() && g.busiest.is_none());
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
