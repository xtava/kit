use std::collections::VecDeque;

use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row, Table, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::app::{ActiveRegion, ConfirmationChoice, InspectorTab, SortBy, StatsApp, StatsOverlay};
use super::contributions::{PROCESS_COMMAND_INLINE, PROCESS_INSPECTOR_INLINE};
use super::host::ProcessAction;
use super::model::{
    CapabilityState, DetailCompleteness, DetailData, DetailOutcome, Observed, ProcessIdentity,
    ResourceSample, SampleReadiness,
};
use super::report;
use crate::tui::{
    render_split_divider, theme::NORD, ActionId, ActionState, ContextMenuLayout, ContextMenuStyle,
    NavigationMap, NavigationRegion, ResolvedAction, SplitDividerStyle, SplitFrame, SplitMinimums,
};

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
const CORE_MAP_LABEL: &str = "CORE MAP  ";
const BUSY_CPU_PERCENT: f32 = 70.0;
const CRITICAL_CPU_PERCENT: f32 = 90.0;
const COMMAND_LABEL: &str = "COMMAND  ";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ProcessRowRegion {
    pub(super) area: Rect,
    pub(super) identity: ProcessIdentity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct InlineActionRegion {
    pub(super) area: Rect,
    pub(super) action: ActionId,
    pub(super) identity: ProcessIdentity,
}

#[derive(Default)]
pub(super) struct UiRegions {
    pub(super) processes: Option<Rect>,
    pub(super) inspector: Option<Rect>,
    pub(super) split: Option<SplitFrame>,
    pub(super) cores: Vec<(Rect, u16)>,
    pub(super) rows: Vec<ProcessRowRegion>,
    pub(super) headers: Vec<(Rect, SortBy)>,
    pub(super) disclosures: Vec<(Rect, super::model::ProcessIdentity)>,
    pub(super) family_rows: Vec<(Rect, usize, super::model::ProcessIdentity)>,
    pub(super) thread_rows: Vec<(Rect, usize)>,
    pub(super) tabs: Vec<(Rect, InspectorTab)>,
    pub(super) inline_actions: Vec<InlineActionRegion>,
    pub(super) context_menu: Option<ContextMenuLayout>,
    pub(super) command_content: Option<Rect>,
    pub(super) command_close: Option<Rect>,
    pub(super) back: Option<Rect>,
    pub(super) confirmation_choices: Vec<(Rect, ConfirmationChoice)>,
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
        let split = SplitFrame::horizontal(chunks[2], app.split_ratio, SplitMinimums::new(54, 42));
        regions.split = Some(split);
        render_processes(frame, app, split.first, true, &mut regions);
        render_inspector(frame, app, split.second, false, &mut regions);
        render_split_divider(
            frame,
            split,
            app.split_drag.is_some(),
            SplitDividerStyle {
                idle_color: BORDER,
                active_color: HIGHLIGHT,
                idle_line: "│",
                idle_grip: "┋",
                active_line: "┃",
            },
        );
    } else if app.active_region == ActiveRegion::Inspector {
        render_inspector(frame, app, chunks[2], true, &mut regions);
    } else {
        render_processes(frame, app, chunks[2], false, &mut regions);
    }
    render_footer(frame, app, chunks[3]);
    match app.overlay.as_ref() {
        Some(StatsOverlay::Confirmation(confirm)) => {
            render_confirmation(frame, confirm, &mut regions)
        }
        Some(StatsOverlay::CommandViewer(viewer)) => {
            render_command_viewer(frame, viewer, &mut regions)
        }
        Some(StatsOverlay::ContextMenu(menu)) => {
            let layout = menu.layout(frame.area());
            menu.render(frame, &layout, ContextMenuStyle::from_theme(NORD));
            regions.context_menu = Some(layout);
        }
        None => {}
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
    let graph_width = usize::from(inner.width.saturating_sub(48) / 2).min(24);
    let busiest = system.cpus.iter().max_by(|left, right| {
        left.usage_percent
            .total_cmp(&right.usage_percent)
            .then_with(|| right.logical_index.cmp(&left.logical_index))
    });
    let peak = busiest.map_or_else(
        || "—".to_owned(),
        |cpu| format!("C{:02} {:>5.1}%", cpu.logical_index, cpu.usage_percent),
    );
    let totals = Line::from(vec![
        Span::styled("CPU AVG ", Style::default().fg(MUTED)),
        Span::styled(
            format!("{:>5.1}% ", system.global_cpu_percent),
            Style::default().fg(cpu_color(system.global_cpu_percent)).add_modifier(Modifier::BOLD),
        ),
        Span::styled(average_cpu_spark(&app.histories, graph_width), Style::default().fg(GOOD)),
        Span::styled("  PEAK CORE ", Style::default().fg(MUTED)),
        Span::styled(
            format!("{peak} "),
            Style::default()
                .fg(busiest.map_or(GOOD, |cpu| cpu_color(cpu.usage_percent)))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(peak_core_spark(&app.histories, graph_width), Style::default().fg(CPU_ACCENT)),
    ]);
    frame.render_widget(Paragraph::new(totals), Rect::new(inner.x, inner.y, inner.width, 1));

    let core_limit = if inner.width >= 120 {
        3
    } else if inner.width >= 78 {
        2
    } else {
        1
    };
    let core_pressure = app.core_pressure(core_limit);
    let mut busy_summary = String::from("TOP CORES NOW/RECENT ");
    if core_pressure.is_empty() {
        busy_summary.push('—');
    } else {
        for (index, core) in core_pressure.iter().enumerate() {
            if index > 0 {
                busy_summary.push_str(" · ");
            }
            busy_summary.push_str(&format!(
                "C{:02} {:.0}/{:.0}%",
                core.logical_index, core.now_percent, core.recent_peak_percent
            ));
        }
    }
    let summary_budget = usize::from(inner.width.saturating_mul(2) / 5);
    let busy_summary = truncate(&busy_summary, summary_budget);
    let summary_width = busy_summary.width().min(usize::from(u16::MAX)) as u16;
    let map_width = inner
        .width
        .saturating_sub(CORE_MAP_LABEL.len() as u16)
        .saturating_sub(summary_width)
        .saturating_sub(2) as usize;
    let cell_count = system.cpus.len().min(map_width);
    let cells = (0..cell_count)
        .filter_map(|cell| {
            let start = cell * system.cpus.len() / cell_count;
            let end = (cell + 1) * system.cpus.len() / cell_count;
            let group = &system.cpus[start..end];
            group.iter().find(|cpu| app.focused_core == Some(cpu.logical_index)).or_else(|| {
                group.iter().max_by(|left, right| {
                    left.usage_percent
                        .total_cmp(&right.usage_percent)
                        .then_with(|| right.logical_index.cmp(&left.logical_index))
                })
            })
        })
        .collect::<Vec<_>>();
    let mut core_spans = vec![Span::styled(CORE_MAP_LABEL, Style::default().fg(MUTED))];
    for cpu in &cells {
        core_spans.push(Span::styled(
            usage_bar(cpu.usage_percent).to_string(),
            if app.focused_core == Some(cpu.logical_index) {
                Style::default().fg(PAPER).bg(SELECTED).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(cpu_color(cpu.usage_percent))
            },
        ));
    }
    core_spans.push(Span::raw("  "));
    let summary_color = core_pressure
        .first()
        .map_or(MUTED, |core| cpu_color(core.now_percent.max(core.recent_peak_percent)));
    core_spans.push(Span::styled(busy_summary, Style::default().fg(summary_color)));
    let core_line = Rect::new(inner.x, inner.y + 1, inner.width, 1);
    frame.render_widget(Paragraph::new(Line::from(core_spans)), core_line);
    for (x, cpu) in (core_line.x + CORE_MAP_LABEL.len() as u16..).zip(cells) {
        regions.cores.push((Rect::new(x, core_line.y, 1, 1), cpu.logical_index));
    }

    let source_limit = if inner.width >= 120 {
        3
    } else if inner.width >= 72 {
        2
    } else {
        1
    };
    let sources = app.pressure_sources(source_limit);
    let mut source_spans =
        vec![Span::styled("PRESSURE SOURCES NOW/RECENT  ", Style::default().fg(MUTED))];
    if sources.is_empty() {
        source_spans.push(Span::styled("—", Style::default().fg(MUTED)));
    } else {
        let name_width = if inner.width >= 120 {
            16
        } else if inner.width >= 72 {
            10
        } else {
            7
        };
        for (index, source) in sources.iter().enumerate() {
            if index > 0 {
                source_spans.push(Span::styled("  ·  ", Style::default().fg(MUTED)));
            }
            let name = truncate(&source.name, name_width);
            source_spans.push(Span::styled(
                format!(
                    "{name}[{}] {:.0}/{:.0}%",
                    source.pid, source.now_percent, source.recent_percent
                ),
                Style::default()
                    .fg(cpu_color(source.now_percent.max(source.recent_percent)))
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }
    frame.render_widget(
        Paragraph::new(Line::from(source_spans)),
        Rect::new(inner.x, inner.y + 2, inner.width, 1),
    );
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
        regions.rows.push(ProcessRowRegion {
            area: Rect::new(inner.x, y, inner.width, 1),
            identity: app.visible[index].key,
        });
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
    let (lines, command_actions) = match app.inspector_tab {
        InspectorTab::Overview => {
            let (lines, command_actions) =
                overview_lines(app, process, family_cpu, family_memory, content);
            (lines, Some(command_actions))
        }
        InspectorTab::Family => {
            (family_lines(app, process, family_cpu, family_memory, content, regions), None)
        }
        InspectorTab::Threads => (thread_lines(app, content, regions), None),
        InspectorTab::Resources => (resources_lines(app), None),
        InspectorTab::Profile => (profile_lines(app), None),
    };
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), content);
    if let Some(command_actions) = command_actions {
        let context = app.action_context(process.identity);
        let actions = app.registry.resolve_menu(PROCESS_COMMAND_INLINE, &context);
        render_inline_actions(
            frame,
            app,
            command_actions,
            actions.items(),
            process.identity,
            regions,
        );
    }
    if inner.height >= 5 {
        let action_line = Rect::new(inner.x, inner.bottom() - 1, inner.width, 1);
        let context = app.action_context(process.identity);
        let actions = app.registry.resolve_menu(PROCESS_INSPECTOR_INLINE, &context);
        render_inline_actions(frame, app, action_line, actions.items(), process.identity, regions);
    }
}

fn overview_lines(
    app: &StatsApp,
    process: &super::model::ProcessSample,
    family_cpu: f32,
    family_memory: u64,
    area: Rect,
) -> (Vec<Line<'static>>, Rect) {
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
    let command_line_y = area.y.saturating_add(lines.len() as u16).saturating_add(1);
    let prefix_width = u16::try_from(COMMAND_LABEL.width()).unwrap_or(area.width).min(area.width);
    let command_actions = Rect::new(
        area.x.saturating_add(prefix_width),
        command_line_y,
        area.width.saturating_sub(prefix_width),
        u16::from(command_line_y < area.bottom()),
    );
    lines.extend([
        Line::from(""),
        Line::from(vec![Span::styled(COMMAND_LABEL, Style::default().fg(MUTED))]),
        Line::styled(process.command.clone(), Style::default().fg(TEXT)),
    ]);
    (lines, command_actions)
}

fn render_inline_actions(
    frame: &mut Frame<'_>,
    app: &StatsApp,
    area: Rect,
    actions: &[ResolvedAction],
    identity: ProcessIdentity,
    regions: &mut UiRegions,
) {
    if actions.is_empty() || area.height == 0 || area.width == 0 {
        return;
    }
    let mut x = area.x;
    for action in actions {
        let remaining = area.right().saturating_sub(x);
        if remaining == 0 {
            break;
        }
        let desired_width = u16::try_from(inline_action_width(action)).unwrap_or(u16::MAX);
        let width = desired_width.min(remaining);
        let slot = Rect::new(x, area.y, width, 1);
        frame.render_widget(Paragraph::new(Line::from(inline_action_spans(action))), slot);
        if app.pointer_enabled && width > 0 {
            regions.inline_actions.push(InlineActionRegion {
                area: slot,
                action: action.id,
                identity,
            });
        }
        x = x.saturating_add(width);
    }
}

fn inline_action_width(action: &ResolvedAction) -> usize {
    let keybinding = action
        .primary_keybinding()
        .map(|keybinding| keybinding.to_string().width().saturating_add(2))
        .unwrap_or(1);
    keybinding.saturating_add(action.title.width())
}

fn inline_action_spans(action: &ResolvedAction) -> Vec<Span<'static>> {
    let (key_style, title_style) = match &action.state {
        ActionState::Enabled => (
            Style::default().fg(HIGHLIGHT).add_modifier(Modifier::BOLD),
            Style::default().fg(MUTED),
        ),
        ActionState::Disabled { .. } => {
            let disabled = Style::default().fg(MUTED).add_modifier(Modifier::DIM);
            (disabled, disabled)
        }
    };
    let keybinding = action
        .primary_keybinding()
        .map_or_else(|| " ".to_owned(), |keybinding| format!(" {keybinding} "));
    vec![Span::styled(keybinding, key_style), Span::styled(action.title, title_style)]
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
                Span::styled("drag/<> ", key),
                Span::styled("resize   ", Style::default().fg(MUTED)),
                Span::styled("= ", key),
                Span::styled("reset   ", Style::default().fg(MUTED)),
                Span::styled("q ", key),
                Span::styled("quit", Style::default().fg(MUTED)),
            ]),
            ActiveRegion::Inspector => Line::from(vec![
                Span::styled(" ←→ ", key),
                Span::styled("tabs / processes   ", Style::default().fg(MUTED)),
                Span::styled("tab ", key),
                Span::styled("region   ", Style::default().fg(MUTED)),
                Span::styled("esc ", key),
                Span::styled("back   ", Style::default().fg(MUTED)),
                Span::styled("q ", key),
                Span::styled("quit", Style::default().fg(MUTED)),
            ]),
        };
        frame.render_widget(Paragraph::new(help).style(Style::default().bg(BACKGROUND)), area);
    }
}

fn render_confirmation(
    frame: &mut Frame<'_>,
    confirm: &super::app::Confirmation,
    regions: &mut UiRegions,
) {
    let area = centered(frame.area(), 58, 9);
    frame.render_widget(Clear, area);
    frame.render_widget(Block::new().style(Style::default().bg(PANEL)), area);
    let selected_action = match confirm.choice {
        ConfirmationChoice::Action(action) => action,
        ConfirmationChoice::Cancel => confirm.requested,
    };
    let warning = if selected_action == ProcessAction::ForceTerminate {
        "Force kill cannot be handled or cleaned up by the process."
    } else {
        "The process may save work and shut down cleanly."
    };
    let prompt = match confirm.requested {
        ProcessAction::GracefulTerminate => {
            format!("End {} (PID {})?", confirm.name, confirm.key.pid)
        }
        ProcessAction::ForceTerminate => {
            format!("Force terminate {} (PID {})?", confirm.name, confirm.key.pid)
        }
    };
    let text = vec![
        Line::from(prompt),
        Line::from(""),
        Line::styled(
            selected_action.label(),
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
    let constraints = confirm.choices.iter().map(|choice| {
        Constraint::Length((UnicodeWidthStr::width(choice.label()) as u16).saturating_add(2))
    });
    let buttons = Layout::horizontal(constraints).split(Rect::new(
        inner.x,
        inner.bottom().saturating_sub(1),
        inner.width,
        1,
    ));
    for (area, choice) in buttons.iter().copied().zip(confirm.choices.iter().copied()) {
        regions.confirmation_choices.push((area, choice));
        let style = if confirm.choice == choice {
            Style::default().fg(PANEL).bg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT).bg(PANEL)
        };
        frame.render_widget(Paragraph::new(format!(" {} ", choice.label())).style(style), area);
    }
}

fn render_command_viewer(
    frame: &mut Frame<'_>,
    viewer: &super::app::CommandViewer,
    regions: &mut UiRegions,
) {
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

fn average_cpu_spark(histories: &[VecDeque<u64>], width: usize) -> String {
    history_spark(histories, width, |values| {
        values.iter().copied().sum::<u64>() / values.len().max(1) as u64
    })
}

fn peak_core_spark(histories: &[VecDeque<u64>], width: usize) -> String {
    history_spark(histories, width, |values| values.iter().copied().max().unwrap_or_default())
}

fn history_spark(
    histories: &[VecDeque<u64>],
    width: usize,
    aggregate: impl Fn(&[u64]) -> u64,
) -> String {
    let sample_count = histories.iter().map(VecDeque::len).min().unwrap_or(0);
    let first = sample_count.saturating_sub(width);
    let values = (first..sample_count)
        .map(|index| {
            let values = histories.iter().map(|history| history[index]).collect::<Vec<_>>();
            usage_bar(aggregate(&values) as f32)
        })
        .collect::<String>();
    format!("{}{values}", " ".repeat(width.saturating_sub(values.chars().count())))
}

fn usage_bar(value: f32) -> char {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    BARS[value.clamp(0.0, 99.0) as usize * BARS.len() / 100]
}

fn cpu_color(value: f32) -> Color {
    if value >= CRITICAL_CPU_PERCENT {
        PAPER
    } else if value >= BUSY_CPU_PERCENT {
        WARN
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
