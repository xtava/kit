use anyhow::{Context as _, Result};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Margin, Position, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use tokio::task::JoinSet;
use tokio::time::{Duration, Instant, MissedTickBehavior};

use crate::tui::{
    theme::TuiTheme, ActionId, ActionInvocation, CommandPalette, CommandPaletteLayout,
    CommandPaletteOutcome, EventReader, KeyChord, KeybindingResolution, KeybindingState, Session,
    SessionOptions,
};

use super::{
    contributions::{
        self, StreamAction, StreamActionContext, StreamActionRegistry, DASHBOARD_ACTIONS,
    },
    controller::{DashboardStatus, StreamController},
};

pub(super) async fn run(controller: StreamController, theme: TuiTheme) -> Result<()> {
    let initial = controller.dashboard_status().await?;
    let mut app = App::new(initial)?;
    let mut terminal =
        Session::open(SessionOptions { mouse_capture: true, bracketed_paste: false })?;
    let mut events = EventReader::start();
    let mut operations = JoinSet::new();
    let mut regions = UiRegions::default();
    let mut events_open = true;
    let mut refresh =
        tokio::time::interval_at(Instant::now() + Duration::from_secs(5), Duration::from_secs(5));
    refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        terminal.draw(|frame| regions = render(frame, &mut app, theme))?;
        if !events_open && !app.busy {
            break;
        }
        tokio::select! {
            event = events.recv(), if events_open => {
                match event {
                    Some(event) => match app.on_event(event, &regions)? {
                        Flow::Continue => {}
                        Flow::Quit => break,
                        Flow::Start(operation) => start_operation(
                            &mut app,
                            controller.clone(),
                            operation,
                            &mut operations,
                        ),
                    },
                    None => events_open = false,
                }
            }
            completed = operations.join_next(), if !operations.is_empty() => {
                app.busy = false;
                match completed {
                    Some(Ok(outcome)) => {
                        if let Some(status) = outcome.status {
                            app.status = status;
                        }
                        if let Some(notice) = outcome.notice {
                            app.notice = Some(notice);
                        }
                    }
                    Some(Err(error)) => app.notice = Some(format!("Stream task failed: {error}")),
                    None => {}
                }
            }
            _ = refresh.tick(), if !app.busy && app.palette.is_none() => start_operation(
                &mut app,
                controller.clone(),
                Operation::AutoRefresh,
                &mut operations,
            ),
        }
    }
    while operations.join_next().await.is_some() {}
    Ok(())
}

#[derive(Default)]
struct UiRegions {
    action_rows: Vec<(Rect, ActionId)>,
    command_palette: Option<CommandPaletteLayout>,
}

struct App {
    status: DashboardStatus,
    notice: Option<String>,
    busy: bool,
    selected_action: usize,
    palette: Option<CommandPalette<StreamActionContext>>,
    registry: StreamActionRegistry,
    keybinding_state: KeybindingState,
}

impl App {
    fn new(status: DashboardStatus) -> Result<Self> {
        Ok(Self {
            status,
            notice: Some(
                "Use Cmd+Shift+M from the window you want; this dashboard handles setup and recall"
                    .to_owned(),
            ),
            busy: false,
            selected_action: 0,
            palette: None,
            registry: contributions::registry().context("build Stream action registry")?,
            keybinding_state: KeybindingState::default(),
        })
    }

    fn context(&self) -> StreamActionContext {
        StreamActionContext { status: self.status.clone(), busy: self.busy }
    }

    fn on_event(&mut self, event: Event, regions: &UiRegions) -> Result<Flow> {
        if matches!(
            event,
            Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            })
        ) {
            if self.busy {
                self.notice =
                    Some("Stream is working; exit is available when it finishes".to_owned());
                return Ok(Flow::Continue);
            }
            return Ok(Flow::Quit);
        }
        if self.palette.is_some() {
            return self.on_palette_event(event, regions);
        }
        match event {
            Event::Key(key) if key.is_press() => self.on_key(key, regions),
            Event::Mouse(mouse) => self.on_mouse(mouse, regions),
            _ => Ok(Flow::Continue),
        }
    }

    fn on_key(&mut self, key: KeyEvent, regions: &UiRegions) -> Result<Flow> {
        match key.code {
            KeyCode::Up => {
                self.selected_action = self.selected_action.saturating_sub(1);
                return Ok(Flow::Continue);
            }
            KeyCode::Down => {
                self.selected_action =
                    (self.selected_action + 1).min(regions.action_rows.len().saturating_sub(1));
                return Ok(Flow::Continue);
            }
            KeyCode::Enter => {
                if let Some((_, action)) = regions.action_rows.get(self.selected_action) {
                    return self.invoke(ActionInvocation::new(*action, self.context()));
                }
                return Ok(Flow::Continue);
            }
            _ => {}
        }
        let Some(chord) = KeyChord::from_event(key) else {
            return Ok(Flow::Continue);
        };
        let context = self.context();
        match self.registry.resolve_keybinding(&mut self.keybinding_state, chord, context) {
            KeybindingResolution::Invoke(invocation) => self.invoke(invocation),
            KeybindingResolution::Pending
            | KeybindingResolution::Unmatched
            | KeybindingResolution::UnmatchedSequence { .. } => Ok(Flow::Continue),
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent, regions: &UiRegions) -> Result<Flow> {
        self.keybinding_state.cancel();
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return Ok(Flow::Continue);
        }
        let position = Position { x: mouse.column, y: mouse.row };
        if let Some((index, (_, action))) =
            regions.action_rows.iter().enumerate().find(|(_, (area, _))| area.contains(position))
        {
            self.selected_action = index;
            return self.invoke(ActionInvocation::new(*action, self.context()));
        }
        Ok(Flow::Continue)
    }

    fn on_palette_event(&mut self, event: Event, regions: &UiRegions) -> Result<Flow> {
        let Some(layout) = regions.command_palette.as_ref() else {
            self.palette = None;
            return Ok(Flow::Continue);
        };
        let outcome =
            self.palette.as_mut().expect("palette presence checked above").on_event(event, layout);
        match outcome {
            CommandPaletteOutcome::Captured => Ok(Flow::Continue),
            CommandPaletteOutcome::Dismissed => {
                self.palette = None;
                Ok(Flow::Continue)
            }
            CommandPaletteOutcome::Invoke(invocation) => {
                self.palette = None;
                self.invoke(invocation)
            }
        }
    }

    fn invoke(&mut self, invocation: ActionInvocation<StreamActionContext>) -> Result<Flow> {
        let action = match self.registry.command_for(&invocation) {
            Ok(action) => action,
            Err(error) => {
                self.notice = Some(error.to_string());
                return Ok(Flow::Continue);
            }
        };
        if self.busy && action == StreamAction::Quit {
            self.notice = Some("Stream is working; exit is available when it finishes".to_owned());
            return Ok(Flow::Continue);
        }
        Ok(match action {
            StreamAction::Recall => Flow::Start(Operation::Recall),
            StreamAction::Recover => Flow::Start(Operation::Recover),
            StreamAction::InstallShortcut => Flow::Start(Operation::InstallShortcut),
            StreamAction::Refresh => Flow::Start(Operation::Refresh),
            StreamAction::OpenCommandPalette => {
                self.palette = Some(CommandPalette::open(self.context(), &self.registry));
                Flow::Continue
            }
            StreamAction::Quit => Flow::Quit,
        })
    }
}

enum Flow {
    Continue,
    Quit,
    Start(Operation),
}

#[derive(Clone, Copy)]
enum Operation {
    Recall,
    Recover,
    InstallShortcut,
    Refresh,
    AutoRefresh,
}

struct OperationOutcome {
    status: Option<DashboardStatus>,
    notice: Option<String>,
}

fn start_operation(
    app: &mut App,
    controller: StreamController,
    operation: Operation,
    operations: &mut JoinSet<OperationOutcome>,
) {
    if app.busy {
        return;
    }
    app.busy = true;
    if !matches!(operation, Operation::AutoRefresh) {
        app.notice = Some("Working…".to_owned());
    }
    operations.spawn(perform_operation(controller, operation));
}

async fn perform_operation(controller: StreamController, operation: Operation) -> OperationOutcome {
    if matches!(operation, Operation::Refresh | Operation::AutoRefresh) {
        return match controller.dashboard_status().await {
            Ok(status) => OperationOutcome {
                status: Some(status),
                notice: matches!(operation, Operation::Refresh)
                    .then(|| "Status refreshed".to_owned()),
            },
            Err(error) => OperationOutcome {
                status: None,
                notice: Some(format!("Status refresh failed: {error:#}")),
            },
        };
    }
    let notice = match operation {
        Operation::Recall => controller.recall().await.map(|report| match report {
            Some(report) => format!("Recalled {} · {}", report.app_name, report.window_title),
            None => "Stream Slot is already empty".to_owned(),
        }),
        Operation::Recover => controller.recover().await.map(|report| match report {
            Some(report) => format!("Recovered {} · {}", report.app_name, report.window_title),
            None => "Nothing needs recovery".to_owned(),
        }),
        Operation::InstallShortcut => controller.install_shortcut().map(|installed| {
            if installed {
                "Installed Cmd+Shift+M global shortcut"
            } else {
                "Cmd+Shift+M was already installed"
            }
            .to_owned()
        }),
        Operation::Refresh | Operation::AutoRefresh => unreachable!("handled above"),
    };
    let status = controller.dashboard_status().await;
    match (notice, status) {
        (Ok(notice), Ok(status)) => OperationOutcome { status: Some(status), notice: Some(notice) },
        (Ok(notice), Err(error)) => OperationOutcome {
            status: None,
            notice: Some(format!("{notice}. Status refresh failed: {error:#}")),
        },
        (Err(error), Ok(status)) => {
            OperationOutcome { status: Some(status), notice: Some(format!("{error:#}")) }
        }
        (Err(error), Err(refresh)) => OperationOutcome {
            status: None,
            notice: Some(format!("{error:#}; status refresh also failed: {refresh:#}")),
        },
    }
}

fn render(frame: &mut Frame<'_>, app: &mut App, theme: TuiTheme) -> UiRegions {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(theme.background)), area);
    let (state_label, state_color) = if app.status.slot.active {
        (" STREAMING ", theme.success)
    } else if app.status.slot.phase.is_some() {
        (" RECOVERY ", theme.warning)
    } else {
        (" READY ", theme.accent)
    };
    let outer = Block::default()
        .title(Line::from(vec![
            Span::styled(" Stream Slot ", Style::default().fg(theme.text_strong).bold()),
            Span::styled(state_label, Style::default().fg(state_color).bold()),
        ]))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border));
    let inner = outer.inner(area).inner(Margin { horizontal: 2, vertical: 1 });
    frame.render_widget(outer, area);
    let rows = Layout::vertical([
        Constraint::Length(7),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .split(inner);

    render_status(frame, rows[0], &app.status, theme);
    frame.render_widget(
        Paragraph::new("Actions").style(Style::default().fg(theme.text_strong).bold()),
        rows[1],
    );
    let mut regions = UiRegions::default();
    let context = app.context();
    let actions = app.registry.resolve_menu(DASHBOARD_ACTIONS, &context);
    app.selected_action = app.selected_action.min(actions.len().saturating_sub(1));
    for (index, action) in actions.items().iter().enumerate().take(usize::from(rows[2].height)) {
        let row = Rect::new(rows[2].x, rows[2].y + index as u16, rows[2].width, 1);
        let selected = index == app.selected_action;
        let enabled = action.state.is_enabled();
        let style = Style::default()
            .fg(if enabled { theme.text } else { theme.text_muted })
            .bg(if selected { theme.selection } else { theme.surface });
        frame.render_widget(Clear, row);
        frame.render_widget(Block::default().style(style), row);
        let columns = Layout::horizontal([Constraint::Fill(1), Constraint::Length(16)]).split(row);
        frame.render_widget(
            Paragraph::new(format!("{}{}", if selected { "› " } else { "  " }, action.title))
                .style(style),
            columns[0],
        );
        frame.render_widget(
            Paragraph::new(
                action.primary_keybinding().map(|binding| binding.to_string()).unwrap_or_default(),
            )
            .alignment(Alignment::Right)
            .style(style.fg(theme.text_muted)),
            columns[1],
        );
        regions.action_rows.push((row, action.id));
    }
    if let Some(notice) = &app.notice {
        frame.render_widget(
            Paragraph::new(notice.as_str())
                .style(Style::default().fg(theme.accent_alt))
                .wrap(Wrap { trim: true }),
            rows[3],
        );
    }
    frame.render_widget(
        Paragraph::new("S recall  E recover  I install shortcut  Ctrl+P commands  q quit")
            .style(Style::default().fg(theme.text_muted)),
        rows[4],
    );
    if let Some(palette) = &app.palette {
        let layout = palette.layout(area);
        palette.render(frame, &layout, theme);
        regions.command_palette = Some(layout);
    }
    regions
}

fn render_status(frame: &mut Frame<'_>, area: Rect, status: &DashboardStatus, theme: TuiTheme) {
    let slot = match (&status.slot.app_name, &status.slot.window_title) {
        (Some(app), Some(title)) if !title.is_empty() => format!("{app} · {title}"),
        (Some(app), _) => app.clone(),
        _ => "Empty — use Cmd+Shift+M from the window you want".to_owned(),
    };
    let lines = [
        status_line("Stream Slot", &slot, status.slot.active, theme),
        status_line(
            "TV display",
            if status.display_connected { "connected" } else { "disconnected" },
            status.display_connected,
            theme,
        ),
        status_line(
            "Sunshine",
            if status.sunshine_owned {
                "running · owned by Kit"
            } else if status.sunshine_running {
                "running · external"
            } else {
                "stopped"
            },
            status.sunshine_running,
            theme,
        ),
        status_line(
            "Global shortcut",
            if status.shortcut_installed { "Cmd+Shift+M" } else { "not installed" },
            status.shortcut_installed,
            theme,
        ),
        status_line(
            "Accessibility",
            if status.accessibility_granted { "granted" } else { "permission required" },
            status.accessibility_granted,
            theme,
        ),
    ];
    frame.render_widget(Paragraph::new(lines.to_vec()), area);
}

fn status_line(label: &str, value: &str, healthy: bool, theme: TuiTheme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<18}"), Style::default().fg(theme.text_muted)),
        Span::styled(
            value.to_owned(),
            Style::default().fg(if healthy { theme.success } else { theme.warning }),
        ),
    ])
}
