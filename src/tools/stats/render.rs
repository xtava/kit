use std::collections::VecDeque;

use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

use super::app::{ActiveRegion, ConfirmationChoice, InspectorTab, SortBy, StatsApp};
use super::host::ProcessAction;
use super::model::{
    CapabilityState, DetailCompleteness, DetailData, DetailOutcome, Observed, ResourceSample,
    SampleReadiness,
};
use super::report;
use crate::tui::{theme::NORD, NavigationMap, NavigationRegion};

const BACKGROUND: Color = NORD.background;
const PANEL: Color = NORD.surface;
const BORDER: Color = NORD.border;
const TEXT: Color = NORD.text;
const PAPER: Color = NORD.text_strong;
const MUTED: Color = NORD.text_muted;
const ACCENT: Color = NORD.accent;
const CPU_ACCENT: Color = NORD.accent_alt;
const HIGHLIGHT: Color = NORD.focus;
const GOOD: Color = NORD.info;
const WARN: Color = NORD.warning;
const SELECTED: Color = NORD.selection;

#[derive(Default)]
pub(super) struct UiRegions {
    pub(super) processes: Option<Rect>,
    pub(super) inspector: Option<Rect>,
    pub(super) cores: Vec<(Rect, u16)>,
    pub(super) rows: Vec<(Rect, usize)>,
    pub(super) headers: Vec<(Rect, SortBy)>,
    pub(super) disclosures: Vec<(Rect, super::model::ProcessIdentity)>,
    pub(super) family_rows: Vec<(Rect, usize, super::model::ProcessIdentity)>,
    pub(super) thread_rows: Vec<(Rect, usize)>,
    pub(super) tabs: Vec<(Rect, InspectorTab)>,
    pub(super) profile: Option<Rect>,
    pub(super) command_open: Option<Rect>,
    pub(super) command_content: Option<Rect>,
    pub(super) command_close: Option<Rect>,
    pub(super) back: Option<Rect>,
    pub(super) end_process: Option<Rect>,
    pub(super) confirm_yes: Option<Rect>,
    pub(super) confirm_force: Option<Rect>,
    pub(super) confirm_cancel: Option<Rect>,
}

impl UiRegions {
    pub(super) fn navigation(&self) -> NavigationMap<ActiveRegion> {
        NavigationMap::new(
            [
                self.processes.map(|area| NavigationRegion::new(ActiveRegion::Processes, area)),
                self.inspector.map(|area| NavigationRegion::new(ActiveRegion::Inspector, area)),
            ]
            .into_iter()
            .flatten(),
        )
    }
}

pub(super) fn render(frame: &mut Frame<'_>, app: &StatsApp) -> UiRegions {
    let area = frame.area();
    let mut regions = UiRegions::default();
    frame.render_widget(Block::new().style(Style::default().bg(BACKGROUND)), area);
    if area.width < 72 || area.height < 20 {
        frame.render_widget(
            Paragraph::new("KIT / STATS\n\nProcess Investigator needs at least 72 × 20\n\nq  quit")
                .style(Style::default().fg(TEXT).bg(BACKGROUND))
                .block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(BORDER))
                        .title(Span::styled(" KIT STATS ", Style::default().fg(ACCENT))),
                )
                .wrap(Wrap { trim: true }),
            area,
        );
        return regions;
    }

    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Min(12),
        Constraint::Length(1),
    ])
    .split(area);
    render_header(frame, app, chunks[0]);
    render_system_band(frame, app, chunks[1], &mut regions);
    let wide = area.width >= 120;
    if wide {
        let columns = Layout::horizontal([Constraint::Percentage(64), Constraint::Percentage(36)])
            .split(chunks[2]);
        render_processes(frame, app, columns[0], true, &mut regions);
        render_inspector(frame, app, columns[1], false, &mut regions);
    } else if app.active_region == ActiveRegion::Inspector {
        render_inspector(frame, app, chunks[2], true, &mut regions);
    } else {
        render_processes(frame, app, chunks[2], false, &mut regions);
    }
    render_footer(frame, app, chunks[3]);
    if app.confirm.is_some() {
        render_confirmation(frame, app, &mut regions);
    } else if app.command_viewer.is_some() {
        render_command_viewer(frame, app, &mut regions);
    }
    regions
}

fn render_header(frame: &mut Frame<'_>, app: &StatsApp, area: Rect) {
    let system = &app.snapshot.system;
    let line = Line::from(vec![
        Span::styled(" KIT / STATS ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(" LIVE ", Style::default().fg(MUTED)),
        Span::styled(
            format!("{:.1}s", app.snapshot.interval_ms as f64 / 1_000.0),
            Style::default().fg(TEXT),
        ),
        Span::styled("  CPU ", Style::default().fg(MUTED)),
        Span::styled(
            format!("{:>5.1}%", system.global_cpu_percent),
            Style::default().fg(cpu_color(system.global_cpu_percent)).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  MEM ", Style::default().fg(MUTED)),
        Span::styled(
            format!(
                "{} / {}",
                report::bytes(system.used_memory_bytes),
                report::bytes(system.total_memory_bytes)
            ),
            Style::default().fg(TEXT),
        ),
        Span::styled("  LOAD ", Style::default().fg(MUTED)),
        Span::styled(
            format!(
                "{:.2} {:.2} {:.2}",
                system.load_average[0], system.load_average[1], system.load_average[2]
            ),
            Style::default().fg(TEXT),
        ),
    ]);
    frame.render_widget(Paragraph::new(line).style(Style::default().bg(BACKGROUND)), area);
}

fn render_system_band(frame: &mut Frame<'_>, app: &StatsApp, area: Rect, regions: &mut UiRegions) {
    let system = &app.snapshot.system;
    let inner = area.inner(Margin { horizontal: 1, vertical: 0 });
    let graph_width = (inner.width / 3).max(12) as usize;
    let total = Line::from(vec![
        Span::styled("CPU HISTORY  ", Style::default().fg(MUTED)),
        Span::styled(global_spark(&app.histories, graph_width), Style::default().fg(CPU_ACCENT)),
        Span::styled("  ", Style::default()),
        Span::styled(
            format!("{:.1}% now", system.global_cpu_percent),
            Style::default().fg(cpu_color(system.global_cpu_percent)).add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(Paragraph::new(total), Rect::new(inner.x, inner.y, inner.width, 1));

    let mut spans = vec![Span::styled("CORES  ", Style::default().fg(MUTED))];
    let available = inner.width.saturating_sub(7) as usize;
    let visible = (available / 6).min(system.cpus.len());
    for (index, cpu) in system.cpus.iter().take(visible).enumerate() {
        let graph = spark(app.histories.get(index)).chars().last().unwrap_or('▁');
        spans.push(Span::styled(
            format!("{:02}{graph}  ", cpu.logical_index),
            if app.focused_core == Some(cpu.logical_index) {
                Style::default().fg(PAPER).bg(SELECTED).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(cpu_color(cpu.usage_percent))
            },
        ));
    }
    if visible < system.cpus.len() {
        spans.push(Span::styled("more…", Style::default().fg(MUTED)));
    }
    let core_line = Rect::new(inner.x, inner.y + 1, inner.width, 1);
    frame.render_widget(Paragraph::new(Line::from(spans)), core_line);
    let mut x = core_line.x + 7;
    for cpu in system.cpus.iter().take(visible) {
        regions.cores.push((Rect::new(x, core_line.y, 4, 1), cpu.logical_index));
        x += 6;
    }
}

fn render_processes(
    frame: &mut Frame<'_>,
    app: &StatsApp,
    area: Rect,
    wide: bool,
    regions: &mut UiRegions,
) {
    regions.processes = Some(area);
    let arrow = if app.descending { "▼" } else { "▲" };
    let title = Line::from(vec![
        Span::styled(" PROCESS TREE ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(
                " {} {arrow}  {} processes",
                app.sort.label(),
                app.snapshot.system.process_count
            ),
            Style::default().fg(MUTED),
        ),
        Span::styled(
            if app.filter.value().is_empty() {
                "  / filter ".into()
            } else {
                format!("  / {} ", app.filter.value())
            },
            Style::default().fg(if app.filtering { TEXT } else { MUTED }),
        ),
    ]);
    let inner = area.inner(Margin { horizontal: 1, vertical: 1 });
    let visible_height = inner.height.saturating_sub(1) as usize;
    let rows = app.visible.iter().skip(app.row_offset).take(visible_height).map(|item| {
        let selected = Some(item.key) == app.selected;
        let marker = if !item.has_children {
            " "
        } else if app.collapsed.contains(&item.key) {
            "▸"
        } else {
            "▾"
        };
        let suffix = if app.collapsed.contains(&item.key) && item.hidden_descendants > 0 {
            format!(" +{}", item.hidden_descendants)
        } else {
            String::new()
        };
        let name = app.process(item.key).map_or("<exited>", |process| process.name.as_str());
        let program = format!("{}{marker} {name}{suffix}", "  ".repeat(item.depth as usize));
        let row_fg = if item.is_context {
            MUTED
        } else if item.is_match {
            ACCENT
        } else {
            TEXT
        };
        let style = if selected {
            Style::default().fg(PAPER).bg(SELECTED).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(row_fg).bg(PANEL)
        };
        let mut cells = vec![Cell::from(program)];
        if wide {
            cells.push(Cell::from(item.pid.to_string()).style(Style::default().fg(MUTED)));
        }
        cells.extend([
            Cell::from(format!("{:.1}%", item.cpu)),
            Cell::from(format!("{:.1}%", item.family_cpu)),
            Cell::from(report::bytes(item.memory)),
        ]);
        Row::new(cells).style(style)
    });
    let mut headers = vec![sort_header(app, "NAME / COMMAND", SortBy::Name)];
    if wide {
        headers.push(sort_header(app, "PID", SortBy::Pid));
    }
    headers.extend([
        sort_header(app, "CPU", SortBy::Cpu),
        Cell::from("FAMILY").style(Style::default().fg(MUTED)),
        sort_header(app, "MEM", SortBy::Memory),
    ]);
    let header = Row::new(headers).style(Style::default().bg(PANEL).add_modifier(Modifier::BOLD));
    let constraints = process_column_constraints(wide);
    let table =
        Table::new(rows, constraints.clone())
            .header(header)
            .style(Style::default().fg(TEXT).bg(PANEL))
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .style(Style::default().bg(PANEL))
                    .border_style(Style::default().fg(
                        if app.active_region == ActiveRegion::Processes { ACCENT } else { BORDER },
                    ))
                    .title(title),
            );
    frame.render_widget(table, area);

    if inner.height < 3 {
        return;
    }
    let columns = Layout::horizontal(constraints).spacing(1).split(inner);
    let header_area = |column: Rect| Rect::new(column.x, inner.y, column.width, 1);
    regions.headers.push((header_area(columns[0]), SortBy::Name));
    if wide {
        regions.headers.extend([
            (header_area(columns[1]), SortBy::Pid),
            (header_area(columns[2]), SortBy::Cpu),
            (header_area(columns[4]), SortBy::Memory),
        ]);
    } else {
        regions.headers.extend([
            (header_area(columns[1]), SortBy::Cpu),
            (header_area(columns[3]), SortBy::Memory),
        ]);
    }
    let row_top = inner.y + 1;
    for (screen_index, index) in
        (app.row_offset..app.visible.len()).take(visible_height).enumerate()
    {
        let y = row_top + screen_index as u16;
        regions.rows.push((Rect::new(inner.x, y, inner.width, 1), index));
        let disclosure_x = inner.x + app.visible[index].depth.saturating_mul(2);
        regions.disclosures.push((Rect::new(disclosure_x, y, 2, 1), app.visible[index].key));
    }
}

fn process_column_constraints(wide: bool) -> Vec<Constraint> {
    if wide {
        vec![
            Constraint::Min(24),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(9),
            Constraint::Length(9),
        ]
    } else {
        vec![
            Constraint::Min(24),
            Constraint::Length(8),
            Constraint::Length(9),
            Constraint::Length(9),
        ]
    }
}

fn sort_header(app: &StatsApp, label: &'static str, sort: SortBy) -> Cell<'static> {
    if app.sort == sort {
        let arrow = if app.descending { "▼" } else { "▲" };
        Cell::from(format!("{label} {arrow}")).style(Style::default().fg(ACCENT))
    } else {
        Cell::from(label).style(Style::default().fg(MUTED))
    }
}

fn render_inspector(
    frame: &mut Frame<'_>,
    app: &StatsApp,
    area: Rect,
    compact: bool,
    regions: &mut UiRegions,
) {
    regions.inspector = Some(area);
    let Some((process, is_live)) = app.selected_inspection() else {
        frame.render_widget(
            Paragraph::new("No process selected")
                .style(Style::default().fg(MUTED).bg(PANEL))
                .block(Block::bordered().border_type(BorderType::Rounded).border_style(
                    Style::default().fg(if app.active_region == ActiveRegion::Inspector {
                        ACCENT
                    } else {
                        BORDER
                    }),
                )),
            area,
        );
        return;
    };
    let row = app.visible.iter().find(|row| row.key == process.identity);
    let family_cpu = row.map_or(process.cpu_percent, |row| row.family_cpu);
    let family_memory = row.map_or(process.rss_bytes, |row| row.family_memory);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .style(Style::default().fg(TEXT).bg(PANEL))
        .border_style(Style::default().fg(if app.active_region == ActiveRegion::Inspector {
            ACCENT
        } else {
            BORDER
        }))
        .title(Line::from(vec![
            Span::styled(
                if compact { " ‹ PROCESS TREE " } else { " " },
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "{} · {} · PID {} ",
                    process.name,
                    if is_live { process.state.label() } else { "exited" },
                    process.identity.pid()
                ),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ]));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if compact {
        regions.back = Some(Rect::new(area.x, area.y, 16.min(area.width), 1));
    }

    let tab_area = Rect::new(inner.x, inner.y, inner.width, 1);
    let tab_width = (inner.width / InspectorTab::ALL.len() as u16).max(1);
    let mut tab_spans = Vec::new();
    for (index, tab) in InspectorTab::ALL.iter().copied().enumerate() {
        let width = if index + 1 == InspectorTab::ALL.len() {
            inner.right().saturating_sub(inner.x + index as u16 * tab_width)
        } else {
            tab_width
        };
        let tab_rect = Rect::new(inner.x + index as u16 * tab_width, inner.y, width, 1);
        regions.tabs.push((tab_rect, tab));
        tab_spans.push(Span::styled(
            format!("{:<width$}", tab.label(), width = width as usize),
            if tab == app.inspector_tab {
                Style::default().fg(PAPER).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default().fg(MUTED)
            },
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(tab_spans)), tab_area);

    let content = Rect::new(inner.x, inner.y + 2, inner.width, inner.height.saturating_sub(3));
    let lines = match app.inspector_tab {
        InspectorTab::Overview => {
            overview_lines(app, process, family_cpu, family_memory, content, regions)
        }
        InspectorTab::Family => {
            family_lines(app, process, family_cpu, family_memory, content, regions)
        }
        InspectorTab::Threads => thread_lines(app, content, regions),
        InspectorTab::Resources => resources_lines(app),
        InspectorTab::Profile => profile_lines(app),
    };
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), content);
    if inner.height >= 5 && is_live {
        let action_line = Rect::new(inner.x, inner.bottom() - 1, inner.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" p ", Style::default().fg(HIGHLIGHT).add_modifier(Modifier::BOLD)),
                Span::styled("Profile", Style::default().fg(MUTED)),
                Span::styled(
                    format!("{:>width$}", "End…", width = inner.width.saturating_sub(10) as usize),
                    Style::default().fg(MUTED),
                ),
            ])),
            action_line,
        );
        regions.profile = Some(Rect::new(inner.x, action_line.y, 10.min(inner.width), 1));
        regions.end_process =
            Some(Rect::new(inner.right().saturating_sub(6), action_line.y, 6.min(inner.width), 1));
    }
}

fn overview_lines(
    app: &StatsApp,
    process: &super::model::ProcessSample,
    family_cpu: f32,
    family_memory: u64,
    area: Rect,
    regions: &mut UiRegions,
) -> Vec<Line<'static>> {
    let core = process.last_cpu.map_or_else(|| "—".into(), |core| format!("C{core}"));
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("CPU       {:>7.1}%", process.cpu_percent),
                Style::default().fg(CPU_ACCENT),
            ),
            Span::styled(format!("    FAMILY {:>7.1}%", family_cpu), Style::default().fg(ACCENT)),
        ]),
        Line::from(format!(
            "MEMORY    {:>8}    FAMILY {:>8}",
            report::bytes(process.rss_bytes),
            report::bytes(family_memory)
        )),
        Line::from(format!(
            "LAST OBSERVED CPU  {core:<6}  UPTIME {}",
            report::duration(process.run_time_seconds)
        )),
    ];
    if let Some(history) = app.selected_history() {
        let width = area.width.saturating_sub(14) as usize;
        lines.push(Line::from(format!(
            "CPU HISTORY  {}",
            history_bars(history.points().map(|point| point.cpu_percent as f64), width)
        )));
        lines.push(Line::from(format!(
            "RSS HISTORY  {}",
            history_bars(history.points().map(|point| point.rss_bytes as f64), width)
        )));
    }
    let command_line = area.y + lines.len() as u16 + 1;
    regions.command_open = Some(Rect::new(
        area.x,
        command_line,
        area.width,
        area.bottom().saturating_sub(command_line),
    ));
    lines.extend([
        Line::from(""),
        Line::from(vec![
            Span::styled("COMMAND", Style::default().fg(MUTED)),
            Span::styled("  v / click to inspect", Style::default().fg(ACCENT)),
        ]),
        Line::styled(process.command.clone(), Style::default().fg(TEXT)),
    ]);
    lines
}

fn thread_lines(app: &StatsApp, area: Rect, regions: &mut UiRegions) -> Vec<Line<'static>> {
    let detail = app.detail.as_deref();
    let outcome = detail.and_then(|detail| match &detail.detail {
        DetailData::Threads { outcome, .. } | DetailData::Core { outcome, .. } => Some(outcome),
        DetailData::Resources { .. } => None,
    });
    let state = match outcome {
        Some(DetailOutcome::Available { readiness, completeness, .. }) => format!(
            "THREADS · {}",
            detail_status(
                app,
                detail.expect("outcome came from detail"),
                *readiness,
                *completeness,
            )
        ),
        Some(DetailOutcome::Unavailable(reason)) => {
            format!("THREADS · {}", detail_unavailable(reason))
        }
        None => "THREADS · LOADING…".to_owned(),
    };
    let mut lines = vec![Line::styled(
        format!(
            "{state}  ·  SORT {} {}",
            app.thread_sort.label(),
            if app.thread_descending { "▼" } else { "▲" }
        ),
        Style::default().fg(MUTED),
    )];
    let rows = app.sorted_threads();
    for (index, thread) in
        rows.iter().enumerate().skip(app.thread_offset).take(area.height.saturating_sub(2) as usize)
    {
        regions
            .thread_rows
            .push((Rect::new(area.x, area.y + lines.len() as u16, area.width, 1), index));
        let style = if index == app.thread_cursor {
            Style::default().fg(PANEL).bg(SELECTED).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT)
        };
        lines.push(Line::styled(
            format!(
                "{:>8}  {:>7}  {:>5}  {:>8}  {:>9}  {}",
                observed_percent(&thread.cpu_percent),
                thread.tid,
                observed_core(&thread.last_cpu),
                observed_seconds(&thread.accumulated_cpu_seconds),
                observed_process_state(&thread.state),
                observed_string(&thread.name),
            ),
            style,
        ));
    }
    lines
}

fn resources_lines(app: &StatsApp) -> Vec<Line<'static>> {
    match app.snapshot.host.resources {
        CapabilityState::Available => match app.detail.as_deref() {
            Some(detail) => match &detail.detail {
                DetailData::Resources { outcome, .. } => match outcome {
                    DetailOutcome::Available { readiness, completeness, value: resources } => {
                        resource_lines(
                            &detail_status(app, detail, *readiness, *completeness),
                            resources,
                        )
                    }
                    DetailOutcome::Unavailable(reason) => vec![Line::styled(
                        format!("RESOURCES · {}", detail_unavailable(reason)),
                        Style::default().fg(MUTED),
                    )],
                },
                DetailData::Threads { .. } | DetailData::Core { .. } => {
                    vec![Line::styled("RESOURCES · LOADING…", Style::default().fg(MUTED))]
                }
            },
            None => vec![Line::styled("RESOURCES · LOADING…", Style::default().fg(MUTED))],
        },
        CapabilityState::Unsupported { reason } => vec![
            Line::styled("RESOURCES UNAVAILABLE", Style::default().fg(MUTED)),
            Line::from(reason),
        ],
    }
}

fn profile_lines(app: &StatsApp) -> Vec<Line<'static>> {
    match app.snapshot.host.code_profile {
        CapabilityState::Available => vec![
            Line::styled("BOUNDED CODE PROFILE", Style::default().fg(ACCENT)),
            Line::from("2s   [5s]   10s   · 99 Hz"),
        ],
        CapabilityState::Unsupported { reason } => vec![
            Line::styled("PROFILE UNAVAILABLE", Style::default().fg(MUTED)),
            Line::from(reason),
        ],
    }
}

fn family_lines<'a>(
    app: &StatsApp,
    process: &super::model::ProcessSample,
    family_cpu: f32,
    family_memory: u64,
    area: Rect,
    regions: &mut UiRegions,
) -> Vec<Line<'a>> {
    let Some(family) = app.selected_family() else {
        return vec![Line::styled(
            "FAMILY RANKINGS UNAVAILABLE FOR EXITED TARGET",
            Style::default().fg(MUTED),
        )];
    };
    let cpu_share = if family_cpu > 0.0 {
        format!("{:.1}%", process.cpu_percent as f64 / family_cpu as f64 * 100.0)
    } else {
        "—".to_owned()
    };
    let memory_share = if family_memory > 0 {
        format!("{:.1}%", process.rss_bytes as f64 / family_memory as f64 * 100.0)
    } else {
        "—".to_owned()
    };
    let mut lines = vec![
        Line::styled("FAMILY · COMPLETE REPAIRED SUBTREE", Style::default().fg(MUTED)),
        Line::from(format!(
            "CHILDREN {} direct  ·  {} descendants",
            family.direct_children.len(),
            family.descendant_count
        )),
        Line::from(format!(
            "CPU  own {:.1}%  ·  family {:.1}%  ·  share {cpu_share}",
            process.cpu_percent, family_cpu
        )),
        Line::from(format!(
            "RSS  own {}  ·  summed family {}  ·  share {memory_share}",
            report::bytes(process.rss_bytes),
            report::bytes(family_memory)
        )),
        Line::from(""),
    ];
    let ranking_space = area.height.saturating_sub(lines.len() as u16) as usize;
    let row_space = ranking_space.saturating_sub(3);
    let base_rows = row_space / 3;
    let extra_rows = row_space % 3;
    let mut row_base = 0;
    for (index, (title, members)) in [
        ("BUSY CHILD BRANCHES", family.direct_children.as_slice()),
        ("HOT DESCENDANTS", family.hot_descendants.as_slice()),
        ("MEMORY DESCENDANTS", family.memory_descendants.as_slice()),
    ]
    .into_iter()
    .enumerate()
    {
        let capacity = base_rows + usize::from(index < extra_rows);
        let active = app.family_cursor.checked_sub(row_base).filter(|local| *local < members.len());
        let start = if capacity > 0 && members.len() > capacity {
            active
                .map(|local| local.saturating_sub(capacity / 2).min(members.len() - capacity))
                .unwrap_or_default()
        } else {
            0
        };
        let end = start.saturating_add(capacity).min(members.len());
        let title = if members.len() > capacity && capacity > 0 {
            format!("{title} · {}–{}/{}", start + 1, end, members.len())
        } else {
            format!("{title} · {}", members.len())
        };
        lines.push(Line::styled(title, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)));
        for (local_index, member) in members.iter().enumerate().skip(start).take(capacity) {
            let Some(candidate) =
                app.snapshot.processes.iter().find(|candidate| candidate.identity == member.key)
            else {
                continue;
            };
            let name_width = area.width.saturating_sub(34).max(6) as usize;
            let name = truncate(&candidate.name, name_width);
            let row_index = row_base + local_index;
            regions.family_rows.push((
                Rect::new(area.x, area.y + lines.len() as u16, area.width, 1),
                row_index,
                member.key,
            ));
            lines.push(Line::styled(
                format!(
                    "{name:<name_width$} {:>6}  {:>6.1}%  F {:>6.1}%  {}",
                    candidate.identity.pid(),
                    candidate.cpu_percent,
                    member.family_cpu_percent,
                    report::bytes(candidate.rss_bytes),
                ),
                if row_index == app.family_cursor {
                    Style::default().fg(PANEL).bg(SELECTED).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(TEXT)
                },
            ));
        }
        row_base += members.len();
    }
    lines
}

fn truncate(value: &str, width: usize) -> String {
    let mut characters = value.chars();
    let mut output = characters.by_ref().take(width).collect::<String>();
    if characters.next().is_some() && width > 0 {
        output.pop();
        output.push('…');
    }
    output
}

fn resource_lines<'a>(state: &str, resources: &ResourceSample) -> Vec<Line<'a>> {
    vec![
        Line::styled(format!("RESOURCES · {state}"), Style::default().fg(MUTED)),
        Line::from(format!("EXEC      {}", observed_path(&resources.executable))),
        Line::from(format!("CWD       {}", observed_path(&resources.current_directory))),
        Line::from(format!(
            "VIRTUAL / ADDRESS SPACE  {}",
            observed_bytes(&resources.virtual_bytes)
        )),
        Line::from(format!(
            "{}  {}",
            resources.open_resource_label.to_ascii_uppercase(),
            observed_number(&resources.open_resources)
        )),
        Line::from(format!(
            "{} READ   {} total  {} /s",
            resources.io_label.to_ascii_uppercase(),
            observed_bytes(&resources.read_bytes),
            observed_rate(&resources.read_bytes_per_second)
        )),
        Line::from(format!(
            "{} WRITE  {} total  {} /s",
            resources.io_label.to_ascii_uppercase(),
            observed_bytes(&resources.write_bytes),
            observed_rate(&resources.write_bytes_per_second)
        )),
    ]
}

fn detail_unavailable(reason: &super::model::DetailUnavailable) -> &'static str {
    use super::model::DetailUnavailable;
    match reason {
        DetailUnavailable::PermissionDenied => "PERMISSION DENIED",
        DetailUnavailable::Unsupported => "UNSUPPORTED",
        DetailUnavailable::TargetGone => "TARGET GONE",
        DetailUnavailable::TargetReplaced => "TARGET REPLACED",
        DetailUnavailable::Failed => "COLLECTION FAILED",
    }
}

fn detail_state(readiness: SampleReadiness, completeness: DetailCompleteness) -> &'static str {
    match (readiness, completeness) {
        (SampleReadiness::Warming, DetailCompleteness::Complete) => "WARMING DELTAS…",
        (SampleReadiness::Warming, DetailCompleteness::Partial) => "WARMING · PARTIAL",
        (SampleReadiness::Ready, DetailCompleteness::Complete) => "LIVE DETAIL",
        (SampleReadiness::Ready, DetailCompleteness::Partial) => "LIVE · PARTIAL",
    }
}

fn detail_status(
    app: &StatsApp,
    detail: &super::model::DetailSnapshot,
    readiness: SampleReadiness,
    completeness: DetailCompleteness,
) -> String {
    let age_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|now| u64::try_from(now.as_millis()).ok())
        .map(|now| now.saturating_sub(detail.sampled_at_ms))
        .unwrap_or_default();
    let minimum_ms =
        u64::try_from(detail.detail.kind().minimum_interval().as_millis()).unwrap_or(u64::MAX);
    let cadence_ms = app.snapshot.interval_ms.max(minimum_ms);
    if age_ms > cadence_ms.saturating_mul(2) {
        format!("STALE {:.1}s · {}", age_ms as f64 / 1_000.0, detail_state(readiness, completeness))
    } else {
        format!("{} · {:.1}s ago", detail_state(readiness, completeness), age_ms as f64 / 1_000.0)
    }
}

fn observed_path(value: &Observed<std::path::PathBuf>) -> String {
    value
        .value()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| observed_state(value).to_owned())
}

fn observed_string(value: &Observed<String>) -> String {
    value.value().cloned().unwrap_or_else(|| observed_state(value).to_owned())
}

fn observed_percent(value: &Observed<f32>) -> String {
    value
        .value()
        .map(|percent| format!("{percent:.1}%"))
        .unwrap_or_else(|| observed_state(value).to_owned())
}

fn observed_seconds(value: &Observed<f64>) -> String {
    value
        .value()
        .map(|seconds| format!("{seconds:.1}s"))
        .unwrap_or_else(|| observed_state(value).to_owned())
}

fn observed_core(value: &Observed<u16>) -> String {
    value.value().map(|core| format!("C{core}")).unwrap_or_else(|| observed_state(value).to_owned())
}

fn observed_process_state(value: &Observed<super::model::ProcessState>) -> String {
    value
        .value()
        .map(|state| state.label().to_owned())
        .unwrap_or_else(|| observed_state(value).to_owned())
}

fn observed_bytes(value: &Observed<u64>) -> String {
    value
        .value()
        .map(|bytes| report::bytes(*bytes))
        .unwrap_or_else(|| observed_state(value).to_owned())
}

fn observed_number(value: &Observed<u64>) -> String {
    value.value().map(u64::to_string).unwrap_or_else(|| observed_state(value).to_owned())
}

fn observed_rate(value: &Observed<f64>) -> String {
    value
        .value()
        .map(|bytes| report::bytes(*bytes as u64))
        .unwrap_or_else(|| observed_state(value).to_owned())
}

fn observed_state<T>(value: &Observed<T>) -> &'static str {
    match value {
        Observed::Value(_) => "available",
        Observed::Warming => "warming",
        Observed::PermissionDenied => "permission denied",
        Observed::Unsupported => "unsupported",
        Observed::TargetGone => "target gone",
        Observed::Failed => "unavailable",
    }
}

fn history_bars(values: impl Iterator<Item = f64>, width: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let values = values.collect::<Vec<_>>();
    let visible = &values[values.len().saturating_sub(width)..];
    let maximum = visible.iter().copied().fold(0.0_f64, f64::max).max(1.0);
    let bars = visible
        .iter()
        .map(|value| {
            let index = ((*value / maximum) * (BARS.len() - 1) as f64).round() as usize;
            BARS[index.min(BARS.len() - 1)]
        })
        .collect::<String>();
    format!("{}{bars}", " ".repeat(width.saturating_sub(bars.chars().count())))
}

fn render_footer(frame: &mut Frame<'_>, app: &StatsApp, area: Rect) {
    if app.filtering {
        frame.render_widget(
            Paragraph::new(format!(" /{}▏", app.filter.value()))
                .style(Style::default().fg(TEXT).bg(BACKGROUND)),
            area,
        );
    } else if let Some(status) = app.action_lifecycle.status().or_else(|| app.status.clone()) {
        frame.render_widget(
            Paragraph::new(format!(" {status}  │  q quit  / search  f focus  enter inspect"))
                .style(Style::default().fg(MUTED).bg(BACKGROUND)),
            area,
        );
    } else {
        let key = Style::default().fg(HIGHLIGHT).add_modifier(Modifier::BOLD);
        let help = match app.active_region {
            ActiveRegion::Processes => Line::from(vec![
                Span::styled(" ↑↓ ", key),
                Span::styled("select   ", Style::default().fg(MUTED)),
                Span::styled("home ", key),
                Span::styled("top   ", Style::default().fg(MUTED)),
                Span::styled("←→ ", key),
                Span::styled("tree / inspect   ", Style::default().fg(MUTED)),
                Span::styled("tab ", key),
                Span::styled("region   ", Style::default().fg(MUTED)),
                Span::styled("/ ", key),
                Span::styled("search   ", Style::default().fg(MUTED)),
                Span::styled("f ", key),
                Span::styled("focus root   ", Style::default().fg(MUTED)),
                Span::styled("q ", key),
                Span::styled("quit", Style::default().fg(MUTED)),
            ]),
            ActiveRegion::Inspector => Line::from(vec![
                Span::styled(" ←→ ", key),
                Span::styled("tabs / processes   ", Style::default().fg(MUTED)),
                Span::styled("tab ", key),
                Span::styled("region   ", Style::default().fg(MUTED)),
                Span::styled("p ", key),
                Span::styled("profile   ", Style::default().fg(MUTED)),
                Span::styled("esc ", key),
                Span::styled("back   ", Style::default().fg(MUTED)),
                Span::styled("q ", key),
                Span::styled("quit", Style::default().fg(MUTED)),
            ]),
        };
        frame.render_widget(Paragraph::new(help).style(Style::default().bg(BACKGROUND)), area);
    }
}

fn render_confirmation(frame: &mut Frame<'_>, app: &StatsApp, regions: &mut UiRegions) {
    let confirm = app.confirm.as_ref().expect("called only with confirmation");
    let area = centered(frame.area(), 58, 9);
    frame.render_widget(Clear, area);
    frame.render_widget(Block::new().style(Style::default().bg(PANEL)), area);
    let warning = if confirm.action == ProcessAction::ForceTerminate
        || confirm.choice == ConfirmationChoice::Force
    {
        "Force kill cannot be handled or cleaned up by the process."
    } else {
        "The process may save work and shut down cleanly."
    };
    let text = vec![
        Line::from(format!("End {} (PID {})?", confirm.name, confirm.key.pid)),
        Line::from(""),
        Line::styled(
            confirm.action.label(),
            Style::default().fg(WARN).add_modifier(Modifier::BOLD),
        ),
        Line::from(warning),
        Line::from(""),
        Line::styled("←→ choose   ENTER activate   ESC cancel", Style::default().fg(ACCENT)),
    ];
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(TEXT).bg(PANEL))
            .wrap(Wrap { trim: true })
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .style(Style::default().bg(PANEL))
                    .border_style(Style::default().fg(WARN))
                    .title(Span::styled(
                        " CONFIRM ACTION ",
                        Style::default().fg(WARN).add_modifier(Modifier::BOLD),
                    )),
            ),
        area,
    );
    let inner = area.inner(Margin { horizontal: 1, vertical: 1 });
    let buttons = Layout::horizontal([
        Constraint::Length(16),
        Constraint::Length(17),
        Constraint::Length(13),
        Constraint::Min(0),
    ])
    .split(Rect::new(inner.x, inner.bottom() - 1, inner.width, 1));
    regions.confirm_yes = Some(buttons[0]);
    regions.confirm_force = Some(buttons[1]);
    regions.confirm_cancel = Some(buttons[2]);
    for (area, label, choice) in [
        (buttons[0], " End process ", ConfirmationChoice::Confirm),
        (buttons[1], " Force terminate ", ConfirmationChoice::Force),
        (buttons[2], " Cancel ", ConfirmationChoice::Cancel),
    ] {
        let style = if confirm.choice == choice {
            Style::default().fg(PANEL).bg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT).bg(PANEL)
        };
        frame.render_widget(Paragraph::new(label).style(style), area);
    }
}

fn render_command_viewer(frame: &mut Frame<'_>, app: &StatsApp, regions: &mut UiRegions) {
    let viewer = app.command_viewer.as_ref().expect("called only with command viewer");
    let width = frame.area().width.saturating_sub(8).min(120);
    let height = frame.area().height.saturating_sub(6).min(36);
    let area = centered(frame.area(), width, height);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .style(Style::default().fg(TEXT).bg(PANEL))
        .border_style(Style::default().fg(ACCENT))
        .title(format!(" FULL COMMAND · {} · PID {} ", viewer.name, viewer.pid));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let chunks = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "Command lines may contain credentials. Nothing is copied or retained. ",
                Style::default().fg(WARN),
            ),
            Span::styled("Esc close", Style::default().fg(ACCENT)),
        ]))
        .wrap(Wrap { trim: true }),
        chunks[0],
    );
    let command_lines = if viewer.command.is_empty() {
        vec![String::new()]
    } else {
        viewer.command.lines().map(str::to_owned).collect::<Vec<_>>()
    };
    let lines = command_lines
        .iter()
        .skip(viewer.row_offset)
        .take(chunks[1].height as usize)
        .map(|line| {
            Line::from(horizontal_slice(line, viewer.column_offset, chunks[1].width as usize))
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines).style(Style::default().fg(TEXT)), chunks[1]);
    regions.command_content = Some(chunks[1]);
    regions.command_close = Some(Rect::new(area.right().saturating_sub(12), area.y, 11, 1));
}

fn horizontal_slice(value: &str, start: usize, width: usize) -> String {
    value.chars().skip(start).take(width).collect()
}

fn spark(history: Option<&VecDeque<u64>>) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    history
        .into_iter()
        .flat_map(|history| history.iter().rev().take(6).rev())
        .map(|value| BARS[(*value).min(99) as usize * BARS.len() / 100])
        .collect()
}

fn global_spark(histories: &[VecDeque<u64>], width: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let sample_count = histories.iter().map(VecDeque::len).min().unwrap_or(0);
    let first = sample_count.saturating_sub(width);
    let values = (first..sample_count)
        .map(|index| {
            let total = histories.iter().map(|history| history[index]).sum::<u64>();
            let average = total / histories.len().max(1) as u64;
            BARS[average.min(99) as usize * BARS.len() / 100]
        })
        .collect::<String>();
    format!("{}{values}", " ".repeat(width.saturating_sub(values.chars().count())))
}

fn cpu_color(value: f32) -> Color {
    if value >= 85.0 {
        PAPER
    } else if value >= 60.0 {
        CPU_ACCENT
    } else {
        GOOD
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}
