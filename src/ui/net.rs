use crate::modules::net::{Net, SpeedPhase, SpeedResult, SpeedState};
use crate::style::Theme;
use crate::ui::widgets::{gauge, spark, truncate};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

/// SPEEDTEST block at full height: title + main line + a third (cancel hint
/// or history sparkline) line.
const SPEED_BLOCK_H: u16 = 3;

fn speed_result_line(r: &SpeedResult, t: Theme) -> Line<'static> {
    let ping = r.ping_ms.map(|p| format!("{p} ms")).unwrap_or_else(|| "n/a".to_string());
    Line::from(vec![
        Span::raw("   "),
        Span::styled(format!("↓ {:.1} Mbps", r.down_mbps), t.value),
        Span::raw("   "),
        Span::styled(format!("↑ {:.1} Mbps", r.up_mbps), t.value),
        Span::raw(format!("   ping {ping}   {}", r.at.format("%Y-%m-%d %H:%M"))),
    ])
}

/// SPEEDTEST content, adapted to how many rows are actually available:
/// `1` row → just the phase/result line, `2+` → title plus that line, `3+`
/// also gets a cancel hint (running) or a download-history sparkline (idle).
fn speed_lines(m: &Net, height: u16, width: u16, t: Theme) -> Vec<Line<'static>> {
    if height == 0 {
        return Vec::new();
    }
    let net = &m.state;
    let main = match &net.speed {
        SpeedState::Running { phase, frac, mbps_so_far } => {
            let label = match phase {
                SpeedPhase::Latency => "latency",
                SpeedPhase::Download => "download",
                SpeedPhase::Upload => "upload",
            };
            Line::from(vec![
                Span::raw(format!("   {label:<9} ")),
                Span::styled(gauge(*frac, 20), t.graph),
                Span::styled(format!(" {mbps_so_far:.1} Mbps"), t.value),
            ])
        }
        SpeedState::Done(r) => speed_result_line(r, t),
        SpeedState::Error(e) => Line::from(Span::styled(truncate(&format!("   error: {e}"), width as usize), t.warn)),
        SpeedState::Idle => match net.speed_history.last() {
            Some(r) => speed_result_line(r, t),
            None => Line::from(Span::styled("   [s] to start", t.frame)),
        },
    };
    if height == 1 {
        return vec![main];
    }
    let mut lines = vec![Line::from(Span::styled(" SPEEDTEST", t.title)), main];
    if height >= SPEED_BLOCK_H {
        if net.speed.is_running() {
            lines.push(Line::from(Span::styled("   [s]/[esc] cancel", t.frame)));
        } else if !net.speed_history.is_empty() {
            let w = (width as usize).saturating_sub(4);
            let samples: Vec<u64> = net.speed_history.iter().map(|r| r.down_mbps.round().max(0.0) as u64).collect();
            lines.push(Line::from(vec![Span::raw("   "), Span::styled(spark(&samples, w, 0), t.graph)]));
        }
    }
    lines
}

pub fn draw(f: &mut Frame, m: &Net, area: Rect, t: Theme) {
    let net = &m.state;
    let n = net.targets.len();
    let mut blocks: Vec<Vec<Line>> = Vec::new();
    let w = area.width.saturating_sub(4) as usize;
    for (i, tg) in net.targets.iter().enumerate() {
        let addr = tg.addr.as_deref().unwrap_or("n/a");
        let head = if tg.label == addr { tg.label.clone() } else { format!("{}  {}", tg.label, addr) };
        let hist = &net.hist[i];
        let samples: Vec<u64> = hist.iter().rev().take(w).rev().map(|s| s.map(|v| (v + 1) as u64).unwrap_or(0)).collect();
        let stat_line = match net.stats(i) {
            Some(s) => {
                let st = if s.loss_pct >= 50 { t.danger } else if s.loss_pct > 0 { t.warn } else { t.value };
                Line::from(vec![
                    Span::raw(format!("   min {:>4}  avg {:>4}  max {:>4} ms   ", s.min, s.avg, s.max)),
                    Span::styled(format!("loss {}%", s.loss_pct), st),
                ])
            }
            None => Line::from(Span::styled("   waiting for replies…", t.frame)),
        };
        blocks.push(vec![
            Line::from(vec![Span::styled(format!(" {head}"), t.title)]),
            Line::from(vec![Span::raw("   "), Span::styled(spark(&samples, w, 0), t.graph)]),
            stat_line,
        ]);
    }
    let mut link = vec![Line::from(Span::styled(" LINK", t.title))];
    match &net.wifi {
        Some(wf) => link.push(Line::from(vec![
            Span::raw(format!("   Wi-Fi {:<20} ", wf.ssid)),
            Span::styled(gauge(wf.signal as f32 / 100.0, 10), t.graph),
            Span::styled(format!(" {}%", wf.signal), t.value),
        ])),
        None => link.push(Line::from(Span::styled("   Wi-Fi n/a", t.frame))),
    }
    match &net.iface {
        Some(i) => link.push(Line::from(format!("   IP {} · GW {} · DNS {}", i.ip, i.gateway, i.dns.as_deref().unwrap_or("n/a")))),
        None => link.push(Line::from(Span::styled("   no IPv4 interface with a gateway", t.frame))),
    }
    blocks.push(link);
    let target_h: u16 = (n * 3) as u16;
    let [top, speed_area, trace_area] =
        Layout::vertical([Constraint::Length(target_h + 3), Constraint::Length(SPEED_BLOCK_H), Constraint::Min(2)]).areas(area);
    let cons: Vec<Constraint> = blocks.iter().map(|b| Constraint::Length(b.len() as u16)).collect();
    let rects = Layout::vertical(cons).flex(Flex::SpaceAround).split(top);
    for (b, r) in blocks.into_iter().zip(rects.iter()) { f.render_widget(Paragraph::new(b), *r); }

    f.render_widget(Paragraph::new(speed_lines(m, speed_area.height, speed_area.width, t)), speed_area);

    let mut tr = vec![Line::from(vec![
        Span::styled(" TRACE ", t.title),
        Span::styled(if net.tracing { "tracing…" } else if net.trace.is_empty() { "[t] traceroute to the first target" } else { "[t] clear" }, t.frame),
    ])];
    let room = trace_area.height.saturating_sub(1) as usize;
    let skip = net.trace.len().saturating_sub(room);
    tr.extend(net.trace.iter().skip(skip).map(|l| Line::from(format!("   {l}"))));
    if let Some(l) = &net.log { tr.push(Line::from(Span::styled(format!("   {l}"), t.warn))); }
    f.render_widget(Paragraph::new(tr), trace_area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemeKind;
    use crate::modules::net::{SpeedEvent, SpeedPhase};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn draw_at(m: &Net, w: u16, h: u16) {
        let t = Theme::new(ThemeKind::Color);
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| draw(f, m, f.area(), t)).unwrap();
    }

    #[test]
    fn draw_does_not_panic_on_tiny_and_normal_areas() {
        draw_at(&Net::default(), 1, 1);
        draw_at(&Net::default(), 40, 12);

        let mut running = Net::default();
        running.state.speed.apply(SpeedEvent::Progress { phase: SpeedPhase::Download, frac: 0.4, mbps_so_far: 123.4 });
        draw_at(&running, 40, 12);
        draw_at(&running, 1, 1);

        let mut done = Net::default();
        done.state.speed_history.push(SpeedResult {
            down_mbps: 245.3,
            up_mbps: 41.2,
            ping_ms: Some(12),
            at: chrono::Local::now(),
        });
        done.state.speed.apply(SpeedEvent::Done(done.state.speed_history[0].clone()));
        draw_at(&done, 40, 12);
        draw_at(&done, 1, 1);
    }

    #[test]
    fn speed_lines_shrinks_to_one_line_when_area_is_short() {
        let t = Theme::new(ThemeKind::Color);
        let mut m = Net::default();
        m.state.speed.apply(SpeedEvent::Done(SpeedResult { down_mbps: 1.0, up_mbps: 2.0, ping_ms: None, at: chrono::Local::now() }));
        assert_eq!(speed_lines(&m, 0, 40, t).len(), 0);
        assert_eq!(speed_lines(&m, 1, 40, t).len(), 1);
        assert!(speed_lines(&m, 2, 40, t).len() >= 2);
    }
}
