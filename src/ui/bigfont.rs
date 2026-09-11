//! Teljes képernyős digitális óra: kézzel rajzolt blokk-font, árnyékkal.
//!
//! Minden glif `ROWS` sor magas; a számjegyek 8, a kettőspont 4 alap-oszlop
//! szélesek (a jobb szélső oszlop mindig üres, ez a glifek közti hézag).
//! A `scale` az alapcellák egész számú nagyítása, plusz 1 sor/oszlop az
//! árnyéknak.
use crate::modules::clock::Clock;
use crate::style::Theme;
use chrono::{Local, Timelike};
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

pub const ROWS: usize = 7;
pub const DIGIT_COLS: usize = 8;
pub const COLON_COLS: usize = 4;
/// Üres oszlop két számjegy között (nagyítva), hogy az árnyék sose érjen a
/// szomszédba; a kettőspont saját belső margóval jön, ott nincs plusz hézag.
pub const GAP: usize = 1;

/// Hézag `c` után, ha `next` következik.
fn gap_after(c: char, next: Option<char>) -> usize {
    let wide = |c: char| c.is_ascii_digit() || c == '-';
    match (wide(c), next.map(wide)) {
        (true, Some(true)) => GAP,
        _ => 0,
    }
}
/// Az árnyék félárnyalatos, így halvány CRT-beállításnál is elválik a betűtől.
pub const SHADOW: char = '\u{2592}';

// Hét-szegmenses sorok: vízszintes szegmensek az 1–5. oszlopban, függőlegesek
// a 0–1. és 5–6. oszlopban (a fél-blokkok adják a rézsútos élt).
const HT: &str = " ▄███▄  ";
const HM: &str = " █████  ";
const HB: &str = " ▀███▀  ";
const VB: &str = "█▌   ▐█ ";
const VL: &str = "█▌      ";
const VR: &str = "     ▐█ ";
const DB: &str = "        ";
const DM: &str = "  ████  ";
const CB: &str = "    ";
const CD: &str = " █  ";

/// A 12 glif: `0`–`9`, `:` és `-`.
pub const GLYPHS: [(char, [&str; ROWS]); 12] = [
    ('0', [HT, VB, VB, VB, VB, VB, HB]),
    ('1', [VR, VR, VR, VR, VR, VR, VR]),
    ('2', [HT, VR, VR, HM, VL, VL, HB]),
    ('3', [HT, VR, VR, HM, VR, VR, HB]),
    ('4', [VB, VB, VB, HM, VR, VR, VR]),
    ('5', [HT, VL, VL, HM, VR, VR, HB]),
    ('6', [HT, VL, VL, HM, VB, VB, HB]),
    ('7', [HT, VR, VR, VR, VR, VR, VR]),
    ('8', [HT, VB, VB, HM, VB, VB, HB]),
    ('9', [HT, VB, VB, HM, VR, VR, HB]),
    (':', [CB, CD, CD, CB, CD, CD, CB]),
    ('-', [DB, DB, DB, DM, DB, DB, DB]),
];

/// A glif sorai, vagy `None` ha nincs ilyen (a szóköz üres kettőspont-hely).
pub fn glyph(c: char) -> Option<&'static [&'static str; ROWS]> {
    GLYPHS.iter().find(|(g, _)| *g == c).map(|(_, rows)| rows)
}

/// Egy karakter alap-szélessége; az ismeretlen karakter 0.
pub fn char_cols(c: char) -> usize {
    match c {
        '0'..='9' | '-' => DIGIT_COLS,
        ':' | ' ' => COLON_COLS,
        _ => 0,
    }
}

/// A szöveg alap-szélessége oszlopokban, a glifek közti hézagokkal.
pub fn text_cols(text: &str) -> usize {
    let mut it = text.chars().peekable();
    let mut w = 0;
    while let Some(c) = it.next() {
        w += char_cols(c) + gap_after(c, it.peek().copied());
    }
    w
}

/// A legnagyobb egész nagyítás, amivel `cols`×`ROWS` + árnyék belefér.
pub fn fit_scale(w: u16, h: u16, cols: usize) -> Option<u16> {
    if cols == 0 {
        return None;
    }
    let sx = w.saturating_sub(1) as usize / cols;
    let sy = h.saturating_sub(1) as usize / ROWS;
    let s = sx.min(sy);
    (s >= 1).then_some(s as u16)
}

/// Mit rajzoljunk: `(másodpercekkel?, nagyítás)`; `None` ha semmi sem fér el.
pub fn plan(w: u16, h: u16) -> Option<(bool, u16)> {
    if let Some(s) = fit_scale(w, h, text_cols("00:00:00")) {
        return Some((true, s));
    }
    fit_scale(w, h, text_cols("00:00")).map(|s| (false, s))
}

fn put(buf: &mut Buffer, clip: Rect, x: i32, y: i32, ch: char, style: Style) {
    if x < clip.x as i32 || y < clip.y as i32 || x >= clip.right() as i32 || y >= clip.bottom() as i32 {
        return;
    }
    if let Some(cell) = buf.cell_mut((x as u16, y as u16)) {
        cell.set_char(ch);
        cell.set_style(style);
    }
}

/// A szöveget `scale`-szeres nagyításban a `(x0, y0)` bal felső sarokba írja,
/// a `clip` téglalapra vágva.
pub(crate) fn blit(buf: &mut Buffer, clip: Rect, x0: i32, y0: i32, text: &str, scale: u16, style: Style, shadow: bool) {
    let s = scale.max(1) as i32;
    let mut gx = 0i32;
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        if let Some(rows) = glyph(c) {
            for (r, row) in rows.iter().enumerate() {
                for (ci, ch) in row.chars().enumerate() {
                    if ch == ' ' {
                        continue;
                    }
                    let ch = if shadow { SHADOW } else { ch };
                    for dy in 0..s {
                        for dx in 0..s {
                            put(buf, clip, x0 + (gx + ci as i32) * s + dx, y0 + r as i32 * s + dy, ch, style);
                        }
                    }
                }
            }
        }
        gx += (char_cols(c) + gap_after(c, it.peek().copied())) as i32;
    }
}

pub fn draw(f: &mut Frame, m: &Clock, area: Rect, t: Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let timer = super::clock::timer_line(m, t);
    let mut body = area;
    if timer.is_some() {
        body.height = body.height.saturating_sub(1);
    }
    let now = Local::now();

    let Some((secs, scale)) = plan(body.width, body.height) else {
        // Semmi sem fér el: sima egysoros óra.
        let line = Line::from(Span::styled(now.format("%H:%M:%S").to_string(), t.title));
        f.render_widget(Paragraph::new(line).alignment(Alignment::Center), Rect { height: 1, ..area });
        if let Some(l) = timer {
            f.render_widget(Paragraph::new(l).alignment(Alignment::Center), last_row(area));
        }
        return;
    };

    // A kettőspont páros másodpercben látszik; helye a villogás alatt is marad.
    let mut text = now.format(if secs { "%H:%M:%S" } else { "%H:%M" }).to_string();
    if now.second() % 2 != 0 {
        text = text.replace(':', " ");
    }
    let cw = (text_cols(&text) * scale as usize) as u16 + 1;
    let ch = ROWS as u16 * scale + 1;
    let with_date = ch + 1 <= body.height;
    let y0 = body.y + (body.height - ch - u16::from(with_date)) / 2;
    let x0 = body.x as i32 + (body.width as i32 - cw as i32).max(0) / 2;

    let buf = f.buffer_mut();
    blit(buf, body, x0 + 1, y0 as i32 + 1, &text, scale, t.frame, true);
    blit(buf, body, x0, y0 as i32, &text, scale, t.title, false);

    if with_date {
        let date = Line::from(Span::styled(now.format("%A, %-d %B %Y").to_string(), t.frame));
        let r = Rect { x: body.x, y: y0 + ch, width: body.width, height: 1 };
        f.render_widget(Paragraph::new(date).alignment(Alignment::Center), r);
    }
    if let Some(l) = timer {
        f.render_widget(Paragraph::new(l).alignment(Alignment::Center), last_row(area));
    }
}

fn last_row(area: Rect) -> Rect {
    Rect { x: area.x, y: area.bottom().saturating_sub(1), width: area.width, height: 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_table_is_complete_and_rectangular() {
        assert_eq!(GLYPHS.len(), 12, "0-9, a kettőspont és a mínusz");
        for c in "0123456789:-".chars() {
            let rows = glyph(c).unwrap_or_else(|| panic!("hiányzó glif: {c}"));
            assert_eq!(rows.len(), ROWS);
            let want = if c == ':' { COLON_COLS } else { DIGIT_COLS };
            assert_eq!(want, char_cols(c));
            for (i, r) in rows.iter().enumerate() {
                assert_eq!(r.chars().count(), want, "{c} {i}. sora nem {want} széles");
            }
        }
        assert_eq!(glyph('x'), None);
        assert_eq!(char_cols('x'), 0);
        assert_eq!(text_cols("00:00:00"), 6 * DIGIT_COLS + 2 * COLON_COLS + 3 * GAP, "hézag csak számjegy-párok közt");
        assert_eq!(text_cols("00 00"), text_cols("00:00"), "a villogó kettőspont helye megmarad");
    }

    #[test]
    fn scale_grows_with_the_area() {
        // 40×12: az órajel másodpercekkel nem fér el, a HH:MM 1-es nagyítással igen.
        assert_eq!(plan(40, 12), Some((false, 1)));
        let (secs, s) = plan(200, 60).unwrap();
        assert!(secs && s >= 3, "200×60 → HH:MM:SS legalább 3× ({secs} {s})");
        assert_eq!(plan(1, 1), None);
        assert_eq!(plan(0, 0), None);
        // A nagyítás minden irányban belefér (+1 az árnyéknak).
        for (w, h) in [(40u16, 12u16), (80, 24), (120, 40), (200, 60)] {
            if let Some((secs, s)) = plan(w, h) {
                let cols = text_cols(if secs { "00:00:00" } else { "00:00" });
                assert!(cols * s as usize + 1 <= w as usize);
                assert!(ROWS * s as usize + 1 <= h as usize);
            }
        }
    }
}
