//! WEATHER nézet. Széles terminálon (≥100 oszlop) három sáv: NOW (blokk-font
//! hőmérséklet, részletek) + AIR & POLLEN (EAQI, szennyezők, pollen, nap-ív),
//! alatta teljes szélességű NEXT 24 HOURS (braille görbe + többsoros
//! csapadék-oszlopok), majd 7 DAYS (közös skálájú hőmérséklet-sávok).
//! Keskenyebben egyszerűsödik, egy soros AIR összefoglalóval.
use crate::modules::weather::Weather;
use crate::style::Theme;
use crate::ui::bigfont;
use crate::ui::widgets::{big_digits, compass, spark, truncate, wmo};
use crate::weather::{aqi_band, pollen_level, AirSnapshot, DayPoint, HourPoint, WeatherSnapshot};
use chrono::{Local, Timelike};
use std::collections::BTreeSet;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Line as CLine};
use ratatui::widgets::{Bar, BarChart, BarGroup, Block, Paragraph};
use ratatui::Frame;

// ---- tiszta segédfüggvények ------------------------------------------------

/// A szélirány (ahonnan fúj) nyila: 0° ↑, 90° →, 225° ↙.
pub fn wind_arrow(deg: u16) -> &'static str {
    const A: [&str; 8] = ["↑", "↗", "→", "↘", "↓", "↙", "←", "↖"];
    A[((deg % 360) as f32 + 22.5) as usize / 45 % 8]
}

/// "HH:MM" → percek éjféltől.
fn minutes(hhmm: &str) -> Option<i32> {
    let (h, m) = hhmm.split_once(':')?;
    Some(h.trim().parse::<i32>().ok()? * 60 + m.trim().parse::<i32>().ok()?)
}

/// A nappal hossza "12h49m" alakban; ismeretlen időpontnál "--".
fn daylight(sunrise: &str, sunset: &str) -> String {
    match (minutes(sunrise), minutes(sunset)) {
        (Some(r), Some(s)) if s > r => format!("{}h{:02}m", (s - r) / 60, (s - r) % 60),
        _ => "--".to_string(),
    }
}

/// A nap helye a nappalon belül (0 = napkelte, 1 = napnyugta); `None` ha a
/// horizont alatt van.
pub fn sun_frac(now_min: i32, sunrise: &str, sunset: &str) -> Option<f32> {
    let (r, s) = (minutes(sunrise)?, minutes(sunset)?);
    (s > r && now_min >= r && now_min <= s).then(|| (now_min - r) as f32 / (s - r) as f32)
}

/// Nap-ív karakterrács: `·` az ív, `☼` a nap helye, éjjel `☾` a horizonton.
pub fn sun_arc(frac: Option<f32>, w: usize, h: usize) -> Vec<String> {
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let top = h - 1;
    let row_of = |x: usize| -> usize {
        let f = if w > 1 { x as f32 / (w - 1) as f32 } else { 0.5 };
        top - ((std::f32::consts::PI * f).sin() * top as f32).round() as usize
    };
    let mut g = vec![vec![' '; w]; h];
    for x in 0..w {
        g[row_of(x)][x] = '·';
    }
    match frac {
        Some(f) => {
            let x = (f.clamp(0.0, 1.0) * (w - 1) as f32).round() as usize;
            g[row_of(x)][x] = '☼';
        }
        None => g[top][w / 2] = '☾',
    }
    g.into_iter().map(|r| r.into_iter().collect()).collect()
}

/// A napi hőmérséklet-sáv kezdete és hossza `width` cellán, a hét közös
/// `lo..hi` skáláján.
fn range_cells(tmin: f32, tmax: f32, lo: f32, hi: f32, width: usize) -> (usize, usize) {
    if width == 0 {
        return (0, 0);
    }
    let span = (hi - lo).max(0.1);
    let pos = |v: f32| (((v - lo) / span).clamp(0.0, 1.0) * width as f32).round() as usize;
    let a = pos(tmin.min(tmax)).min(width - 1);
    let b = pos(tmax.max(tmin)).clamp(a + 1, width);
    (a, b - a)
}

/// A napi sáv szövegként: `░` a skálán kívül, `▓` a nap tartománya.
pub fn range_bar(tmin: f32, tmax: f32, lo: f32, hi: f32, width: usize) -> String {
    let (a, n) = range_cells(tmin, tmax, lo, hi, width);
    format!("{}{}{}", "░".repeat(a), "▓".repeat(n), "░".repeat(width - a - n))
}

/// Csapadék-valószínűségek (%) → `rows` sornyi blokk-karakter, felül a sor teteje.
/// Oszloponként `rows * 8` nyolcad áll rendelkezésre.
pub fn precip_rows(pcts: &[u8], rows: usize) -> Vec<String> {
    const BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    (0..rows)
        .map(|r| {
            pcts.iter()
                .map(|&p| {
                    let total = p.min(100) as usize * rows * 8 / 100;
                    BLOCKS[total.saturating_sub((rows - 1 - r) * 8).min(8)]
                })
                .collect()
        })
        .collect()
}

/// Szennyezőanyag-sáv a saját EAQI-határaihoz mérve (`limits[3]` a teljes skála).
pub fn mini_bar(v: f32, limits: [f32; 4], width: usize) -> String {
    let n = ((v / limits[3].max(0.1)).clamp(0.0, 1.0) * width as f32).round() as usize;
    format!("{}{}", "▓".repeat(n), "░".repeat(width - n))
}

/// Emberi AIR-minősítés az EAQI-ból: (szint, magyarázat).
pub fn air_verdict(aqi: u16) -> (&'static str, &'static str) {
    match aqi {
        0..=20 => ("GOOD", "safe for outdoor activity"),
        21..=40 => ("FAIR", "fine for most people"),
        41..=60 => ("MODERATE", "sensitive people take it easy"),
        61..=80 => ("POOR", "limit time outdoors"),
        81..=100 => ("VERY POOR", "stay inside if you can"),
        _ => ("EXTREMELY POOR", "avoid outdoor exertion"),
    }
}

/// Emberi POLLEN-minősítés: a legerősebb faj szintje + a nem nulla fajok listája
/// csökkenő sorrendben, `"név érték"` alakban.
pub fn pollen_verdict(pollen: &[(&'static str, f32)]) -> (&'static str, String) {
    let worst = pollen.iter().map(|(_, v)| *v).fold(0.0f32, f32::max);
    let level = match pollen_level(worst) {
        "none" => "NONE",
        "low" => "LOW",
        "moderate" => "MODERATE",
        "high" => "HIGH",
        _ => "VERY HIGH",
    };
    let mut items: Vec<&(&'static str, f32)> = pollen.iter().filter(|(_, v)| *v > 0.0).collect();
    items.sort_by(|a, b| b.1.total_cmp(&a.1));
    let detail = items.iter().map(|(n, v)| format!("{n} {v:.0}")).collect::<Vec<_>>().join(" · ");
    (level, detail)
}

/// UV-index sáv (WHO): (szint, teendő).
pub fn uv_band(uv: f32) -> (&'static str, &'static str) {
    match uv {
        u if u < 3.0 => ("LOW", "no protection needed"),
        u if u < 6.0 => ("MODERATE", "hat and shade at midday"),
        u if u < 8.0 => ("HIGH", "sunscreen, shade 11-15h"),
        u if u < 11.0 => ("VERY HIGH", "avoid the midday sun"),
        _ => ("EXTREME", "stay in the shade"),
    }
}

fn uv_style(uv: f32, t: Theme) -> Style {
    match uv_band(uv).0 {
        "LOW" => t.value,
        "EXTREME" | "VERY HIGH" => t.danger,
        _ => t.warn,
    }
}

fn pollen_style(level: &str, t: Theme) -> Style {
    match level {
        "VERY HIGH" => t.danger,
        "HIGH" => t.warn,
        _ => t.frame,
    }
}

/// Egysoros AIR összefoglaló: `AIR good · pollen high (ragweed)`.
pub fn air_summary(a: &AirSnapshot) -> String {
    let (level, _) = air_verdict(a.aqi);
    let mut s = format!("AIR {}", level.to_lowercase());
    let (plevel, _) = pollen_verdict(&a.pollen);
    if plevel != "NONE" {
        if let Some((n, _)) = a.pollen.iter().max_by(|a, b| a.1.total_cmp(&b.1)) {
            s.push_str(&format!(" · pollen {} ({n})", plevel.to_lowercase()));
        }
    }
    s
}

/// Azonos karakterek futamai: "░░▓" → [('░', 2), ('▓', 1)].
fn runs(s: &str) -> Vec<(char, usize)> {
    let mut out: Vec<(char, usize)> = Vec::new();
    for c in s.chars() {
        match out.last_mut() {
            Some((p, n)) if *p == c => *n += 1,
            _ => out.push((c, 1)),
        }
    }
    out
}

/// Az `i`. óra oszlopa `width` cellán.
fn col_of(i: usize, n: usize, width: usize) -> usize {
    if n < 2 || width == 0 {
        return 0;
    }
    i * (width - 1) / (n - 1)
}

/// `col_of` inverze: melyik óra tartozik a `c`. oszlophoz. Ezzel a
/// csapadék-oszlopok ugyanarra a skálára esnek, mint a görbe pontjai.
fn hour_of(c: usize, n: usize, width: usize) -> usize {
    if n < 2 || width < 2 {
        return 0;
    }
    let denom = (width - 1) * 2;
    ((c * (n - 1) * 2 + (width - 1)) / denom).min(n - 1)
}

/// Hány óránként kapjon feliratot a görbe/tengely a panel szélessége alapján:
/// szűken 3 óra, ≥140 oszloptól 2, ≥200 oszloptól minden óra.
fn label_step(width: u16) -> usize {
    if width >= 200 {
        1
    } else if width >= 140 {
        2
    } else {
        3
    }
}

/// Az átfedő feliratokat kiszűri: `(x, karakterszám)` sorozatból csak azok
/// indexei maradnak, amelyek az előző megtartott felirat vége (x + hossz ×
/// `x_per_col`) után kezdődnek.
fn keep_non_overlapping(items: &[(f64, usize)], x_per_col: f64) -> Vec<usize> {
    let mut last_end = f64::NEG_INFINITY;
    let mut kept = Vec::new();
    for (i, &(x, len)) in items.iter().enumerate() {
        if x < last_end {
            continue;
        }
        kept.push(i);
        last_end = x + len as f64 * x_per_col;
    }
    kept
}

fn fg(s: Style) -> Color {
    s.fg.unwrap_or(Color::Reset)
}

fn minutes_now() -> i32 {
    let now = Local::now();
    now.hour() as i32 * 60 + now.minute() as i32
}

// ---- belépési pont ---------------------------------------------------------

pub fn draw(f: &mut Frame, m: &Weather, area: Rect, t: Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let Some(w) = &m.snap else {
        let msg = match &m.err {
            Some(e) => format!(" offline: {e}"),
            None => " fetching…".to_string(),
        };
        f.render_widget(Paragraph::new(msg).style(t.frame), area);
        return;
    };

    let [head, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(u16::from(m.err.is_some())),
    ])
    .areas(area);
    draw_head(f, w, t, head);
    if area.width >= 100 && body.height >= 14 {
        draw_wide(f, w, t, body);
    } else {
        draw_narrow(f, w, t, body, area.width >= 80);
    }
    if let Some(e) = &m.err {
        f.render_widget(Paragraph::new(Span::styled(format!(" refresh failed: {e}"), t.warn)), foot);
    }
}

fn draw_head(f: &mut Frame, w: &WeatherSnapshot, t: Theme, area: Rect) {
    let mut left = vec![
        Span::styled(format!(" {} ", w.place), t.title),
        Span::styled(format!("· updated {}", w.fetched_at.format("%H:%M")), t.frame),
    ];
    if w.is_stale() {
        left.push(Span::styled("  [STALE]", t.warn));
    }
    let right = format!("sunrise {} · sunset {} · {} daylight ", w.sunrise, w.sunset, daylight(&w.sunrise, &w.sunset));
    let rw = (right.chars().count() as u16).min(area.width);
    let [l, r] = Layout::horizontal([Constraint::Min(0), Constraint::Length(rw)]).areas(area);
    f.render_widget(Paragraph::new(Line::from(left)), l);
    f.render_widget(Paragraph::new(Span::styled(right, t.frame)).alignment(Alignment::Right), r);
}

// ---- széles elrendezés -----------------------------------------------------

fn draw_wide(f: &mut Frame, w: &WeatherSnapshot, t: Theme, area: Rect) {
    // C = 7 nap + keret, a maradékon A ≈ 30 %, B ≈ 40 %.
    let days_h = if area.height >= 18 { 9 } else { 0 };
    let rest = area.height - days_h;
    // A NOW-nak kell 8 sor a blokk-fonthoz + szöveg, az órás panelnek legalább 6.
    let top_h = (rest * 3 / 7).max(12).min(rest.saturating_sub(6)).max(6.min(rest));
    let [top, hours, days] = Layout::vertical([
        Constraint::Length(top_h),
        Constraint::Length(rest - top_h),
        Constraint::Length(days_h),
    ])
    .areas(area);
    let now_w = 44.min(area.width / 2);
    let [now, air_uv] = Layout::horizontal([Constraint::Length(now_w), Constraint::Min(0)]).areas(top);
    let uv_w = 28.min(air_uv.width / 2);
    let [air, uv] = Layout::horizontal([Constraint::Min(0), Constraint::Length(uv_w)]).areas(air_uv);
    draw_now_panel(f, w, t, now);
    draw_air_panel(f, w, t, air);
    draw_uv_panel(f, w, t, uv);
    draw_hours_panel(f, w, t, hours);
    if days_h > 0 {
        let block = Block::bordered().title(Span::styled(" 7 DAYS ", t.title)).border_style(t.frame);
        let inner = block.inner(days);
        f.render_widget(block, days);
        draw_day_rows(f, w, t, inner);
    }
}

fn draw_now_panel(f: &mut Frame, w: &WeatherSnapshot, t: Theme, area: Rect) {
    let block = Block::bordered().title(Span::styled(" NOW ", t.title)).border_style(t.frame);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let (icon, _, text) = wmo(w.code);
    let digits = format!("{:.0}", w.temp);
    let cols = bigfont::text_cols(&digits) as u16;
    let head_h = bigfont::ROWS as u16 + 1;
    let mut y = inner.y;
    if inner.height >= head_h + 4 && inner.width >= cols + 3 {
        let buf = f.buffer_mut();
        bigfont::blit(buf, inner, inner.x as i32 + 1, inner.y as i32 + 1, &digits, 1, t.frame, true);
        bigfont::blit(buf, inner, inner.x as i32, inner.y as i32, &digits, 1, t.value, false);
        let deg = Rect { x: inner.x + cols + 1, y: inner.y, width: 1, height: 1 };
        f.render_widget(Paragraph::new(Span::styled("°", t.value)), deg.intersection(inner));
        let ix = inner.x + cols + 3;
        if ix + 11 <= inner.right() {
            let r = Rect { x: ix, y: inner.y + 2, width: 11, height: 3 }.intersection(inner);
            let lines: Vec<Line> = icon.iter().map(|s| Line::from(Span::styled(*s, t.graph))).collect();
            f.render_widget(Paragraph::new(lines), r);
        }
        y += head_h;
    } else {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(format!(" {:.0}°C", w.temp), t.value))),
            Rect { height: 1, ..inner },
        );
        y += 1;
    }

    let mut lines = vec![Line::from(Span::styled(text, t.value))];
    lines.extend(detail_lines(w, t));
    let text_h = (lines.len() as u16).min(inner.bottom().saturating_sub(y));
    f.render_widget(Paragraph::new(lines), Rect { x: inner.x, y, width: inner.width, height: text_h });
}

/// Nap-ív a terület aljára (ív + napkelte/napnyugta felirat), ha `y` fölött van hely.
fn draw_sun_arc(f: &mut Frame, w: &WeatherSnapshot, t: Theme, inner: Rect, y: u16) {
    let left = inner.bottom().saturating_sub(y);
    if left < 3 || inner.width < 12 {
        return;
    }
    let arc_h = (left - 1).min(8);
    let frac = if w.is_day { sun_frac(minutes_now(), &w.sunrise, &w.sunset) } else { None };
    let rows = sun_arc(frac, inner.width as usize, arc_h as usize);
    let lines: Vec<Line> = rows.iter().map(|r| arc_line(r, t)).collect();
    let y0 = inner.bottom() - arc_h - 1;
    f.render_widget(Paragraph::new(lines), Rect { x: inner.x, y: y0, width: inner.width, height: arc_h });
    let pad = (inner.width as usize).saturating_sub(w.sunrise.chars().count() + w.sunset.chars().count());
    let label = format!("{}{}{}", w.sunrise, " ".repeat(pad), w.sunset);
    f.render_widget(
        Paragraph::new(Span::styled(label, t.frame)),
        Rect { x: inner.x, y: inner.bottom() - 1, width: inner.width, height: 1 },
    );
}

/// EAQI / szennyezőanyag színe: good–fair `value`, moderate `warn`, poor+ `danger`.
fn air_style(v: f32, limits: [f32; 4], t: Theme) -> Style {
    if v > limits[2] {
        t.danger
    } else if v > limits[1] {
        t.warn
    } else {
        t.value
    }
}

fn draw_air_panel(f: &mut Frame, w: &WeatherSnapshot, t: Theme, area: Rect) {
    let block = Block::bordered().title(Span::styled(" AIR & POLLEN ", t.title)).border_style(t.frame);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let mut lines: Vec<Line> = Vec::new();
    match &w.air {
        None => lines.push(Line::from(Span::styled("AIR: n/a — no data from Open-Meteo", t.frame))),
        Some(a) => {
            let (alevel, adetail) = air_verdict(a.aqi);
            lines.push(Line::from(Span::styled(
                format!("AIR: {alevel} — {adetail}"),
                air_style(a.aqi as f32, [20.0, 40.0, 60.0, 100.0], t).add_modifier(Modifier::BOLD),
            )));
            let (plevel, pdetail) = pollen_verdict(&a.pollen);
            let ptext =
                if pdetail.is_empty() { format!("POLLEN: {plevel}") } else { format!("POLLEN: {plevel} — {pdetail}") };
            lines.push(Line::from(Span::styled(truncate(&ptext, inner.width as usize), pollen_style(plevel, t))));
            lines.push(Line::from(Span::styled(format!("EAQI {} · {}", a.aqi, aqi_band(a.aqi)), t.frame)));
            // Szűk fél panelen a szennyező-sávok elmaradnak, a két minősítő sor marad.
            if inner.width >= 34 {
                let bar_w = (inner.width as usize).saturating_sub(22).min(12);
                for (name, v, limits) in [
                    ("PM2.5", a.pm2_5, [25.0, 50.0, 75.0, 100.0]),
                    ("PM10", a.pm10, [50.0, 100.0, 150.0, 200.0]),
                    ("O₃", a.ozone, [100.0, 130.0, 240.0, 380.0]),
                    ("NO₂", a.no2, [40.0, 90.0, 120.0, 230.0]),
                ] {
                    lines.push(Line::from(vec![
                        Span::styled(format!("{name:<6}{v:>4.0} µg/m³ "), t.frame),
                        Span::styled(mini_bar(v, limits, bar_w), air_style(v, limits, t)),
                    ]));
                }
            }
        }
    }
    let h = (lines.len() as u16).min(inner.height);
    f.render_widget(Paragraph::new(lines), Rect { height: h, ..inner });
}

const UV_LABELS: [&str; 12] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11+"];

/// Az UV-skála három sora: számok, blokkok (a jelenlegi értékig kitöltve), jelölő `▲`.
fn uv_scale_lines(uv: f32, fill: Style, t: Theme) -> [Line<'static>; 3] {
    let idx = (uv.round() as i32).clamp(0, 11) as usize;
    let numbers: String = UV_LABELS.iter().map(|l| format!("{l:>3}")).collect();
    let blocks: Vec<Span<'static>> = UV_LABELS
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let filled = i <= idx;
            Span::styled(if filled { "███" } else { "░░░" }, if filled { fill } else { t.frame })
        })
        .collect();
    let mut marker = vec![' '; UV_LABELS.len() * 3];
    marker[idx * 3 + 1] = '▲';
    [
        Line::from(Span::styled(numbers, t.frame)),
        Line::from(blocks),
        Line::from(Span::styled(marker.into_iter().collect::<String>(), fill)),
    ]
}

fn draw_uv_panel(f: &mut Frame, w: &WeatherSnapshot, t: Theme, area: Rect) {
    let block = Block::bordered().title(Span::styled(" UV ", t.title)).border_style(t.frame);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let Some(uv) = w.uv else {
        f.render_widget(Paragraph::new(Span::styled("n/a", t.frame)), inner);
        return;
    };
    let (level, detail) = uv_band(uv);
    let style = uv_style(uv, t);
    let mut lines = vec![Line::from(Span::styled(
        format!("UV {uv:.0} · {level} — {detail}"),
        style.add_modifier(Modifier::BOLD),
    ))];
    lines.extend(uv_scale_lines(uv, style, t));
    if let Some(d) = w.daily.first() {
        lines.push(Line::from(Span::styled(format!("today max UV {:.0}", d.uv_max), t.frame)));
    }
    let week = w
        .daily
        .iter()
        .take(7)
        .map(|d| format!("{} {:.0}", d.date.format("%a").to_string().to_uppercase(), d.uv_max))
        .collect::<Vec<_>>()
        .join(" · ");
    if !week.is_empty() && week.chars().count() <= inner.width as usize {
        lines.push(Line::from(Span::styled(week, t.frame)));
    }
    let h = (lines.len() as u16).min(inner.height);
    f.render_widget(Paragraph::new(lines), Rect { height: h, ..inner });
    draw_sun_arc(f, w, t, inner, inner.y + h + 1);
}

fn arc_line(row: &str, t: Theme) -> Line<'static> {
    let spans: Vec<Span<'static>> = row
        .chars()
        .map(|c| Span::styled(c.to_string(), if c == '☼' || c == '☾' { t.warn } else { t.frame }))
        .collect();
    Line::from(spans)
}

fn detail_lines(w: &WeatherSnapshot, t: Theme) -> Vec<Line<'static>> {
    let d = w.daily.first();
    let uv = w.uv.map(|u| format!("{u:.0}")).unwrap_or_else(|| "-".into());
    vec![
        Line::from(format!(
            "feels {:.0}°   ↑ {:.0}°  ↓ {:.0}°",
            w.feels,
            d.map(|d| d.tmax).unwrap_or(w.temp),
            d.map(|d| d.tmin).unwrap_or(w.temp)
        )),
        Line::from(format!(
            "☂ {} %  precip {:.1} mm  clouds {} %",
            w.hourly.first().map(|h| h.precip).unwrap_or(0),
            w.precip_mm,
            w.clouds
        )),
        Line::from(format!("wind {} {} {:.0} km/h", compass(w.wind_dir), wind_arrow(w.wind_dir), w.wind_kmh)),
        Line::from(Span::styled(
            format!("humidity {} %  ·  {:.0} hPa  ·  UV {uv}", w.humidity, w.pressure),
            t.frame,
        )),
    ]
}

fn draw_hours_panel(f: &mut Frame, w: &WeatherSnapshot, t: Theme, area: Rect) {
    let hs = &w.hourly[..w.hourly.len().min(24)];
    let mm: f32 = hs.iter().map(|h| h.precip_mm).sum();
    let gust = hs.iter().map(|h| h.wind_kmh).fold(0.0f32, f32::max);
    let mut title = " NEXT 24 HOURS ".to_string();
    if mm > 0.0 {
        title = format!("{}· {mm:.1} mm ", title);
    }
    if area.width >= 50 && gust > 0.0 {
        title = format!("{title}· wind ≤ {gust:.0} km/h ");
    }
    let block = Block::bordered().title(Span::styled(title, t.title)).border_style(t.frame);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 12 || inner.height < 4 {
        return;
    }
    if hs.is_empty() {
        f.render_widget(Paragraph::new(Span::styled(" no hourly data", t.frame)), inner);
        return;
    }

    // 3 sornyi csapadék-oszlop, magas panelen (kb. 40 soros terminál) 4.
    let bars_h = (if inner.height >= 14 { 4 } else { 3 }).min(inner.height.saturating_sub(3));
    let [chart, glyphs, precip, labels] = Layout::vertical([
        Constraint::Min(2),
        Constraint::Length(u16::from(inner.height >= 8 + bars_h)),
        Constraint::Length(bars_h),
        Constraint::Length(1),
    ])
    .areas(inner);
    let [axis, plot] = Layout::horizontal([Constraint::Length(5), Constraint::Min(4)]).areas(chart);

    // Braille görbe: a hőfok-tartomány 1 fokkal tágítva, hogy a szélső pont se
    // essen a panel peremére.
    let lo = hs.iter().map(|h| h.temp).fold(f32::MAX, f32::min).floor() as f64 - 1.0;
    let hi = hs.iter().map(|h| h.temp).fold(f32::MIN, f32::max).ceil() as f64 + 1.0;
    let mut ax = vec![Line::from(""); axis.height as usize];
    for (row, v) in [(0usize, hi), ((axis.height / 2) as usize, (lo + hi) / 2.0), ((axis.height - 1) as usize, lo)] {
        ax[row] = Line::from(Span::styled(format!("{v:.0}° "), t.frame));
    }
    f.render_widget(Paragraph::new(ax).alignment(Alignment::Right), axis);

    let pts: Vec<(f64, f64)> = hs.iter().enumerate().map(|(i, h)| (i as f64, h.temp as f64)).collect();
    let (line, now) = (fg(t.graph), fg(t.warn));
    let step = label_step(area.width);

    // Feliratozandó órák: az ütem szerintiek `t.value`-val, a min/max pont
    // pluszban `t.title`-lel, ha az ütem még nem fedte le.
    let cadence: BTreeSet<usize> = (0..hs.len()).step_by(step).collect();
    let min_i = hs.iter().enumerate().min_by(|a, b| a.1.temp.total_cmp(&b.1.temp)).map(|(i, _)| i).unwrap_or(0);
    let max_i = hs.iter().enumerate().max_by(|a, b| a.1.temp.total_cmp(&b.1.temp)).map(|(i, _)| i).unwrap_or(0);
    let mut extras: BTreeSet<usize> = BTreeSet::from([min_i, max_i]);
    extras.retain(|i| !cadence.contains(i));
    let mut idxs: Vec<(usize, Style)> =
        cadence.iter().map(|&i| (i, t.value)).chain(extras.iter().map(|&i| (i, t.title))).collect();
    idxs.sort_by_key(|(i, _)| *i);

    // Átfedés-elnyomás: a következő felirat csak a legutóbbi megtartott
    // felirat vége (karakterszám × oszloponkénti x-egység) után kezdődhet.
    let x_per_col = (hs.len() as f64 - 1.0).max(1.0) / (plot.width as f64).max(1.0);
    let items: Vec<(f64, usize)> =
        idxs.iter().map(|&(i, _)| (i as f64, format!("{:.0}°", hs[i].temp).chars().count())).collect();
    let temp_labels: Vec<(f64, f64, String, Style)> = keep_non_overlapping(&items, x_per_col)
        .into_iter()
        .map(|k| {
            let (i, style) = idxs[k];
            let text = format!("{:.0}°", hs[i].temp);
            let y = (hs[i].temp as f64 + 0.8).min(hi - 0.2);
            (i as f64, y, text, style)
        })
        .collect();

    let canvas = Canvas::default()
        .marker(Marker::Braille)
        .x_bounds([0.0, (hs.len() as f64 - 1.0).max(1.0)])
        .y_bounds([lo, hi])
        .paint(move |ctx| {
            ctx.draw(&CLine { x1: 0.0, y1: lo, x2: 0.0, y2: hi, color: now });
            for p in pts.windows(2) {
                ctx.draw(&CLine { x1: p[0].0, y1: p[0].1, x2: p[1].0, y2: p[1].1, color: line });
            }
            for (x, y, text, style) in &temp_labels {
                ctx.print(*x, *y, Line::from(Span::styled(text.clone(), *style)));
            }
        });
    f.render_widget(canvas, plot);

    let width = plot.width as usize;
    if glyphs.height > 0 {
        let mut g = vec![' '; width];
        for (i, h) in hs.iter().enumerate().step_by(3) {
            g[col_of(i, hs.len(), width)] = wmo(h.code).1.chars().next().unwrap_or(' ');
        }
        let row: String = g.into_iter().collect();
        f.render_widget(Paragraph::new(Span::styled(row, t.graph)), Rect { x: plot.x, width: plot.width, ..glyphs });
    }

    draw_precip_bars(f, hs, t, Rect { x: plot.x, width: plot.width, ..precip }, step);

    let mut lbl = vec![' '; width];
    for (i, h) in hs.iter().enumerate().step_by(step) {
        let c = col_of(i, hs.len(), width);
        if c + 2 <= width {
            for (k, ch) in format!("{:02}", h.hour).chars().enumerate() {
                lbl[c + k] = ch;
            }
        }
    }
    let row: String = lbl.into_iter().collect();
    f.render_widget(Paragraph::new(Span::styled(row, t.frame)), Rect { x: plot.x, width: plot.width, ..labels });
}

/// Csapadék-valószínűség többsoros oszlopdiagramként; a 0. oszlop a `now` jel
/// (a görbe függőleges vonalának folytatása), ≥1 mm a sáv tetején számmal.
fn draw_precip_bars(f: &mut Frame, hs: &[HourPoint], t: Theme, area: Rect, step: usize) {
    let (width, rows) = (area.width as usize, area.height as usize);
    if width == 0 || rows == 0 {
        return;
    }
    let idx: Vec<usize> = (0..width).map(|c| hour_of(c, hs.len(), width)).collect();
    let pcts: Vec<u8> = idx.iter().map(|&i| hs[i].precip).collect();
    let mut grid: Vec<Vec<char>> = precip_rows(&pcts, rows).iter().map(|r| r.chars().collect()).collect();
    // mm-felirat az oszlop tetejére, ha ≥1 mm és elfér az óra első oszlopa fölött.
    for c in 0..width {
        if c > 0 && idx[c] == idx[c - 1] || hs[idx[c]].precip_mm < 1.0 {
            continue;
        }
        let label = format!("{:.0}", hs[idx[c]].precip_mm);
        let top = grid.iter().position(|r| r[c] != ' ').unwrap_or(rows);
        if top == 0 || c + label.len() > width {
            continue;
        }
        let y = top - 1;
        if (c..c + label.len()).all(|x| grid[y][x] == ' ') {
            for (k, ch) in label.chars().enumerate() {
                grid[y][c + k] = ch;
            }
        }
    }
    // %-felirat a ≥30%-os oszlopok fölé, az óratengellyel egyező ütemben; a
    // fix sorbüdzsén belül csak akkor, ha van hozzá szabad sor.
    for c in 0..width {
        if c > 0 && idx[c] == idx[c - 1] || idx[c] % step != 0 || pcts[c] < 30 {
            continue;
        }
        let label = format!("{}%", pcts[c]);
        if c + label.len() > width {
            continue;
        }
        let top = grid.iter().position(|r| r[c] != ' ').unwrap_or(rows);
        let Some(y) = (top > 0).then(|| top - 1).or((rows.saturating_sub(top) >= 2).then_some(top)) else {
            continue;
        };
        if (c..c + label.len()).all(|x| grid[y][x] == ' ') {
            for (k, ch) in label.chars().enumerate() {
                grid[y][c + k] = ch;
            }
        }
    }
    for (r, row) in grid.iter_mut().enumerate() {
        // Csak akkor rajzoljuk rá a "now" jelet, ha az oszlopban nincs valódi
        // csapadék-blokk – így a marker sosem színezi át az 1. óra sávját.
        let marker = row[0] == ' ';
        if marker {
            row[0] = '│';
        }
        let spans: Vec<Span<'static>> = row
            .iter()
            .enumerate()
            .map(|(c, &ch)| {
                let st = if c == 0 && marker {
                    t.warn
                } else if ch.is_ascii_digit() || ch == '%' {
                    t.frame
                } else if pcts[c] >= 50 {
                    t.title
                } else {
                    t.value
                };
                Span::styled(ch.to_string(), st)
            })
            .collect();
        f.render_widget(Paragraph::new(Line::from(spans)), Rect { y: area.y + r as u16, height: 1, ..area });
    }
}

fn draw_day_rows(f: &mut Frame, w: &WeatherSnapshot, t: Theme, area: Rect) {
    let n = w.daily.len().min(area.height as usize).min(7);
    if n == 0 || area.width == 0 {
        return;
    }
    let days = &w.daily[..n];
    let lo = days.iter().map(|d| d.tmin).fold(f32::MAX, f32::min);
    let hi = days.iter().map(|d| d.tmax).fold(f32::MIN, f32::max);
    let today = Local::now().date_naive();
    let bar_w = if area.width >= 56 { 12 } else { 0 };
    for (i, d) in days.iter().enumerate() {
        let r = Rect { x: area.x, y: area.y + i as u16, width: area.width, height: 1 };
        f.render_widget(Paragraph::new(day_row(d, lo, hi, bar_w, area.width, d.date == today, t)), r);
    }
}

fn day_row(d: &DayPoint, lo: f32, hi: f32, bar_w: usize, width: u16, today: bool, t: Theme) -> Line<'static> {
    let (_, glyph, text) = wmo(d.code);
    let name = d.date.format("%a").to_string().to_uppercase();
    let mut spans = vec![
        Span::styled(format!("{name:<5}"), if today { t.title } else { t.value }),
        Span::styled(format!("{:<13}", truncate(text, 12)), t.frame),
        Span::styled(format!("{glyph} {:>3} % ", d.precip_pct), if d.precip_pct >= 50 { t.title } else { t.frame }),
    ];
    if bar_w > 0 {
        spans.push(Span::styled(format!(" {:>3}° ", d.tmin), t.frame));
        // A sáv egyforma karakterekből álló futamai egy-egy span-be kerülnek.
        for (ch, n) in runs(&range_bar(d.tmin, d.tmax, lo, hi, bar_w)) {
            spans.push(Span::styled(ch.to_string().repeat(n), if ch == '▓' { t.value } else { t.frame }));
        }
        spans.push(Span::styled(format!(" {:>3}°", d.tmax), t.value));
    } else {
        spans.push(Span::styled(format!("{:>3}°", d.tmax), t.value));
        spans.push(Span::styled(format!(" / {:>3}°", d.tmin), t.frame));
    }
    if width >= 76 {
        spans.push(Span::styled(
            format!("  {:>4.1} mm  wind {:>2.0} km/h  UV {:.0}", d.precip_mm, d.wind_max, d.uv_max),
            t.frame,
        ));
    }
    Line::from(spans)
}

// ---- keskeny elrendezés ----------------------------------------------------

fn draw_narrow(f: &mut Frame, w: &WeatherSnapshot, t: Theme, area: Rect, big: bool) {
    let hourly_h = if big && area.height >= 18 && !w.hourly.is_empty() { (area.height / 3).clamp(6, 10) } else { 0 };
    let now_h = (if big { 7 } else { 6 }).min(area.height);
    let [now, hourly, days] =
        Layout::vertical([Constraint::Length(now_h), Constraint::Length(hourly_h), Constraint::Min(0)]).areas(area);
    draw_now_text(f, w, t, now, big);
    if hourly_h > 0 {
        draw_hourly_bars(f, w, t, hourly);
    }
    if days.height > 1 {
        f.render_widget(Paragraph::new(Span::styled(" 7 DAYS", t.title)), Rect { height: 1, ..days });
        draw_day_rows(f, w, t, Rect { y: days.y + 1, height: days.height - 1, ..days });
    }
}

fn draw_now_text(f: &mut Frame, w: &WeatherSnapshot, t: Theme, area: Rect, big: bool) {
    if area.height == 0 {
        return;
    }
    let (_, _, text) = wmo(w.code);
    let [ic, dt] = Layout::horizontal([Constraint::Length(if big { 13 } else { 0 }), Constraint::Min(10)]).areas(area);
    if big {
        let rows = big_digits(&format!("{:.0}°", w.temp));
        let lines: Vec<Line> = rows.iter().map(|r| Line::from(Span::styled(r.clone(), t.value))).collect();
        f.render_widget(Paragraph::new(lines), ic);
    }
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{:.0}°C  ", w.temp), t.value),
        Span::styled(text, if big { t.title } else { t.value }),
    ])];
    lines.extend(detail_lines(w, t));
    if let Some(a) = &w.air {
        lines.push(Line::from(Span::styled(truncate(&air_summary(a), dt.width as usize), t.frame)));
    }
    f.render_widget(Paragraph::new(lines), dt);
}

fn draw_hourly_bars(f: &mut Frame, w: &WeatherSnapshot, t: Theme, area: Rect) {
    let cols = ((area.width as usize).saturating_sub(2) / 4).min(w.hourly.len());
    if cols == 0 || area.height < 3 {
        return;
    }
    let hs: &[HourPoint] = &w.hourly[..cols];
    let [title, precip, chart] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(3)]).areas(area);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!(" NEXT {cols} HOURS"), t.title),
            Span::styled("  ☂ precip", t.frame),
        ])),
        title,
    );
    // Óránként 4 cella (3 széles oszlop + 1 rés); a jel az oszlop 2. cellájába kerül.
    let precip_line: String = hs.iter().map(|h| format!(" {}  ", spark(&[h.precip as u64], 1, 100))).collect();
    f.render_widget(Paragraph::new(Line::from(Span::styled(precip_line, t.graph))), precip);
    // A BarChart u64-et vár: a minimumhoz képest ábrázolunk, a felirat a valós fokot mutatja.
    let min = hs.iter().map(|h| h.temp).fold(f32::MAX, f32::min).floor();
    let bars: Vec<Bar> = hs
        .iter()
        .map(|h| {
            Bar::default()
                .value((h.temp - min + 1.0).max(0.0) as u64)
                .text_value(format!("{:.0}°", h.temp))
                .label(Line::from(format!("{:02}", h.hour)))
        })
        .collect();
    let bc = BarChart::default()
        .data(BarGroup::default().bars(&bars))
        .bar_width(3)
        .bar_gap(1)
        .bar_style(t.graph)
        .value_style(t.value)
        .label_style(t.frame);
    f.render_widget(bc, chart);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemeKind;
    use chrono::Duration;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn snap() -> WeatherSnapshot {
        let today = Local::now().date_naive();
        WeatherSnapshot {
            place: "Budapest".into(),
            fetched_at: Local::now(),
            temp: -3.4,
            feels: 17.0,
            humidity: 88,
            wind_kmh: 8.0,
            wind_dir: 0,
            pressure: 1004.0,
            code: 61,
            uv: Some(1.0),
            is_day: true,
            clouds: 90,
            precip_mm: 1.2,
            sunrise: "06:15".into(),
            sunset: "19:04".into(),
            hourly: (0..24)
                .map(|i| HourPoint {
                    hour: ((10 + i) % 24) as u8,
                    temp: 14.0 + (i as f32 * 0.7) % 6.0,
                    precip: (i * 4) as u8,
                    code: [0u8, 3, 61, 71, 95][i % 5],
                    wind_kmh: 8.0 + i as f32,
                    precip_mm: 0.1 * i as f32,
                })
                .collect(),
            daily: (0..7)
                .map(|i| DayPoint {
                    date: today + Duration::days(i),
                    tmax: 18.0 + i as f32,
                    tmin: 14.0 - i as f32,
                    code: [0u8, 3, 61, 71, 95, 45, 80][i as usize],
                    precip_pct: (i * 15) as u8,
                    precip_mm: 1.2,
                    wind_max: 21.0,
                    uv_max: 3.0,
                })
                .collect(),
            air: Some(AirSnapshot {
                aqi: 32,
                pm10: 12.4,
                pm2_5: 8.1,
                ozone: 61.0,
                no2: 10.5,
                pollen: vec![("ragweed", 120.0), ("grass", 45.0)],
            }),
        }
    }

    fn draw_at(m: &Weather, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let t = Theme::new(ThemeKind::Color);
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, m, f.area(), t)).unwrap();
        term.backend().buffer().clone()
    }

    fn text_of(buf: &ratatui::buffer::Buffer) -> String {
        buf.content().iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn wind_arrow_points_where_the_wind_comes_from() {
        assert_eq!(wind_arrow(0), "↑");
        assert_eq!(wind_arrow(90), "→");
        assert_eq!(wind_arrow(225), "↙");
        assert_eq!(wind_arrow(359), "↑");
        assert_eq!(wind_arrow(720), "↑");
    }

    #[test]
    fn sun_position_and_arc() {
        let (r, s) = ("06:15", "19:04");
        assert_eq!(sun_frac(5 * 60, r, s), None, "napkelte előtt a horizont alatt");
        assert_eq!(sun_frac(20 * 60, r, s), None, "napnyugta után is");
        let f = sun_frac((375 + 1144) / 2, r, s).unwrap();
        assert!((f - 0.5).abs() < 0.01, "délben a pálya közepén: {f}");
        let arc = sun_arc(Some(0.5), 21, 4);
        assert_eq!(arc.len(), 4);
        assert!(arc[0].contains('☼'), "délben a tetőponton: {arc:?}");
        assert!(sun_arc(Some(0.0), 21, 4)[3].contains('☼'), "napkeltekor a bal alsó vég");
        assert!(sun_arc(None, 21, 4)[3].contains('☾'), "éjjel a horizonton");
        assert!(sun_arc(None, 0, 4).is_empty());
        assert_eq!(daylight("06:15", "19:04"), "12h49m");
        assert_eq!(daylight("--:--", "19:04"), "--");
    }

    #[test]
    fn range_bar_uses_the_shared_scale() {
        // A hét skálája 8..24 fok: a 14..18 fokos nap a sáv közepét foglalja el.
        let b = range_bar(14.0, 18.0, 8.0, 24.0, 12);
        assert_eq!(b.chars().count(), 12);
        assert_eq!(b, "░░░░░▓▓▓░░░░");
        assert_eq!(range_bar(8.0, 24.0, 8.0, 24.0, 12), "▓".repeat(12), "a teljes tartomány kitölt");
        assert_eq!(range_bar(20.0, 20.0, 8.0, 24.0, 12).matches('▓').count(), 1, "egy pontnyi nap is látszik");
        assert_eq!(range_bar(1.0, 2.0, 8.0, 8.0, 0), "");
    }

    #[test]
    fn precip_rows_scale_over_the_available_rows() {
        assert_eq!(precip_rows(&[0], 3), vec![" ".to_string(); 3]);
        assert_eq!(precip_rows(&[100], 3), vec!["█".to_string(); 3]);
        assert_eq!(precip_rows(&[50], 3), vec![" ", "▄", "█"]);
        assert_eq!(precip_rows(&[0, 50, 100], 4), vec!["  █", "  █", " ██", " ██"]);
        assert!(precip_rows(&[50], 0).is_empty());
    }

    #[test]
    fn mini_bar_and_air_summary() {
        assert_eq!(mini_bar(0.0, [25.0, 50.0, 75.0, 100.0], 8), "░░░░░░░░");
        assert_eq!(mini_bar(50.0, [25.0, 50.0, 75.0, 100.0], 8), "▓▓▓▓░░░░");
        assert_eq!(mini_bar(999.0, [25.0, 50.0, 75.0, 100.0], 8), "▓▓▓▓▓▓▓▓");
        assert_eq!(mini_bar(9.0, [1.0, 2.0, 3.0, 4.0], 0), "");
        let a = AirSnapshot { aqi: 32, pollen: vec![("grass", 45.0)], ..Default::default() };
        assert_eq!(air_summary(&a), "AIR fair · pollen moderate (grass)");
        assert_eq!(air_summary(&AirSnapshot::default()), "AIR good");
    }

    #[test]
    fn air_pollen_and_uv_verdicts() {
        for (aqi, level, detail) in [
            (0u16, "GOOD", "safe for outdoor activity"),
            (20, "GOOD", "safe for outdoor activity"),
            (21, "FAIR", "fine for most people"),
            (40, "FAIR", "fine for most people"),
            (41, "MODERATE", "sensitive people take it easy"),
            (60, "MODERATE", "sensitive people take it easy"),
            (61, "POOR", "limit time outdoors"),
            (80, "POOR", "limit time outdoors"),
            (81, "VERY POOR", "stay inside if you can"),
            (100, "VERY POOR", "stay inside if you can"),
            (101, "EXTREMELY POOR", "avoid outdoor exertion"),
        ] {
            assert_eq!(air_verdict(aqi), (level, detail), "aqi {aqi}");
        }

        let (level, detail) = pollen_verdict(&[("ragweed", 219.0), ("birch", 30.0)]);
        assert_eq!((level, detail.as_str()), ("VERY HIGH", "ragweed 219 · birch 30"));
        let (level, detail) = pollen_verdict(&[("grass", 12.0)]);
        assert_eq!((level, detail.as_str()), ("LOW", "grass 12"));
        let (level, detail) = pollen_verdict(&[]);
        assert_eq!((level, detail.as_str()), ("NONE", ""));
        assert_eq!(pollen_verdict(&[("grass", 0.0)]).0, "NONE", "csak nulla érték → nincs pollen");

        for (uv, level) in [
            (2.9f32, "LOW"),
            (3.0, "MODERATE"),
            (5.9, "MODERATE"),
            (6.0, "HIGH"),
            (7.9, "HIGH"),
            (8.0, "VERY HIGH"),
            (10.9, "VERY HIGH"),
            (11.0, "EXTREME"),
        ] {
            assert_eq!(uv_band(uv).0, level, "uv {uv}");
        }
    }

    #[test]
    fn label_step_scales_with_panel_width() {
        assert_eq!(label_step(50), 3);
        assert_eq!(label_step(139), 3);
        assert_eq!(label_step(140), 2);
        assert_eq!(label_step(199), 2);
        assert_eq!(label_step(200), 1);
    }

    #[test]
    fn keep_non_overlapping_drops_colliding_labels() {
        // 3 x-egység/oszlop: egy 2 karakteres felirat 6 egységet foglal, ez
        // elfedi a 4 egységre lévő jelöltet, de a 8 egységre lévőt már nem.
        let items = [(0.0, 2usize), (4.0, 2), (8.0, 2)];
        assert_eq!(keep_non_overlapping(&items, 3.0), vec![0, 2]);
        assert_eq!(keep_non_overlapping(&[], 1.0), Vec::<usize>::new());
    }

    #[test]
    fn draws_at_every_size_without_panic() {
        let mut m = Weather::new();
        for (w, h) in [(1u16, 1u16), (40, 12), (80, 24), (100, 30), (140, 45)] {
            draw_at(&m, w, h); // adat nélkül is
        }
        m.snap = Some(snap());
        for (w, h) in [(1u16, 1u16), (2, 40), (40, 12), (60, 8), (80, 24), (100, 30), (140, 45)] {
            draw_at(&m, w, h);
        }
        m.err = Some("dns".into());
        let big = text_of(&draw_at(&m, 140, 45));
        assert!(big.contains("NOW"));
        assert!(big.contains("NEXT 24 HOURS"), "hiányzik az órás panel");
        assert!(big.contains("AIR & POLLEN"), "hiányzik a levegő panel");
        assert!(big.contains("AIR:"), "hiányzik az AIR-minősítés");
        assert!(big.contains("UV"), "hiányzik az UV panel");
        assert!(big.contains("EAQI 32"), "hiányzik az EAQI");
        assert!(big.contains("7 DAYS"));
        assert!(big.contains("refresh failed: dns"));
        let med = text_of(&draw_at(&m, 80, 24));
        assert!(med.contains("7 DAYS"));
        assert!(med.contains("AIR fair · pollen high (ragweed)"), "a közepes nézetben is legyen AIR sor");
        assert!(med.contains("NEXT 19 HOURS"), "80 oszlopon 19 óra fér ki, a cím ezt tükrözze");
        assert!(!med.contains("NEXT 24 HOURS"), "ne állítson 24 órát, ha csak 19-et rajzol");
    }

    #[test]
    fn hours_panel_prints_value_labels_on_a_wide_panel() {
        let w = snap();
        let t = Theme::new(ThemeKind::Color);
        let area = Rect { x: 0, y: 0, width: 140, height: 45 };
        let mut term = Terminal::new(TestBackend::new(140, 45)).unwrap();
        term.draw(|f| draw_hours_panel(f, &w, t, area)).unwrap();
        let text = text_of(&term.backend().buffer().clone());
        assert!(text.contains('°'), "legyen legalább egy hőfok-felirat a görbén: {text}");
    }

    #[test]
    fn hour_of_inverts_col_of_at_exact_grid_points() {
        // 96 = 4*(n-1): kerek osztó, hogy a lekerekítés pontosan visszaforduljon.
        let (n, width) = (25usize, 97usize);
        for i in 0..n {
            let c = col_of(i, n, width);
            assert_eq!(hour_of(c, n, width), i, "i={i} c={c}");
        }
        assert_eq!(hour_of(0, 1, 10), 0, "1 óránál mindig 0. oszlop");
        assert_eq!(hour_of(5, 24, 1), 0, "1 széles panelnél nincs osztás");
    }

    #[test]
    fn pollen_line_truncates_within_one_row() {
        let mut w = snap();
        w.air = Some(AirSnapshot {
            aqi: 32,
            pm10: 12.4,
            pm2_5: 8.1,
            ozone: 61.0,
            no2: 10.5,
            pollen: vec![
                ("ragweed", 120.0),
                ("grass", 45.0),
                ("mugwort", 12.0),
                ("birch", 45.0),
                ("olive", 8.0),
                ("alder", 20.0),
            ],
        });
        let t = Theme::new(ThemeKind::Color);
        let area = Rect { x: 0, y: 0, width: 56, height: 10 };
        let mut term = Terminal::new(TestBackend::new(56, 10)).unwrap();
        term.draw(|f| draw_air_panel(f, &w, t, area)).unwrap();
        let buf = term.backend().buffer().clone();
        // inner = area mínusz 1 cellás keret minden oldalon → 54 széles;
        // a POLLEN sor a 2. tartalmi sor (AIR, POLLEN, EAQI, szennyezők...).
        let row_y = area.y + 1 + 1;
        let row: String = (1..55).map(|x| buf[(x, row_y)].symbol().to_string()).collect();
        assert!(row.trim_end().ends_with('…'), "a hosszú pollen sor levágva: {row:?}");
    }

    #[test]
    fn precip_marker_does_not_recolor_a_real_bar_in_column_zero() {
        let t = Theme::new(ThemeKind::Color);
        let hs: Vec<HourPoint> = (0..4)
            .map(|i| HourPoint {
                hour: i as u8,
                temp: 10.0,
                precip: if i == 0 { 80 } else { 0 },
                code: 0,
                wind_kmh: 0.0,
                precip_mm: 0.0,
            })
            .collect();
        let area = Rect { x: 0, y: 0, width: 8, height: 3 };
        let mut term = Terminal::new(TestBackend::new(8, 3)).unwrap();
        term.draw(|f| draw_precip_bars(f, &hs, t, area, 3)).unwrap();
        let buf = term.backend().buffer().clone();
        let bottom = &buf[(0, 2)];
        assert_ne!(bottom.symbol(), "│", "80%-nál valódi blokk legyen, ne a now-jel");
        assert_eq!(
            fg(bottom.style()),
            fg(t.title),
            "magas oszlop az 1. órában is a többivel egyező színt kapjon, ne t.warn"
        );
    }
}
