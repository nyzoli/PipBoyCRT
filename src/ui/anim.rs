use crate::style::Theme;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

const SHADES: [char; 5] = ['·', '░', '▒', '▓', '█'];

/// 1: kirajzolás, 2: telítődés, 3: riasztás, 4: lüktetés.
pub fn phase(frame: u32) -> u8 {
    match frame { 0..=11 => 1, 12..=23 => 2, 24..=39 => 3, _ => 4 }
}

fn hash(r: u32, c: u32, k: u32) -> f32 {
    let mut h = r.wrapping_mul(73856093) ^ c.wrapping_mul(19349663) ^ k.wrapping_mul(83492791);
    h = (h ^ (h >> 13)).wrapping_mul(1274126177);
    ((h ^ (h >> 16)) as f32) / (u32::MAX as f32)
}

enum Cell { Core, Blade }

fn in_trefoil(x: f32, y: f32, r0: f32, r1: f32, r2: f32) -> Option<Cell> {
    let r = (x * x + y * y).sqrt();
    if r <= r0 { return Some(Cell::Core); }
    if r < r1 || r > r2 { return None; }
    let mut a = (-y).atan2(x).to_degrees();
    if a < 0.0 { a += 360.0; }
    for c in [90.0f32, 210.0, 330.0] {
        let d = (((a - c) % 360.0 + 540.0) % 360.0 - 180.0).abs();
        if d <= 30.0 { return Some(Cell::Blade); }
    }
    None
}

/// Sugárzás-jel a `cols`×`rows` rácsra (2:1 cellaarány), a `frame` szerinti fázisban.
pub fn trefoil(cols: u16, rows: u16, frame: u32, t: Theme) -> Vec<Line<'static>> {
    let (cols, rows) = (cols.max(1) as u32, rows.max(1) as u32);
    let (cx, cy) = (cols as f32 / 2.0, rows as f32 / 2.0);
    let r2 = (rows as f32 * 0.43).min(cols as f32 * 0.2).max(1.0);
    let (r1, r0) = (r2 * 0.3, r2 * 0.15);
    let reveal = (frame as f32 / 12.0).min(1.0) * r2;
    let fill = ((frame as i32 - 12) / 3).clamp(0, 4) as usize;
    let pulse = (frame >= 40).then(|| if (frame / 3) % 2 == 0 { 4 } else { 3 });
    let density = (frame as f32 * 0.002).min(0.06);
    let style_for = |s: usize| -> Style { if s >= 3 { t.value } else if s >= 1 { t.graph } else { t.frame } };

    let mut lines = Vec::with_capacity(rows as usize);
    for r in 0..rows {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut run = String::new();
        let mut run_style = t.frame;
        let flush = |run: &mut String, spans: &mut Vec<Span<'static>>, st: Style| {
            if !run.is_empty() { spans.push(Span::styled(std::mem::take(run), st)); }
        };
        for c in 0..cols {
            let x = (c as f32 - cx) / 2.0;
            let y = r as f32 - cy;
            let rad = (x * x + y * y).sqrt();
            let (ch, st) = match in_trefoil(x, y, r0, r1, r2) {
                Some(cell) if rad <= reveal => {
                    let mut s = pulse.unwrap_or(fill);
                    if fill > 0 && fill < 4 && rad > reveal - 2.5 { s = s.saturating_sub(1).max(1); }
                    if matches!(cell, Cell::Core) && s >= 3 { ('█', t.value) } else { (SHADES[s], style_for(s)) }
                }
                Some(_) if rad <= reveal + 1.2 => ('·', t.frame),
                Some(_) => (' ', t.frame),
                None if rad > r2 + 0.5 && hash(r, c, frame / 4) < density => (if hash(c, r, frame) < 0.5 { '·' } else { '˙' }, t.frame),
                None => (' ', t.frame),
            };
            if st != run_style { flush(&mut run, &mut spans, run_style); run_style = st; }
            run.push(ch);
        }
        flush(&mut run, &mut spans, run_style);
        lines.push(Line::from(spans));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemeKind;
    use crate::style::Theme;

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect()
    }

    #[test]
    fn frame_zero_is_only_dots_and_center_fills_later() {
        let t = Theme::new(ThemeKind::Mono);
        let f0 = plain(&trefoil(56, 26, 0, t));
        assert_eq!(f0.len(), 26);
        assert!(f0.iter().all(|l| l.chars().all(|c| c == ' ' || c == '·')));
        let f60 = plain(&trefoil(56, 26, 60, t));
        assert_eq!(f60[13].chars().nth(28), Some('█'), "center cell");
        assert!(f60[0].chars().filter(|c| !c.is_whitespace()).count() < 10, "top row is mostly empty (sparse particles only)");
    }

    #[test]
    fn small_area_does_not_panic() {
        let t = Theme::new(ThemeKind::Color);
        for frame in [0, 15, 30, 60, 500] {
            let l = trefoil(40, 6, frame, t);
            assert_eq!(l.len(), 6);
        }
        assert_eq!(phase(0), 1);
        assert_eq!(phase(12), 2);
        assert_eq!(phase(24), 3);
        assert_eq!(phase(40), 4);
    }
}
