//! Terminal experience for browsing and repossessing sessions.
//!
//! The destination inherits the real terminal. Possess leaves alternate-screen mode first
//! so nested TUIs do not corrupt scrollback or raw-input state.

use crate::domain::{Harness, LaunchRequest, SessionSummary, TargetKind, TargetOption};
use crate::engine::Engine;
use crate::launcher;
use anyhow::Result;
use chrono::{DateTime, Utc};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap,
};
use ratatui::{Frame, Terminal};
use std::io::{self, Stdout};
use std::time::{Duration, Instant};

type Term = Terminal<CrosstermBackend<Stdout>>;

#[derive(Debug)]
enum Mode {
    Browse,
    Search,
    Wizard(WizardChoice),
    Help,
}

#[derive(Debug, Default, Clone, Copy)]
struct WizardChoice {
    step: usize,
    destination: usize,
    model: usize,
    agent: usize,
}

struct App {
    sessions: Vec<SessionSummary>,
    visible: Vec<usize>,
    table: TableState,
    filter: String,
    mode: Mode,
    status: String,
    tick: usize,
    should_quit: bool,
}

impl App {
    fn new(sessions: Vec<SessionSummary>) -> Self {
        let visible = (0..sessions.len()).collect();
        let mut table = TableState::default();
        if !sessions.is_empty() {
            table.select(Some(0));
        }
        Self {
            sessions,
            visible,
            table,
            filter: String::new(),
            mode: Mode::Browse,
            status: "Enter handoff  •  r resume  •  / search  •  R refresh  •  ? help".into(),
            tick: 0,
            should_quit: false,
        }
    }

    fn selected(&self) -> Option<&SessionSummary> {
        self.table
            .selected()
            .and_then(|row| self.visible.get(row))
            .and_then(|idx| self.sessions.get(*idx))
    }
    fn next(&mut self) {
        if self.visible.is_empty() {
            return;
        }
        let index = self
            .table
            .selected()
            .map(|i| (i + 1).min(self.visible.len() - 1))
            .unwrap_or(0);
        self.table.select(Some(index));
    }
    fn previous(&mut self) {
        let index = self.table.selected().unwrap_or(0).saturating_sub(1);
        self.table.select(Some(index));
    }
    fn move_by(&mut self, rows: isize) {
        if self.visible.is_empty() {
            return;
        }
        let current = self.table.selected().unwrap_or(0) as isize;
        let last = self.visible.len().saturating_sub(1) as isize;
        self.table
            .select(Some((current + rows).clamp(0, last) as usize));
    }
    fn apply_filter(&mut self) {
        let needle = self.filter.to_ascii_lowercase();
        self.visible = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| {
                needle.is_empty()
                    || [
                        session.title.as_str(),
                        session.preview.as_str(),
                        session.qualified_id.as_str(),
                        session.cwd.to_string_lossy().as_ref(),
                        session.harness.command(),
                    ]
                    .iter()
                    .any(|v| fuzzy(v, &needle))
            })
            .map(|(idx, _)| idx)
            .collect();
        self.table.select((!self.visible.is_empty()).then_some(0));
    }
}

pub fn run(engine: &Engine) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    let result = run_loop(&mut terminal, engine);
    restore(&mut terminal)?;
    result
}

fn run_loop(terminal: &mut Term, engine: &Engine) -> Result<()> {
    let mut app = App::new(engine.scan());
    let mut last_tick = Instant::now();
    while !app.should_quit {
        terminal.draw(|frame| draw(frame, &mut app, engine))?;
        let timeout = Duration::from_millis(120).saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    handle_key(terminal, engine, &mut app, key.code, key.modifiers)?;
                }
                Event::Mouse(mouse) if matches!(app.mode, Mode::Browse) => match mouse.kind {
                    MouseEventKind::ScrollDown => app.next(),
                    MouseEventKind::ScrollUp => app.previous(),
                    _ => {}
                },
                _ => {}
            }
        }
        if last_tick.elapsed() >= Duration::from_millis(120) {
            app.tick = app.tick.wrapping_add(1);
            last_tick = Instant::now();
        }
    }
    Ok(())
}

fn handle_key(
    terminal: &mut Term,
    engine: &Engine,
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> Result<()> {
    if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
        app.should_quit = true;
        return Ok(());
    }
    match &mut app.mode {
        Mode::Search => match code {
            KeyCode::Esc | KeyCode::Enter => app.mode = Mode::Browse,
            KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
                app.filter.clear();
                app.apply_filter();
            }
            KeyCode::Backspace => {
                app.filter.pop();
                app.apply_filter();
            }
            KeyCode::Char(c) => {
                app.filter.push(c);
                app.apply_filter();
            }
            _ => {}
        },
        Mode::Help => {
            app.mode = Mode::Browse;
        }
        Mode::Browse => match code {
            KeyCode::Char('q') => app.should_quit = true,
            KeyCode::Char('?') => app.mode = Mode::Help,
            KeyCode::Char('/') => app.mode = Mode::Search,
            KeyCode::Down | KeyCode::Char('j') => app.next(),
            KeyCode::Up | KeyCode::Char('k') => app.previous(),
            KeyCode::PageDown => app.move_by(10),
            KeyCode::PageUp => app.move_by(-10),
            KeyCode::Home | KeyCode::Char('g') => {
                app.table.select((!app.visible.is_empty()).then_some(0))
            }
            KeyCode::End | KeyCode::Char('G') => app.table.select(app.visible.len().checked_sub(1)),
            KeyCode::Enter => {
                if app.selected().is_some() {
                    app.mode = Mode::Wizard(WizardChoice::default());
                }
            }
            KeyCode::Char('r') => resume_selected(terminal, engine, app)?,
            KeyCode::Char('R') => {
                app.sessions = engine.scan();
                app.apply_filter();
                app.status = format!("Refreshed {} sessions", app.sessions.len());
            }
            _ => {}
        },
        Mode::Wizard(choice) => {
            let targets = engine.targets(Harness::ALL[choice.destination]);
            let count = match choice.step {
                0 => Harness::ALL.len(),
                1 => targets
                    .iter()
                    .filter(|t| t.kind == TargetKind::Model)
                    .count()
                    .max(1),
                _ => targets
                    .iter()
                    .filter(|t| t.kind == TargetKind::Agent)
                    .count()
                    .max(1),
            };
            match code {
                KeyCode::Esc => app.mode = Mode::Browse,
                KeyCode::Down | KeyCode::Char('j') => {
                    match choice.step {
                        0 => choice.destination = (choice.destination + 1).min(count - 1),
                        1 => choice.model = (choice.model + 1).min(count - 1),
                        _ => choice.agent = (choice.agent + 1).min(count - 1),
                    }
                    if choice.step == 0 {
                        choice.model = 0;
                        choice.agent = 0;
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    match choice.step {
                        0 => choice.destination = choice.destination.saturating_sub(1),
                        1 => choice.model = choice.model.saturating_sub(1),
                        _ => choice.agent = choice.agent.saturating_sub(1),
                    }
                    if choice.step == 0 {
                        choice.model = 0;
                        choice.agent = 0;
                    }
                }
                KeyCode::Left | KeyCode::BackTab => choice.step = choice.step.saturating_sub(1),
                KeyCode::Right | KeyCode::Tab | KeyCode::Enter if choice.step < 3 => {
                    choice.step += 1
                }
                KeyCode::Enter if choice.step == 3 => {
                    let destination_value = Harness::ALL[choice.destination];
                    let selected_model = targets
                        .iter()
                        .filter(|t| t.kind == TargetKind::Model)
                        .nth(choice.model);
                    let selected_agent = targets
                        .iter()
                        .filter(|t| t.kind == TargetKind::Agent)
                        .nth(choice.agent);
                    perform_transfer(
                        terminal,
                        engine,
                        app,
                        destination_value,
                        selected_model,
                        selected_agent,
                    )?;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn perform_transfer(
    terminal: &mut Term,
    engine: &Engine,
    app: &mut App,
    destination: Harness,
    model: Option<&TargetOption>,
    agent: Option<&TargetOption>,
) -> Result<()> {
    let Some(summary) = app.selected().cloned() else {
        return Ok(());
    };
    app.status = format!("Possessing {}…", summary.title);
    terminal.draw(|frame| draw(frame, app, engine))?;
    let request = LaunchRequest {
        destination,
        model: model.map(|v| v.id.clone()).filter(|v| v != "default"),
        agent: agent.map(|v| v.id.clone()).filter(|v| v != "default"),
        context_tokens: engine.config.context_tokens,
        launch: true,
    };
    match engine.transfer(&summary, &request) {
        Ok(transfer) => {
            // The child needs direct ownership of raw input for approvals and its own TUI.
            // We rebuild our screen from fresh session metadata after it returns.
            suspend(terminal)?;
            let result = launcher::launch(&transfer.prepared);
            resume_terminal(terminal)?;
            app.sessions = engine.scan();
            app.visible = (0..app.sessions.len()).collect();
            app.table.select((!app.sessions.is_empty()).then_some(0));
            app.mode = Mode::Browse;
            app.status = match result {
                Ok(status) => format!(
                    "Returned from {} ({status}) • package {}",
                    destination, transfer.package.id
                ),
                Err(error) => format!(
                    "Launch failed: {error} • package preserved at {}",
                    transfer.package.dir.display()
                ),
            };
        }
        Err(error) => {
            app.mode = Mode::Browse;
            app.status = format!("Handoff failed: {error}");
        }
    }
    Ok(())
}

fn resume_selected(terminal: &mut Term, engine: &Engine, app: &mut App) -> Result<()> {
    let Some(summary) = app.selected().cloned() else {
        return Ok(());
    };
    let prepared = launcher::native_resume(&engine.config, &summary);
    suspend(terminal)?;
    let result = launcher::launch(&prepared);
    resume_terminal(terminal)?;
    app.sessions = engine.scan();
    app.visible = (0..app.sessions.len()).collect();
    app.status = result
        .map(|s| format!("Returned from {} ({s})", summary.harness))
        .unwrap_or_else(|e| format!("Resume failed: {e}"));
    Ok(())
}

fn draw(frame: &mut Frame, app: &mut App, engine: &Engine) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
            Constraint::Min(8),
            Constraint::Length(1),
        ])
        .split(area);
    draw_header(frame, chunks[0], app, engine);
    if area.width >= 100 {
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
            .split(chunks[1]);
        draw_table(frame, body[0], app);
        draw_preview(frame, body[1], app, engine);
    } else {
        draw_table(frame, chunks[1], app);
    }
    let status = if matches!(app.mode, Mode::Search) {
        format!("/{}█", app.filter)
    } else {
        app.status.clone()
    };
    frame.render_widget(
        Paragraph::new(status).style(Style::default().fg(Color::Rgb(143, 130, 190))),
        chunks[2],
    );
    match app.mode {
        Mode::Wizard(ref choice) => draw_wizard(frame, centered(area, 72, 72), app, engine, choice),
        Mode::Help => draw_help(frame, centered(area, 62, 60)),
        _ => {}
    }
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App, engine: &Engine) {
    let ghost = if engine.config.ascii_only {
        vec![
            "    .-.  ",
            "  .'   '.",
            " /  o o  \\",
            "|    ^    |",
            " '.___.'",
            " P O S S E S S",
        ]
    } else if area.width < 70 {
        vec!["  ▄▀▀▄", " █ ◕◕ █", " █  ▄ █", "  ▀▀▀▀", " POSSESS"]
    } else {
        vec![
            "       ▄████▄",
            "     ▄█ ◉  ◉ █▄",
            "    █     ▄    █",
            "    █  ▄████▄  █",
            "     ▀█▄▀  ▀▄█▀",
            "  P O S S E S S",
        ]
    };
    let pulse = [
        Color::Rgb(132, 92, 246),
        Color::Rgb(153, 112, 255),
        Color::Rgb(111, 210, 255),
    ][if engine.config.reduced_motion {
        1
    } else {
        (app.tick / 3) % 3
    }];
    let lines: Vec<Line> = ghost
        .into_iter()
        .map(|line| {
            Line::from(Span::styled(
                line,
                Style::default().fg(pulse).add_modifier(Modifier::BOLD),
            ))
        })
        .collect();
    let title = Paragraph::new(lines).alignment(Alignment::Left);
    frame.render_widget(title, area);
    let tagline = Paragraph::new(Text::from(vec![
        Line::from(Span::styled(
            "REPOSSESS YOUR CONTEXT",
            Style::default()
                .fg(Color::Rgb(211, 203, 255))
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "Cross-harness session continuity",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!("{}", app.visible.len()),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" sessions  •  "),
            Span::styled(
                "4",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" harnesses"),
        ]),
    ]))
    .alignment(Alignment::Right);
    frame.render_widget(tagline, area);
}

fn draw_table(frame: &mut Frame, area: Rect, app: &mut App) {
    let rows = app
        .visible
        .iter()
        .filter_map(|idx| app.sessions.get(*idx))
        .map(|session| {
            Row::new(vec![
                format!("{} {}", session.harness.icon(), session.harness),
                session.title.clone(),
                project_name(session),
                session.model.clone().unwrap_or_else(|| "default".into()),
                relative_time(session.updated_at.or(session.created_at)),
            ])
        });
    let table = Table::new(
        rows,
        [
            Constraint::Length(15),
            Constraint::Min(22),
            Constraint::Length(18),
            Constraint::Length(16),
            Constraint::Length(8),
        ],
    )
    .header(
        Row::new(["HARNESS", "SESSION", "PROJECT", "MODEL", "AGE"]).style(
            Style::default()
                .fg(Color::Rgb(157, 138, 245))
                .add_modifier(Modifier::BOLD),
        ),
    )
    .row_highlight_style(
        Style::default()
            .bg(Color::Rgb(47, 37, 73))
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("  ▸ ")
    .block(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::Rgb(67, 58, 95)))
            .title(format!(
                " Sessions{} ",
                if app.filter.is_empty() {
                    "".into()
                } else {
                    format!(" • filter: {}", app.filter)
                }
            )),
    );
    frame.render_stateful_widget(table, area, &mut app.table);
}

fn draw_preview(frame: &mut Frame, area: Rect, app: &App, engine: &Engine) {
    let Some(session) = app.selected() else {
        frame.render_widget(
            Paragraph::new("No sessions found. Run `possess doctor` to inspect adapters.")
                .block(Block::default().borders(Borders::LEFT)),
            area,
        );
        return;
    };
    let fidelity = Harness::ALL
        .iter()
        .map(|h| format!("{} → {}", h, engine.fidelity(session.harness, *h)))
        .collect::<Vec<_>>()
        .join("\n");
    let text = vec![
        Line::from(Span::styled(
            &session.title,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("ID  ", Style::default().fg(Color::DarkGray)),
            Span::raw(&session.qualified_id),
        ]),
        Line::from(vec![
            Span::styled("CWD ", Style::default().fg(Color::DarkGray)),
            Span::raw(session.cwd.display().to_string()),
        ]),
        Line::from(vec![
            Span::styled("Updated ", Style::default().fg(Color::DarkGray)),
            Span::raw(
                session
                    .updated_at
                    .map(|v| v.to_rfc3339())
                    .unwrap_or_else(|| "unknown".into()),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "LAST KNOWN STATE",
            Style::default()
                .fg(Color::Rgb(157, 138, 245))
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(session.preview.clone()),
        Line::from(""),
        Line::from(Span::styled(
            "DESTINATION FIDELITY",
            Style::default()
                .fg(Color::Rgb(157, 138, 245))
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(fidelity),
    ];
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: true }).block(
            Block::default()
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(Color::Rgb(67, 58, 95)))
                .padding(ratatui::widgets::Padding::horizontal(2)),
        ),
        area,
    );
}

fn draw_wizard(frame: &mut Frame, area: Rect, app: &App, engine: &Engine, choice: &WizardChoice) {
    let WizardChoice {
        step,
        destination,
        model,
        agent,
    } = *choice;
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(" POSSESS SESSION ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Rgb(153, 112, 255)))
        .style(Style::default().bg(Color::Rgb(19, 16, 29)));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(4),
        Constraint::Length(3),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(format!(
            "1 Destination {}  2 Model {}  3 Agent {}  4 Confirm",
            if step == 0 { "●" } else { "✓" },
            if step == 1 {
                "●"
            } else if step > 1 {
                "✓"
            } else {
                "○"
            },
            if step == 2 {
                "●"
            } else if step > 2 {
                "✓"
            } else {
                "○"
            }
        ))
        .alignment(Alignment::Center)
        .style(Style::default().fg(Color::Rgb(190, 174, 255))),
        chunks[0],
    );
    if step == 0 {
        let items = Harness::ALL.iter().map(|harness| {
            ListItem::new(format!(
                " {}  {:<14}  {}",
                harness.icon(),
                harness,
                engine.fidelity(
                    app.selected().map(|s| s.harness).unwrap_or(*harness),
                    *harness
                )
            ))
        });
        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .bg(Color::Rgb(47, 37, 73))
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▸");
        let mut state = ListState::default().with_selected(Some(destination));
        frame.render_stateful_widget(list, chunks[1], &mut state);
    } else if step == 1 || step == 2 {
        let targets = engine.targets(Harness::ALL[destination]);
        let kind = if step == 1 {
            TargetKind::Model
        } else {
            TargetKind::Agent
        };
        let models: Vec<_> = targets.into_iter().filter(|v| v.kind == kind).collect();
        let labels = if models.is_empty() {
            vec![ListItem::new(if step == 1 {
                " Use destination default model"
            } else {
                " Use destination default agent"
            })]
        } else {
            models
                .iter()
                .map(|v| ListItem::new(format!(" {}", v.label)))
                .collect()
        };
        let mut state =
            ListState::default().with_selected(Some(if step == 1 { model } else { agent }));
        frame.render_stateful_widget(
            List::new(labels)
                .highlight_style(Style::default().bg(Color::Rgb(47, 37, 73)))
                .highlight_symbol("▸"),
            chunks[1],
            &mut state,
        );
    } else {
        let source = app.selected().unwrap();
        let destination = Harness::ALL[destination];
        let targets = engine.targets(destination);
        let model_label = targets
            .iter()
            .filter(|target| target.kind == TargetKind::Model)
            .nth(model)
            .map(|target| target.label.as_str())
            .unwrap_or("destination default");
        let agent_label = targets
            .iter()
            .filter(|target| target.kind == TargetKind::Agent)
            .nth(agent)
            .map(|target| target.label.as_str())
            .unwrap_or("destination default");
        let body = format!(
            "{} {}\n\nwill be repossessed by\n\n{} {}\n\nModel: {}\nAgent/profile: {}\nFidelity: {}\nContext: smart • {} tokens\nWorkspace: {}\n\nA private full archive is created before launch.",
            source.harness.icon(),
            source.title,
            destination.icon(),
            destination,
            model_label,
            agent_label,
            engine.fidelity(source.harness, destination),
            engine.config.context_tokens,
            source.cwd.display()
        );
        frame.render_widget(
            Paragraph::new(body)
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
            chunks[1],
        );
    }
    frame.render_widget(
        Paragraph::new(if step == 3 {
            "Enter repossess  •  ← back  •  Esc cancel"
        } else {
            "Enter/→ next  •  ↑↓ choose  •  Esc cancel"
        })
        .alignment(Alignment::Center)
        .style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );
}

fn draw_help(frame: &mut Frame, area: Rect) {
    frame.render_widget(Clear, area);
    let help = "NAVIGATION\n  j/k or ↑/↓    select session\n  PgUp/PgDn      jump ten sessions\n  g/G            first / last\n  /              fuzzy search (Ctrl+U clears)\n\nACTIONS\n  Enter          repossess into another harness\n  r              resume in original harness\n  R              refresh all stores\n  ?              this help\n  q / Ctrl+C     quit\n\nPossess never mutates the source session.";
    frame.render_widget(
        Paragraph::new(help)
            .block(
                Block::default()
                    .title(" HELP ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .padding(ratatui::widgets::Padding::uniform(2)),
            )
            .style(Style::default().bg(Color::Rgb(19, 16, 29))),
        area,
    );
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - height) / 2),
        Constraint::Percentage(height),
        Constraint::Percentage((100 - height) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - width) / 2),
        Constraint::Percentage(width),
        Constraint::Percentage((100 - width) / 2),
    ])
    .split(vertical[1])[1]
}

fn project_name(session: &SessionSummary) -> String {
    session
        .cwd
        .file_name()
        .map(|v| v.to_string_lossy().to_string())
        .unwrap_or_else(|| "—".into())
}
fn relative_time(time: Option<DateTime<Utc>>) -> String {
    let Some(time) = time else {
        return "—".into();
    };
    let seconds = (Utc::now() - time).num_seconds().max(0);
    if seconds < 60 {
        "now".into()
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3600)
    } else if seconds < 2_592_000 {
        format!("{}d", seconds / 86_400)
    } else {
        time.format("%b %d").to_string()
    }
}
fn fuzzy(value: &str, needle: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if lower.contains(needle) {
        return true;
    }
    let mut chars = lower.chars();
    needle
        .chars()
        .all(|wanted| chars.by_ref().any(|candidate| candidate == wanted))
}

fn suspend(terminal: &mut Term) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}
fn resume_terminal(terminal: &mut Term) -> Result<()> {
    enable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableMouseCapture
    )?;
    terminal.clear()?;
    Ok(())
}
fn restore(terminal: &mut Term) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}
