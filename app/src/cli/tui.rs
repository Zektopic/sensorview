//! `sensorview top` — a live terminal dashboard.
//!
//! The point is remote use: over SSH on a headless box there is no window, and
//! `stream` is for machines rather than eyes.
//!
//! # Terminal restoration is a correctness requirement, not polish
//!
//! Raw mode plus the alternate screen changes global terminal state that
//! outlives the process. The release profile is `panic = "abort"`, so there is
//! no unwinding and no `Drop` to fall back on — a panic in the render loop
//! would leave the user with no echo and no line discipline, in a shell they
//! then have to `reset`. So teardown is installed as a panic hook *before*
//! entering raw mode, and runs on every exit path.

use std::io::{self, Stdout};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table};
use ratatui::{Frame, Terminal};

use crate::format::format_value;
use crate::model::{Hardware, Sensor, SensorType};
use crate::poll;
use crate::runtime::Runtime;
use crate::state::TelemetryFrame;

use super::render::matches;

/// Set once the terminal is in raw mode, so the panic hook only undoes state
/// that was actually applied.
static RAW_MODE_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn run(rt: &Runtime, filter: Option<String>, timeout: Duration) -> ExitCode {
    if let Err(f) = rt.wait_for_usable_frame(timeout) {
        if f.seq == 0 {
            eprintln!("sensorview: no sensor data after {timeout:?} — is a backend available?");
            return ExitCode::FAILURE;
        }
    }

    // Installed *before* raw mode so a panic between here and the first draw is
    // still cleaned up.
    install_panic_hook();

    let mut terminal = match enter() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("sensorview: could not initialise the terminal: {e}");
            return ExitCode::FAILURE;
        }
    };

    let result = event_loop(&mut terminal, rt, filter.map(|f| f.to_lowercase()));
    leave();

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sensorview: {e}");
            ExitCode::FAILURE
        }
    }
}

fn event_loop(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
    rt: &Runtime,
    filter: Option<String>,
) -> io::Result<()> {
    let started = Instant::now();
    loop {
        let frame = rt.store.load();
        terminal.draw(|f| draw(f, &frame, filter.as_deref(), started))?;

        // Poll for input on a short timeout so the display still refreshes when
        // nobody is typing.
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                // Windows reports both press and release; acting on both would
                // double every keystroke.
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                        return Ok(())
                    }
                    KeyCode::Char('r') => {
                        // Same command the GUI's Reset Min/Max button sends.
                        let _ = rt.poller.sender().send(poll::Command::ResetMinMax);
                    }
                    _ => {}
                }
            }
        }
    }
}

fn draw(f: &mut Frame, frame: &TelemetryFrame, filter: Option<&str>, started: Instant) {
    let areas = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Length(4), // gauges
        Constraint::Min(3),    // table
        Constraint::Length(1), // footer
    ])
    .split(f.area());

    let uptime = started.elapsed().as_secs();
    let header = Line::from(vec![
        Span::styled(
            "SensorView",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "  {}  ·  {} sensors  ·  seq {}  ·  {}:{:02}:{:02}",
            frame.source,
            frame.sensor_count(),
            frame.seq,
            uptime / 3600,
            (uptime % 3600) / 60,
            uptime % 60
        )),
    ]);
    f.render_widget(Paragraph::new(header), areas[0]);

    // Headline gauges: the three numbers anyone actually watches.
    let gauge_areas = Layout::horizontal([
        Constraint::Ratio(1, 3),
        Constraint::Ratio(1, 3),
        Constraint::Ratio(1, 3),
    ])
    .split(areas[1]);
    let cpu = first_matching(&frame.tree, SensorType::Load, "total cpu");
    let mem = first_matching(&frame.tree, SensorType::Load, "memory");
    let gpu = first_matching(&frame.tree, SensorType::Load, "gpu");
    for (i, (label, sensor)) in [("CPU", cpu), ("Memory", mem), ("GPU", gpu)].iter().enumerate() {
        let pct = sensor.and_then(|s| s.value).unwrap_or(0.0).clamp(0.0, 100.0);
        f.render_widget(
            Gauge::default()
                .block(Block::default().borders(Borders::ALL).title(*label))
                .gauge_style(Style::default().fg(gauge_colour(pct)))
                .ratio(f64::from(pct) / 100.0)
                .label(match sensor.and_then(|s| s.value) {
                    Some(v) => format!("{v:.0}%"),
                    // Distinguishes "idle" from "no such sensor".
                    None => "—".to_string(),
                }),
            gauge_areas[i],
        );
    }

    let rows = table_rows(&frame.tree, filter);
    let table = Table::new(
        rows,
        [
            Constraint::Min(24),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(12),
        ],
    )
    .header(
        Row::new(["Sensor", "Current", "Min", "Max"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(table, areas[2]);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " q quit   r reset min/max ",
            Style::default().fg(Color::DarkGray),
        ))),
        areas[3],
    );
}

fn gauge_colour(pct: f32) -> Color {
    match pct {
        p if p >= 85.0 => Color::Red,
        p if p >= 60.0 => Color::Yellow,
        _ => Color::Green,
    }
}

/// Flatten the tree into rows, with a heading row per hardware node.
fn table_rows<'a>(tree: &'a [Hardware], filter: Option<&str>) -> Vec<Row<'a>> {
    let mut rows = Vec::new();
    fn walk<'a>(rows: &mut Vec<Row<'a>>, tree: &'a [Hardware], filter: Option<&str>) {
        for hw in tree {
            let shown: Vec<&Sensor> = hw
                .sensors
                .iter()
                .filter(|s| filter.is_none_or(|n| matches(s, n)))
                .collect();
            if !shown.is_empty() {
                rows.push(
                    Row::new(vec![Cell::from(hw.name.clone())]).style(
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    ),
                );
                for s in shown {
                    rows.push(Row::new(vec![
                        Cell::from(format!("  {}", s.name)),
                        Cell::from(format_value(s.value, s.sensor_type))
                            .style(Style::default().fg(Color::White)),
                        Cell::from(format_value(s.min, s.sensor_type))
                            .style(Style::default().fg(Color::DarkGray)),
                        Cell::from(format_value(s.max, s.sensor_type))
                            .style(Style::default().fg(Color::DarkGray)),
                    ]));
                }
            }
            walk(rows, &hw.sub_hardware, filter);
        }
    }
    walk(&mut rows, tree, filter);
    rows
}

/// First sensor of `ty` whose name contains `needle`, anywhere in the tree.
fn first_matching<'a>(tree: &'a [Hardware], ty: SensorType, needle: &str) -> Option<&'a Sensor> {
    for hw in tree {
        for s in &hw.sensors {
            if s.sensor_type == ty && s.name.to_lowercase().contains(needle) {
                return Some(s);
            }
        }
        if let Some(found) = first_matching(&hw.sub_hardware, ty, needle) {
            return Some(found);
        }
    }
    None
}

// ---- Terminal state ------------------------------------------------------

fn enter() -> io::Result<Terminal<ratatui::backend::CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    RAW_MODE_ACTIVE.store(true, Ordering::SeqCst);
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    Terminal::new(ratatui::backend::CrosstermBackend::new(stdout))
}

/// Undo everything `enter` did. Safe to call twice, and safe to call when
/// `enter` failed partway.
fn leave() {
    if !RAW_MODE_ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let _ = io::stdout().execute(LeaveAlternateScreen);
    let _ = disable_raw_mode();
}

/// Restore the terminal before the default hook prints the panic message —
/// otherwise the message itself is rendered into the alternate screen and
/// vanishes with it, leaving a broken terminal and no explanation.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        leave();
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::HardwareType;

    fn s(name: &str, ty: SensorType, value: Option<f32>) -> Sensor {
        Sensor {
            identifier: format!("/{name}"),
            name: name.into(),
            sensor_type: ty,
            index: 0,
            value,
            min: Some(1.0),
            max: Some(2.0),
            avg: None,
        }
    }

    fn tree() -> Vec<Hardware> {
        vec![Hardware {
            identifier: "/cpu".into(),
            name: "Apple M5".into(),
            hardware_type: HardwareType::Cpu,
            sensors: vec![
                s("Total CPU Usage", SensorType::Load, Some(42.0)),
                s("PMU tdie6", SensorType::Temperature, Some(38.0)),
            ],
            sub_hardware: vec![Hardware {
                identifier: "/gpu".into(),
                name: "GPU".into(),
                hardware_type: HardwareType::GpuApple,
                sensors: vec![s("GPU Core", SensorType::Load, Some(9.0))],
                sub_hardware: Vec::new(),
            }],
        }]
    }

    #[test]
    fn headline_gauges_find_their_sensors_including_nested() {
        assert_eq!(
            first_matching(&tree(), SensorType::Load, "total cpu").unwrap().value,
            Some(42.0)
        );
        // The GPU is a sub_hardware node.
        assert_eq!(
            first_matching(&tree(), SensorType::Load, "gpu").unwrap().value,
            Some(9.0)
        );
        // Type must match too — there is no Load sensor called "tdie".
        assert!(first_matching(&tree(), SensorType::Load, "tdie").is_none());
    }

    #[test]
    fn rows_include_a_heading_per_node_and_recurse() {
        // Heading + 2 sensors, then heading + 1 sensor.
        assert_eq!(table_rows(&tree(), None).len(), 5);
    }

    #[test]
    fn filtered_nodes_contribute_no_heading() {
        // Only "Total CPU Usage" matches, so the GPU node adds nothing at all.
        assert_eq!(table_rows(&tree(), Some("total cpu")).len(), 2);
        assert_eq!(table_rows(&tree(), Some("zzz")).len(), 0);
    }

    #[test]
    fn gauge_colour_escalates_with_load() {
        assert_eq!(gauge_colour(10.0), Color::Green);
        assert_eq!(gauge_colour(70.0), Color::Yellow);
        assert_eq!(gauge_colour(95.0), Color::Red);
    }

    /// `leave` runs from both the normal exit path and the panic hook, so it
    /// has to tolerate being called when raw mode was never entered.
    #[test]
    fn leave_is_idempotent_and_safe_without_enter() {
        assert!(!RAW_MODE_ACTIVE.load(Ordering::SeqCst));
        leave();
        leave();
    }

    /// Renders the whole dashboard into an in-memory buffer, which needs no
    /// TTY — so the layout is covered by CI and not only by looking at it.
    #[test]
    fn dashboard_renders_headline_values_and_rows() {
        let frame = TelemetryFrame { seq: 7, tree: tree(), ..Default::default() };
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|f| draw(f, &frame, None, Instant::now()))
            .expect("draw must not fail");

        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();

        assert!(text.contains("SensorView"), "header missing:\n{text}");
        assert!(text.contains("seq 7"), "sequence missing");
        // Gauge labels: CPU is 42 %, GPU 9 %.
        assert!(text.contains("42%"), "CPU gauge missing");
        assert!(text.contains("9%"), "GPU gauge missing");
        // Table content, including a nested node's sensor.
        assert!(text.contains("Apple M5"), "hardware heading missing");
        assert!(text.contains("PMU tdie6"), "sensor row missing");
        assert!(text.contains("q quit"), "footer missing");
    }

    /// A sensor with no reading must render as an em dash in the gauge label,
    /// not as 0 % — "idle" and "not present" are different states.
    #[test]
    fn absent_headline_sensor_renders_as_a_dash() {
        let frame = TelemetryFrame {
            seq: 1,
            tree: vec![Hardware {
                identifier: "/x".into(),
                name: "X".into(),
                hardware_type: HardwareType::Cpu,
                sensors: vec![s("Total CPU Usage", SensorType::Load, None)],
                sub_hardware: Vec::new(),
            }],
            ..Default::default()
        };
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &frame, None, Instant::now())).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("—"), "absent reading must show an em dash:\n{text}");
    }
}
