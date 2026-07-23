//! Reusable context-menu state, geometry, rendering, and input capture.
//!
//! The caller resolves domain-owned action data before opening the menu and remains responsible for
//! executing returned invocations. One [`ContextMenuLayout`] is both the render description and the
//! subsequent mouse hit map.

use std::borrow::Cow;

use crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::layout::{Margin, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::actions::{
    ActionId, ActionInvocation, ActionState, KeyChord, ResolvedAction, ResolvedActions,
};
use super::theme::{TuiTheme, NORD};

#[derive(Clone, Copy, Debug)]
pub struct ContextMenuStyle {
    pub background: Style,
    pub border: Style,
    pub item: Style,
    pub selected: Style,
    pub disabled: Style,
    pub selected_disabled: Style,
    pub shortcut: Style,
    pub separator: Style,
}

impl ContextMenuStyle {
    pub fn from_theme(theme: TuiTheme) -> Self {
        Self {
            background: Style::default().bg(theme.surface),
            border: Style::default().fg(theme.border).bg(theme.surface),
            item: Style::default().fg(theme.text).bg(theme.surface),
            selected: Style::default()
                .fg(theme.text_strong)
                .bg(theme.selection)
                .add_modifier(Modifier::BOLD),
            disabled: Style::default().fg(theme.text_muted).bg(theme.surface),
            selected_disabled: Style::default().fg(theme.text_muted).bg(theme.selection),
            shortcut: Style::default().fg(theme.accent).bg(theme.surface),
            separator: Style::default().fg(theme.border).bg(theme.surface),
        }
    }
}

impl Default for ContextMenuStyle {
    fn default() -> Self {
        Self::from_theme(NORD)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextMenuItemLayout {
    area: Rect,
    action: ActionId,
}

impl ContextMenuItemLayout {
    pub const fn area(&self) -> Rect {
        self.area
    }

    pub const fn action(&self) -> ActionId {
        self.action
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ContextMenuLayout {
    area: Rect,
    items: Vec<ContextMenuItemLayout>,
    separators: Vec<Rect>,
}

impl ContextMenuLayout {
    pub const fn area(&self) -> Rect {
        self.area
    }

    pub fn items(&self) -> &[ContextMenuItemLayout] {
        &self.items
    }

    pub fn separators(&self) -> &[Rect] {
        &self.separators
    }

    pub fn item_at(&self, position: Position) -> Option<ActionId> {
        self.items.iter().find(|item| item.area.contains(position)).map(|item| item.action)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextMenuOutcome<C> {
    Captured,
    Dismissed,
    Unavailable { action: ActionId, reason: Cow<'static, str> },
    Invoke(ActionInvocation<C>),
}

pub struct ContextMenu<C> {
    anchor: Position,
    context: C,
    items: ResolvedActions,
    selected: usize,
}

impl<C> ContextMenu<C> {
    pub fn open(anchor: Position, context: C, items: ResolvedActions) -> Option<Self> {
        if items.is_empty() {
            return None;
        }
        Some(Self { anchor, context, items, selected: 0 })
    }

    pub const fn context(&self) -> &C {
        &self.context
    }

    pub const fn selected(&self) -> usize {
        self.selected
    }

    pub fn items(&self) -> &[ResolvedAction] {
        self.items.items()
    }

    pub fn layout(&self, viewport: Rect) -> ContextMenuLayout {
        let logical_rows = self.logical_rows();
        let desired_width = self
            .items
            .items()
            .iter()
            .map(item_width)
            .max()
            .unwrap_or_default()
            .saturating_add(2)
            .max(18);
        let desired_height =
            u16::try_from(logical_rows.len()).unwrap_or(u16::MAX).saturating_add(2);
        let width = desired_width.min(viewport.width);
        let height = desired_height.min(viewport.height);
        let area = Rect::new(
            clamp_axis(self.anchor.x, viewport.x, viewport.right(), width),
            clamp_axis(self.anchor.y, viewport.y, viewport.bottom(), height),
            width,
            height,
        );
        let inner = area.inner(Margin { horizontal: 1, vertical: 1 });
        let capacity = usize::from(inner.height);
        let selected_row = logical_rows
            .iter()
            .position(|row| matches!(row, LogicalRow::Item(index) if *index == self.selected))
            .unwrap_or_default();
        let start = selected_row
            .saturating_sub(capacity / 2)
            .min(logical_rows.len().saturating_sub(capacity));
        let mut items = Vec::new();
        let mut separators = Vec::new();
        for (offset, row) in logical_rows.iter().skip(start).take(capacity).enumerate() {
            let area = Rect::new(inner.x, inner.y.saturating_add(offset as u16), inner.width, 1);
            match row {
                LogicalRow::Item(index) => {
                    items.push(ContextMenuItemLayout {
                        area,
                        action: self.items.items()[*index].id,
                    });
                }
                LogicalRow::Separator => separators.push(area),
            }
        }
        ContextMenuLayout { area, items, separators }
    }

    pub fn render(
        &self,
        frame: &mut Frame<'_>,
        layout: &ContextMenuLayout,
        style: ContextMenuStyle,
    ) {
        frame.render_widget(Clear, layout.area);
        frame.render_widget(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .style(style.background)
                .border_style(style.border),
            layout.area,
        );
        for separator in &layout.separators {
            frame.render_widget(
                Paragraph::new("─".repeat(usize::from(separator.width))).style(style.separator),
                *separator,
            );
        }
        for item_layout in &layout.items {
            let Some(index) = self.item_index(item_layout.action) else {
                continue;
            };
            let item = &self.items.items()[index];
            frame.render_widget(
                Paragraph::new(item_line(
                    item,
                    item_layout.area.width,
                    index == self.selected,
                    style,
                )),
                item_layout.area,
            );
        }
    }

    fn item_index(&self, action: ActionId) -> Option<usize> {
        self.items.items().iter().position(|item| item.id == action)
    }

    fn logical_rows(&self) -> Vec<LogicalRow> {
        let mut rows = Vec::with_capacity(self.items.len().saturating_mul(2));
        let mut previous_group = None;
        for (index, item) in self.items.items().iter().enumerate() {
            if previous_group.is_some_and(|group| group != item.group) {
                rows.push(LogicalRow::Separator);
            }
            rows.push(LogicalRow::Item(index));
            previous_group = Some(item.group);
        }
        rows
    }
}

impl<C: Clone> ContextMenu<C> {
    pub fn on_event(&mut self, event: Event, layout: &ContextMenuLayout) -> ContextMenuOutcome<C> {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Release => ContextMenuOutcome::Captured,
            Event::Key(key) => {
                let Some(chord) = KeyChord::from_event(key) else {
                    return ContextMenuOutcome::Captured;
                };
                match menu_control(chord) {
                    Some(MenuControl::Previous) => {
                        self.move_selection(-1);
                        ContextMenuOutcome::Captured
                    }
                    Some(MenuControl::Next) => {
                        self.move_selection(1);
                        ContextMenuOutcome::Captured
                    }
                    Some(MenuControl::First) => {
                        self.selected = 0;
                        ContextMenuOutcome::Captured
                    }
                    Some(MenuControl::Last) => {
                        self.selected = self.items.len().saturating_sub(1);
                        ContextMenuOutcome::Captured
                    }
                    Some(MenuControl::Activate) => self.activate_selected(),
                    Some(MenuControl::Dismiss) => ContextMenuOutcome::Dismissed,
                    None => self.activate_keybinding(chord),
                }
            }
            Event::Mouse(mouse) => {
                let position = Position { x: mouse.column, y: mouse.row };
                match mouse.kind {
                    MouseEventKind::Moved => {
                        if let Some(index) =
                            layout.item_at(position).and_then(|action| self.item_index(action))
                        {
                            self.selected = index;
                        }
                        ContextMenuOutcome::Captured
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(index) =
                            layout.item_at(position).and_then(|action| self.item_index(action))
                        {
                            self.selected = index;
                            self.activate_selected()
                        } else if layout.area.contains(position) {
                            ContextMenuOutcome::Captured
                        } else {
                            ContextMenuOutcome::Dismissed
                        }
                    }
                    MouseEventKind::Down(_) if !layout.area.contains(position) => {
                        ContextMenuOutcome::Dismissed
                    }
                    _ => ContextMenuOutcome::Captured,
                }
            }
            _ => ContextMenuOutcome::Captured,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        self.selected =
            (self.selected as isize + delta).rem_euclid(self.items.len() as isize) as usize;
    }

    fn activate_keybinding(&mut self, chord: KeyChord) -> ContextMenuOutcome<C> {
        let Some(index) = self.items.items().iter().position(|item| {
            item.keybindings.iter().any(|binding| binding.direct_chord() == Some(chord))
        }) else {
            return ContextMenuOutcome::Captured;
        };
        self.selected = index;
        self.activate_selected()
    }

    fn activate_selected(&self) -> ContextMenuOutcome<C> {
        let item = &self.items.items()[self.selected];
        match &item.state {
            ActionState::Enabled => {
                ContextMenuOutcome::Invoke(ActionInvocation::new(item.id, self.context.clone()))
            }
            ActionState::Disabled { reason } => {
                ContextMenuOutcome::Unavailable { action: item.id, reason: reason.clone() }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum LogicalRow {
    Item(usize),
    Separator,
}

#[derive(Clone, Copy)]
enum MenuControl {
    Previous,
    Next,
    First,
    Last,
    Activate,
    Dismiss,
}

fn menu_control(chord: KeyChord) -> Option<MenuControl> {
    if !chord.modifiers().is_empty() {
        return None;
    }
    match chord.code() {
        KeyCode::Up | KeyCode::Char('k') => Some(MenuControl::Previous),
        KeyCode::Down | KeyCode::Char('j') => Some(MenuControl::Next),
        KeyCode::Home => Some(MenuControl::First),
        KeyCode::End => Some(MenuControl::Last),
        KeyCode::Enter => Some(MenuControl::Activate),
        KeyCode::Esc | KeyCode::Char('q') => Some(MenuControl::Dismiss),
        _ => None,
    }
}

fn menu_keybinding(item: &ResolvedAction) -> Option<KeyChord> {
    item.keybindings
        .iter()
        .filter_map(|binding| binding.direct_chord())
        .find(|chord| menu_control(*chord).is_none())
}

fn clamp_axis(anchor: u16, start: u16, end: u16, length: u16) -> u16 {
    let latest = end.saturating_sub(length).max(start);
    anchor.clamp(start, latest)
}

fn item_width(item: &ResolvedAction) -> u16 {
    let reason = match &item.state {
        ActionState::Enabled => 0,
        ActionState::Disabled { reason } => 3 + reason.width(),
    };
    let shortcut =
        menu_keybinding(item).map(|chord| chord.to_string().width() + 2).unwrap_or_default();
    u16::try_from(2 + item.title.width() + reason + shortcut).unwrap_or(u16::MAX)
}

fn item_line(
    item: &ResolvedAction,
    width: u16,
    selected: bool,
    style: ContextMenuStyle,
) -> Line<'static> {
    let row_style = match (&item.state, selected) {
        (ActionState::Enabled, false) => style.item,
        (ActionState::Enabled, true) => style.selected,
        (ActionState::Disabled { .. }, false) => style.disabled,
        (ActionState::Disabled { .. }, true) => style.selected_disabled,
    };
    let reason = match &item.state {
        ActionState::Enabled => String::new(),
        ActionState::Disabled { reason } => format!(" — {reason}"),
    };
    let left = format!(" {}{reason}", item.title);
    let shortcut = menu_keybinding(item).map(|chord| format!("{} ", chord));
    let total_width = usize::from(width);
    let shortcut = shortcut.filter(|shortcut| shortcut.width() + 3 <= total_width);
    let shortcut_width = shortcut.as_deref().map(UnicodeWidthStr::width).unwrap_or_default();
    let left_width = total_width.saturating_sub(shortcut_width);
    let left = fit(&left, left_width);
    let shortcut_style = if selected { row_style } else { style.shortcut };
    Line::from(vec![
        Span::styled(left, row_style),
        Span::styled(shortcut.unwrap_or_default(), shortcut_style),
    ])
}

fn fit(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }

    let mut output = String::new();
    let mut used = 0;
    for character in value.chars() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width > width {
            break;
        }
        output.push(character);
        used += character_width;
    }
    output.push_str(&" ".repeat(width.saturating_sub(used)));
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::actions::{
        ActionRegistry, ActionRegistryBuilder, ActionSpec, CommandPalettePlacement,
        KeybindingPlacement, MenuId, MenuPlacement,
    };
    use crossterm::event::{KeyEvent, KeyModifiers, MouseEvent};
    use ratatui::backend::TestBackend;

    const OPEN: ActionId = ActionId::new("fixture.item.open");
    const DELETE: ActionId = ActionId::new("fixture.item.delete");
    const INSPECT: ActionId = ActionId::new("fixture.other.inspect");
    const MENU: MenuId = MenuId::new("fixture.item.context");
    const OTHER_MENU: MenuId = MenuId::new("fixture.other.context");

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct Context {
        target: u32,
        visible: bool,
        writable: bool,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Command {
        Open,
        Delete,
        Inspect,
    }

    fn visible(context: &Context) -> bool {
        context.visible
    }

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

    fn registry() -> ActionRegistry<Context, Command> {
        let mut builder = ActionRegistryBuilder::new();
        builder
            .register_action(ActionSpec {
                id: OPEN,
                title: "Open item",
                command: Command::Open,
                enablement: enabled,
                command_palette: CommandPalettePlacement::Hidden,
            })
            .register_action(ActionSpec {
                id: DELETE,
                title: "Delete item",
                command: Command::Delete,
                enablement: writable,
                command_palette: CommandPalettePlacement::Hidden,
            })
            .register_action(ActionSpec {
                id: INSPECT,
                title: "Inspect other item",
                command: Command::Inspect,
                enablement: enabled,
                command_palette: CommandPalettePlacement::Hidden,
            })
            .place_menu(MenuPlacement {
                menu: MENU,
                action: OPEN,
                group: "navigation",
                group_order: 10,
                order: 10,
                when: visible,
            })
            .place_menu(MenuPlacement {
                menu: MENU,
                action: DELETE,
                group: "destructive",
                group_order: 20,
                order: 10,
                when: visible,
            })
            .place_menu(MenuPlacement {
                menu: OTHER_MENU,
                action: INSPECT,
                group: "navigation",
                group_order: 10,
                order: 10,
                when: visible,
            })
            .bind_key(KeybindingPlacement {
                binding: KeyChord::new(KeyCode::Char('o'), KeyModifiers::CONTROL).into(),
                action: OPEN,
                when: visible,
            })
            .bind_key(KeybindingPlacement {
                binding: KeyChord::new(KeyCode::Delete, KeyModifiers::NONE).into(),
                action: DELETE,
                when: visible,
            })
            .bind_key(KeybindingPlacement {
                binding: KeyChord::new(KeyCode::Char('j'), KeyModifiers::NONE).into(),
                action: OPEN,
                when: visible,
            })
            .bind_key(KeybindingPlacement {
                binding: KeyChord::new(KeyCode::Enter, KeyModifiers::CONTROL).into(),
                action: OPEN,
                when: visible,
            })
            .bind_key(KeybindingPlacement {
                binding: KeyChord::new(KeyCode::Char('j'), KeyModifiers::CONTROL).into(),
                action: DELETE,
                when: visible,
            })
            .bind_key(KeybindingPlacement {
                binding: KeyChord::new(KeyCode::Char('q'), KeyModifiers::CONTROL).into(),
                action: OPEN,
                when: visible,
            });
        builder.build().unwrap()
    }

    fn menu(context: Context, anchor: Position) -> ContextMenu<Context> {
        let resolved = registry().resolve_menu(MENU, &context);
        ContextMenu::open(anchor, context, resolved).unwrap()
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn modified_key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    fn mouse(kind: MouseEventKind, position: Position) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: position.x,
            row: position.y,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn empty_resolved_menu_does_not_open() {
        let context = Context { target: 7, visible: false, writable: true };
        assert!(ContextMenu::open(
            Position { x: 0, y: 0 },
            context,
            registry().resolve_menu(MENU, &context),
        )
        .is_none());
    }

    #[test]
    fn keyboard_navigation_invokes_exact_context_and_reports_disabled_reason() {
        let context = Context { target: 41, visible: true, writable: false };
        let mut menu = menu(context, Position { x: 2, y: 2 });
        let layout = menu.layout(Rect::new(0, 0, 50, 12));

        assert_eq!(menu.on_event(key(KeyCode::Down), &layout), ContextMenuOutcome::Captured);
        assert_eq!(menu.selected(), 1);
        assert_eq!(
            menu.on_event(key(KeyCode::Enter), &layout),
            ContextMenuOutcome::Unavailable { action: DELETE, reason: Cow::Borrowed("read only") }
        );
        assert_eq!(menu.selected(), 1, "unavailable activation keeps the menu open");

        assert_eq!(menu.on_event(key(KeyCode::Home), &layout), ContextMenuOutcome::Captured);
        assert_eq!(
            menu.on_event(key(KeyCode::Enter), &layout),
            ContextMenuOutcome::Invoke(ActionInvocation::new(OPEN, context))
        );
        assert_eq!(menu.on_event(key(KeyCode::End), &layout), ContextMenuOutcome::Captured);
        assert_eq!(menu.selected(), 1);
        assert_eq!(menu.on_event(key(KeyCode::Esc), &layout), ContextMenuOutcome::Dismissed);
        assert_eq!(menu.on_event(key(KeyCode::Char('q')), &layout), ContextMenuOutcome::Dismissed);
    }

    #[test]
    fn enabled_menu_shortcut_invokes_its_exact_item_on_repeat() {
        let context = Context { target: 42, visible: true, writable: true };
        let mut menu = menu(context, Position { x: 2, y: 2 });
        let layout = menu.layout(Rect::new(0, 0, 50, 12));
        let repeated = Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('o'),
            KeyModifiers::CONTROL,
            KeyEventKind::Repeat,
        ));

        assert_eq!(
            menu.on_event(repeated, &layout),
            ContextMenuOutcome::Invoke(ActionInvocation::new(OPEN, context))
        );
        assert_eq!(menu.selected(), 0);
    }

    #[test]
    fn disabled_menu_shortcut_reports_unavailable_for_its_exact_item() {
        let context = Context { target: 43, visible: true, writable: false };
        let mut menu = menu(context, Position { x: 2, y: 2 });
        let layout = menu.layout(Rect::new(0, 0, 50, 12));

        assert_eq!(
            menu.on_event(key(KeyCode::Delete), &layout),
            ContextMenuOutcome::Unavailable { action: DELETE, reason: Cow::Borrowed("read only") }
        );
        assert_eq!(menu.selected(), 1);
    }

    #[test]
    fn navigation_keys_take_precedence_over_menu_shortcuts() {
        let context = Context { target: 44, visible: true, writable: true };
        let mut menu = menu(context, Position { x: 2, y: 2 });
        let layout = menu.layout(Rect::new(0, 0, 50, 12));
        assert!(menu.items()[0].keybindings.iter().any(|binding| binding.direct_chord()
            == Some(KeyChord::new(KeyCode::Char('j'), KeyModifiers::NONE))));

        assert_eq!(menu.on_event(key(KeyCode::Char('j')), &layout), ContextMenuOutcome::Captured);
        assert_eq!(menu.selected(), 1);
    }

    #[test]
    fn modified_enter_j_and_q_invoke_contributed_shortcuts() {
        let context = Context { target: 45, visible: true, writable: true };
        let mut menu = menu(context, Position { x: 2, y: 2 });
        let layout = menu.layout(Rect::new(0, 0, 50, 12));

        assert_eq!(
            menu.on_event(modified_key(KeyCode::Enter, KeyModifiers::CONTROL), &layout),
            ContextMenuOutcome::Invoke(ActionInvocation::new(OPEN, context))
        );
        assert_eq!(menu.selected(), 0);

        assert_eq!(
            menu.on_event(modified_key(KeyCode::Char('j'), KeyModifiers::CONTROL), &layout,),
            ContextMenuOutcome::Invoke(ActionInvocation::new(DELETE, context))
        );
        assert_eq!(menu.selected(), 1);

        assert_eq!(
            menu.on_event(modified_key(KeyCode::Char('q'), KeyModifiers::CONTROL), &layout,),
            ContextMenuOutcome::Invoke(ActionInvocation::new(OPEN, context))
        );
        assert_eq!(menu.selected(), 0);
    }

    #[test]
    fn reserved_bare_controls_are_not_advertised_as_menu_shortcuts() {
        let reserved = [
            KeyChord::new(KeyCode::Up, KeyModifiers::NONE),
            KeyChord::new(KeyCode::Down, KeyModifiers::NONE),
            KeyChord::new(KeyCode::Char('j'), KeyModifiers::NONE),
            KeyChord::new(KeyCode::Char('k'), KeyModifiers::NONE),
            KeyChord::new(KeyCode::Home, KeyModifiers::NONE),
            KeyChord::new(KeyCode::End, KeyModifiers::NONE),
            KeyChord::new(KeyCode::Enter, KeyModifiers::NONE),
            KeyChord::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyChord::new(KeyCode::Char('q'), KeyModifiers::NONE),
        ];
        for chord in reserved {
            let item = ResolvedAction {
                id: OPEN,
                title: "Open item",
                group: "fixture",
                state: ActionState::Enabled,
                keybindings: vec![chord.into()],
            };
            assert_eq!(menu_keybinding(&item), None, "reserved control {chord} was advertised");
        }

        let modified = KeyChord::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        let item = ResolvedAction {
            id: OPEN,
            title: "Open item",
            group: "fixture",
            state: ActionState::Enabled,
            keybindings: vec![
                KeyChord::new(KeyCode::Char('q'), KeyModifiers::NONE).into(),
                modified.into(),
            ],
        };
        assert_eq!(menu_keybinding(&item), Some(modified));
        let rendered = item_line(&item, 30, false, ContextMenuStyle::default())
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("Ctrl+Q"));
    }

    #[test]
    fn mouse_uses_the_published_layout_and_consumes_outside_clicks() {
        let context = Context { target: 9, visible: true, writable: false };
        let mut menu = menu(context, Position { x: 8, y: 3 });
        let layout = menu.layout(Rect::new(0, 0, 50, 12));
        let delete = layout.items().iter().find(|item| item.action() == DELETE).unwrap().area();
        let delete_point = Position { x: delete.x, y: delete.y };

        assert_eq!(
            menu.on_event(mouse(MouseEventKind::Moved, delete_point), &layout),
            ContextMenuOutcome::Captured
        );
        assert_eq!(menu.selected(), 1);
        assert!(matches!(
            menu.on_event(mouse(MouseEventKind::Down(MouseButton::Left), delete_point), &layout,),
            ContextMenuOutcome::Unavailable { action: DELETE, .. }
        ));

        let border = Position { x: layout.area().x, y: layout.area().y };
        assert_eq!(
            menu.on_event(mouse(MouseEventKind::Down(MouseButton::Left), border), &layout),
            ContextMenuOutcome::Captured
        );
        let outside = Position { x: 0, y: 0 };
        assert_eq!(
            menu.on_event(mouse(MouseEventKind::Down(MouseButton::Left), outside), &layout),
            ContextMenuOutcome::Dismissed
        );
    }

    #[test]
    fn mismatched_layout_cannot_panic_or_activate_an_unrelated_item() {
        let context = Context { target: 10, visible: true, writable: true };
        let source = menu(context, Position { x: 2, y: 2 });
        let source_layout = source.layout(Rect::new(0, 0, 50, 12));
        let source_area = source_layout.items()[0].area();
        let source_point = Position { x: source_area.x, y: source_area.y };
        assert_eq!(source_layout.item_at(source_point), Some(OPEN));

        let mut foreign = ContextMenu::open(
            Position { x: 8, y: 3 },
            context,
            registry().resolve_menu(OTHER_MENU, &context),
        )
        .unwrap();

        let backend = TestBackend::new(50, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| foreign.render(frame, &source_layout, ContextMenuStyle::default()))
            .unwrap();

        assert_eq!(
            foreign.on_event(mouse(MouseEventKind::Moved, source_point), &source_layout),
            ContextMenuOutcome::Captured
        );
        assert_eq!(foreign.selected(), 0);
        assert_eq!(
            foreign.on_event(
                mouse(MouseEventKind::Down(MouseButton::Left), source_point),
                &source_layout,
            ),
            ContextMenuOutcome::Captured
        );
        assert_eq!(foreign.selected(), 0);
    }

    #[test]
    fn geometry_clamps_at_four_corners_and_keeps_selection_visible_after_resize() {
        let viewport = Rect::new(10, 20, 40, 10);
        for anchor in [
            Position { x: viewport.x, y: viewport.y },
            Position { x: viewport.right() - 1, y: viewport.y },
            Position { x: viewport.x, y: viewport.bottom() - 1 },
            Position { x: viewport.right() - 1, y: viewport.bottom() - 1 },
        ] {
            let layout =
                menu(Context { target: 1, visible: true, writable: true }, anchor).layout(viewport);
            assert_eq!(layout.area().intersection(viewport), layout.area());
            assert!(layout.items().iter().all(|item| {
                let area = item.area();
                viewport.contains(Position { x: area.x, y: area.y })
            }));
        }

        for viewport in [
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 1, 1),
            Rect::new(0, 0, 4, 2),
            Rect::new(0, 0, 12, 4),
        ] {
            let layout = menu(
                Context { target: 2, visible: true, writable: false },
                Position { x: u16::MAX, y: u16::MAX },
            )
            .layout(viewport);
            assert_eq!(layout.area().intersection(viewport), layout.area());
        }

        let mut resized =
            menu(Context { target: 3, visible: true, writable: true }, Position { x: 45, y: 10 });
        let large = resized.layout(Rect::new(0, 0, 50, 12));
        resized.on_event(key(KeyCode::End), &large);
        let compact = resized.layout(Rect::new(0, 0, 16, 3));
        let selected = resized.items()[resized.selected()].id;
        assert!(compact.items().iter().any(|item| item.action() == selected));
        assert_ne!(large.area(), compact.area());
    }

    #[test]
    fn rendering_uses_resolved_titles_shortcuts_disabled_reason_and_group_separator() {
        let menu =
            menu(Context { target: 5, visible: true, writable: false }, Position { x: 48, y: 10 });
        let backend = TestBackend::new(50, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut layout = ContextMenuLayout::default();
        terminal
            .draw(|frame| {
                layout = menu.layout(frame.area());
                menu.render(frame, &layout, ContextMenuStyle::default());
            })
            .unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        for expected in ["Open item", "Ctrl+O", "Delete item", "read only", "─"] {
            assert!(screen.contains(expected), "missing rendered metadata {expected:?}");
        }
        assert_eq!(layout.separators().len(), 1);
        assert_eq!(layout.items().len(), 2);
    }

    #[test]
    fn unicode_labels_use_terminal_cell_width_for_geometry_and_truncation() {
        let item = ResolvedAction {
            id: OPEN,
            title: "界面",
            group: "fixture",
            state: ActionState::disabled("危険"),
            keybindings: vec![KeyChord::new(KeyCode::Char('界'), KeyModifiers::CONTROL).into()],
        };

        assert_eq!(item_width(&item), 22);
        let line = item_line(&item, 16, false, ContextMenuStyle::default());
        assert_eq!(line.spans.iter().map(|span| span.content.width()).sum::<usize>(), 16);

        assert_eq!(fit("界e\u{301}", 3), "界e\u{301}");
        assert_eq!(fit("界e\u{301}", 2), "界");
        assert_eq!(fit("e\u{301}", 1), "e\u{301}");
        assert_eq!(fit("界", 1), " ");
    }

    #[test]
    fn minimum_viewports_render_and_keep_hit_regions_in_bounds() {
        let context = Context { target: 6, visible: true, writable: false };
        let anchor = Position { x: u16::MAX, y: u16::MAX };

        let mut zero_menu = menu(context, anchor);
        let zero_layout = zero_menu.layout(Rect::new(0, 0, 0, 0));
        assert!(zero_layout.items().is_empty());
        assert_eq!(zero_layout.item_at(Position { x: 0, y: 0 }), None);
        assert_eq!(
            zero_menu
                .on_event(mouse(MouseEventKind::Moved, Position { x: 0, y: 0 }), &zero_layout,),
            ContextMenuOutcome::Captured
        );

        for (width, height) in [(1, 1), (4, 2)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            let mut layout = ContextMenuLayout::default();
            terminal
                .draw(|frame| {
                    let menu = menu(context, anchor);
                    layout = menu.layout(frame.area());
                    menu.render(frame, &layout, ContextMenuStyle::default());
                })
                .unwrap();

            let viewport = Rect::new(0, 0, width, height);
            assert_eq!(layout.area().intersection(viewport), layout.area());
            assert!(layout
                .items()
                .iter()
                .all(|item| item.area().intersection(viewport) == item.area()));
            assert_eq!(layout.item_at(Position { x: 0, y: 0 }), None);

            let mut event_menu = menu(context, anchor);
            assert_eq!(
                event_menu.on_event(
                    mouse(MouseEventKind::Down(MouseButton::Left), Position { x: 0, y: 0 }),
                    &layout,
                ),
                ContextMenuOutcome::Captured
            );
        }
    }
}
