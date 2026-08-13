use ratatui::{
    layout::{Alignment, Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::tui::{
    markdown::MarkdownRenderer,
    render_split_divider, render_vertical_scrollbar,
    theme::{TuiTheme, NORD},
    ContextMenuStyle, ScrollbarLayout, ScrollbarStyle, SelectableRegion, SplitDividerStyle,
    SplitFrame, SplitMinimums, ViewportMetrics,
};

use super::super::model::{
    DoctorIssue, LibraryReport, ProjectionId, ProjectionReport, ProjectionScope, ProjectionState,
    RepositoryReport, SkillName, SkillStatus, SkillsSnapshot,
};

const WIDE_PROJECTION_COLUMN_WIDTH: u16 = 12;
const COMPACT_PROJECTION_COLUMN_WIDTH: u16 = 7;
const WIDE_MATRIX_MINIMUM_WIDTH: u16 = 64;
use super::{
    form::{CreateField, CreateSkillForm, CreateSkillLayout, LibraryForm, LibraryLayout},
    App, DetailTab, DoctorView, SkillsRegion, SkillsSelectionSurface, Surface, UiRegions,
    DASHBOARD_ACTIONS, MIN_CATALOG_WIDTH, MIN_DETAILS_WIDTH,
};

pub(super) fn render(frame: &mut Frame<'_>, app: &mut App) -> UiRegions {
    let area = frame.area();
    frame.render_widget(Block::new().style(Style::default().bg(NORD.background)), area);
    let mut regions = UiRegions::default();
    if let Surface::Settings(editor) = &mut app.surface {
        editor.render(frame, area);
        return regions;
    }

    let rows =
        Layout::vertical([Constraint::Length(2), Constraint::Fill(1), Constraint::Length(2)])
            .split(area);
    render_header(frame, rows[0], app);
    if rows[1].width < MIN_CATALOG_WIDTH + MIN_DETAILS_WIDTH + 1 {
        regions.compact = true;
        regions.catalog = Some(rows[1]);
        regions.details = Some(rows[1]);
        match app.active_region {
            SkillsRegion::Catalog => render_catalog(frame, rows[1], app, &mut regions),
            SkillsRegion::Details => render_details(frame, rows[1], app, &mut regions),
        }
    } else {
        let split = SplitFrame::horizontal(
            rows[1],
            app.split_ratio,
            SplitMinimums::new(MIN_CATALOG_WIDTH, MIN_DETAILS_WIDTH),
        );
        regions.split = Some(split);
        regions.catalog = Some(split.first);
        regions.details = Some(split.second);
        render_catalog(frame, split.first, app, &mut regions);
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
    }
    render_footer(frame, rows[2], app);

    if let Some(menu) = app.menu.as_ref() {
        let layout = menu.layout(area);
        menu.render(frame, &layout, ContextMenuStyle::from_theme(NORD));
        regions.context_menu = Some(layout);
    }
    match &mut app.surface {
        Surface::CommandPalette(palette) => {
            let layout = palette.layout(area);
            palette.render(frame, &layout, NORD);
            regions.command_palette = Some(layout);
        }
        Surface::CreateSkill(form) => {
            let library = match &app.library {
                LibraryReport::Configured { path } => Some(path.as_path()),
                LibraryReport::Unconfigured => None,
            };
            regions.create_skill =
                Some(render_create_skill(frame, area, form, library, app.snapshot.as_ref(), NORD));
        }
        Surface::Library(form) => {
            regions.library = Some(render_library(frame, area, form, NORD));
        }
        Surface::Doctor(view) => {
            let (rows, close) = render_doctor(frame, area, view, NORD);
            regions.doctor_rows = rows;
            regions.doctor_close = Some(close);
        }
        Surface::Help => {
            regions.help_close = Some(render_help(frame, area, NORD));
        }
        Surface::Normal | Surface::Search(_) | Surface::Settings(_) => {}
    }
    if !matches!(&app.surface, Surface::Normal) || app.menu.is_some() {
        regions.selectable.clear();
    }
    regions
}

fn render_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let (library, project) = match (&app.library, &app.snapshot) {
        (LibraryReport::Unconfigured, _) => {
            ("not configured".to_owned(), "project unavailable".to_owned())
        }
        (LibraryReport::Configured { path }, Some(snapshot)) => {
            let project = match &snapshot.repository {
                RepositoryReport::Available { root } => root.display().to_string(),
                RepositoryReport::Unavailable { reason } => format!("unavailable · {reason}"),
            };
            (path.display().to_string(), project)
        }
        (LibraryReport::Configured { path }, None) => {
            (path.display().to_string(), "project unavailable".to_owned())
        }
    };
    let issues = app.snapshot.as_ref().map_or(0, |snapshot| snapshot.problem_count());
    let columns = Layout::horizontal([Constraint::Fill(1), Constraint::Length(18)]).split(area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    " SKILLS ",
                    Style::default().fg(NORD.text_strong).add_modifier(Modifier::BOLD),
                ),
                Span::styled(library, Style::default().fg(NORD.accent)),
            ]),
            Line::from(vec![
                Span::styled(" current project ", Style::default().fg(NORD.text_muted)),
                Span::styled(project, Style::default().fg(NORD.text)),
            ]),
        ]),
        columns[0],
    );
    let issue_style = if issues == 0 {
        Style::default().fg(NORD.success)
    } else {
        Style::default().fg(NORD.warning).add_modifier(Modifier::BOLD)
    };
    frame.render_widget(
        Paragraph::new(if issues == 0 {
            "healthy".to_owned()
        } else {
            format!("{issues} issue(s)")
        })
        .alignment(Alignment::Right)
        .style(issue_style),
        columns[1],
    );
}

fn render_catalog(frame: &mut Frame<'_>, area: Rect, app: &mut App, regions: &mut UiRegions) {
    let focused = app.active_region == SkillsRegion::Catalog;
    let total = app.snapshot.as_ref().map_or(0, |snapshot| snapshot.skills.len());
    let count = app.visible_skills.len();
    let invalid = app.snapshot.as_ref().map_or(0, |snapshot| snapshot.invalid.len());
    let visible =
        if app.search_query.is_empty() { count.to_string() } else { format!("{count}/{total}") };
    let title = if invalid == 0 {
        format!(" Skills {visible} ")
    } else {
        format!(" Skills {visible} · {invalid} invalid ")
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused { NORD.focus } else { NORD.border }));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let projection_column_width = if inner.width >= WIDE_MATRIX_MINIMUM_WIDTH {
        WIDE_PROJECTION_COLUMN_WIDTH
    } else {
        COMPACT_PROJECTION_COLUMN_WIDTH
    };
    let rows =
        Layout::vertical([Constraint::Length(1), Constraint::Length(2), Constraint::Fill(1)])
            .split(inner);
    render_search(frame, rows[0], app, regions);
    render_matrix_header(frame, rows[1], projection_column_width);
    regions.catalog_content = Some(rows[2]);
    if count == 0 {
        let empty = if !app.search_query.is_empty() {
            format!("No skills match /{}", app.search_query)
        } else if matches!(&app.library, LibraryReport::Unconfigured) {
            "Configure a library to begin".to_owned()
        } else {
            "No valid canonical skills · press n to create one".to_owned()
        };
        frame.render_widget(
            Paragraph::new(empty)
                .style(Style::default().fg(NORD.text_muted))
                .wrap(Wrap { trim: true }),
            rows[2].inner(Margin { horizontal: 1, vertical: 1 }),
        );
        return;
    }

    let metrics = ViewportMetrics::new(count, usize::from(rows[2].height));
    app.catalog_viewport.normalize(metrics);
    if let Some(selected) = app.selected_index() {
        app.catalog_viewport.ensure_visible(selected, metrics);
    }
    let top = app.catalog_viewport.top(metrics);
    let range = app.catalog_viewport.visible_range(metrics);
    let scrollbar = ScrollbarLayout::vertical_right(rows[2], metrics, top);
    let row_width = rows[2].width.saturating_sub(u16::from(scrollbar.is_some()));
    let row_area = Rect::new(rows[2].x, rows[2].y, row_width, rows[2].height);
    for (visible, index) in range.enumerate() {
        let status =
            app.status(&app.visible_skills[index]).expect("filtered skill comes from snapshot");
        let row = Rect::new(row_area.x, row_area.y + visible as u16, row_area.width, 1);
        render_skill_row(frame, row, status, app, projection_column_width, regions);
    }
    if let Some(scrollbar) = scrollbar {
        render_vertical_scrollbar(frame, scrollbar, false, scrollbar_style(NORD));
    }
}

fn render_search(frame: &mut Frame<'_>, area: Rect, app: &App, regions: &mut UiRegions) {
    regions.search = Some(area);
    let (value, active) = match &app.surface {
        Surface::Search(editor) => (editor.value(), true),
        _ if app.search_query.is_empty() => ("Search skills", false),
        _ => (app.search_query.as_str(), false),
    };
    frame.render_widget(
        Paragraph::new(format!(" / {value}")).style(
            Style::default()
                .fg(if active { NORD.text_strong } else { NORD.text_muted })
                .bg(if active { NORD.selection } else { NORD.surface }),
        ),
        area,
    );
    if let Surface::Search(editor) = &app.surface {
        let input = Rect::new(area.x.saturating_add(3), area.y, area.width.saturating_sub(3), 1);
        set_editor_cursor(frame, input, editor);
    }
}

fn render_matrix_header(frame: &mut Frame<'_>, area: Rect, projection_column_width: u16) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let matrix_width = (projection_column_width * 4).min(area.width);
    let name_width = area.width.saturating_sub(matrix_width);
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);
    let groups = Layout::horizontal([
        Constraint::Length(name_width),
        Constraint::Length(projection_column_width * 2),
        Constraint::Length(projection_column_width * 2),
    ])
    .split(rows[0]);
    frame.render_widget(
        Paragraph::new(" SKILL").style(Style::default().fg(NORD.text_muted)),
        groups[0],
    );
    for (group, label) in groups[1..].iter().zip(["THIS PROJECT", "ALL PROJECTS"]) {
        frame.render_widget(
            Paragraph::new(label)
                .alignment(Alignment::Center)
                .style(Style::default().fg(NORD.text_muted).add_modifier(Modifier::BOLD)),
            *group,
        );
    }
    let columns = Layout::horizontal([
        Constraint::Length(name_width),
        Constraint::Length(projection_column_width),
        Constraint::Length(projection_column_width),
        Constraint::Length(projection_column_width),
        Constraint::Length(projection_column_width),
    ])
    .split(rows[1]);
    let labels = if projection_column_width == WIDE_PROJECTION_COLUMN_WIDTH {
        ["Claude Code", "Codex", "Claude Code", "Codex"]
    } else {
        ["Claude", "Codex", "Claude", "Codex"]
    };
    for (column, label) in columns[1..].iter().zip(labels) {
        frame.render_widget(
            Paragraph::new(label)
                .alignment(Alignment::Center)
                .style(Style::default().fg(NORD.text_muted)),
            *column,
        );
    }
}

fn render_skill_row(
    frame: &mut Frame<'_>,
    row: Rect,
    status: &SkillStatus,
    app: &App,
    projection_column_width: u16,
    regions: &mut UiRegions,
) {
    let selected = app.selected.as_deref() == Some(status.skill.name.as_str());
    let row_style = Style::default()
        .fg(if selected { NORD.text_strong } else { NORD.text })
        .bg(if selected { NORD.selection } else { NORD.surface });
    frame.render_widget(Block::new().style(row_style), row);
    let matrix_width = (projection_column_width * 4).min(row.width);
    let name_width = row.width.saturating_sub(matrix_width);
    let columns = Layout::horizontal([
        Constraint::Length(name_width),
        Constraint::Length(projection_column_width),
        Constraint::Length(projection_column_width),
        Constraint::Length(projection_column_width),
        Constraint::Length(projection_column_width),
    ])
    .split(row);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(if selected { "› " } else { "  " }, row_style.fg(NORD.accent)),
            Span::styled(status.skill.name.as_str().to_owned(), row_style),
        ])),
        columns[0],
    );
    let name = status.skill.name.as_str().to_owned();
    regions.skill_rows.push((columns[0], name.clone()));
    for (index, id) in ProjectionId::ALL.iter().enumerate() {
        let cell = columns[index + 1];
        let report = status.projections.iter().find(|report| report.id() == *id);
        let (symbol, color) = projection_symbol(report, NORD);
        let cell_focused =
            selected && app.active_region == SkillsRegion::Catalog && app.projection_index == index;
        let style = Style::default()
            .fg(color)
            .bg(if cell_focused {
                NORD.focus
            } else if selected {
                NORD.selection
            } else {
                NORD.surface
            })
            .add_modifier(if cell_focused { Modifier::BOLD } else { Modifier::empty() });
        frame.render_widget(Paragraph::new(symbol).alignment(Alignment::Center).style(style), cell);
        regions.projection_cells.push((cell, name.clone(), *id));
    }
}

fn projection_symbol(report: Option<&ProjectionReport>, theme: TuiTheme) -> (&'static str, Color) {
    match report {
        Some(ProjectionReport::Observed {
            projection: ProjectionState::Enabled { .. }, ..
        }) => ("✓", theme.success),
        Some(ProjectionReport::Observed { projection: ProjectionState::Disabled, .. }) => {
            ("·", theme.text_muted)
        }
        Some(ProjectionReport::Observed {
            projection: ProjectionState::BrokenLink { .. }, ..
        }) => ("!", theme.warning),
        Some(ProjectionReport::Observed {
            projection: ProjectionState::ForeignLink { .. },
            ..
        }) => ("↗", theme.warning),
        Some(ProjectionReport::Observed {
            projection: ProjectionState::Occupied { .. }, ..
        }) => ("■", theme.warning),
        Some(ProjectionReport::Unavailable { .. }) | None => ("?", theme.danger),
    }
}

fn render_details(frame: &mut Frame<'_>, area: Rect, app: &mut App, regions: &mut UiRegions) {
    let focused = app.active_region == SkillsRegion::Details;
    let title = app
        .selected_status()
        .map(|status| format!(" {} ", status.skill.name))
        .unwrap_or_else(|| " Details ".to_owned());
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused { NORD.focus } else { NORD.border }));
    let inner = block.inner(area).inner(Margin { horizontal: 1, vertical: 0 });
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let action_height = if inner.height >= 12 { 3 } else { 0 };
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(action_height),
    ])
    .split(inner);
    render_tabs(frame, rows[0], app, regions);
    render_detail_content(frame, rows[1], app, regions);
    if action_height > 0 {
        render_actions(frame, rows[2], app, regions);
    }
}

fn render_tabs(frame: &mut Frame<'_>, area: Rect, app: &App, regions: &mut UiRegions) {
    let widths = DetailTab::ALL.map(|tab| Constraint::Length(tab.label().width() as u16 + 3));
    let columns = Layout::horizontal(widths).split(area);
    for (column, tab) in columns.iter().zip(DetailTab::ALL) {
        let selected = app.detail_tab == tab;
        frame.render_widget(
            Paragraph::new(format!(" {} ", tab.label())).style(
                Style::default()
                    .fg(if selected { NORD.text_strong } else { NORD.text_muted })
                    .bg(if selected { NORD.selection } else { NORD.surface })
                    .add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() }),
            ),
            *column,
        );
        regions.tab_rows.push((*column, tab));
    }
}

fn render_detail_content(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &mut App,
    regions: &mut UiRegions,
) {
    regions.detail_content = Some(area);
    let text = match app.detail_tab {
        DetailTab::Overview => overview_text(app),
        DetailTab::Content => content_text(app, area.width.saturating_sub(1)),
        DetailTab::Diagnostics => diagnostics_text(app),
    };
    let content_len = text.lines.len();
    regions.detail_content_len = content_len;
    let metrics = ViewportMetrics::new(content_len, usize::from(area.height));
    app.detail_viewport.normalize(metrics);
    let top = app.detail_viewport.top(metrics);
    let scrollbar = ScrollbarLayout::vertical_right(area, metrics, top);
    let content_width = area.width.saturating_sub(u16::from(scrollbar.is_some()));
    let content_area = Rect::new(area.x, area.y, content_width, area.height);
    frame.render_widget(
        Paragraph::new(text).scroll((u16::try_from(top).unwrap_or(u16::MAX), 0)),
        content_area,
    );
    if let Some(scrollbar) = scrollbar {
        render_vertical_scrollbar(frame, scrollbar, false, scrollbar_style(NORD));
    }
    if content_area.height > 0 {
        regions.selectable.push(SelectableRegion::new(
            SkillsSelectionSurface::Details,
            content_area,
            top as i64,
            0,
            app.document_revision,
        ));
    }
}

fn overview_text(app: &App) -> Text<'static> {
    let Some(status) = app.selected_status() else {
        return Text::from(vec![Line::styled(
            "Select or create a canonical skill.",
            Style::default().fg(NORD.text_muted),
        )]);
    };
    let mut lines = vec![
        detail_line("Name", status.skill.name.as_str(), NORD.text_strong),
        detail_line("Description", &status.skill.description, NORD.text),
        detail_line("Canonical", &status.skill.path.display().to_string(), NORD.accent),
        Line::default(),
    ];
    let id = ProjectionId::ALL[app.projection_index];
    lines.push(Line::styled(
        "Availability",
        Style::default().fg(NORD.text_strong).add_modifier(Modifier::BOLD),
    ));
    lines.push(detail_line("Where", id.scope.label(), NORD.text_strong));
    lines.push(detail_line("App", id.target.label(), NORD.text_strong));
    match app.selected_projection() {
        Some(ProjectionReport::Observed { path, projection, .. }) => {
            lines.push(detail_line(
                "Status",
                projection.short_label(),
                projection_color(projection, NORD),
            ));
            lines.push(detail_line("Discovery path", &path.display().to_string(), NORD.accent_alt));
            match projection {
                ProjectionState::Enabled { target } | ProjectionState::BrokenLink { target } => {
                    lines.push(detail_line(
                        "Link target",
                        &target.display().to_string(),
                        NORD.text,
                    ));
                }
                ProjectionState::ForeignLink { target, resolved_target } => {
                    lines.push(detail_line(
                        "Link target",
                        &target.display().to_string(),
                        NORD.warning,
                    ));
                    lines.push(detail_line(
                        "Resolves to",
                        &resolved_target.display().to_string(),
                        NORD.warning,
                    ));
                }
                ProjectionState::Disabled | ProjectionState::Occupied { .. } => {}
            }
        }
        Some(ProjectionReport::Unavailable { reason, .. }) => {
            lines.push(detail_line("Status", "unavailable", NORD.danger));
            lines.push(detail_line("Reason", reason, NORD.warning));
        }
        None => lines.push(detail_line("Status", "missing observation", NORD.danger)),
    }
    lines.push(Line::default());
    lines.push(Line::styled(
        match id.scope {
            ProjectionScope::ThisProject => "Available only while you work in this Git project.",
            ProjectionScope::AllProjects => {
                "Available from every project you open on this computer."
            }
        },
        Style::default().fg(NORD.text_muted),
    ));
    lines.push(Line::styled(
        "Space toggles only an absent or exact manager-owned link. Foreign, broken, and occupied paths are never overwritten or removed.",
        Style::default().fg(NORD.text_muted),
    ));
    Text::from(lines)
}

fn content_text(app: &App, width: u16) -> Text<'static> {
    match &app.document {
        Some(document) if app.selected.as_deref() == Some(document.skill.as_str()) => {
            MarkdownRenderer::new(NORD).render(&document.markdown, width.max(1))
        }
        Some(_) | None => Text::from(Line::styled(
            "Loading canonical SKILL.md…",
            Style::default().fg(NORD.text_muted),
        )),
    }
}

fn diagnostics_text(app: &App) -> Text<'static> {
    let mut lines = Vec::new();
    if let Some(status) = app.selected_status() {
        lines.push(Line::styled(
            "Availability by destination",
            Style::default().fg(NORD.text_strong).add_modifier(Modifier::BOLD),
        ));
        for id in ProjectionId::ALL {
            let report = status.projections.iter().find(|report| report.id() == id);
            let (label, color) = match report {
                Some(ProjectionReport::Observed { projection, .. }) => {
                    (projection.short_label().to_owned(), projection_color(projection, NORD))
                }
                Some(ProjectionReport::Unavailable { .. }) | None => {
                    ("unavailable".to_owned(), NORD.danger)
                }
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{:<32}", id.label()), Style::default().fg(NORD.text_muted)),
                Span::styled(label, Style::default().fg(color)),
            ]));
        }
        lines.push(Line::default());
    }
    if let Some(snapshot) = &app.snapshot {
        if !snapshot.invalid.is_empty() {
            lines.push(Line::styled(
                "Invalid canonical entries",
                Style::default().fg(NORD.warning).add_modifier(Modifier::BOLD),
            ));
            for invalid in &snapshot.invalid {
                lines.push(Line::styled(
                    format!("{} · {}", invalid.directory, invalid.error),
                    Style::default().fg(NORD.warning),
                ));
            }
            lines.push(Line::default());
        }
    }
    match &app.doctor {
        Some(report) if report.healthy() => lines.push(Line::styled(
            "Doctor: healthy",
            Style::default().fg(NORD.success).add_modifier(Modifier::BOLD),
        )),
        Some(report) => {
            lines.push(Line::styled(
                format!("Doctor · {} issue(s)", report.issues.len()),
                Style::default().fg(NORD.warning).add_modifier(Modifier::BOLD),
            ));
            for issue in &report.issues {
                lines.push(Line::styled(doctor_issue(issue), Style::default().fg(NORD.warning)));
            }
        }
        None => lines.push(Line::styled(
            "Press D to run a fresh diagnosis.",
            Style::default().fg(NORD.text_muted),
        )),
    }
    Text::from(lines)
}

fn doctor_issue(issue: &DoctorIssue) -> String {
    match issue {
        DoctorIssue::LibraryUnconfigured => "library · not configured".to_owned(),
        DoctorIssue::LibraryUnavailable { path, error } => {
            format!("library {} · {error}", path.display())
        }
        DoctorIssue::RepositoryUnavailable { error } => format!("project · {error}"),
        DoctorIssue::InvalidSkill { directory, error, .. } => {
            format!("skill {directory} · {error}")
        }
        DoctorIssue::ProjectionProblem { skill, projection, state, .. } => {
            format!("{} {} · {}", skill, projection.label(), state.short_label())
        }
        DoctorIssue::ProjectionUnavailable { skill, projection, error } => {
            format!("{} {} · {error}", skill, projection.label())
        }
    }
}

fn render_actions(frame: &mut Frame<'_>, area: Rect, app: &App, regions: &mut UiRegions) {
    let context = app.action_context(regions);
    let actions = app.registry.resolve_menu(DASHBOARD_ACTIONS, &context);
    for (row_index, action) in actions.items().iter().take(usize::from(area.height)).enumerate() {
        let row = Rect::new(area.x, area.y + row_index as u16, area.width, 1);
        let enabled = action.state.is_enabled();
        let shortcut =
            action.primary_keybinding().map(|binding| binding.to_string()).unwrap_or_default();
        let columns = Layout::horizontal([Constraint::Fill(1), Constraint::Length(12)]).split(row);
        frame.render_widget(
            Paragraph::new(format!("  {}", action.title)).style(Style::default().fg(if enabled {
                NORD.text
            } else {
                NORD.text_muted
            })),
            columns[0],
        );
        frame.render_widget(
            Paragraph::new(shortcut)
                .alignment(Alignment::Right)
                .style(Style::default().fg(NORD.text_muted)),
            columns[1],
        );
        regions.action_rows.push((row, action.id));
    }
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let hint =
        " ↑↓/jk skill  ←→ destination  Space toggle  n new  / search  D doctor  Ctrl-P commands  ? help  q quit ";
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(hint, Style::default().fg(NORD.text_muted)),
            Line::styled(
                app.notice.as_deref().unwrap_or("Ready"),
                Style::default().fg(if app.notice.is_some() {
                    NORD.accent_alt
                } else {
                    NORD.text_muted
                }),
            ),
        ]),
        area,
    );
}

fn render_create_skill(
    frame: &mut Frame<'_>,
    area: Rect,
    form: &CreateSkillForm,
    library: Option<&std::path::Path>,
    snapshot: Option<&SkillsSnapshot>,
    theme: TuiTheme,
) -> CreateSkillLayout {
    let popup = centered(area, area.width.min(84), area.height.min(16));
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" Create canonical skill ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.focus));
    let inner = block.inner(popup).inner(Margin { horizontal: 2, vertical: 1 });
    frame.render_widget(block, popup);
    let mut fields = Vec::new();
    for (index, field) in CreateField::ALL.iter().enumerate() {
        let y = inner.y + index as u16 * 3;
        if y + 1 >= inner.bottom() {
            break;
        }
        frame.render_widget(
            Paragraph::new(field.label()).style(Style::default().fg(theme.text_muted)),
            Rect::new(inner.x, y, inner.width, 1),
        );
        let input = Rect::new(inner.x, y + 1, inner.width, 1);
        let selected = form.active == index;
        frame.render_widget(
            Paragraph::new(form.inputs[index].value()).style(
                Style::default().fg(theme.text_strong).bg(if selected {
                    theme.selection
                } else {
                    theme.surface
                }),
            ),
            input,
        );
        fields.push(input);
        if selected {
            set_editor_cursor(frame, input, &form.inputs[index]);
        }
    }
    let name = form.inputs[CreateField::Name as usize].value().trim();
    let description = form.inputs[CreateField::Description as usize].value().trim();
    let parsed_name = SkillName::parse(name.to_owned());
    let destination = library.map(|library| library.join(name).join("SKILL.md"));
    let destination_absent = parsed_name.is_ok()
        && snapshot.is_none_or(|snapshot| {
            !snapshot.skills.iter().any(|status| status.skill.name.as_str() == name)
                && !snapshot.invalid.iter().any(|invalid| invalid.directory == name)
        });
    let preview_y = inner.y + 6;
    frame.render_widget(
        Paragraph::new(format!(
            "Will create  {}",
            destination.as_ref().map_or_else(
                || "library not configured".to_owned(),
                |path| path.display().to_string()
            )
        ))
        .style(Style::default().fg(theme.accent)),
        Rect::new(inner.x, preview_y, inner.width, 1),
    );
    for (offset, (valid, label)) in [
        (parsed_name.is_ok(), "name is spec-valid"),
        (destination_absent, "destination is absent"),
        (!description.is_empty(), "description is present"),
    ]
    .into_iter()
    .enumerate()
    {
        frame.render_widget(
            Paragraph::new(format!("{} {label}", if valid { "✓" } else { "·" }))
                .style(Style::default().fg(if valid { theme.success } else { theme.text_muted })),
            Rect::new(inner.x, preview_y + 2 + offset as u16, inner.width, 1),
        );
    }
    let buttons =
        Layout::horizontal([Constraint::Length(12), Constraint::Length(11), Constraint::Fill(1)])
            .split(Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1));
    frame.render_widget(
        Paragraph::new("[ Create ]")
            .style(Style::default().fg(theme.text_strong).add_modifier(Modifier::BOLD)),
        buttons[0],
    );
    frame.render_widget(
        Paragraph::new("[ Cancel ]").style(Style::default().fg(theme.text_muted)),
        buttons[1],
    );
    CreateSkillLayout { fields, submit: buttons[0], cancel: buttons[1] }
}

fn render_library(
    frame: &mut Frame<'_>,
    area: Rect,
    form: &LibraryForm,
    theme: TuiTheme,
) -> LibraryLayout {
    let popup = centered(area, area.width.min(84), area.height.min(13));
    frame.render_widget(Clear, popup);
    let title = if form.required {
        " Set up canonical Skills library "
    } else {
        " Change canonical Skills library "
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.focus));
    let inner = block.inner(popup).inner(Margin { horizontal: 2, vertical: 1 });
    frame.render_widget(block, popup);
    frame.render_widget(
        Paragraph::new(
            "One source of truth. Choose where each skill is available and whether Claude Code or Codex can discover it.",
        )
            .style(Style::default().fg(theme.text))
            .wrap(Wrap { trim: true }),
        Rect::new(inner.x, inner.y, inner.width, 2),
    );
    frame.render_widget(
        Paragraph::new("Library path").style(Style::default().fg(theme.text_muted)),
        Rect::new(inner.x, inner.y + 3, inner.width, 1),
    );
    let path = Rect::new(inner.x, inner.y + 4, inner.width, 1);
    frame.render_widget(
        Paragraph::new(form.path.value()).style(
            Style::default().fg(theme.text_strong).bg(if form.active == 0 {
                theme.selection
            } else {
                theme.surface
            }),
        ),
        path,
    );
    if form.active == 0 {
        set_editor_cursor(frame, path, &form.path);
    }
    frame.render_widget(
        Paragraph::new("Choose explicitly whether Kit may create the directory.")
            .style(Style::default().fg(theme.text_muted)),
        Rect::new(inner.x, inner.y + 6, inner.width, 1),
    );
    let buttons = Layout::horizontal([
        Constraint::Length(23),
        Constraint::Length(26),
        Constraint::Length(11),
        Constraint::Fill(1),
    ])
    .split(Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1));
    frame.render_widget(
        Paragraph::new("[ Configure existing ]").style(
            Style::default()
                .fg(theme.text_strong)
                .bg(if form.active == 1 { theme.selection } else { theme.surface })
                .add_modifier(Modifier::BOLD),
        ),
        buttons[0],
    );
    frame.render_widget(
        Paragraph::new("[ Create and configure ]").style(
            Style::default()
                .fg(theme.text_strong)
                .bg(if form.active == 2 { theme.selection } else { theme.surface })
                .add_modifier(Modifier::BOLD),
        ),
        buttons[1],
    );
    frame.render_widget(
        Paragraph::new("[ Cancel ]").style(
            Style::default().fg(theme.text_muted).bg(if form.active == 3 {
                theme.selection
            } else {
                theme.surface
            }),
        ),
        buttons[2],
    );
    LibraryLayout { path, configure: buttons[0], create: buttons[1], cancel: buttons[2] }
}

fn render_doctor(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &DoctorView,
    theme: TuiTheme,
) -> (Vec<(Rect, usize)>, Rect) {
    let popup = centered(area, area.width.min(100), area.height.min(28));
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" Skills doctor ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.focus));
    let inner = block.inner(popup).inner(Margin { horizontal: 2, vertical: 1 });
    frame.render_widget(block, popup);
    let status = if view.report.healthy() {
        "Healthy".to_owned()
    } else {
        format!("{} issue(s)", view.report.issues.len())
    };
    frame.render_widget(
        Paragraph::new(status).style(
            Style::default()
                .fg(if view.report.healthy() { theme.success } else { theme.warning })
                .add_modifier(Modifier::BOLD),
        ),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let list_height = inner.height.saturating_sub(4);
    let capacity = usize::from(list_height / 2).max(1);
    let start = view.selected.saturating_sub(capacity.saturating_sub(1));
    let mut rows = Vec::new();
    for (visible, (index, issue)) in
        view.report.issues.iter().enumerate().skip(start).take(capacity).enumerate()
    {
        let row = Rect::new(inner.x, inner.y + 2 + visible as u16 * 2, inner.width, 2);
        let selected = index == view.selected;
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(
                        if selected { "› " } else { "  " },
                        Style::default().fg(theme.accent),
                    ),
                    Span::styled(
                        doctor_issue(issue),
                        Style::default().fg(if selected { theme.text_strong } else { theme.text }),
                    ),
                ]),
                Line::styled(
                    doctor_issue_path(issue).unwrap_or_default(),
                    Style::default().fg(theme.text_muted),
                ),
            ])
            .style(Style::default().bg(if selected {
                theme.selection
            } else {
                theme.surface
            })),
            row,
        );
        rows.push((row, index));
    }
    let close = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width.min(72), 1);
    frame.render_widget(
        Paragraph::new(" Enter inspect   c copy path   r refresh   Esc dashboard ")
            .style(Style::default().fg(theme.text_muted)),
        close,
    );
    (rows, close)
}

fn render_help(frame: &mut Frame<'_>, area: Rect, theme: TuiTheme) -> Rect {
    let popup = centered(area, area.width.min(88), area.height.min(29));
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" Skills help ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.focus));
    let inner = block.inner(popup).inner(Margin { horizontal: 2, vertical: 1 });
    frame.render_widget(block, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                "Navigation",
                Style::default().fg(theme.text_strong).add_modifier(Modifier::BOLD),
            ),
            Line::raw("  ↑/k, ↓/j       select skill"),
            Line::raw("  ←, →           choose availability destination"),
            Line::raw("  Enter          inspect selected skill"),
            Line::raw("  Tab            switch catalog/details"),
            Line::default(),
            Line::styled(
                "What the columns mean",
                Style::default().fg(theme.text_strong).add_modifier(Modifier::BOLD),
            ),
            Line::raw("  This project    only while you work in this Git project"),
            Line::raw("  All projects    every project you open on this computer"),
            Line::raw("  Claude Code     discovered through .claude/skills"),
            Line::raw("  Codex           discovered through .agents/skills"),
            Line::default(),
            Line::styled(
                "Actions",
                Style::default().fg(theme.text_strong).add_modifier(Modifier::BOLD),
            ),
            Line::raw("  Space          enable or disable immediately"),
            Line::raw("  Shift-F10      focused skill actions"),
            Line::raw("  n              new canonical skill"),
            Line::raw("  /              search catalog"),
            Line::raw("  D / L / r      doctor / library / refresh"),
            Line::raw("  s / Ctrl-P     settings / command palette"),
            Line::default(),
            Line::raw("  ✓ enabled   · disabled   ! broken   ↗ foreign   ■ occupied"),
        ])
        .style(Style::default().fg(theme.text)),
        inner,
    );
    let close = Rect::new(inner.x, inner.bottom().saturating_sub(1), 22.min(inner.width), 1);
    frame.render_widget(
        Paragraph::new("[ Esc / ? close ]").style(Style::default().fg(theme.text_muted)),
        close,
    );
    close
}

fn doctor_issue_path(issue: &DoctorIssue) -> Option<String> {
    match issue {
        DoctorIssue::LibraryUnavailable { path, .. }
        | DoctorIssue::InvalidSkill { path, .. }
        | DoctorIssue::ProjectionProblem { path, .. } => Some(path.display().to_string()),
        DoctorIssue::LibraryUnconfigured
        | DoctorIssue::RepositoryUnavailable { .. }
        | DoctorIssue::ProjectionUnavailable { .. } => None,
    }
}

fn detail_line(label: &str, value: &str, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<16}"), Style::default().fg(NORD.text_muted)),
        Span::styled(value.to_owned(), Style::default().fg(color)),
    ])
}

fn projection_color(state: &ProjectionState, theme: TuiTheme) -> Color {
    match state {
        ProjectionState::Enabled { .. } => theme.success,
        ProjectionState::Disabled => theme.text_muted,
        ProjectionState::BrokenLink { .. }
        | ProjectionState::ForeignLink { .. }
        | ProjectionState::Occupied { .. } => theme.warning,
    }
}

fn scrollbar_style(theme: TuiTheme) -> ScrollbarStyle {
    ScrollbarStyle {
        track_color: theme.border,
        thumb_color: theme.accent,
        active_thumb_color: theme.focus,
        track_symbol: "│",
        thumb_symbol: "┃",
    }
}

fn set_editor_cursor(frame: &mut Frame<'_>, area: Rect, editor: &crate::tui::LineEditor) {
    let cursor = u16::try_from(editor.value()[..editor.cursor()].width())
        .unwrap_or(u16::MAX)
        .min(area.width.saturating_sub(1));
    frame.set_cursor_position((area.x.saturating_add(cursor), area.y));
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}
