//! `update` — discover and install signed-for-transport Kit release binaries.

mod actions;

use std::env;

use anyhow::Result;
use async_trait::async_trait;
use clap::{ArgMatches, Command, CommandFactory, FromArgMatches, Parser};
use crossterm::event::{Event, MouseButton, MouseEventKind};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use crate::{
    framework::{Context, Terminal, Tool, ToolMeta},
    release::{ReleaseUpdater, UpdateAvailability, UpdateOutcome},
    tui::{
        theme::NORD, ActionInvocation, EventReader, KeyChord, KeybindingResolution,
        KeybindingState, Session, SessionOptions,
    },
};

use actions::{PromptActionContext, PromptActionRegistry, PromptCommand, CHOICES};

const RELEASES_URL: &str = "https://github.com/xtava/kit/releases";

pub fn tool() -> UpdateTool {
    UpdateTool
}

pub struct UpdateTool;

#[derive(Parser)]
#[command(
    name = "update",
    about = "Update Kit and reconcile Console",
    long_about = "Downloads a digest-verified GitHub release into Kit's managed executable path, then reconciles the native Console service."
)]
struct UpdateArgs;

#[async_trait]
impl Tool for UpdateTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "update",
            about: "Update Kit and reconcile Console",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> Command {
        UpdateArgs::command()
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        UpdateArgs::from_arg_matches(matches)?;
        let outcome =
            perform_update(&cx.processes, cx.term.stdout_tty && !cx.out.is_json()).await?;
        if cx.out.is_json() {
            cx.out.json(&outcome)
        } else {
            print_outcome(&outcome);
            Ok(())
        }
    }
}

/// Show a cached update notification before normal command dispatch.
///
/// Release metadata refreshes in the background and never delays the requested command. The
/// return value is `true` only when an update was installed and the caller should exit instead of
/// running a binary that has just replaced itself.
pub async fn startup() -> Result<bool> {
    let args = env::args_os().collect::<Vec<_>>();
    let terminal = Terminal::detect();
    if !should_notify(&args, &terminal) {
        return Ok(false);
    }

    let updater = ReleaseUpdater::new();
    let cached = updater.cached();
    if cached.as_ref().is_none_or(|cached| cached.stale) {
        tokio::spawn(async move {
            if let Err(error) = updater.check().await {
                let _ = error;
            }
        });
    }

    let Some(cached) = cached.filter(|cached| !cached.dismissed) else {
        return Ok(false);
    };
    let UpdateAvailability::Available { latest, .. } = cached.availability else {
        return Ok(false);
    };

    match prompt(&latest).await? {
        UpdateChoice::UpdateNow => {
            let processes = crate::framework::process::ProcessSupervisor::bootstrap()?;
            let outcome = perform_update(&processes, true).await?;
            print_outcome(&outcome);
            Ok(true)
        }
        UpdateChoice::Later => Ok(false),
        UpdateChoice::SkipVersion => {
            updater.dismiss(&latest)?;
            Ok(false)
        }
    }
}

async fn perform_update(
    processes: &crate::framework::process::ProcessSupervisor,
    show_progress: bool,
) -> Result<UpdateOutcome> {
    Ok(ReleaseUpdater::new().install_managed(processes, show_progress).await?.outcome)
}

fn print_outcome(outcome: &UpdateOutcome) {
    match outcome {
        UpdateOutcome::Updated { to, .. } => println!("Kit updated to {to}"),
        UpdateOutcome::AlreadyCurrent { version } => {
            println!("Kit is already up to date ({version})")
        }
    }
}

fn should_notify(args: &[std::ffi::OsString], terminal: &Terminal) -> bool {
    if !terminal.interactive() || args.len() <= 1 {
        return false;
    }

    let args = args.iter().skip(1).filter_map(|arg| arg.to_str()).collect::<Vec<_>>();
    if args.iter().any(|arg| matches!(*arg, "--json" | "--help" | "-h" | "--version" | "-V")) {
        return false;
    }

    args.iter().find(|arg| !arg.starts_with('-')).is_some_and(|command| *command != "update")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UpdateChoice {
    UpdateNow,
    Later,
    SkipVersion,
}

impl UpdateChoice {
    const ALL: [Self; 3] = [Self::UpdateNow, Self::Later, Self::SkipVersion];
}

#[derive(Clone, Debug, Default)]
struct PromptRegions {
    choices: Vec<(Rect, crate::tui::ActionId)>,
}

async fn prompt(latest: &str) -> Result<UpdateChoice> {
    let mut session =
        Session::open(SessionOptions { mouse_capture: true, bracketed_paste: false })?;
    let mut selected = 0;
    let mut regions = PromptRegions::default();
    let registry = actions::registry()?;
    let mut keybinding_state = KeybindingState::default();

    loop {
        session.draw(|frame| regions = render_prompt(frame, latest, selected, &registry))?;
        let Some(event) = EventReader::read_once().await else {
            return Ok(UpdateChoice::Later);
        };
        match event {
            Event::Key(key) => {
                let Some(chord) = KeyChord::from_event(key) else { continue };
                let context = PromptActionContext { selected };
                let invocation =
                    match registry.resolve_keybinding(&mut keybinding_state, chord, context) {
                        KeybindingResolution::Invoke(invocation) => invocation,
                        KeybindingResolution::Pending
                        | KeybindingResolution::Unmatched
                        | KeybindingResolution::UnmatchedSequence { .. } => continue,
                    };
                if let Some(choice) =
                    execute_prompt_command(registry.command_for(&invocation)?, &mut selected)
                {
                    return Ok(choice);
                }
            }
            Event::Mouse(mouse) => {
                let point = Position { x: mouse.column, y: mouse.row };
                if let Some((index, (_, action))) =
                    regions.choices.iter().enumerate().find(|(_, (area, _))| area.contains(point))
                {
                    selected = index;
                    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                        let invocation =
                            ActionInvocation::new(*action, PromptActionContext { selected });
                        if let Some(choice) = execute_prompt_command(
                            registry.command_for(&invocation)?,
                            &mut selected,
                        ) {
                            return Ok(choice);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn execute_prompt_command(command: PromptCommand, selected: &mut usize) -> Option<UpdateChoice> {
    match command {
        PromptCommand::MoveUp => *selected = selected.saturating_sub(1),
        PromptCommand::MoveDown => {
            *selected = selected.saturating_add(1).min(UpdateChoice::ALL.len() - 1)
        }
        PromptCommand::ChooseNow => return Some(UpdateChoice::UpdateNow),
        PromptCommand::ChooseLater | PromptCommand::Dismiss => return Some(UpdateChoice::Later),
        PromptCommand::ChooseSkip => return Some(UpdateChoice::SkipVersion),
        PromptCommand::Activate => return Some(UpdateChoice::ALL[*selected]),
    }
    None
}

fn render_prompt(
    frame: &mut Frame<'_>,
    latest: &str,
    selected: usize,
    registry: &PromptActionRegistry,
) -> PromptRegions {
    let area = centered_rect(frame.area(), 68, 15);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default()
            .title(Line::from(Span::styled(
                " Kit update available ",
                Style::default().fg(NORD.warning).add_modifier(Modifier::BOLD),
            )))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(NORD.focus)),
        area,
    );

    let inner = area.inner(ratatui::layout::Margin { horizontal: 2, vertical: 1 });
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(env!("CARGO_PKG_VERSION"), Style::default().fg(NORD.text_muted)),
            Span::styled("  →  ", Style::default().fg(NORD.text_muted)),
            Span::styled(latest, Style::default().fg(NORD.success).add_modifier(Modifier::BOLD)),
        ]))
        .alignment(Alignment::Center),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(format!("{RELEASES_URL}/tag/v{latest}"))
            .style(Style::default().fg(NORD.accent))
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        rows[1],
    );

    let mut regions = PromptRegions::default();
    let choices = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1); 3])
        .split(rows[3]);
    let resolved = registry.resolve_menu(CHOICES, &PromptActionContext { selected });
    for (index, action) in resolved.items().iter().enumerate() {
        regions.choices.push((choices[index], action.id));
        let active = index == selected;
        let marker = if active { "› " } else { "  " };
        let style = if active {
            Style::default().fg(NORD.text_strong).bg(NORD.selection).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(NORD.text)
        };
        frame.render_widget(
            Paragraph::new(format!("{marker}{}", action.title)).style(style),
            choices[index],
        );
    }

    frame.render_widget(
        Paragraph::new(prompt_help(registry, selected))
            .style(Style::default().fg(NORD.text_muted))
            .alignment(Alignment::Center),
        rows[4],
    );
    regions
}

fn prompt_help(registry: &PromptActionRegistry, selected: usize) -> String {
    registry
        .resolve_command_palette(&PromptActionContext { selected })
        .items()
        .iter()
        .filter_map(|action| {
            action.primary_keybinding().map(|binding| format!("{binding} {}", action.title))
        })
        .collect::<Vec<_>>()
        .join("  ")
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn interactive() -> Terminal {
        Terminal { stdin_tty: true, stdout_tty: true }
    }

    #[test]
    fn startup_notification_only_runs_for_interactive_tool_commands() {
        assert!(should_notify(&args(&["kit", "tail"]), &interactive()));
        assert!(!should_notify(&args(&["kit"]), &interactive()));
        assert!(!should_notify(&args(&["kit", "update"]), &interactive()));
        assert!(!should_notify(&args(&["kit", "--json", "tail"]), &interactive()));
        assert!(!should_notify(&args(&["kit", "tail", "--help"]), &interactive()));
        assert!(!should_notify(
            &args(&["kit", "tail"]),
            &Terminal { stdin_tty: true, stdout_tty: false }
        ));
    }

    #[test]
    fn prompt_navigation_is_bounded_and_selects_the_active_choice() {
        let mut selected = 0;
        assert_eq!(execute_prompt_command(PromptCommand::MoveUp, &mut selected), None);
        assert_eq!(selected, 0);
        execute_prompt_command(PromptCommand::MoveDown, &mut selected);
        execute_prompt_command(PromptCommand::MoveDown, &mut selected);
        execute_prompt_command(PromptCommand::MoveDown, &mut selected);
        assert_eq!(selected, 2);
        assert_eq!(
            execute_prompt_command(PromptCommand::Activate, &mut selected),
            Some(UpdateChoice::SkipVersion)
        );
        assert_eq!(
            execute_prompt_command(PromptCommand::Dismiss, &mut selected),
            Some(UpdateChoice::Later)
        );
    }
}
