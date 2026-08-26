//! A small terminal dashboard: the weekly-budget plot from the web, in the
//! terminal. Reads straight from the DB (no server needed). `q` quits, ↑/↓ or
//! Tab switches service, `r` refreshes.

use anyhow::Result;
use std::io;
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    prelude::*,
    widgets::{Axis, Block, Borders, Chart, Dataset, Gauge, GraphType, Paragraph},
};

use crate::config::Config;
use crate::db::DbPool;
use crate::forecast::{self, Cumulative};

fn status_color(status: &str) -> Color {
    match status {
        "green" => Color::Green,
        "amber" => Color::Yellow,
        "red" => Color::Red,
        _ => Color::Gray,
    }
}

fn refresh(pool: &DbPool, cfg: &Config, services: &[&str]) -> Vec<Cumulative> {
    let now = chrono::Utc::now();
    services
        .iter()
        .filter_map(|s| forecast::cumulative_view(pool, cfg, s, now).ok())
        .collect()
}

pub fn run(pool: &DbPool, cfg: &Config) -> Result<()> {
    let services = forecast::enabled_services(cfg);
    if services.is_empty() {
        println!("no services enabled in config");
        return Ok(());
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut term = Terminal::new(CrosstermBackend::new(stdout))?;

    let mut selected = 0usize;
    let mut data = refresh(pool, cfg, &services);
    let mut last = Instant::now();

    let res = (|| -> Result<()> {
        loop {
            term.draw(|f| ui(f, &data, selected))?;
            if event::poll(Duration::from_millis(250))? {
                if let Event::Key(k) = event::read()? {
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                            if !services.is_empty() {
                                selected = (selected + 1) % services.len();
                            }
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            if !services.is_empty() {
                                selected = (selected + services.len() - 1) % services.len();
                            }
                        }
                        KeyCode::Char('r') => {
                            data = refresh(pool, cfg, &services);
                            last = Instant::now();
                        }
                        _ => {}
                    }
                }
            }
            if last.elapsed() > Duration::from_secs(3) {
                data = refresh(pool, cfg, &services);
                last = Instant::now();
            }
            if selected >= data.len().max(1) {
                selected = 0;
            }
        }
        Ok(())
    })();

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    res
}

fn label(service: &str) -> &str {
    match service {
        "claude_code" => "Claude Code",
        "codex" => "Codex",
        "openrouter" => "OpenRouter",
        other => other,
    }
}

fn pct(v: Option<f64>) -> String {
    v.map(|p| format!("{:.0}%", p * 100.0)).unwrap_or_else(|| "—".into())
}

fn ui(f: &mut Frame, data: &[Cumulative], selected: usize) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                              // title
            Constraint::Length(data.len() as u16 * 3 + 1),      // gauges
            Constraint::Min(8),                                 // chart
            Constraint::Length(1),                              // footer
        ])
        .split(f.area());

    f.render_widget(
        Paragraph::new(Span::styled(
            " metoks — weekly budget",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )),
        outer[0],
    );

    // Gauges, one per service.
    if !data.is_empty() {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints(vec![Constraint::Length(3); data.len()])
            .split(outer[1]);
        for (i, c) in data.iter().enumerate() {
            let color = status_color(&c.status);
            let sel = i == selected;
            let title = format!(
                " {} {}",
                if sel { "▸" } else { " " },
                label(&c.service)
            );
            let ratio = c.pct_now.unwrap_or(0.0).clamp(0.0, 1.0);
            let lbl = format!(
                "now {} · proj {}{}",
                pct(c.pct_now),
                pct(c.pct_projected),
                if c.low_confidence { " (early)" } else { "" }
            );
            let g = Gauge::default()
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(if sel {
                            Style::default().fg(Color::Cyan)
                        } else {
                            Style::default().fg(Color::DarkGray)
                        })
                        .title(Span::styled(title, Style::default().add_modifier(Modifier::BOLD))),
                )
                .gauge_style(Style::default().fg(color))
                .ratio(ratio)
                .label(lbl);
            f.render_widget(g, rows[i]);
        }
    }

    // Chart for the selected service.
    if let Some(c) = data.get(selected) {
        render_chart(f, outer[2], c);
    }

    f.render_widget(
        Paragraph::new(Span::styled(
            " q quit · ↑/↓ or Tab switch · r refresh",
            Style::default().fg(Color::DarkGray),
        )),
        outer[3],
    );
}

fn render_chart(f: &mut Frame, area: Rect, c: &Cumulative) {
    let color = status_color(&c.status);
    let ac = c.axis_cap.unwrap_or(0.0);
    let ws = c.window_start.timestamp() as f64;
    let we = c.window_end.timestamp() as f64;
    let span_h = ((we - ws) / 3600.0).max(1.0);
    let hours = |t: chrono::DateTime<chrono::Utc>| (t.timestamp() as f64 - ws) / 3600.0;

    // Actual cumulative % (tokens / axis_cap).
    let actual: Vec<(f64, f64)> = if ac > 0.0 {
        c.token_points
            .iter()
            .map(|p| (hours(p.ts), p.cum.iter().sum::<f64>() / ac * 100.0))
            .collect()
    } else {
        Vec::new()
    };
    let proj: Vec<(f64, f64)> = c.cone_pct.iter().map(|p| (hours(p.ts), p.mid)).collect();
    let band_hi: Vec<(f64, f64)> = c.cone_pct.iter().map(|p| (hours(p.ts), p.hi)).collect();
    let band_lo: Vec<(f64, f64)> = c.cone_pct.iter().map(|p| (hours(p.ts), p.lo)).collect();
    let fids: Vec<(f64, f64)> = c.fiducials.iter().map(|fd| (hours(fd.ts), fd.percent)).collect();
    let cap_line: Vec<(f64, f64)> = vec![(0.0, 100.0), (span_h, 100.0)];

    let mut datasets = vec![
        Dataset::default()
            .name("cap 100%")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Red))
            .data(&cap_line),
    ];
    if !band_hi.is_empty() {
        datasets.push(
            Dataset::default()
                .name("±1σ")
                .marker(symbols::Marker::Dot)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(Color::DarkGray))
                .data(&band_hi),
        );
        datasets.push(
            Dataset::default()
                .marker(symbols::Marker::Dot)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(Color::DarkGray))
                .data(&band_lo),
        );
    }
    if !proj.is_empty() {
        datasets.push(
            Dataset::default()
                .name("projected")
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(color).add_modifier(Modifier::DIM))
                .data(&proj),
        );
    }
    if !actual.is_empty() {
        datasets.push(
            Dataset::default()
                .name("observed")
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(color))
                .data(&actual),
        );
    }
    if !fids.is_empty() {
        datasets.push(
            Dataset::default()
                .name("readings")
                .marker(symbols::Marker::Block)
                .graph_type(GraphType::Scatter)
                .style(Style::default().fg(Color::White))
                .data(&fids),
        );
    }

    let day_labels: Vec<Span> = (0..=(span_h as i64 / 24))
        .map(|d| {
            let t = c.window_start + chrono::Duration::days(d);
            Span::raw(t.format("%a").to_string())
        })
        .collect();

    let title = format!(
        " {} — {} used, proj {}  (cap {})",
        label(&c.service),
        pct(c.pct_now),
        pct(c.pct_projected),
        c.cap
            .map(|v| crate::tui::fmt_tokens(v))
            .unwrap_or_else(|| "—".into()),
    );

    let chart = Chart::new(datasets)
        .block(Block::default().borders(Borders::ALL).title(title))
        .x_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([0.0, span_h])
                .labels(day_labels),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([0.0, 110.0])
                .labels(vec![
                    Span::raw("0"),
                    Span::raw("50"),
                    Span::raw("100"),
                ]),
        );
    f.render_widget(chart, area);
}

pub fn fmt_tokens(n: f64) -> String {
    if n >= 1e9 {
        format!("{:.2}B", n / 1e9)
    } else if n >= 1e6 {
        format!("{:.1}M", n / 1e6)
    } else if n >= 1e3 {
        format!("{:.1}K", n / 1e3)
    } else {
        format!("{n:.0}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forecast::{ConePoint, Cumulative, FiducialPoint, TokenCumPoint};
    use crate::models::Unit;
    use chrono::{Duration, Utc};
    use ratatui::backend::TestBackend;

    fn sample() -> Cumulative {
        let ws = Utc::now() - Duration::days(3);
        Cumulative {
            service: "claude_code".into(),
            unit: Unit::Tokens,
            mode: "fixed".into(),
            window_start: ws,
            window_end: ws + Duration::days(7),
            now: Utc::now(),
            cap: Some(1_000_000.0),
            cap_source: Some("fiducial".into()),
            consumed: 300_000.0,
            projected: 800_000.0,
            pct_now: Some(0.3),
            pct_projected: Some(0.8),
            status: "amber".into(),
            eta_to_limit: None,
            forecast_model: "trend".into(),
            low_confidence: false,
            axis_cap: Some(1_000_000.0),
            models: vec!["claude-opus-4-8".into()],
            token_points: vec![
                TokenCumPoint { ts: ws, cum: vec![0.0] },
                TokenCumPoint { ts: Utc::now(), cum: vec![300_000.0] },
            ],
            cone_pct: vec![
                ConePoint { ts: Utc::now(), lo: 30.0, mid: 30.0, hi: 30.0 },
                ConePoint { ts: ws + Duration::days(7), lo: 60.0, mid: 80.0, hi: 100.0 },
            ],
            fiducials: vec![FiducialPoint { ts: Utc::now(), percent: 28.0 }],
            pace_weekly: 500_000.0,
            pace_sigma: 100_000.0,
        }
    }

    #[test]
    fn renders_without_panic() {
        let backend = TestBackend::new(120, 40);
        let mut term = Terminal::new(backend).unwrap();
        let data = vec![sample()];
        term.draw(|f| ui(f, &data, 0)).unwrap();
    }
}
