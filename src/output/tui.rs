//! Live dashboard for `--tui`: per link, a chart of the motion score over the
//! last minute with the threshold line and the state; the current zone when
//! there are several links; and recent events and log messages.
//!
//! Quit with `q`, `Esc` or Ctrl+C. The terminal is restored on exit, on
//! errors (`Drop`) and on panic: `ratatui::try_init` installs a panic hook
//! that restores the terminal before the default hook prints the panic.

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use anyhow::{Context, Result};
use ratatui::crossterm::event::{self, Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, Chart, Dataset, GraphType, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use super::{clock, format_event, set_log_capture, take_captured_logs, unit, EventSink};
use crate::detector::{Event, MotionState, ScoreKind};

/// Seconds of score history shown per link.
const HISTORY_S: f64 = 60.0;
/// Lines of events/logs kept.
const LOG_LINES: usize = 200;
/// Rows of the events/log pane.
const LOG_ROWS: u16 = 8;

struct LinkPanel {
    kind: ScoreKind,
    /// (seconds since start, score)
    history: VecDeque<(f64, f64)>,
    score: Option<f32>,
    threshold: Option<f32>,
    state: MotionState,
}

pub struct TuiSink {
    terminal: Option<DefaultTerminal>,
    links: BTreeMap<String, LinkPanel>,
    zone: Option<(String, f32)>,
    lines: VecDeque<String>,
    first_ts: Option<u64>,
    now_s: f64,
}

impl TuiSink {
    /// Take over the terminal (raw mode, alternate screen).
    pub fn start() -> Result<Self> {
        set_log_capture(true);
        let terminal = match ratatui::try_init() {
            Ok(t) => t,
            Err(e) => {
                ratatui::restore();
                set_log_capture(false);
                return Err(e).context("starting the terminal UI (is this a terminal?)");
            }
        };
        Ok(Self {
            terminal: Some(terminal),
            links: BTreeMap::new(),
            zone: None,
            lines: VecDeque::new(),
            first_ts: None,
            now_s: 0.0,
        })
    }

    fn push_line(&mut self, line: String) {
        if self.lines.len() == LOG_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    fn restore(&mut self) {
        if self.terminal.take().is_some() {
            ratatui::restore();
            set_log_capture(false);
            // Anything logged since the last redraw would otherwise be lost.
            for line in take_captured_logs() {
                eprintln!("{line}");
            }
        }
    }

    fn draw(&mut self) -> Result<()> {
        let Some(mut terminal) = self.terminal.take() else {
            return Ok(());
        };
        // The completed frame borrows the terminal; we only need success.
        let result = terminal.draw(|frame| render(frame, self)).map(|_| ());
        self.terminal = Some(terminal);
        result.context("drawing the terminal UI")?;
        Ok(())
    }
}

impl EventSink for TuiSink {
    fn handle(&mut self, event: &Event) -> Result<()> {
        match event {
            Event::Status {
                link_id,
                ts_unix_ms,
                kind,
                score,
                threshold,
                state,
            } => {
                let first = *self.first_ts.get_or_insert(*ts_unix_ms);
                let t = ts_unix_ms.saturating_sub(first) as f64 / 1000.0;
                self.now_s = self.now_s.max(t);
                let panel = self
                    .links
                    .entry(link_id.clone())
                    .or_insert_with(|| LinkPanel {
                        kind: *kind,
                        history: VecDeque::new(),
                        score: None,
                        threshold: None,
                        state: *state,
                    });
                if let Some(s) = score {
                    panel.history.push_back((t, f64::from(*s)));
                }
                while panel
                    .history
                    .front()
                    .is_some_and(|(old, _)| t - old > HISTORY_S)
                {
                    panel.history.pop_front();
                }
                panel.score = *score;
                panel.threshold = *threshold;
                panel.state = *state;
            }
            Event::ZoneChanged {
                zone, confidence, ..
            } => {
                self.zone = zone.clone().map(|z| (z, *confidence));
                if let Some(line) = format_event(event) {
                    self.push_line(line);
                }
            }
            Event::CalibrationStarted {
                link_id,
                ts_unix_ms,
                duration_ms,
            } => self.push_line(format!(
                "{} {link_id}: CALIBRATING for {:.0} s: keep the laptop still and leave the room",
                clock(*ts_unix_ms),
                *duration_ms as f64 / 1000.0
            )),
            other => {
                if let Some(line) = format_event(other) {
                    self.push_line(line);
                }
            }
        }
        Ok(())
    }

    fn tick(&mut self) -> Result<bool> {
        while event::poll(Duration::ZERO).context("reading keyboard")? {
            if let TermEvent::Key(key) = event::read().context("reading keyboard")? {
                let ctrl_c =
                    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
                if key.kind == KeyEventKind::Press
                    && (ctrl_c || matches!(key.code, KeyCode::Char('q') | KeyCode::Esc))
                {
                    return Ok(true);
                }
            }
        }
        for line in take_captured_logs() {
            self.push_line(format!("log: {line}"));
        }
        self.draw()?;
        Ok(false)
    }

    fn finish(&mut self) -> Result<()> {
        self.restore();
        Ok(())
    }
}

impl Drop for TuiSink {
    fn drop(&mut self) {
        self.restore();
    }
}

fn render(frame: &mut Frame, ui: &TuiSink) {
    let [header, body, log] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(LOG_ROWS + 2),
    ])
    .areas(frame.area());

    frame.render_widget(Paragraph::new(header_line(ui)), header);

    if ui.links.is_empty() {
        frame.render_widget(
            Paragraph::new("waiting for samples...").block(Block::bordered()),
            body,
        );
    } else {
        let rows = Layout::vertical(vec![Constraint::Fill(1); ui.links.len()]).split(body);
        for ((link, panel), area) in ui.links.iter().zip(rows.iter()) {
            render_link(frame, *area, link, panel, ui.now_s);
        }
    }

    let shown = usize::from(LOG_ROWS);
    let lines: Vec<Line> = ui
        .lines
        .iter()
        .skip(ui.lines.len().saturating_sub(shown))
        .map(|l| Line::from(l.as_str()))
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" events ")),
        log,
    );
}

fn header_line(ui: &TuiSink) -> Line<'static> {
    let mut spans = vec![Span::styled(
        " wisense ",
        Style::default().add_modifier(Modifier::BOLD),
    )];
    if ui.links.len() >= 2 {
        let zone = match &ui.zone {
            Some((zone, lead)) => Span::styled(
                format!(" zone: {zone} (lead {lead:.2}) "),
                Style::default().fg(Color::Black).bg(Color::Yellow),
            ),
            None => Span::raw(" zone: none "),
        };
        spans.push(zone);
    }
    spans.push(Span::styled(
        "  q quit",
        Style::default().fg(Color::DarkGray),
    ));
    Line::from(spans)
}

fn render_link(frame: &mut Frame, area: Rect, link: &str, p: &LinkPanel, now_s: f64) {
    let u = unit(p.kind);
    let score = p
        .score
        .map_or_else(|| "--".to_string(), |s| format!("{s:.2}{u}"));
    let (state, color) = match p.state {
        MotionState::Calibrating => ("calibrating", Color::Blue),
        MotionState::Idle => ("idle", Color::Green),
        MotionState::Motion => ("MOTION", Color::Red),
    };
    let threshold = p
        .threshold
        .map_or_else(|| "--".to_string(), |t| format!("{t:.2}{u}"));
    let title = Line::from(vec![
        Span::raw(format!(" {link} ")),
        Span::styled(
            format!(" {state} "),
            Style::default().fg(Color::Black).bg(color),
        ),
        Span::raw(format!(" score {score}  threshold {threshold} ")),
    ]);

    let x_min = now_s - HISTORY_S;
    let scores: Vec<(f64, f64)> = p.history.iter().copied().collect();
    let peak = scores.iter().map(|(_, s)| *s).fold(0.0, f64::max);
    let thr = p.threshold.map(f64::from);
    let y_max = (thr.unwrap_or(0.0) * 2.0).max(peak * 1.1).max(0.1);
    let thr_line: Vec<(f64, f64)> = thr
        .map(|t| vec![(x_min, t), (now_s, t)])
        .unwrap_or_default();

    let mut datasets = vec![Dataset::default()
        .marker(Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Cyan))
        .data(&scores)];
    if !thr_line.is_empty() {
        datasets.push(
            Dataset::default()
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(Color::Red))
                .data(&thr_line),
        );
    }
    let chart = Chart::new(datasets)
        .block(
            Block::bordered()
                .title(title)
                .border_style(Style::default().fg(color)),
        )
        .legend_position(None)
        .x_axis(
            Axis::default()
                .bounds([x_min, now_s])
                .labels(["-60 s", "-30 s", "now"])
                .style(Style::default().fg(Color::DarkGray)),
        )
        .y_axis(
            Axis::default()
                .bounds([0.0, y_max])
                .labels(["0".to_string(), format!("{y_max:.1}")])
                .style(Style::default().fg(Color::DarkGray)),
        );
    frame.render_widget(chart, area);
}
