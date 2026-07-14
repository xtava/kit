use std::collections::VecDeque;

use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

use super::app::{ActiveRegion, InspectorTab, SortBy, StatsApp};
use super::host::ProcessAction;
use super::model::{CapabilityState, DetailOutcome, Observed};
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
    pub(super) tabs: Vec<(Rect, InspectorTab)>,
    pub(super) profile: Option<Rect>,
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
    let rows = app.visible.iter().skip(app.row_offset).map(|item| {
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
        let program = format!("{}{marker} {}{suffix}", "  ".repeat(item.depth as usize), item.name);
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
    let mut headers = vec!["NAME / COMMAND"];
    if wide {
        headers.push("PID");
    }
    headers.extend(["CPU", "FAMILY", "MEM"]);
    let header =
        Row::new(headers).style(Style::default().fg(MUTED).bg(PANEL).add_modifier(Modifier::BOLD));
    let constraints = if wide {
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
    };
    let table =
        Table::new(rows, constraints)
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
    regions.headers.push((Rect::new(inner.x, inner.y, inner.width / 2, 1), SortBy::Name));
    let row_top = inner.y + 1;
    let visible_height = inner.height.saturating_sub(1) as usize;
    for (screen_index, index) in
        (app.row_offset..app.visible.len()).take(visible_height).enumerate()
    {
        let y = row_top + screen_index as u16;
        regions.rows.push((Rect::new(inner.x, y, inner.width, 1), index));
        let disclosure_x = inner.x + app.visible[index].depth.saturating_mul(2);
        regions.disclosures.push((Rect::new(disclosure_x, y, 2, 1), app.visible[index].key));
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
    let core = process.last_cpu.map_or_else(|| "—".into(), |core| format!("C{core}"));
    let lines = match app.inspector_tab {
        InspectorTab::Overview => {
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(
                        format!("CPU       {:>7.1}%", process.cpu_percent),
                        Style::default().fg(CPU_ACCENT),
                    ),
                    Span::styled(
                        format!("    FAMILY {:>7.1}%", family_cpu),
                        Style::default().fg(ACCENT),
                    ),
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
                let width = content.width.saturating_sub(14) as usize;
                lines.push(Line::from(format!(
                    "CPU HISTORY  {}",
                    history_bars(history.points().map(|point| point.cpu_percent as f64), width)
                )));
                lines.push(Line::from(format!(
                    "RSS HISTORY  {}",
                    history_bars(history.points().map(|point| point.rss_bytes as f64), width)
                )));
            }
            lines.extend([
                Line::from(""),
                Line::styled("COMMAND", Style::default().fg(MUTED)),
                Line::styled(process.command.clone(), Style::default().fg(TEXT)),
            ]);
            lines
        }
        InspectorTab::Family => vec![
            Line::styled("DESCENDANT-INCLUSIVE TOTALS", Style::default().fg(MUTED)),
            Line::from(format!("CPU      {:.1}%", family_cpu)),
            Line::from(format!("MEMORY   {}", report::bytes(family_memory))),
            Line::from(format!(
                "CHILDREN {} direct",
                app.snapshot
                    .processes
                    .iter()
                    .filter(|candidate| candidate.parent_pid == Some(process.identity.pid()))
                    .count()
            )),
        ],
        InspectorTab::Threads => {
            let detail = app.detail.as_deref();
            let mut lines = vec![Line::styled(
                match detail.map(|detail| &detail.outcome) {
                    Some(DetailOutcome::Ready { .. }) => "THREADS · LIVE DETAIL".to_owned(),
                    Some(DetailOutcome::Warming { .. }) => "THREADS · WARMING DELTAS…".to_owned(),
                    Some(DetailOutcome::Unavailable { reason }) => {
                        format!("THREADS · {}", detail_unavailable(reason))
                    }
                    None => "THREADS · LOADING…".to_owned(),
                },
                Style::default().fg(MUTED),
            )];
            lines.extend(
                detail
                    .into_iter()
                    .flat_map(|detail| detail.threads().into_iter().flatten())
                    .filter(|thread| process.identity.stable_key() == Some(thread.process))
                    .take(content.height.saturating_sub(2) as usize)
                    .map(|thread| {
                        Line::from(format!(
                            "{:>7.1}%  {:>8}  {}",
                            thread.cpu_percent,
                            thread.last_cpu.map_or_else(|| "—".into(), |core| format!("C{core}")),
                            thread.name
                        ))
                    }),
            );
            lines
        }
        InspectorTab::Resources => match app.snapshot.host.resources {
            CapabilityState::Available => app
                .detail
                .as_deref()
                .and_then(|detail| detail.resources().map(|resources| (detail, resources)))
                .map_or_else(
                    || vec![Line::styled("RESOURCES · LOADING…", Style::default().fg(MUTED))],
                    |(detail, resources)| {
                        vec![
                            Line::styled(
                                format!(
                                    "RESOURCES · {}",
                                    match &detail.outcome {
                                        DetailOutcome::Ready { .. } => "LIVE".to_owned(),
                                        DetailOutcome::Warming { .. } => {
                                            "WARMING I/O RATE…".to_owned()
                                        }
                                        DetailOutcome::Unavailable { reason } => {
                                            detail_unavailable(reason).to_owned()
                                        }
                                    }
                                ),
                                Style::default().fg(MUTED),
                            ),
                            Line::from(format!(
                                "EXEC      {}",
                                observed_path(&resources.executable)
                            )),
                            Line::from(format!(
                                "CWD       {}",
                                observed_path(&resources.current_directory)
                            )),
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
                    },
                ),
            CapabilityState::Unsupported { reason } => vec![
                Line::styled("RESOURCES UNAVAILABLE", Style::default().fg(MUTED)),
                Line::from(reason),
            ],
        },
        InspectorTab::Profile => match app.snapshot.host.code_profile {
            CapabilityState::Available => vec![
                Line::styled("BOUNDED CODE PROFILE", Style::default().fg(ACCENT)),
                Line::from("2s   [5s]   10s   · 99 Hz"),
            ],
            CapabilityState::Unsupported { reason } => vec![
                Line::styled("PROFILE UNAVAILABLE", Style::default().fg(MUTED)),
                Line::from(reason),
            ],
        },
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

fn observed_path(value: &Observed<std::path::PathBuf>) -> String {
    value
        .value()
        .map(|path| path.to_string_lossy().into_owned())
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
    } else if let Some(status) = &app.status {
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
    let warning = if confirm.action == ProcessAction::ForceTerminate {
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
        Line::styled("ENTER confirm   F force kill   ESC cancel", Style::default().fg(ACCENT)),
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
