use crate::style::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// 3×5 blokk-számjegyek a nagy hőfok-kijelzéshez: 0–9, majd '-', '°' és ':'.
const DIGITS: [[&str; 5]; 13] = [
    ["███", "█ █", "█ █", "█ █", "███"],
    [" █ ", "██ ", " █ ", " █ ", "███"],
    ["███", "  █", "███", "█  ", "███"],
    ["███", "  █", "███", "  █", "███"],
    ["█ █", "█ █", "███", "  █", "  █"],
    ["███", "█  ", "███", "  █", "███"],
    ["███", "█  ", "███", "█ █", "███"],
    ["███", "  █", "  █", "  █", "  █"],
    ["███", "█ █", "███", "█ █", "███"],
    ["███", "█ █", "███", "  █", "███"],
    ["   ", "   ", "███", "   ", "   "],
    ["██ ", "██ ", "██ ", "   ", "   "],
    [" ", "█", " ", "█", " "],
];

/// "22°" / "-3°" / "1:5°" → 5 sor blokk-glif, a glifek között egy szóköz; a sorok végéről a szóköz levágva.
pub fn big_digits(text: &str) -> [String; 5] {
    let mut rows: [String; 5] = Default::default();
    let mut first = true;
    for c in text.chars() {
        let g = match c {
            '0'..='9' => &DIGITS[c as usize - '0' as usize],
            '-' => &DIGITS[10],
            '°' => &DIGITS[11],
            ':' => &DIGITS[12],
            _ => continue,
        };
        for (r, row) in rows.iter_mut().enumerate() {
            if !first {
                row.push(' ');
            }
            row.push_str(g[r]);
        }
        first = false;
    }
    for row in rows.iter_mut() {
        let trimmed = row.trim_end().to_string();
        *row = trimmed;
    }
    rows
}

/// Az utolsó `width` érték oszlopdiagramja. `max == 0` → a szeletmaximumhoz skáláz.
/// Rövid sorozatot balról `▁`-gyel tölt, hogy a szélesség mindig `width` legyen.
pub fn spark(values: &[u64], width: usize, max: u64) -> String {
    let start = values.len().saturating_sub(width);
    let slice = &values[start..];
    let max = if max == 0 { slice.iter().copied().max().unwrap_or(0) } else { max }.max(1);
    let mut s = String::with_capacity(width * 3);
    for _ in slice.len()..width {
        s.push(BARS[0]);
    }
    for &v in slice {
        let i = (v.min(max) * 7 + max / 2) / max;
        s.push(BARS[i as usize]);
    }
    s
}

fn zone(frac: f32, t: Theme) -> Style {
    if frac >= 0.85 { t.vu_high } else if frac >= 0.6 { t.vu_mid } else { t.vu_low }
}

/// Vízszintes VU: `level_pct` 0–100, zónánként színezve (zöld → sárga → piros); a kitöltetlen rész `░` frame-stílusban.
pub fn vu_bar(level_pct: u8, width: usize, t: Theme) -> Line<'static> {
    let filled = (level_pct.min(100) as usize * width) / 100;
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_style: Option<Style> = None;
    for i in 0..width {
        let (ch, st) = if i < filled { ('█', zone((i as f32 + 0.5) / width.max(1) as f32, t)) } else { ('░', t.frame) };
        if run_style != Some(st) {
            if let Some(s) = run_style {
                spans.push(Span::styled(std::mem::take(&mut run), s));
            }
            run_style = Some(st);
        }
        run.push(ch);
    }
    if let Some(s) = run_style {
        spans.push(Span::styled(run, s));
    }
    Line::from(spans)
}

/// Spektrum-oszlopok: sávonként (cella-1) széles oszlop + 1 rés; a sorok felülről lefelé; a szint zónánként színezett.
pub fn spectrum(bands: &[u8], area: Rect, t: Theme) -> Vec<Line<'static>> {
    let h = area.height.max(1) as usize;
    let n = bands.len().max(1);
    let cell = ((area.width as usize) / n).max(1);
    let bar_w = (cell - 1).max(1);
    let mut lines = Vec::with_capacity(h);
    for row in 0..h {
        let frac = (h - row) as f32 / h as f32; // 1.0 a legfelső sor
        let mut spans: Vec<Span<'static>> = Vec::new();
        for &b in bands {
            let on = (b as f32 / 100.0) >= frac - 1e-6;
            let cells = if on { "█".repeat(bar_w) } else { " ".repeat(bar_w) };
            spans.push(Span::styled(cells, if on { zone(frac, t) } else { t.frame }));
            spans.push(Span::raw(" ".repeat(cell - bar_w)));
        }
        lines.push(Line::from(spans));
    }
    lines
}

pub fn gauge(ratio: f32, width: usize) -> String {
    let filled = (ratio.clamp(0.0, 1.0) * width as f32).round() as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

pub fn bytes(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", U[i]) }
}

/// Bájt/s → bit/s felirat (hálózathoz).
pub fn bits_rate(bytes_per_s: u64) -> String {
    let bps = bytes_per_s as f64 * 8.0;
    if bps >= 1e6 {
        format!("{:.1} Mb/s", bps / 1e6)
    } else if bps >= 1e3 {
        format!("{:.0} kb/s", bps / 1e3)
    } else {
        format!("{bps:.0} b/s")
    }
}

/// Shortens `s` to at most `max` chars, replacing the last char with `…` when
/// it doesn't fit whole.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

pub fn compass(deg: u16) -> &'static str {
    const P: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
    P[((deg as f32 + 22.5) / 45.0) as usize % 8]
}

const SUN: [&str; 3] = ["  \\  |  /  ", " ─  ( )  ─ ", "  /  |  \\  "];
const PARTLY: [&str; 3] = ["   \\  /    ", " ─ ( ).-.  ", "    (    ) "];
const CLOUD: [&str; 3] = ["     .--.  ", "  .-(    ).", " (___.__)_)"];
const FOG: [&str; 3] = [" _ - _ - _ ", "  - _ - _  ", " _ - _ - _ "];
const RAIN: [&str; 3] = ["     .-.   ", "    (   ). ", "   ' ' ' ' "];
const SNOW: [&str; 3] = ["     .-.   ", "    (   ). ", "   * * * * "];
const STORM: [&str; 3] = ["     .-.   ", "    (   ). ", "   /' /'/  "];
const UNKNOWN: [&str; 3] = ["           ", "     ?     ", "           "];

/// WMO időjárás-kód → (3 soros ikon, 1 karakteres jel, magyar szöveg).
pub fn wmo(code: u8) -> (&'static [&'static str; 3], &'static str, &'static str) {
    match code {
        0 => (&SUN, "☼", "clear"),
        1 => (&SUN, "☼", "mostly clear"),
        2 => (&PARTLY, "☁", "partly cloudy"),
        3 => (&CLOUD, "☁", "overcast"),
        45 | 48 => (&FOG, "≡", "fog"),
        51 | 53 | 55 | 56 | 57 => (&RAIN, "☂", "drizzle"),
        61 | 63 | 65 | 66 | 67 => (&RAIN, "☂", "rain"),
        71 | 73 | 75 | 77 => (&SNOW, "*", "snow"),
        80 | 81 | 82 => (&RAIN, "☂", "shower"),
        85 | 86 => (&SNOW, "*", "snow shower"),
        95 | 96 | 99 => (&STORM, "!", "thunderstorm"),
        _ => (&UNKNOWN, "?", "unknown"),
    }
}

// ---- vt100 screen -> ratatui spans -----------------------------------------
// (shared by TERM and ART; both render a `vt100::Screen`)

/// xterm's RGB values for the 16 ANSI colors.
const ANSI16: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (205, 0, 0),
    (0, 205, 0),
    (205, 205, 0),
    (0, 0, 238),
    (205, 0, 205),
    (0, 205, 205),
    (229, 229, 229),
    (127, 127, 127),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (92, 92, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];

/// RGB of an xterm 256-color index: 0–15 the palette, 16–231 the 6×6×6 cube,
/// 232–255 the grey ramp.
fn xterm_rgb(idx: u8) -> (u8, u8, u8) {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    match idx {
        0..=15 => ANSI16[idx as usize],
        16..=231 => {
            let i = idx - 16;
            (LEVELS[(i / 36) as usize], LEVELS[(i / 6 % 6) as usize], LEVELS[(i % 6) as usize])
        }
        _ => {
            let v = 8 + 10 * (idx - 232);
            (v, v, v)
        }
    }
}

/// Nearest of the 16 ANSI colors, squared RGB distance.
fn nearest16(rgb: (u8, u8, u8)) -> u8 {
    let dist = |c: &(u8, u8, u8)| {
        let sq = |a: u8, b: u8| (i32::from(a) - i32::from(b)).pow(2);
        sq(c.0, rgb.0) + sq(c.1, rgb.1) + sq(c.2, rgb.2)
    };
    ANSI16.iter().enumerate().min_by_key(|(_, c)| dist(c)).map(|(i, _)| i as u8).unwrap_or(7)
}

/// vt100 color → one of the 16 ANSI colors. `None` = the terminal default,
/// where the caller keeps the theme's own fg/bg.
pub fn map_color(c: vt100::Color) -> Option<Color> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(n) if n < 16 => Some(Color::Indexed(n)),
        vt100::Color::Idx(n) => Some(Color::Indexed(nearest16(xterm_rgb(n)))),
        vt100::Color::Rgb(r, g, b) => Some(Color::Indexed(nearest16((r, g, b)))),
    }
}

fn cell_style(cell: &vt100::Cell, t: Theme) -> Style {
    let mut st = t.text;
    if let Some(fg) = map_color(cell.fgcolor()) {
        st = st.fg(fg);
    }
    if let Some(bg) = map_color(cell.bgcolor()) {
        st = st.bg(bg);
    }
    let mut m = Modifier::empty();
    if cell.bold() {
        m |= Modifier::BOLD;
    }
    if cell.dim() {
        m |= Modifier::DIM;
    }
    if cell.italic() {
        m |= Modifier::ITALIC;
    }
    if cell.underline() {
        m |= Modifier::UNDERLINED;
    }
    if cell.inverse() {
        m |= Modifier::REVERSED;
    }
    st.add_modifier(m)
}

/// One screen row as merged style runs. A wide character keeps its own cell
/// and the following continuation cell is skipped — the glyph itself is two
/// columns wide, so the row still lines up.
pub fn screen_line(screen: &vt100::Screen, row: u16, cols: u16, t: Theme) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_style: Option<Style> = None;
    for col in 0..cols {
        let Some(cell) = screen.cell(row, col) else { break };
        if cell.is_wide_continuation() {
            continue;
        }
        let style = cell_style(cell, t);
        if run_style != Some(style) {
            if let Some(prev) = run_style.take() {
                spans.push(Span::styled(std::mem::take(&mut run), prev));
            }
            run_style = Some(style);
        }
        if cell.has_contents() {
            run.push_str(cell.contents());
        } else {
            run.push(' ');
        }
    }
    if let Some(prev) = run_style {
        spans.push(Span::styled(run, prev));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vu_bar_zones() {
        let t = crate::style::Theme::new(crate::config::ThemeKind::Color);
        let l = vu_bar(100, 20, t);
        let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text.chars().count(), 20);
        assert!(text.chars().all(|c| c == '█'));
        assert_eq!(l.spans.len(), 3, "low/mid/high zones");
        let l = vu_bar(0, 10, t);
        let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.chars().all(|c| c == '░'));
    }

    #[test]
    fn spectrum_rows_and_width() {
        let t = crate::style::Theme::new(crate::config::ThemeKind::Mono);
        let area = ratatui::layout::Rect::new(0, 0, 48, 8);
        let lines = spectrum(&[100u8; 16], area, t);
        assert_eq!(lines.len(), 8);
        let top: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(top.contains('█'), "full bands reach the top row");
        assert!(top.chars().count() <= 48);
        let zero = spectrum(&[0u8; 16], area, t);
        let bottom: String = zero[7].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!bottom.contains('█'));
    }

    #[test]
    fn spark_scales_to_fixed_max_and_pads_left() {
        assert_eq!(spark(&[0, 50, 100], 3, 100), "▁▅█");
        assert_eq!(spark(&[100], 3, 100), "▁▁█");
    }

    #[test]
    fn spark_autoscale_when_max_zero() {
        assert_eq!(spark(&[1, 2, 4], 3, 0), "▃▅█");
    }

    #[test]
    fn spark_takes_last_width_values() {
        assert_eq!(spark(&[100, 0, 0], 2, 100), "▁▁");
    }

    #[test]
    fn gauge_fills_proportionally() {
        assert_eq!(gauge(0.5, 4), "██░░");
        assert_eq!(gauge(2.0, 3), "███");
    }

    #[test]
    fn bytes_and_rates_are_human() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(18_400_000_000), "17.1 GB");
        assert_eq!(bits_rate(1_550_000), "12.4 Mb/s");
        assert_eq!(bits_rate(20_000), "160 kb/s");
    }

    #[test]
    fn wmo_maps_known_and_unknown() {
        assert_eq!(wmo(0).2, "clear");
        assert_eq!(wmo(95).2, "thunderstorm");
        assert_eq!(wmo(200).2, "unknown");
    }

    #[test]
    fn truncate_shortens_with_ellipsis() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hell…");
        assert_eq!(truncate("x", 0), "");
        assert_eq!(truncate("", 5), "");
    }

    #[test]
    fn compass_points() {
        assert_eq!(compass(0), "N");
        assert_eq!(compass(290), "W");
        assert_eq!(compass(315), "NW");
        assert_eq!(compass(180), "S");
    }

    #[test]
    fn big_digits_colon_and_degree() {
        let r = big_digits("1:5°");
        assert_eq!(r[1], "██  █ █   ██");
        assert_eq!(r[2], " █    ███ ██");
        assert!(r.iter().all(|row| row.chars().count() <= 12));
    }

    #[test]
    fn big_digits_render_rows() {
        let r = big_digits("-3°");
        assert_eq!(r[2], "███ ███ ██");
        assert_eq!(r[0], "    ███ ██");
        assert_eq!(r[4], "    ███");
        assert!(r.iter().all(|row| row.chars().count() <= 11));
    }
}
