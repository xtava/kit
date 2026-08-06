use ratatui::{
    layout::{Constraint, Layout, Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::tui::{
    render_split_divider,
    theme::{TuiTheme, NORD},
    ContextMenuStyle, SplitDividerStyle, SplitFrame, SplitMinimums,
};

use super::{
    super::controller::CheckStatus,
    form::{AddField, AddProjectForm, AddProjectLayout, ConfirmationLayout},
    App, ProjectState, SessionHealth, Surface, SyncRegion, UiRegions, DASHBOARD_ACTIONS,
    MIN_DETAILS_WIDTH, MIN_PROJECTS_WIDTH,
};

pub(super) fn render(frame: &mut Frame<'_>, app: &mut App) -> UiRegions {
    let area = frame.area();
    frame.render_widget(Block::new().style(Style::default().bg(NORD.background)), area);
    let mut regions = UiRegions::default();
    if let Surface::Settings(editor) = &mut app.surface {
        editor.render(frame, area);
        return regions;
    }
    let split = SplitFrame::horizontal(
        area,
        app.split_ratio,
        SplitMinimums::new(MIN_PROJECTS_WIDTH, MIN_DETAILS_WIDTH),
    );
    regions.split = Some(split);
    regions.projects = Some(split.first);
    regions.details = Some(split.second);
    render_projects(frame, split.first, app, &mut regions);
    render_details(frame, split.second, app, &mut regions);
    render_split_divider(
        frame,
        split,
        app.split_drag.is_some(),
        SplitDividerStyle {
            idle_color: NORD.border,
            active_color: NORD.focus,
            idle_line: "│",
            idle_grip: "┃",
            active_line: "┃",
        },
    );
    if let Some(menu) = app.menu.as_ref() {
        let layout = menu.layout(area);
        menu.render(frame, &layout, ContextMenuStyle::from_theme(NORD));
        regions.context_menu = Some(layout);
    }
    let confirmation = match &app.surface {
        Surface::ConfirmRemove { project, confirm } => Some((
            app.report(*project)
                .map(|report| report.project.name().to_owned())
                .unwrap_or_else(|| "Synced Project".to_owned()),
            *confirm,
        )),
        _ => None,
    };
    match &mut app.surface {
        Surface::CommandPalette(palette) => {
            let layout = palette.layout(area);
            palette.render(frame, &layout, NORD);
            regions.command_palette = Some(layout);
        }
        Surface::AddProject(form) => {
            regions.add_project = Some(render_add_project(frame, area, form, NORD));
        }
        Surface::ConfirmRemove { .. } => {
            let (name, confirm) = confirmation.expect("confirmation surface inspected above");
            regions.confirmation = Some(render_confirmation(frame, area, &name, confirm, NORD));
        }
        Surface::Normal | Surface::Settings(_) => {}
    }
    regions
}

fn render_projects(frame: &mut Frame<'_>, area: Rect, app: &App, regions: &mut UiRegions) {
    let focused = app.active_region == SyncRegion::Projects;
    let title = if app.operation.is_busy() {
        format!(" Synced Projects {} · working… ", app.reports.len())
    } else {
        format!(" Synced Projects {} ", app.reports.len())
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused { NORD.focus } else { NORD.border }));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if app.reports.is_empty() {
        frame.render_widget(
            Paragraph::new("No projects").style(Style::default().fg(NORD.text_muted)),
            inner.inner(Margin { horizontal: 1, vertical: 1 }),
        );
        return;
    }
    for (index, report) in app.reports.iter().take(usize::from(inner.height)).enumerate() {
        let row = Rect::new(inner.x, inner.y + index as u16, inner.width, 1);
        let selected = app.selected == Some(report.project.id());
        let style = Style::default()
            .fg(if selected { NORD.text_strong } else { NORD.text })
            .bg(if selected { NORD.selection } else { NORD.surface });
        frame.render_widget(Block::new().style(style), row);
        let columns = Layout::horizontal([Constraint::Fill(1), Constraint::Length(12)]).split(row);
        let marker = if selected { "› " } else { "  " };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(marker, style.fg(NORD.accent)),
                Span::styled(report.project.name(), style),
            ])),
            columns[0],
        );
        frame.render_widget(
            Paragraph::new(report.state.label())
                .alignment(ratatui::layout::Alignment::Right)
                .style(style.fg(state_color(report.state, NORD))),
            columns[1],
        );
        regions.project_rows.push((row, report.project.id()));
    }
}

fn render_details(frame: &mut Frame<'_>, area: Rect, app: &mut App, regions: &mut UiRegions) {
    let focused = app.active_region == SyncRegion::Details;
    let title = app
        .selected_report()
        .map(|report| format!(" {} ", report.project.name()))
        .unwrap_or_else(|| " Details ".to_owned());
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused { NORD.focus } else { NORD.border }));
    let inner = block.inner(area).inner(Margin { horizontal: 1, vertical: 1 });
    frame.render_widget(block, area);
    let mut y = inner.y;
    if let Some(report) = app.selected_report() {
        render_detail_line(
            frame,
            inner,
            &mut y,
            "State",
            report.state.label(),
            state_color(report.state, NORD),
        );
        render_detail_line(
            frame,
            inner,
            &mut y,
            "Local",
            &report.project.local().root().display().to_string(),
            NORD.text,
        );
        render_detail_line(
            frame,
            inner,
            &mut y,
            "Remote",
            &format!(
                "{} · {}",
                report.project.remote().unix_user(),
                report.project.remote().root().display()
            ),
            NORD.text,
        );
        if let Some(session) = report.sessions.first() {
            render_detail_line(
                frame,
                inner,
                &mut y,
                "Cycles",
                &session.successful_cycles.to_string(),
                NORD.text,
            );
            if !session.conflicts.is_empty() {
                render_detail_line(
                    frame,
                    inner,
                    &mut y,
                    "Conflicts",
                    &session.conflicts.len().to_string(),
                    NORD.warning,
                );
            }
        }
    } else {
        render_text(
            frame,
            inner,
            &mut y,
            "Add a project to keep source aligned across machines.",
            NORD.text_muted,
        );
    }
    if let Some(doctor) = &app.doctor {
        y = y.saturating_add(1);
        render_text(
            frame,
            inner,
            &mut y,
            &doctor.mutagen.detail,
            check_color(doctor.mutagen.status, NORD),
        );
        render_text(
            frame,
            inner,
            &mut y,
            &doctor.tailscale.detail,
            check_color(doctor.tailscale.status, NORD),
        );
        if let Some(remote) = &doctor.remote {
            render_text(frame, inner, &mut y, &remote.detail, check_color(remote.status, NORD));
        }
        if let Some(project) = &doctor.project {
            render_text(frame, inner, &mut y, &project.detail, check_color(project.status, NORD));
        }
    }
    y = y.saturating_add(1);
    let context = app.action_context(regions);
    let actions = app.registry.resolve_menu(DASHBOARD_ACTIONS, &context);
    app.detail_action = app.detail_action.min(actions.len().saturating_sub(1));
    for (index, action) in actions.items().iter().enumerate() {
        if y >= inner.bottom() {
            break;
        }
        let row = Rect::new(inner.x, y, inner.width, 1);
        let selected = focused && index == app.detail_action;
        let enabled = action.state.is_enabled();
        let style = Style::default()
            .fg(if enabled { NORD.text } else { NORD.text_muted })
            .bg(if selected { NORD.selection } else { NORD.surface });
        frame.render_widget(Block::new().style(style), row);
        let shortcut =
            action.primary_keybinding().map(|binding| binding.to_string()).unwrap_or_default();
        let columns = Layout::horizontal([Constraint::Fill(1), Constraint::Length(14)]).split(row);
        frame.render_widget(
            Paragraph::new(format!("{}{}", if selected { "› " } else { "  " }, action.title))
                .style(style),
            columns[0],
        );
        frame.render_widget(
            Paragraph::new(shortcut)
                .alignment(ratatui::layout::Alignment::Right)
                .style(style.fg(NORD.text_muted)),
            columns[1],
        );
        regions.action_rows.push((row, action.id));
        y += 1;
    }
    if let Some(notice) = &app.notice {
        let height = 2.min(inner.height);
        let notice_area =
            Rect::new(inner.x, inner.bottom().saturating_sub(height), inner.width, height);
        frame.render_widget(
            Paragraph::new(notice.as_str())
                .style(Style::default().fg(NORD.accent_alt))
                .wrap(Wrap { trim: true }),
            notice_area,
        );
    }
}

fn render_detail_line(
    frame: &mut Frame<'_>,
    area: Rect,
    y: &mut u16,
    label: &str,
    value: &str,
    color: ratatui::style::Color,
) {
    if *y >= area.bottom() {
        return;
    }
    let row = Rect::new(area.x, *y, area.width, 1);
    let columns = Layout::horizontal([Constraint::Length(10), Constraint::Fill(1)]).split(row);
    frame.render_widget(
        Paragraph::new(label).style(Style::default().fg(NORD.text_muted)),
        columns[0],
    );
    frame.render_widget(Paragraph::new(value).style(Style::default().fg(color)), columns[1]);
    *y += 1;
}

fn render_text(
    frame: &mut Frame<'_>,
    area: Rect,
    y: &mut u16,
    value: &str,
    color: ratatui::style::Color,
) {
    if *y < area.bottom() {
        frame.render_widget(
            Paragraph::new(value).style(Style::default().fg(color)),
            Rect::new(area.x, *y, area.width, 1),
        );
        *y += 1;
    }
}

fn render_add_project(
    frame: &mut Frame<'_>,
    area: Rect,
    form: &AddProjectForm,
    theme: TuiTheme,
) -> AddProjectLayout {
    let popup = centered(area, area.width.min(78), area.height.min(20));
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" New Synced Project ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.focus));
    let inner = block.inner(popup).inner(Margin { horizontal: 2, vertical: 1 });
    frame.render_widget(block, popup);
    let mut fields = Vec::new();
    for (index, field) in AddField::ALL.iter().enumerate() {
        let y = inner.y + index as u16 * 2;
        if y + 1 >= inner.bottom() {
            break;
        }
        frame.render_widget(
            Paragraph::new(field.label()).style(Style::default().fg(theme.text_muted)),
            Rect::new(inner.x, y, inner.width, 1),
        );
        let input_area = Rect::new(inner.x, y + 1, inner.width, 1);
        let selected = index == form.active;
        frame.render_widget(
            Paragraph::new(form.inputs[index].value()).style(
                Style::default().fg(theme.text_strong).bg(if selected {
                    theme.selection
                } else {
                    theme.surface
                }),
            ),
            input_area,
        );
        fields.push(input_area);
        if selected {
            let cursor =
                u16::try_from(form.inputs[index].value()[..form.inputs[index].cursor()].width())
                    .unwrap_or(u16::MAX)
                    .min(input_area.width.saturating_sub(1));
            frame.set_cursor_position((input_area.x.saturating_add(cursor), input_area.y));
        }
    }
    let buttons_y = inner.bottom().saturating_sub(1);
    let buttons =
        Layout::horizontal([Constraint::Length(12), Constraint::Length(10), Constraint::Fill(1)])
            .split(Rect::new(inner.x, buttons_y, inner.width, 1));
    let submit = buttons[0];
    let cancel = buttons[1];
    frame.render_widget(
        Paragraph::new("[ Create ]")
            .style(Style::default().fg(theme.text_strong).add_modifier(Modifier::BOLD)),
        submit,
    );
    frame.render_widget(
        Paragraph::new("[ Cancel ]").style(Style::default().fg(theme.text_muted)),
        cancel,
    );
    AddProjectLayout { fields, submit, cancel }
}

fn render_confirmation(
    frame: &mut Frame<'_>,
    area: Rect,
    project: &str,
    confirm: bool,
    theme: TuiTheme,
) -> ConfirmationLayout {
    let popup = centered(area, area.width.min(58), area.height.min(9));
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" Remove Synced Project ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.warning));
    let inner = block.inner(popup).inner(Margin { horizontal: 2, vertical: 1 });
    frame.render_widget(block, popup);
    frame.render_widget(
        Paragraph::new(format!("Remove {project:?}? Synchronized files will be preserved."))
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(theme.text)),
        Rect::new(inner.x, inner.y, inner.width, inner.height.saturating_sub(2)),
    );
    let row = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    let columns =
        Layout::horizontal([Constraint::Length(14), Constraint::Length(12), Constraint::Fill(1)])
            .split(row);
    let confirm_area = columns[0];
    let cancel_area = columns[1];
    frame.render_widget(
        Paragraph::new("[ Remove ]").style(
            Style::default()
                .fg(if confirm { theme.text_strong } else { theme.warning })
                .bg(if confirm { theme.selection } else { theme.surface }),
        ),
        confirm_area,
    );
    frame.render_widget(
        Paragraph::new("[ Cancel ]").style(Style::default().fg(theme.text).bg(if confirm {
            theme.surface
        } else {
            theme.selection
        })),
        cancel_area,
    );
    ConfirmationLayout { confirm: confirm_area, cancel: cancel_area }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn state_color(state: ProjectState, theme: TuiTheme) -> ratatui::style::Color {
    match state {
        ProjectState::Creating => theme.accent,
        ProjectState::Session(SessionHealth::Healthy) => theme.success,
        ProjectState::Session(SessionHealth::Synchronizing) => theme.accent,
        ProjectState::Session(SessionHealth::Paused) => theme.text_muted,
        ProjectState::Session(SessionHealth::Conflicted)
        | ProjectState::Session(SessionHealth::Offline)
        | ProjectState::Session(SessionHealth::Error)
        | ProjectState::Removing
        | ProjectState::Missing
        | ProjectState::Duplicate
        | ProjectState::Incompatible
        | ProjectState::Stale
        | ProjectState::NeedsPause
        | ProjectState::NeedsResume => theme.warning,
    }
}

fn check_color(status: CheckStatus, theme: TuiTheme) -> ratatui::style::Color {
    match status {
        CheckStatus::Ready => theme.success,
        CheckStatus::ActionRequired => theme.warning,
    }
}
