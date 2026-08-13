//! Registry-backed command discovery for interactive tools.
//!
//! The palette deliberately owns no commands or handlers. It filters a resolved snapshot from
//! [`ActionRegistry`](crate::tui::ActionRegistry) and returns the same typed [`ActionInvocation`]
//! used by keybindings and menus.

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use super::fuzzy::Matcher;
use super::theme::TuiTheme;
use super::{
    fit_terminal_text, terminal_text_width, ActionInvocation, ActionRegistry, ActionState,
    CellAlignment, CellOverflow, LineEditor, ResolvedAction, ResolvedActions,
};

const MIN_WIDTH: u16 = 38;
const MAX_WIDTH: u16 = 88;
const MIN_HEIGHT: u16 = 9;
const MAX_HEIGHT: u16 = 24;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommandPaletteLayout {
    pub popup: Rect,
    pub input: Rect,
    pub rows: Vec<(Rect, usize)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandPaletteOutcome<C> {
    Captured,
    Dismissed,
    Invoke(ActionInvocation<C>),
}

pub struct CommandPalette<C> {
    context: C,
    commands: Vec<ResolvedAction>,
    query: LineEditor,
    matches: Vec<usize>,
    selected: usize,
    scroll: usize,
    notice: Option<String>,
}

impl<C: Clone> CommandPalette<C> {
    pub fn open<Command>(context: C, registry: &ActionRegistry<C, Command>) -> Self {
        let resolved = registry.resolve_command_palette(&context);
        Self::from_resolved(context, resolved)
    }

    pub fn from_resolved(context: C, resolved: ResolvedActions) -> Self {
        let commands = resolved.items().to_vec();
        let matches = (0..commands.len()).collect();
        Self {
            context,
            commands,
            query: LineEditor::default(),
            matches,
            selected: 0,
            scroll: 0,
            notice: None,
        }
    }

    pub fn query(&self) -> &str {
        self.query.value()
    }

    pub fn selected_action(&self) -> Option<&ResolvedAction> {
        self.matches.get(self.selected).map(|index| &self.commands[*index])
    }

    pub fn layout(&self, area: Rect) -> CommandPaletteLayout {
        let width = area.width.saturating_sub(4).clamp(MIN_WIDTH.min(area.width), MAX_WIDTH);
        let desired_height = self.matches.len().saturating_add(6) as u16;
        let height = desired_height.clamp(MIN_HEIGHT.min(area.height), MAX_HEIGHT.min(area.height));
        let popup = centered(area, width, height);
        let inner = popup.inner(ratatui::layout::Margin { horizontal: 2, vertical: 1 });
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(u16::from(self.notice.is_some())),
        ])
        .split(inner);
        let input = chunks[0];
        let visible_rows = usize::from(chunks[2].height);
        let start = self.scroll.min(self.matches.len().saturating_sub(visible_rows));
        let rows = self
            .matches
            .iter()
            .enumerate()
            .skip(start)
            .take(visible_rows)
            .map(|(match_index, _)| {
                (
                    Rect {
                        x: chunks[2].x,
                        y: chunks[2].y + u16::try_from(match_index - start).unwrap_or(0),
                        width: chunks[2].width,
                        height: 1,
                    },
                    match_index,
                )
            })
            .collect();
        CommandPaletteLayout { popup, input, rows }
    }

    pub fn on_event(
        &mut self,
        event: Event,
        layout: &CommandPaletteLayout,
    ) -> CommandPaletteOutcome<C> {
        match event {
            Event::Key(key) => self.on_key(key, layout.rows.len()),
            Event::Mouse(mouse) => self.on_mouse(mouse, layout),
            Event::Paste(text) => {
                for character in text.chars().filter(|character| !character.is_control()) {
                    self.query.insert(character);
                }
                self.refilter();
                CommandPaletteOutcome::Captured
            }
            _ => CommandPaletteOutcome::Captured,
        }
    }

    pub fn render(&self, frame: &mut Frame<'_>, layout: &CommandPaletteLayout, theme: TuiTheme) {
        frame.render_widget(Clear, layout.popup);
        let border = Block::default()
            .title(Line::from(vec![
                Span::styled(" Commands ", Style::default().fg(theme.text_strong)),
                Span::styled(
                    format!("{} ", self.matches.len()),
                    Style::default().fg(theme.text_muted),
                ),
            ]))
            .title_alignment(Alignment::Left)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.focus));
        frame.render_widget(border, layout.popup);

        let input_line = if self.query.value().is_empty() {
            Line::from(vec![
                Span::styled("› ", Style::default().fg(theme.accent)),
                Span::styled(
                    "Type a command…",
                    Style::default().fg(theme.text_muted).add_modifier(Modifier::ITALIC),
                ),
            ])
        } else {
            Line::from(vec![
                Span::styled("› ", Style::default().fg(theme.accent)),
                Span::styled(self.query.value(), Style::default().fg(theme.text_strong)),
            ])
        };
        frame.render_widget(Paragraph::new(input_line), layout.input);

        if self.matches.is_empty() {
            let area = Rect {
                x: layout.input.x,
                y: layout.input.y.saturating_add(2),
                width: layout.input.width,
                height: 1,
            };
            if area.y < layout.popup.bottom() {
                frame.render_widget(
                    Paragraph::new("No matching commands")
                        .style(Style::default().fg(theme.text_muted)),
                    area,
                );
            }
        } else {
            for (area, match_index) in &layout.rows {
                let command = &self.commands[self.matches[*match_index]];
                let selected = *match_index == self.selected;
                let enabled = command.state.is_enabled();
                let background = selected.then_some(theme.selection);
                let foreground = match (selected, enabled) {
                    (true, _) => theme.text_strong,
                    (false, true) => theme.text,
                    (false, false) => theme.text_muted,
                };
                let style = Style::default().fg(foreground).bg(background.unwrap_or(theme.surface));
                frame.render_widget(Block::default().style(style), *area);

                let columns =
                    Layout::horizontal([Constraint::Fill(1), Constraint::Length(24)]).split(*area);
                let label = Line::from(vec![
                    Span::styled(
                        fit_terminal_text(
                            command.group,
                            12,
                            CellAlignment::Left,
                            CellOverflow::Clip,
                        ),
                        style.fg(if selected { theme.accent_alt } else { theme.text_muted }),
                    ),
                    Span::styled(command.title, style),
                ]);
                frame.render_widget(Paragraph::new(label), columns[0]);
                let keybinding = command
                    .primary_keybinding()
                    .map(|binding| binding.to_string())
                    .unwrap_or_default();
                frame.render_widget(
                    Paragraph::new(keybinding)
                        .alignment(Alignment::Right)
                        .style(style.fg(theme.text_muted)),
                    columns[1],
                );
            }
        }

        if let Some(notice) = &self.notice {
            let area = Rect {
                x: layout.popup.x.saturating_add(2),
                y: layout.popup.bottom().saturating_sub(2),
                width: layout.popup.width.saturating_sub(4),
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(notice.as_str())
                    .style(Style::default().fg(theme.warning))
                    .wrap(Wrap { trim: true }),
                area,
            );
        }

        let cursor_cells =
            u16::try_from(terminal_text_width(&self.query.value()[..self.query.cursor()]))
                .unwrap_or(u16::MAX);
        let cursor_x = layout
            .input
            .x
            .saturating_add(2)
            .saturating_add(cursor_cells)
            .min(layout.input.right().saturating_sub(1));
        frame.set_cursor_position(Position::new(cursor_x, layout.input.y));
    }

    fn on_key(&mut self, key: KeyEvent, visible_rows: usize) -> CommandPaletteOutcome<C> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return CommandPaletteOutcome::Captured;
        }
        match key.code {
            KeyCode::Esc => return CommandPaletteOutcome::Dismissed,
            KeyCode::Char('c' | 'g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return CommandPaletteOutcome::Dismissed;
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_selection(-1, visible_rows);
            }
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_selection(1, visible_rows);
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.query.clear();
                self.refilter();
            }
            KeyCode::Up | KeyCode::BackTab => self.move_selection(-1, visible_rows),
            KeyCode::Down | KeyCode::Tab => self.move_selection(1, visible_rows),
            KeyCode::PageUp => self.move_selection(-(visible_rows.max(1) as isize), visible_rows),
            KeyCode::PageDown => self.move_selection(visible_rows.max(1) as isize, visible_rows),
            KeyCode::Enter => return self.invoke_selected(),
            _ => {
                let before = self.query.value().to_owned();
                self.query.apply_key(key);
                if self.query.value() != before {
                    self.refilter();
                }
            }
        }
        CommandPaletteOutcome::Captured
    }

    fn on_mouse(
        &mut self,
        mouse: MouseEvent,
        layout: &CommandPaletteLayout,
    ) -> CommandPaletteOutcome<C> {
        let position = (mouse.column, mouse.row);
        if !contains(layout.popup, position)
            && mouse.kind == MouseEventKind::Down(MouseButton::Left)
        {
            return CommandPaletteOutcome::Dismissed;
        }
        if let Some((_, index)) = layout.rows.iter().find(|(area, _)| contains(*area, position)) {
            self.selected = *index;
            self.notice = None;
            if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                return self.invoke_selected();
            }
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.move_selection(-1, layout.rows.len()),
            MouseEventKind::ScrollDown => self.move_selection(1, layout.rows.len()),
            _ => {}
        }
        CommandPaletteOutcome::Captured
    }

    fn invoke_selected(&mut self) -> CommandPaletteOutcome<C> {
        let Some(command) = self.selected_action() else {
            return CommandPaletteOutcome::Captured;
        };
        match &command.state {
            ActionState::Enabled => CommandPaletteOutcome::Invoke(ActionInvocation::new(
                command.id,
                self.context.clone(),
            )),
            ActionState::Disabled { reason } => {
                self.notice = Some(reason.to_string());
                CommandPaletteOutcome::Captured
            }
        }
    }

    fn refilter(&mut self) {
        self.notice = None;
        self.matches = if self.query.value().trim().is_empty() {
            (0..self.commands.len()).collect()
        } else {
            let mut matcher = Matcher::case_insensitive(self.query.value());
            let mut matches = self
                .commands
                .iter()
                .enumerate()
                .filter_map(|(index, command)| {
                    let candidate =
                        format!("{} {} {}", command.group, command.title, command.id.as_str());
                    matcher.score(&candidate).map(|score| (score, index))
                })
                .collect::<Vec<_>>();
            matches.sort_by_key(|(score, index)| (*score, *index));
            matches.into_iter().map(|(_, index)| index).collect()
        };
        self.selected = 0;
        self.scroll = 0;
    }

    fn move_selection(&mut self, delta: isize, visible_rows: usize) {
        if self.matches.is_empty() {
            return;
        }
        let last = self.matches.len() - 1;
        self.selected = self.selected.saturating_add_signed(delta).min(last);
        let visible_rows = visible_rows.max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + visible_rows {
            self.scroll = self.selected + 1 - visible_rows;
        }
        self.notice = None;
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 3,
        width,
        height,
    }
}

fn contains(area: Rect, (x, y): (u16, u16)) -> bool {
    x >= area.x && x < area.right() && y >= area.y && y < area.bottom()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::{
        ActionId, ActionRegistryBuilder, ActionSpec, CommandPalettePlacement, KeyChord,
        KeybindingPlacement,
    };

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct Context {
        writable: bool,
    }

    #[derive(Clone, Copy)]
    enum Command {
        Open,
        Delete,
    }

    const OPEN: ActionId = ActionId::new("fixture.open");
    const DELETE: ActionId = ActionId::new("fixture.delete");

    fn enabled(_: &Context) -> ActionState {
        ActionState::Enabled
    }

    fn writable(context: &Context) -> ActionState {
        if context.writable {
            ActionState::Enabled
        } else {
            ActionState::disabled("read only")
        }
    }

    fn always(_: &Context) -> bool {
        true
    }

    fn palette(context: Context) -> CommandPalette<Context> {
        let mut builder = ActionRegistryBuilder::new();
        builder
            .register_action(ActionSpec {
                id: OPEN,
                title: "Open item",
                command: Command::Open,
                enablement: enabled,
                command_palette: CommandPalettePlacement::Visible {
                    group: "Item",
                    group_order: 10,
                    order: 10,
                },
            })
            .register_action(ActionSpec {
                id: DELETE,
                title: "Delete item",
                command: Command::Delete,
                enablement: writable,
                command_palette: CommandPalettePlacement::Visible {
                    group: "Item",
                    group_order: 10,
                    order: 20,
                },
            })
            .bind_key(KeybindingPlacement {
                binding: KeyChord::new(KeyCode::Char('o'), KeyModifiers::CONTROL).into(),
                action: OPEN,
                when: always,
            });
        CommandPalette::open(context, &builder.build().unwrap())
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn fuzzy_search_returns_the_registry_invocation() {
        let mut palette = palette(Context { writable: true });
        let layout = palette.layout(Rect::new(0, 0, 100, 30));
        for character in "delete".chars() {
            assert_eq!(
                palette.on_event(key(KeyCode::Char(character)), &layout),
                CommandPaletteOutcome::Captured
            );
        }
        assert_eq!(palette.query(), "delete");
        assert_eq!(palette.selected_action().map(|action| action.id), Some(DELETE));
        assert_eq!(
            palette.on_event(key(KeyCode::Enter), &layout),
            CommandPaletteOutcome::Invoke(ActionInvocation::new(
                DELETE,
                Context { writable: true }
            ))
        );
    }

    #[test]
    fn disabled_command_stays_open_and_explains_why() {
        let mut palette = palette(Context { writable: false });
        let layout = palette.layout(Rect::new(0, 0, 100, 30));
        palette.on_event(key(KeyCode::Down), &layout);
        assert_eq!(palette.on_event(key(KeyCode::Enter), &layout), CommandPaletteOutcome::Captured);
        assert_eq!(palette.notice.as_deref(), Some("read only"));
    }

    #[test]
    fn mouse_uses_rendered_row_geometry() {
        let mut palette = palette(Context { writable: true });
        let layout = palette.layout(Rect::new(0, 0, 100, 30));
        let delete_row = layout.rows[1].0;
        let outcome = palette.on_event(
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: delete_row.x,
                row: delete_row.y,
                modifiers: KeyModifiers::NONE,
            }),
            &layout,
        );
        assert_eq!(
            outcome,
            CommandPaletteOutcome::Invoke(ActionInvocation::new(
                DELETE,
                Context { writable: true }
            ))
        );
    }

    #[test]
    fn control_navigation_and_query_clear_are_supported() {
        let mut palette = palette(Context { writable: true });
        let layout = palette.layout(Rect::new(0, 0, 100, 30));
        palette.on_event(
            Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL)),
            &layout,
        );
        assert_eq!(palette.selected_action().map(|action| action.id), Some(DELETE));
        palette.on_event(
            Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
            &layout,
        );
        assert_eq!(palette.selected_action().map(|action| action.id), Some(OPEN));

        palette.on_event(key(KeyCode::Char('d')), &layout);
        assert_eq!(palette.query(), "d");
        palette.on_event(
            Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            &layout,
        );
        assert_eq!(palette.query(), "");
        assert_eq!(palette.selected_action().map(|action| action.id), Some(OPEN));
    }
}
