use crate::modules::stat::Stat;
use crate::stat::fmt_uptime;
use crate::style::Theme;
use crate::ui::widgets::{bits_rate, bytes, gauge, spark};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use std::collections::VecDeque;

fn hist(h: &VecDeque<u64>) -> Vec<u64> {
    h.iter().copied().collect()
}

pub fn draw(f: &mut Frame, m: &Stat, area: Rect, t: Theme) {
    let Some(s) = &m.snap else {
        f.render_widget(Paragraph::new(" fetching…").style(t.frame), area);
        return;
    };
    let single_col = area.width < 100;
    let (left, right) = if single_col {
        (area, area)
    } else {
        let [left, right] = Layout::horizontal([Constraint::Percentage(56), Constraint::Percentage(44)]).areas(area);
        (left, right)
    };
    let (lw, rw) = if single_col {
        (area.width.saturating_sub(8) as usize, area.width.saturating_sub(24) as usize)
    } else {
        (left.width.saturating_sub(8) as usize, right.width.saturating_sub(24) as usize)
    };

    // ---- bal oszlop: CPU, GPU, NET, összegzés ----
    let mut cpu_lines: Vec<Line> = Vec::new();
    cpu_lines.push(Line::from(vec![
        Span::styled(" CPU  ", t.title),
        Span::raw(format!("{} · {}c · {:.1} GHz", s.cpu_brand.trim(), s.cpu_cores.len(), s.cpu_mhz as f32 / 1000.0)),
    ]));
    cpu_lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(spark(&hist(&m.cpu_hist), lw, 100), t.graph),
        Span::styled(format!(" {:3.0}%", s.cpu_total), t.load(s.cpu_total)),
    ]));
    let cores: Vec<(usize, f32)> = s.cpu_cores.iter().copied().enumerate().collect();
    for chunk in cores.chunks(4) {
        let mut spans = vec![Span::raw(" ")];
        for (idx, c) in chunk {
            spans.push(Span::styled(format!("{idx:2} "), t.frame));
            spans.push(Span::styled(gauge(c / 100.0, 6), t.graph));
            spans.push(Span::styled(format!(" {c:3.0}%  "), t.load(*c)));
        }
        cpu_lines.push(Line::from(spans));
    }

    let mut gpu_lines: Vec<Line> = Vec::new();
    match &s.gpu {
        Some(g) => {
            gpu_lines.push(Line::from(vec![Span::styled(" GPU  ", t.title), Span::raw(format!("VRAM {}", bytes(g.mem_used)))]));
            gpu_lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(spark(&hist(&m.gpu_hist), lw, 100), t.graph),
                Span::styled(format!(" {:3.0}%", g.util), t.load(g.util)),
            ]));
        }
        None => gpu_lines.push(Line::from(vec![Span::styled(" GPU  ", t.title), Span::styled("n/a", t.frame)])),
    }

    let mut net_lines: Vec<Line> = Vec::new();
    net_lines.push(Line::from(vec![Span::styled(" NET  ", t.title), Span::raw(s.net_iface.clone())]));
    let nw = lw.saturating_sub(14) / 2;
    net_lines.push(Line::from(vec![
        Span::raw(" ↓ "),
        Span::styled(spark(&hist(&m.rx_hist), nw, 0), t.graph),
        Span::styled(format!(" {:>10}", bits_rate(s.rx_bps)), t.value),
        Span::raw("  ↑ "),
        Span::styled(spark(&hist(&m.tx_hist), nw, 0), t.graph),
        Span::styled(format!(" {:>10}", bits_rate(s.tx_bps)), t.value),
    ]));

    let mut sum_lines: Vec<Line> = Vec::new();
    let top = s.top.iter().map(|(n, c)| format!("{n} {c:.0}%")).collect::<Vec<_>>().join("  ");
    sum_lines.push(Line::from(vec![
        Span::styled(" UPTIME ", t.title),
        Span::raw(fmt_uptime(s.uptime_s)),
        Span::styled("   PROCS ", t.title),
        Span::raw(s.procs.to_string()),
        Span::styled("   TOP ", t.title),
        Span::raw(top),
    ]));

    // ---- jobb oszlop: MEM, SWAP, DISK ----
    let mut mem_lines: Vec<Line> = Vec::new();
    let mem_pct = if s.mem_total > 0 { s.mem_used as f32 / s.mem_total as f32 * 100.0 } else { 0.0 };
    mem_lines.push(Line::from(vec![Span::styled(" MEM  ", t.title), Span::raw(bytes(s.mem_total))]));
    mem_lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(gauge(mem_pct / 100.0, rw), t.graph),
        Span::styled(format!(" {:>9} {:3.0}%", bytes(s.mem_used), mem_pct), t.load(mem_pct)),
    ]));
    if s.swap_total > 0 {
        let sp = s.swap_used as f32 / s.swap_total as f32;
        mem_lines.push(Line::from(vec![
            Span::styled(" swap ", t.frame),
            Span::styled(gauge(sp, rw.saturating_sub(5)), t.graph),
            Span::raw(format!(" {:>9} {:3.0}%", bytes(s.swap_used), sp * 100.0)),
        ]));
    }

    let mut disk_lines: Vec<Line> = Vec::new();
    disk_lines.push(Line::from(Span::styled(" DISK", t.title)));
    for d in &s.disks {
        let pct = d.used as f32 / d.total as f32 * 100.0;
        let st = if pct >= 95.0 { t.danger } else if pct >= 90.0 { t.warn } else { t.value };
        disk_lines.push(Line::from(vec![
            Span::raw(format!(" {:<3}", d.mount)),
            Span::styled(gauge(pct / 100.0, rw.saturating_sub(3)), t.graph),
            Span::styled(format!(" {:>7}/{:<7}", bytes(d.used), bytes(d.total)), st),
        ]));
        disk_lines.push(Line::from(vec![
            Span::styled("     R ", t.frame),
            Span::raw(format!("{}/s", bytes(d.read_bps))),
            Span::styled("   W ", t.frame),
            Span::raw(format!("{}/s", bytes(d.write_bps))),
        ]));
    }

    if single_col {
        let blocks = [cpu_lines, gpu_lines, net_lines, sum_lines, mem_lines, disk_lines];
        let cons: Vec<Constraint> = blocks.iter().map(|b| Constraint::Length(b.len() as u16)).collect();
        let rects = Layout::vertical(cons).flex(Flex::SpaceAround).split(area);
        for (lines, r) in blocks.into_iter().zip(rects.iter()) {
            f.render_widget(Paragraph::new(lines), *r);
        }
        return;
    }

    let left_blocks = [cpu_lines, gpu_lines, net_lines, sum_lines];
    let left_cons: Vec<Constraint> = left_blocks.iter().map(|b| Constraint::Length(b.len() as u16)).collect();
    let left_rects = Layout::vertical(left_cons).flex(Flex::SpaceAround).split(left);
    for (lines, r) in left_blocks.into_iter().zip(left_rects.iter()) {
        f.render_widget(Paragraph::new(lines), *r);
    }

    let right_blocks = [mem_lines, disk_lines];
    let right_cons: Vec<Constraint> = right_blocks.iter().map(|b| Constraint::Length(b.len() as u16)).collect();
    let right_rects = Layout::vertical(right_cons).flex(Flex::SpaceAround).split(right);
    for (lines, r) in right_blocks.into_iter().zip(right_rects.iter()) {
        f.render_widget(Paragraph::new(lines), *r);
    }
}
