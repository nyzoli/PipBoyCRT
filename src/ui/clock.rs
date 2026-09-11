use crate::clock::{moon_phase, ClockView, TimerState};
use crate::modules::clock::Clock;
use crate::style::Theme;
use crate::ui::anim::{phase, trefoil};
use crate::ui::widgets::{big_digits, gauge};
use chrono::{Datelike, Local, Utc};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use std::time::Instant;

fn mmss(d: std::time::Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}", s / 60, s % 60)
}

/// A futo/szunetelo idozito egysoros kijelzese a teljes kepernyos nezetek
/// aljara; `None`, ha az idozito all.
pub fn timer_line(m: &Clock, t: Theme) -> Option<Line<'static>> {
    let tm = &m.state.timer;
    let (label, style) = match tm.state {
        TimerState::Running { .. } => ("running", t.value),
        TimerState::Paused => ("paused", t.warn),
        _ => return None,
    };
    Some(Line::from(Span::styled(format!("\u{23f1} {} {label}", mmss(tm.remaining)), style)))
}

pub fn draw(f: &mut Frame, m: &Clock, area: Rect, t: Theme) {
    let timer = &m.state.timer;
    let now = Instant::now();

    if timer.fired() {
        let frame = timer.frame(now);
        let [art, info] = Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).areas(area);
        f.render_widget(Paragraph::new(trefoil(art.width, art.height, frame, t)), art);
        let blink = phase(frame) >= 3 && (frame / 3) % 2 == 0;
        let rads = (frame as u64 * frame as u64 * 26 / 10).min(9999);
        let tk = match phase(frame) { 1 => "tk", 2 => "tk tk tk", _ => "tk-tk-tk-tk" };
        let lines = vec![
            Line::from(vec![
                Span::styled("   ▌ RADS CRITICAL ▐", if blink { t.warn } else { t.frame }),
                Span::styled(format!("   {tk}"), t.frame),
            ]),
            Line::from(vec![
                Span::styled("   rads/s ", t.frame), Span::styled(format!("{rads:>4}"), t.value),
                Span::styled("   QUEST TIMER ", t.frame), Span::styled("00:00", if blink { t.warn } else { t.value }),
                Span::styled("   [any key] dismiss", t.frame),
            ]),
        ];
        f.render_widget(Paragraph::new(lines), info);
        return;
    }

    match m.view {
        ClockView::Normal => {}
        ClockView::Digital => return super::bigfont::draw(f, m, area, t),
        ClockView::Analog => return super::analog::draw(f, m, area, t),
    }

    let local = Local::now();
    let big = big_digits(&local.format("%H:%M:%S").to_string());
    let mut clock: Vec<Line> = big.iter().map(|r| Line::from(Span::styled(format!("  {r}"), t.value))).collect();
    clock.push(Line::from(Span::styled(
        format!("  {} · {} · week {}", local.format("%A"), local.format("%Y-%m-%d"), local.iso_week().week()),
        t.frame,
    )));

    let moon = moon_phase(local.date_naive());
    let (sr, ss) = m.sun_times();
    let sun = vec![Line::from(vec![
        Span::styled(" SUN & MOON ", t.title),
        Span::raw(format!("sunrise {sr} · sunset {ss} · moon ")),
        Span::styled(format!("{} {}% {}", moon.glyph, moon.pct, moon.name), t.value),
    ])];

    let mut world = vec![Line::from(Span::styled(" WORLD", t.title))];
    let utc = Utc::now();
    for (name, tz) in &m.state.zones {
        let z = utc.with_timezone(tz);
        let day = if z.date_naive() != local.date_naive() { z.format("%a").to_string() } else { String::new() };
        world.push(Line::from(vec![
            Span::raw(format!("   {name:<14} ")),
            Span::styled(z.format("%H:%M").to_string(), t.value),
            Span::styled(format!("  {day}"), t.warn),
        ]));
    }
    if m.state.zones.is_empty() { world.push(Line::from(Span::styled("   no zones configured", t.frame))); }

    let mut qt = vec![Line::from(Span::styled(" QUEST TIMER", t.title))];
    match timer.state {
        TimerState::Running { .. } | TimerState::Paused => {
            let digits = big_digits(&mmss(timer.remaining));
            qt.extend(digits.iter().map(|r| Line::from(Span::styled(format!("   {r}"), t.value))));
            let ratio = 1.0 - timer.remaining.as_secs_f32() / timer.length.as_secs_f32().max(1.0);
            let w = area.width.saturating_sub(12) as usize;
            let label = if matches!(timer.state, TimerState::Paused) { "PAUSED" } else { "running" };
            qt.push(Line::from(vec![Span::raw("   "), Span::styled(gauge(ratio, w.min(60)), t.graph), Span::styled(format!(" {label}"), if label == "PAUSED" { t.warn } else { t.frame })]));
        }
        _ => qt.push(Line::from(vec![Span::styled(format!("   {}", mmss(timer.length)), t.value), Span::styled("   [Enter] start   [x] reset   [ ] ±5 min", t.frame)])),
    }

    let blocks = [clock, sun, world, qt];
    let cons: Vec<Constraint> = blocks.iter().map(|b| Constraint::Length(b.len() as u16)).collect();
    let rects = Layout::vertical(cons).flex(Flex::SpaceAround).split(area);
    for (b, r) in blocks.into_iter().zip(rects.iter()) { f.render_widget(Paragraph::new(b), *r); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::Timer;
    use crate::config::ThemeKind;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn draw_at(m: &Clock, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let t = Theme::new(ThemeKind::Color);
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, m, f.area(), t)).unwrap();
        term.backend().buffer().clone()
    }

    #[test]
    fn every_view_draws_on_tiny_and_large_areas() {
        for view in [ClockView::Normal, ClockView::Digital, ClockView::Analog] {
            let mut m = Clock::new();
            m.view = view;
            for (w, h) in [(1u16, 1u16), (40, 12), (120, 40)] {
                draw_at(&m, w, h);
            }
            // Futó időzítővel (egysoros kijelzés a nagy nézetek alján) is.
            m.state.timer.toggle(Instant::now());
            draw_at(&m, 40, 12);
            draw_at(&m, 1, 1);
            // A lejárt időzítő animációja minden nézetben átveszi a képet.
            m.state.timer = Timer::new(1);
            m.state.timer.state = crate::clock::TimerState::Fired { since: Instant::now() };
            draw_at(&m, 80, 24);
            draw_at(&m, 1, 1);
        }
    }

    #[test]
    fn digital_view_paints_block_glyphs() {
        let mut m = Clock::new();
        m.view = ClockView::Digital;
        let buf = draw_at(&m, 120, 40);
        let painted: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(painted.contains('█'), "a nagy blokk-font kirajzolódik");
    }

    #[test]
    fn timer_line_only_while_the_timer_runs() {
        let t = Theme::new(ThemeKind::Color);
        let mut m = Clock::new();
        assert!(timer_line(&m, t).is_none(), "álló időzítő → nincs sor");
        m.state.timer.toggle(Instant::now());
        let running = timer_line(&m, t).unwrap().to_string();
        assert!(running.contains("running") && running.contains("25:00"), "{running}");
        m.state.timer.toggle(Instant::now());
        assert!(timer_line(&m, t).unwrap().to_string().contains("paused"));
    }
}
