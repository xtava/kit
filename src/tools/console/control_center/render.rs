use ratatui::{
    layout::{Constraint, Layout, Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph},
    Frame,
};

use crate::tui::{
    fit_terminal_text, render_vertical_scrollbar, terminal_text_width, theme::NORD, CellAlignment,
    CellOverflow, CommandPaletteLayout, ContextMenuLayout, ContextMenuStyle, LineEditor,
    ScrollbarLayout, ScrollbarStyle, SelectableRegion, ViewportMetrics,
};

use super::{
    application::{ControlCenterApp, ControlCenterOverlay, ControlCenterSelectionSurface},
    model::{
        ControlCenterStory, MachineAction, MachineDiscoveryState, MachineRowProjection,
        MachineRowWidth,
    },
};

const NORMAL_WIDTH: u16 = 78;
const WIDE_WIDTH: u16 = 118;

#[derive(Default)]
pub(super) struct ControlCenterRegions {
    pub(super) machine_list: Option<Rect>,
    pub(super) machine_rows: Vec<(Rect, String)>,
    pub(super) machine_scrollbar: Option<ScrollbarLayout>,
    pub(super) primary_action: Option<Rect>,
    pub(super) new_session_action: Option<Rect>,
    pub(super) refresh_action: Option<Rect>,
    pub(super) command_palette: Option<CommandPaletteLayout>,
    pub(super) context_menu: Option<ContextMenuLayout>,
    pub(super) selectable: Vec<SelectableRegion<ControlCenterSelectionSurface>>,
}

pub(super) fn render(frame: &mut Frame<'_>, app: &mut ControlCenterApp) -> ControlCenterRegions {
    let area = frame.area();
    frame.render_widget(Clear, area);
    let content =
        area.inner(Margin { horizontal: 2.min(area.width / 4), vertical: 1.min(area.height / 4) });
    let sections = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(4),
        Constraint::Length(if content.height >= 12 { 5 } else { 3 }),
    ])
    .split(content);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Machines", Style::default().fg(NORD.accent).add_modifier(Modifier::BOLD)),
            Span::styled(
                match app.state.discovery {
                    MachineDiscoveryState::Discovering => "  checking",
                    MachineDiscoveryState::Ready => "",
                    MachineDiscoveryState::AuthenticationRequired => "  login required",
                    MachineDiscoveryState::Unavailable { .. } => "  unavailable",
                },
                Style::default().fg(NORD.text_muted),
            ),
        ])),
        sections[0],
    );

    let mut regions = ControlCenterRegions::default();
    match app.state.story() {
        ControlCenterStory::Machines => {
            let row_width = if content.width >= WIDE_WIDTH {
                MachineRowWidth::Wide
            } else if content.width >= NORMAL_WIDTH {
                MachineRowWidth::Normal
            } else {
                MachineRowWidth::Compact
            };
            app.machine_metrics =
                ViewportMetrics::new(app.state.machines.len(), usize::from(sections[1].height));
            if let Some(selected) = app.selected_index() {
                app.machine_viewport.ensure_visible(selected, app.machine_metrics);
            } else {
                app.machine_viewport.normalize(app.machine_metrics);
            }
            let visible = app.machine_viewport.visible_range(app.machine_metrics);
            let top = visible.start;
            let items = app
                .state
                .machines
                .get(visible.clone())
                .unwrap_or_default()
                .iter()
                .map(|machine| ListItem::new(machine_row_line(machine.row(row_width))))
                .collect::<Vec<_>>();
            let mut list_state = ListState::default();
            list_state.select(app.selected_index().and_then(|index| index.checked_sub(top)));
            let list = List::new(items)
                .highlight_symbol("› ")
                .highlight_style(Style::default().bg(NORD.selection).add_modifier(Modifier::BOLD));
            frame.render_stateful_widget(list, sections[1], &mut list_state);
            regions.machine_list = Some(sections[1]);
            regions.machine_rows = app
                .state
                .machines
                .get(visible.clone())
                .unwrap_or_default()
                .iter()
                .enumerate()
                .map(|(index, machine)| {
                    (
                        Rect::new(
                            sections[1].x,
                            sections[1].y + index as u16,
                            sections[1].width,
                            1,
                        ),
                        machine.identity.stable_node_id.clone(),
                    )
                })
                .collect();
            regions.machine_scrollbar =
                ScrollbarLayout::vertical_right(sections[1], app.machine_metrics, top);
            if let Some(scrollbar) = regions.machine_scrollbar {
                render_vertical_scrollbar(
                    frame,
                    scrollbar,
                    app.machine_scrollbar_drag.is_some(),
                    ScrollbarStyle {
                        track_color: NORD.border,
                        thumb_color: NORD.text_muted,
                        active_thumb_color: NORD.accent,
                        track_symbol: "│",
                        thumb_symbol: "┃",
                    },
                );
            }
        }
        ControlCenterStory::Discovering => {
            frame.render_widget(
                Paragraph::new("Discovering your machines…")
                    .style(Style::default().fg(NORD.text_muted)),
                sections[1],
            );
        }
        ControlCenterStory::AuthenticationRequired => {
            frame.render_widget(
                Paragraph::new("Tailscale authentication is required.")
                    .style(Style::default().fg(NORD.warning)),
                sections[1],
            );
        }
        ControlCenterStory::Empty => {
            frame.render_widget(
                Paragraph::new("No machines are available yet.")
                    .style(Style::default().fg(NORD.text_muted)),
                sections[1],
            );
        }
        ControlCenterStory::Unavailable { detail } => {
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled("Could not discover machines.", Style::default().fg(NORD.danger)),
                    Line::styled(detail, Style::default().fg(NORD.text_muted)),
                ]),
                sections[1],
            );
            regions.selectable.push(SelectableRegion::new(
                ControlCenterSelectionSurface::DiscoveryDetail,
                sections[1],
                0,
                0,
                app.content_revision,
            ));
        }
    }

    render_selected_machine(frame, app, sections[2], &mut regions);
    match app.overlay.as_mut() {
        Some(ControlCenterOverlay::CommandPalette(command_palette)) => {
            let layout = command_palette.layout(area);
            command_palette.render(frame, &layout, NORD);
            regions.command_palette = Some(layout);
        }
        Some(ControlCenterOverlay::ContextMenu(context_menu)) => {
            let layout = context_menu.layout(area);
            context_menu.render(frame, &layout, ContextMenuStyle::from_theme(NORD));
            regions.context_menu = Some(layout);
        }
        Some(ControlCenterOverlay::Details { stable_node_id }) => {
            if let Some(machine) = app
                .state
                .machines
                .iter()
                .find(|machine| &machine.identity.stable_node_id == stable_node_id)
            {
                render_machine_details(
                    frame,
                    area,
                    machine.details(),
                    app.content_revision,
                    &mut regions,
                );
            }
        }
        Some(ControlCenterOverlay::Settings(settings)) => {
            settings.render(frame, area);
        }
        Some(ControlCenterOverlay::UnixUser { input, notice, .. }) => {
            render_unix_user(frame, area, input, notice.as_deref());
        }
        None => {}
    }
    if app.overlay.is_some() && !matches!(app.overlay, Some(ControlCenterOverlay::Details { .. })) {
        regions.selectable.clear();
    }
    let selectable = regions.selectable.clone();
    app.selection.capture_frame(
        frame,
        &selectable,
        Style::default().bg(NORD.selection).add_modifier(Modifier::REVERSED),
    );
    regions
}

fn render_machine_details(
    frame: &mut Frame<'_>,
    area: Rect,
    details: Vec<(&'static str, String)>,
    revision: u64,
    regions: &mut ControlCenterRegions,
) {
    let width = area.width.saturating_sub(4).min(76);
    let height = (details.len() as u16 + 4).min(area.height.saturating_sub(2));
    let horizontal =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(width), Constraint::Fill(1)])
            .split(area);
    let vertical =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(height), Constraint::Fill(1)])
            .split(horizontal[1]);
    let popup = vertical[1];
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" Machine details ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(NORD.focus));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let mut lines = details
        .into_iter()
        .map(|(label, value)| {
            Line::from(vec![
                Span::styled(
                    fit_terminal_text(label, 18, CellAlignment::Left, CellOverflow::Clip),
                    Style::default().fg(NORD.text_muted),
                ),
                Span::styled(value, Style::default().fg(NORD.text_strong)),
            ])
        })
        .collect::<Vec<_>>();
    lines.push(Line::styled("Enter or Esc closes", Style::default().fg(NORD.text_muted)));
    frame.render_widget(Paragraph::new(lines), inner);
    if inner.height > 1 {
        regions.selectable.push(SelectableRegion::new(
            ControlCenterSelectionSurface::MachineDetails,
            Rect::new(inner.x, inner.y, inner.width, inner.height.saturating_sub(1)),
            0,
            0,
            revision,
        ));
    }
}

fn render_unix_user(frame: &mut Frame<'_>, area: Rect, input: &LineEditor, notice: Option<&str>) {
    let width = area.width.saturating_sub(4).min(64);
    let height = if notice.is_some() { 8 } else { 7 };
    let horizontal =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(width), Constraint::Fill(1)])
            .split(area);
    let vertical = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(height.min(area.height)),
        Constraint::Fill(1),
    ])
    .split(horizontal[1]);
    let popup = vertical[1];
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" Unix user ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(NORD.focus));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let lines = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new("Account name on the selected machine")
            .style(Style::default().fg(NORD.text_muted)),
        lines[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("› ", Style::default().fg(NORD.accent)),
            Span::styled(format!("{}▏", input.value()), Style::default().fg(NORD.text_strong)),
        ])),
        lines[1],
    );
    let hint = notice.unwrap_or("Enter saves · Esc cancels");
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(if notice.is_some() {
            NORD.warning
        } else {
            NORD.text_muted
        })),
        lines[2],
    );
}

fn machine_row_line(row: MachineRowProjection) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            fit_terminal_text(&row.name, 20, CellAlignment::Left, CellOverflow::Clip),
            Style::default().fg(NORD.text),
        ),
        Span::styled(
            fit_terminal_text(&row.status, 18, CellAlignment::Left, CellOverflow::Clip),
            Style::default().fg(NORD.text_strong),
        ),
    ];
    for value in
        [row.role.map(str::to_owned), row.operating_system, row.sessions, row.unix_user, row.build]
            .into_iter()
            .flatten()
    {
        spans.push(Span::styled(format!("  {value}"), Style::default().fg(NORD.text_muted)));
    }
    Line::from(spans)
}

fn render_selected_machine(
    frame: &mut Frame<'_>,
    app: &ControlCenterApp,
    area: Rect,
    regions: &mut ControlCenterRegions,
) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(NORD.border));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(machine) = app.selected_machine() else {
        let action = if app.tailscale_login_cancel.is_some() {
            Some(MachineAction::CancelOperation)
        } else if matches!(app.state.discovery, MachineDiscoveryState::AuthenticationRequired) {
            Some(MachineAction::AuthenticateTailscale)
        } else {
            None
        };
        let label = action.map(|action| format!("[ {} ]", action.contract().title));
        let message = app
            .notice
            .as_deref()
            .map(str::to_owned)
            .or_else(|| label.clone())
            .unwrap_or_else(|| "R refreshes discovery · q closes Console".to_owned());
        frame.render_widget(
            Paragraph::new(message).style(Style::default().fg(NORD.text_muted)),
            inner,
        );
        if app.notice.is_none() {
            regions.primary_action = label.map(|label| {
                let width = u16::try_from(terminal_text_width(&label)).unwrap_or(u16::MAX);
                Rect::new(inner.x, inner.y, width, 1)
            });
        }
        return;
    };
    let action = machine.primary_action().contract();
    let available_actions = machine.available_actions();
    let can_create_session = available_actions.contains(&MachineAction::NewSession);
    let action_label = format!("[ {} ]", action.title);
    let new_session_label = "[ New session ]";
    let refresh_label = "[ Refresh ]";
    let action_style =
        if matches!(machine.primary_action(), MachineAction::Connect | MachineAction::NewSession) {
            Style::default().fg(NORD.accent)
        } else {
            Style::default().fg(NORD.warning)
        };
    let mut lines = vec![Line::from(vec![
        Span::styled(
            &machine.identity.display_name,
            Style::default().fg(NORD.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", machine.identity.selector),
            Style::default().fg(NORD.text_muted),
        ),
    ])];
    if let Some(notice) = app.notice.as_deref() {
        lines.push(Line::styled(notice, Style::default().fg(NORD.warning)));
    } else {
        lines.push(Line::from(vec![
            Span::styled(action_label.clone(), action_style),
            Span::raw("  "),
            Span::styled(
                new_session_label,
                Style::default().fg(if can_create_session {
                    NORD.text_strong
                } else {
                    NORD.text_muted
                }),
            ),
            Span::raw("  "),
            Span::styled(refresh_label, Style::default().fg(NORD.text_strong)),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), inner);

    if app.notice.is_none() && inner.height >= 2 {
        let actions_y = inner.y + 1;
        let action_width = u16::try_from(terminal_text_width(&action_label)).unwrap_or(u16::MAX);
        let new_session_width =
            u16::try_from(terminal_text_width(new_session_label)).unwrap_or(u16::MAX);
        let refresh_width = u16::try_from(terminal_text_width(refresh_label)).unwrap_or(u16::MAX);
        regions.primary_action = Some(Rect::new(inner.x, actions_y, action_width, 1));
        let new_session_x = inner.x.saturating_add(action_width).saturating_add(2);
        regions.new_session_action =
            can_create_session.then_some(Rect::new(new_session_x, actions_y, new_session_width, 1));
        regions.refresh_action = Some(Rect::new(
            new_session_x.saturating_add(new_session_width).saturating_add(2),
            actions_y,
            refresh_width,
            1,
        ));
    }
}
