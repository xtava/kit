//! `update` — discover and install signed-for-transport Kit release binaries.

use std::{
    env, fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context as AnyhowContext, Result};
use async_trait::async_trait;
use clap::{ArgMatches, Command, CommandFactory, FromArgMatches, Parser};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use directories::ProjectDirs;
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use serde::{Deserialize, Serialize};

use crate::{
    framework::{AtomicFileWriter, Context, Terminal, Tool, ToolMeta},
    tui::{theme::NORD, EventReader, Session, SessionOptions},
};

const REPO_OWNER: &str = "xtava";
const REPO_NAME: &str = "kit";
const CHECK_INTERVAL: Duration = Duration::from_secs(20 * 60 * 60);
const CACHE_FILE: &str = "version.json";
const RELEASES_URL: &str = "https://github.com/xtava/kit/releases";

pub fn tool() -> UpdateTool {
    UpdateTool
}

pub struct UpdateTool;

#[derive(Parser)]
#[command(
    name = "update",
    about = "Update Kit to the newest GitHub release",
    long_about = "Downloads the newest compatible Kit release, verifies GitHub's published SHA-256 asset digest, and replaces the running executable. No source checkout, Git, Cargo, or Rust toolchain is required."
)]
struct UpdateArgs;

#[async_trait]
impl Tool for UpdateTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "update",
            about: "Update Kit to the newest GitHub release",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> Command {
        UpdateArgs::command()
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        UpdateArgs::from_arg_matches(matches)?;
        if cx.out.is_json() {
            bail!("kit update does not support --json because it reports download progress");
        }

        perform_update(cx.term.stdout_tty).await
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

    let Some(path) = cache_path() else {
        return Ok(false);
    };
    let cached = read_cache(&path);
    if cached.as_ref().is_none_or(VersionCache::is_stale) {
        let refresh_path = path.clone();
        tokio::spawn(async move {
            if let Err(error) = refresh_cache(&refresh_path).await {
                let _ = error;
            }
        });
    }

    let Some(cached) = cached.filter(VersionCache::has_visible_update) else {
        return Ok(false);
    };

    match prompt(&cached.latest_version).await? {
        UpdateChoice::UpdateNow => {
            perform_update(true).await?;
            Ok(true)
        }
        UpdateChoice::Later => Ok(false),
        UpdateChoice::SkipVersion => {
            dismiss_version(&path, &cached.latest_version)?;
            Ok(false)
        }
    }
}

async fn perform_update(show_progress: bool) -> Result<()> {
    let updater = updater(show_progress)?;
    let status = updater.update_async().await.context("update Kit from GitHub Releases")?;

    if status.is_updated() {
        println!("Kit updated to {}", status.version());
    } else {
        println!("Kit is already up to date ({})", status.version());
    }
    Ok(())
}

fn updater(show_progress: bool) -> Result<self_update::backends::github::AsyncUpdate> {
    self_update::backends::github::Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name("kit")
        .current_version(env!("CARGO_PKG_VERSION"))
        .show_download_progress(show_progress)
        .show_output(false)
        .no_confirm(true)
        .verify_release_digest(true)
        .build_async()
        .context("configure the Kit release updater")
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

#[derive(Clone, Debug, Deserialize, Serialize)]
struct VersionCache {
    latest_version: String,
    checked_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dismissed_version: Option<String>,
}

impl VersionCache {
    fn is_stale(&self) -> bool {
        now().saturating_sub(self.checked_at) >= CHECK_INTERVAL.as_secs()
    }

    fn has_visible_update(&self) -> bool {
        self.dismissed_version.as_deref() != Some(&self.latest_version)
            && self_update::version::bump_is_greater(
                env!("CARGO_PKG_VERSION"),
                &self.latest_version,
            )
            .unwrap_or(false)
    }
}

fn cache_path() -> Option<PathBuf> {
    let project = ProjectDirs::from("", "", "kit")?;
    let base = project.state_dir().unwrap_or_else(|| project.data_local_dir());
    Some(base.join("updates").join(CACHE_FILE))
}

fn read_cache(path: &Path) -> Option<VersionCache> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn refresh_cache(path: &Path) -> Result<()> {
    let latest = updater(false)?
        .is_update_available_async()
        .await
        .context("check the latest Kit release")?
        .map(|release| release.version().to_owned())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned());
    write_cache(path, |current| VersionCache {
        latest_version: latest,
        checked_at: now(),
        dismissed_version: current.and_then(|cache| cache.dismissed_version),
    })
}

fn dismiss_version(path: &Path, version: &str) -> Result<()> {
    write_cache(path, |current| VersionCache {
        latest_version: version.to_owned(),
        checked_at: current.as_ref().map_or_else(now, |cache| cache.checked_at),
        dismissed_version: Some(version.to_owned()),
    })
}

fn write_cache(
    path: &Path,
    update: impl FnOnce(Option<VersionCache>) -> VersionCache,
) -> Result<()> {
    let dir = path.parent().context("resolve the Kit update cache directory")?;
    let writer = AtomicFileWriter::new(dir, ".version.lock", ".version");
    let _lock = writer.lock().context("lock the Kit update cache")?;
    let cache = update(read_cache(path));
    let bytes = serde_json::to_vec_pretty(&cache).context("serialize the Kit update cache")?;
    writer.replace(path, &bytes).context("write the Kit update cache")
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UpdateChoice {
    UpdateNow,
    Later,
    SkipVersion,
}

impl UpdateChoice {
    const ALL: [Self; 3] = [Self::UpdateNow, Self::Later, Self::SkipVersion];

    fn label(self) -> &'static str {
        match self {
            Self::UpdateNow => "Update now",
            Self::Later => "Later",
            Self::SkipVersion => "Skip this version",
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct PromptRegions {
    choices: [Rect; 3],
}

async fn prompt(latest: &str) -> Result<UpdateChoice> {
    let mut session =
        Session::open(SessionOptions { mouse_capture: true, bracketed_paste: false })?;
    let mut input = EventReader::start();
    let mut selected = 0;
    let mut regions = PromptRegions::default();

    loop {
        session.draw(|frame| regions = render_prompt(frame, latest, selected))?;
        let Some(event) = input.recv().await else {
            return Ok(UpdateChoice::Later);
        };
        match event {
            Event::Key(key) => match prompt_key(key, &mut selected) {
                PromptAction::Continue => {}
                PromptAction::Choose(choice) => return Ok(choice),
            },
            Event::Mouse(mouse) => {
                let point = Position { x: mouse.column, y: mouse.row };
                if let Some(index) = regions.choices.iter().position(|area| area.contains(point)) {
                    selected = index;
                    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                        return Ok(UpdateChoice::ALL[index]);
                    }
                }
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PromptAction {
    Continue,
    Choose(UpdateChoice),
}

fn prompt_key(key: KeyEvent, selected: &mut usize) -> PromptAction {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return PromptAction::Continue;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c' | 'd'))
    {
        return PromptAction::Choose(UpdateChoice::Later);
    }

    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            *selected = selected.saturating_sub(1);
            PromptAction::Continue
        }
        KeyCode::Down | KeyCode::Char('j') => {
            *selected = (*selected + 1).min(UpdateChoice::ALL.len() - 1);
            PromptAction::Continue
        }
        KeyCode::Char('1') => PromptAction::Choose(UpdateChoice::UpdateNow),
        KeyCode::Char('2') => PromptAction::Choose(UpdateChoice::Later),
        KeyCode::Char('3') => PromptAction::Choose(UpdateChoice::SkipVersion),
        KeyCode::Enter => PromptAction::Choose(UpdateChoice::ALL[*selected]),
        KeyCode::Esc => PromptAction::Choose(UpdateChoice::Later),
        _ => PromptAction::Continue,
    }
}

fn render_prompt(frame: &mut Frame<'_>, latest: &str, selected: usize) -> PromptRegions {
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
    for (index, choice) in UpdateChoice::ALL.into_iter().enumerate() {
        regions.choices[index] = choices[index];
        let active = index == selected;
        let marker = if active { "› " } else { "  " };
        let style = if active {
            Style::default().fg(NORD.text_strong).bg(NORD.selection).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(NORD.text)
        };
        frame.render_widget(
            Paragraph::new(format!("{marker}{}", choice.label())).style(style),
            choices[index],
        );
    }

    frame.render_widget(
        Paragraph::new("↑↓ select  ·  Enter confirm  ·  mouse click  ·  Esc later")
            .style(Style::default().fg(NORD.text_muted))
            .alignment(Alignment::Center),
        rows[4],
    );
    regions
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

    use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState};

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
    fn cache_only_surfaces_newer_undismissed_versions() {
        let mut cache = VersionCache {
            latest_version: "99.0.0".into(),
            checked_at: now(),
            dismissed_version: None,
        };
        assert!(cache.has_visible_update());
        cache.dismissed_version = Some("99.0.0".into());
        assert!(!cache.has_visible_update());
        cache.latest_version = env!("CARGO_PKG_VERSION").into();
        cache.dismissed_version = None;
        assert!(!cache.has_visible_update());
    }

    #[test]
    fn prompt_navigation_is_bounded_and_selects_the_active_choice() {
        let key = |code| KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        let mut selected = 0;
        assert_eq!(prompt_key(key(KeyCode::Up), &mut selected), PromptAction::Continue);
        assert_eq!(selected, 0);
        prompt_key(key(KeyCode::Down), &mut selected);
        prompt_key(key(KeyCode::Down), &mut selected);
        prompt_key(key(KeyCode::Down), &mut selected);
        assert_eq!(selected, 2);
        assert_eq!(
            prompt_key(key(KeyCode::Enter), &mut selected),
            PromptAction::Choose(UpdateChoice::SkipVersion)
        );
        assert_eq!(
            prompt_key(key(KeyCode::Esc), &mut selected),
            PromptAction::Choose(UpdateChoice::Later)
        );
    }
}
