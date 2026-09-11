//! Teljes képernyős analóg óra: a panel egésze a számlap, órajelzésekkel a
//! szélein, mutatókkal a középponttól kifelé és egy kronográf dátumablakkal.
//!
//! A `Canvas` határai a cellák 2:1 arányát egyenlítik ki (`y` kétszer akkora
//! tartomány, mint ahány sor), így a panel négyzet-arányos marad.
use crate::modules::clock::Clock;
use crate::style::Theme;
use chrono::{Datelike, Local, Timelike, Weekday};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Circle, Context, Line as CLine, Points};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

/// Óramutató szöge fokban, 0° = felfelé, az óramutató járása szerint.
pub fn hour_angle(h: u32, m: u32) -> f64 {
    ((h % 12) as f64 + m as f64 / 60.0) * 30.0
}

/// Percmutató szöge fokban.
pub fn minute_angle(m: u32, s: u32) -> f64 {
    (m as f64 + s as f64 / 60.0) * 6.0
}

/// Másodpercmutató szöge fokban.
pub fn second_angle(s: u32) -> f64 {
    s as f64 * 6.0
}

/// A hét napjának hárombetűs angol rövidítése a kronográf-ablakhoz.
pub fn weekday3(w: Weekday) -> &'static str {
    match w {
        Weekday::Mon => "MON",
        Weekday::Tue => "TUE",
        Weekday::Wed => "WED",
        Weekday::Thu => "THU",
        Weekday::Fri => "FRI",
        Weekday::Sat => "SAT",
        Weekday::Sun => "SUN",
    }
}

/// A középpontból `deg` fokos irányba induló sugár metszéspontja a
/// `[-w,w] × [-h,h]` téglalap szélével.
pub fn edge_point(deg: f64, w: f64, h: f64) -> (f64, f64) {
    let a = deg.to_radians();
    let (dx, dy) = (a.sin(), a.cos());
    let t = (w / dx.abs()).min(h / dy.abs());
    (dx * t, dy * t)
}

fn scale(p: (f64, f64), f: f64) -> (f64, f64) {
    (p.0 * f, p.1 * f)
}

fn fg(s: Style) -> Color {
    s.fg.unwrap_or(Color::Reset)
}

/// Egyenes a középponttól a `to` pontig, `thick` egységnyi oldalirányú
/// eltolással megvastagítva.
fn hand(ctx: &mut Context, deg: f64, to: (f64, f64), color: Color, thick: i32) {
    let a = deg.to_radians();
    let (px, py) = (a.cos(), -a.sin()); // merőleges irány
    for i in -thick..=thick {
        let (ox, oy) = (px * i as f64, py * i as f64);
        ctx.draw(&CLine { x1: ox, y1: oy, x2: to.0 + ox, y2: to.1 + oy, color });
    }
}

/// Vastag jel a panel szélétől befelé, `deg` irányban, `len` hosszan.
fn tick(ctx: &mut Context, deg: f64, w: f64, h: f64, len: f64, color: Color, thick: i32) {
    let a = deg.to_radians();
    let (dx, dy) = (a.sin(), a.cos());
    let (px, py) = (a.cos(), -a.sin());
    let (ex, ey) = edge_point(deg, w, h);
    let (ix, iy) = (ex - dx * len, ey - dy * len);
    for i in -thick..=thick {
        let (ox, oy) = (px * i as f64, py * i as f64);
        ctx.draw(&CLine { x1: ex + ox, y1: ey + oy, x2: ix + ox, y2: iy + oy, color });
    }
}

pub fn draw(f: &mut Frame, m: &Clock, area: Rect, t: Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let timer = super::clock::timer_line(m, t);
    let [head, dial, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(u16::from(timer.is_some())),
    ])
    .areas(area);

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" CLOCK", t.title),
            Span::styled(" · analog", t.frame),
        ])),
        head,
    );
    if let Some(l) = timer {
        f.render_widget(Paragraph::new(l).alignment(Alignment::Center), foot);
    }
    if dial.width == 0 || dial.height == 0 {
        return;
    }

    let now = Local::now();
    let (h, mi, s) = (now.hour(), now.minute(), now.second());
    let day = format!("[{} {:02}]", weekday3(now.weekday()), now.day());
    let (w, hh) = (dial.width as f64, 2.0 * dial.height as f64);
    // A számlap téglalapja: a canvas határai kis ráhagyással beljebb.
    let (dw, dh) = ((w - 1.5).max(0.5), (hh - 1.5).max(0.5));
    let tick_len = (dw.min(dh) * 0.06).max(2.0);
    let show_minutes = dial.width >= 60;
    let show_date = dial.width >= 40 && dial.height >= 10;
    let (dim, val, hi) = (fg(t.frame), fg(t.value), fg(t.title));

    let canvas = Canvas::default()
        .marker(Marker::Braille)
        .x_bounds([-w, w])
        .y_bounds([-hh, hh])
        .paint(move |ctx| {
            if show_minutes {
                for i in 0..60 {
                    let (x, y) = edge_point(i as f64 * 6.0, dw, dh);
                    ctx.draw(&Points { coords: &[(x, y)], color: dim });
                }
            }
            for k in 0..12 {
                let deg = k as f64 * 30.0;
                if k % 3 == 0 {
                    tick(ctx, deg, dw, dh, tick_len, hi, 1);
                } else {
                    tick(ctx, deg, dw, dh, tick_len, val, 0);
                }
            }
            ctx.layer();

            if show_date {
                ctx.print(
                    0.35 * w,
                    0.0,
                    Line::from(Span::styled(day.clone(), t.value.add_modifier(Modifier::BOLD))),
                );
            }
            ctx.layer();

            hand(ctx, hour_angle(h, mi), scale(edge_point(hour_angle(h, mi), dw, dh), 0.6), hi, 1);
            hand(ctx, minute_angle(mi, s), scale(edge_point(minute_angle(mi, s), dw, dh), 0.9), hi, 0);
            hand(ctx, second_angle(s), scale(edge_point(second_angle(s), dw, dh), 0.95), val, 0);
            ctx.draw(&Points { coords: &[(0.0, 0.0)], color: hi });
            ctx.draw(&Circle { x: 0.0, y: 0.0, radius: (dw.min(dh) * 0.03).max(1.0), color: hi });
        });
    f.render_widget(canvas, dial);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hand_angles() {
        assert_eq!(hour_angle(12, 0), 0.0);
        assert_eq!(hour_angle(0, 0), 0.0);
        assert_eq!(hour_angle(3, 0), 90.0);
        assert_eq!(hour_angle(1, 30), 45.0);
        assert_eq!(minute_angle(0, 30), 3.0, "6:00:30 → a percmutató 3°-nál jár");
        assert_eq!(minute_angle(15, 0), 90.0);
        assert_eq!(second_angle(45), 270.0);
    }

    #[test]
    fn weekday_abbreviations() {
        let all: Vec<&str> = [
            Weekday::Mon,
            Weekday::Tue,
            Weekday::Wed,
            Weekday::Thu,
            Weekday::Fri,
            Weekday::Sat,
            Weekday::Sun,
        ]
        .iter()
        .map(|w| weekday3(*w))
        .collect();
        assert_eq!(all, ["MON", "TUE", "WED", "THU", "FRI", "SAT", "SUN"]);
        assert!(all.iter().all(|s| s.len() == 3));
    }

    #[test]
    fn edge_point_hits_rect_border() {
        let close = |(x, y): (f64, f64), (ex, ey): (f64, f64)| {
            (x - ex).abs() < 1e-9 && (y - ey).abs() < 1e-9
        };
        assert!(close(edge_point(0.0, 10.0, 20.0), (0.0, 20.0)));
        assert!(close(edge_point(90.0, 10.0, 20.0), (10.0, 0.0)));
        assert!(close(edge_point(180.0, 10.0, 20.0), (0.0, -20.0)));
        assert!(close(edge_point(270.0, 10.0, 20.0), (-10.0, 0.0)));
        assert!(close(edge_point(45.0, 10.0, 10.0), (10.0, 10.0)));
    }
}
