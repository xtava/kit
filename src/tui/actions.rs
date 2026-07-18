//! Immutable, typed action contribution mechanics for interactive tools.
//!
//! Tools own action vocabulary and execution. This module validates stable identities and projects
//! caller-owned action data into menus and keybindings without storing handlers or mutable state.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use thiserror::Error;

const MAX_ID_LEN: usize = 96;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ActionId(&'static str);

impl ActionId {
    pub const fn new(value: &'static str) -> Self {
        Self(value)
    }

    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for ActionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MenuId(&'static str);

impl MenuId {
    pub const fn new(value: &'static str) -> Self {
        Self(value)
    }

    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for MenuId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActionState {
    Enabled,
    Disabled { reason: Cow<'static, str> },
}

impl ActionState {
    pub fn disabled(reason: impl Into<Cow<'static, str>>) -> Self {
        Self::Disabled { reason: reason.into() }
    }

    pub const fn is_enabled(&self) -> bool {
        matches!(self, Self::Enabled)
    }
}

pub struct ActionSpec<C, Command> {
    pub id: ActionId,
    pub title: &'static str,
    pub command: Command,
    pub enablement: fn(&C) -> ActionState,
}

pub struct MenuPlacement<C> {
    pub menu: MenuId,
    pub action: ActionId,
    pub group: &'static str,
    pub group_order: i16,
    pub order: i16,
    pub when: fn(&C) -> bool,
}

pub struct KeybindingPlacement<C> {
    pub chord: KeyChord,
    pub action: ActionId,
    pub when: fn(&C) -> bool,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct KeyChord {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl KeyChord {
    pub fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        normalize_chord(code, modifiers)
    }

    pub fn from_event(event: KeyEvent) -> Option<Self> {
        match event.kind {
            KeyEventKind::Press | KeyEventKind::Repeat => {
                Some(Self::new(event.code, event.modifiers))
            }
            KeyEventKind::Release => None,
        }
    }

    pub fn code(self) -> KeyCode {
        self.code
    }

    pub fn modifiers(self) -> KeyModifiers {
        self.modifiers
    }
}

impl fmt::Display for KeyChord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (modifier, label) in [
            (KeyModifiers::CONTROL, "Ctrl"),
            (KeyModifiers::ALT, "Alt"),
            (KeyModifiers::SUPER, "Super"),
            (KeyModifiers::HYPER, "Hyper"),
            (KeyModifiers::META, "Meta"),
            (KeyModifiers::SHIFT, "Shift"),
        ] {
            if self.modifiers.contains(modifier) {
                write!(formatter, "{label}+")?;
            }
        }
        match self.code {
            KeyCode::Backspace => formatter.write_str("Backspace"),
            KeyCode::Enter => formatter.write_str("Enter"),
            KeyCode::Left => formatter.write_str("Left"),
            KeyCode::Right => formatter.write_str("Right"),
            KeyCode::Up => formatter.write_str("Up"),
            KeyCode::Down => formatter.write_str("Down"),
            KeyCode::Home => formatter.write_str("Home"),
            KeyCode::End => formatter.write_str("End"),
            KeyCode::PageUp => formatter.write_str("PageUp"),
            KeyCode::PageDown => formatter.write_str("PageDown"),
            KeyCode::Tab => formatter.write_str("Tab"),
            KeyCode::BackTab => formatter.write_str("BackTab"),
            KeyCode::Delete => formatter.write_str("Delete"),
            KeyCode::Insert => formatter.write_str("Insert"),
            KeyCode::Esc => formatter.write_str("Esc"),
            KeyCode::Char(' ') => formatter.write_str("Space"),
            KeyCode::Char(character) => write!(formatter, "{character}"),
            KeyCode::F(number) => write!(formatter, "F{number}"),
            other => write!(formatter, "{other:?}"),
        }
    }
}

fn normalize_chord(code: KeyCode, mut modifiers: KeyModifiers) -> KeyChord {
    let code = if let KeyCode::Char(character) = code {
        if modifiers.contains(KeyModifiers::SHIFT) && character.is_ascii_alphabetic() {
            modifiers.remove(KeyModifiers::SHIFT);
            KeyCode::Char(character.to_ascii_uppercase())
        } else {
            KeyCode::Char(character)
        }
    } else {
        code
    };
    KeyChord { code, modifiers }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedAction {
    pub id: ActionId,
    pub title: &'static str,
    pub group: &'static str,
    pub state: ActionState,
    pub keybindings: Vec<KeyChord>,
}

impl ResolvedAction {
    pub fn primary_keybinding(&self) -> Option<KeyChord> {
        self.keybindings.first().copied()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ResolvedMenu {
    items: Vec<ResolvedAction>,
}

impl ResolvedMenu {
    pub fn items(&self) -> &[ResolvedAction] {
        &self.items
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionInvocation<C> {
    pub action: ActionId,
    pub context: C,
}

impl<C> ActionInvocation<C> {
    pub const fn new(action: ActionId, context: C) -> Self {
        Self { action, context }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ActionUnavailable {
    #[error("action {action} is not registered")]
    Unknown { action: ActionId },
    #[error("action {action} is unavailable: {reason}")]
    Disabled { action: ActionId, reason: Cow<'static, str> },
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ActionRegistryError {
    #[error("invalid action ID {id:?}")]
    InvalidActionId { id: &'static str },
    #[error("invalid menu ID {id:?}")]
    InvalidMenuId { id: &'static str },
    #[error("duplicate action ID {id}")]
    DuplicateAction { id: ActionId },
    #[error("menu {menu} places action {action} more than once")]
    DuplicateMenuPlacement { menu: MenuId, action: ActionId },
    #[error("menu {menu} group {group:?} uses conflicting group orders {first} and {second}")]
    ConflictingMenuGroupOrder { menu: MenuId, group: &'static str, first: i16, second: i16 },
    #[error("{projection} references unknown action {action}")]
    UnknownAction { projection: &'static str, action: ActionId },
    #[error("key chord {chord} is bound to both {first} and {second}")]
    DuplicateKeyChord { chord: KeyChord, first: ActionId, second: ActionId },
}

pub struct ActionRegistryBuilder<C, Command> {
    actions: Vec<ActionSpec<C, Command>>,
    menu_placements: Vec<MenuPlacement<C>>,
    keybindings: Vec<KeybindingPlacement<C>>,
}

impl<C, Command> Default for ActionRegistryBuilder<C, Command> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C, Command> ActionRegistryBuilder<C, Command> {
    pub const fn new() -> Self {
        Self { actions: Vec::new(), menu_placements: Vec::new(), keybindings: Vec::new() }
    }

    pub fn register_action(&mut self, action: ActionSpec<C, Command>) -> &mut Self {
        self.actions.push(action);
        self
    }

    pub fn place_menu(&mut self, placement: MenuPlacement<C>) -> &mut Self {
        self.menu_placements.push(placement);
        self
    }

    pub fn bind_key(&mut self, placement: KeybindingPlacement<C>) -> &mut Self {
        self.keybindings.push(placement);
        self
    }

    pub fn build(mut self) -> Result<ActionRegistry<C, Command>, ActionRegistryError> {
        let mut action_indices = HashMap::with_capacity(self.actions.len());
        for (index, action) in self.actions.iter().enumerate() {
            if !valid_qualified_id(action.id.as_str()) {
                return Err(ActionRegistryError::InvalidActionId { id: action.id.as_str() });
            }
            if action_indices.insert(action.id, index).is_some() {
                return Err(ActionRegistryError::DuplicateAction { id: action.id });
            }
        }

        let mut placed_actions = HashSet::with_capacity(self.menu_placements.len());
        let mut group_orders = HashMap::with_capacity(self.menu_placements.len());
        for placement in &self.menu_placements {
            if !valid_qualified_id(placement.menu.as_str()) {
                return Err(ActionRegistryError::InvalidMenuId { id: placement.menu.as_str() });
            }
            if !action_indices.contains_key(&placement.action) {
                return Err(ActionRegistryError::UnknownAction {
                    projection: "menu placement",
                    action: placement.action,
                });
            }
            if let Some(first) =
                group_orders.insert((placement.menu, placement.group), placement.group_order)
            {
                if first != placement.group_order {
                    return Err(ActionRegistryError::ConflictingMenuGroupOrder {
                        menu: placement.menu,
                        group: placement.group,
                        first,
                        second: placement.group_order,
                    });
                }
            }
            if !placed_actions.insert((placement.menu, placement.action)) {
                return Err(ActionRegistryError::DuplicateMenuPlacement {
                    menu: placement.menu,
                    action: placement.action,
                });
            }
        }

        let mut chord_actions = HashMap::with_capacity(self.keybindings.len());
        for placement in &self.keybindings {
            if !action_indices.contains_key(&placement.action) {
                return Err(ActionRegistryError::UnknownAction {
                    projection: "keybinding",
                    action: placement.action,
                });
            }
            if let Some(first) = chord_actions.insert(placement.chord, placement.action) {
                return Err(ActionRegistryError::DuplicateKeyChord {
                    chord: placement.chord,
                    first,
                    second: placement.action,
                });
            }
        }

        self.menu_placements.sort_by(|left, right| {
            (left.menu, left.group_order, left.group, left.order, left.action).cmp(&(
                right.menu,
                right.group_order,
                right.group,
                right.order,
                right.action,
            ))
        });

        Ok(ActionRegistry {
            actions: self.actions,
            action_indices,
            menu_placements: self.menu_placements,
            keybindings: self.keybindings,
        })
    }
}

pub struct ActionRegistry<C, Command> {
    actions: Vec<ActionSpec<C, Command>>,
    action_indices: HashMap<ActionId, usize>,
    menu_placements: Vec<MenuPlacement<C>>,
    keybindings: Vec<KeybindingPlacement<C>>,
}

impl<C, Command> ActionRegistry<C, Command> {
    pub fn resolve_menu(&self, menu: MenuId, context: &C) -> ResolvedMenu {
        let items = self
            .menu_placements
            .iter()
            .filter(|placement| placement.menu == menu && (placement.when)(context))
            .map(|placement| {
                let action = self.action(placement.action);
                ResolvedAction {
                    id: action.id,
                    title: action.title,
                    group: placement.group,
                    state: (action.enablement)(context),
                    keybindings: self
                        .keybindings
                        .iter()
                        .filter(|binding| binding.action == action.id && (binding.when)(context))
                        .map(|binding| binding.chord)
                        .collect(),
                }
            })
            .collect::<Vec<_>>();
        ResolvedMenu { items }
    }

    pub fn resolve_keybinding(&self, chord: KeyChord, context: C) -> Option<ActionInvocation<C>> {
        let binding = self
            .keybindings
            .iter()
            .find(|binding| binding.chord == chord && (binding.when)(&context))?;
        Some(ActionInvocation::new(binding.action, context))
    }

    fn action(&self, id: ActionId) -> &ActionSpec<C, Command> {
        &self.actions[self.action_indices[&id]]
    }
}

impl<C, Command: Clone> ActionRegistry<C, Command> {
    pub fn command_for(
        &self,
        invocation: &ActionInvocation<C>,
    ) -> Result<Command, ActionUnavailable> {
        let Some(index) = self.action_indices.get(&invocation.action) else {
            return Err(ActionUnavailable::Unknown { action: invocation.action });
        };
        let action = &self.actions[*index];
        match (action.enablement)(&invocation.context) {
            ActionState::Enabled => Ok(action.command.clone()),
            ActionState::Disabled { reason } => {
                Err(ActionUnavailable::Disabled { action: invocation.action, reason })
            }
        }
    }
}

fn valid_qualified_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_ID_LEN || !value.is_ascii() || !value.contains('.') {
        return false;
    }
    if !bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        || !bytes.last().is_some_and(u8::is_ascii_alphanumeric)
    {
        return false;
    }
    bytes
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPEN: ActionId = ActionId::new("fixture.item.open");
    const DELETE: ActionId = ActionId::new("fixture.item.delete");
    const DETAILS: ActionId = ActionId::new("fixture.item.details");
    const MENU: MenuId = MenuId::new("fixture.item.context");

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FixtureCommand {
        Open,
        Delete,
        Details,
    }

    #[derive(Clone, Copy)]
    struct FixtureContext {
        visible: bool,
        writable: bool,
    }

    fn always(_: &FixtureContext) -> bool {
        true
    }

    fn visible(context: &FixtureContext) -> bool {
        context.visible
    }

    fn enabled(_: &FixtureContext) -> ActionState {
        ActionState::Enabled
    }

    fn writable(context: &FixtureContext) -> ActionState {
        if context.writable {
            ActionState::Enabled
        } else {
            ActionState::disabled("read only")
        }
    }

    fn fixture_builder() -> ActionRegistryBuilder<FixtureContext, FixtureCommand> {
        let mut builder = ActionRegistryBuilder::new();
        builder
            .register_action(ActionSpec {
                id: OPEN,
                title: "Open",
                command: FixtureCommand::Open,
                enablement: enabled,
            })
            .register_action(ActionSpec {
                id: DELETE,
                title: "Delete",
                command: FixtureCommand::Delete,
                enablement: writable,
            })
            .register_action(ActionSpec {
                id: DETAILS,
                title: "Details",
                command: FixtureCommand::Details,
                enablement: enabled,
            });
        builder
    }

    fn build_error<C, Command>(builder: ActionRegistryBuilder<C, Command>) -> ActionRegistryError {
        match builder.build() {
            Ok(_) => panic!("expected contribution graph to be rejected"),
            Err(error) => error,
        }
    }

    #[test]
    fn validates_qualified_id_grammar() {
        for valid in ["tool.action", "tool.item:open", "tool.item_open", "tool.item-open", "a.b"] {
            let mut builder = ActionRegistryBuilder::<FixtureContext, FixtureCommand>::new();
            builder.register_action(ActionSpec {
                id: ActionId::new(valid),
                title: "Valid",
                command: FixtureCommand::Open,
                enablement: enabled,
            });
            assert!(builder.build().is_ok(), "expected valid ID {valid:?}");
        }

        let too_long = "a.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        for invalid in ["", "plain", ".starts", "ends.", "bad space.id", "bad/id", too_long] {
            let mut builder = ActionRegistryBuilder::<FixtureContext, FixtureCommand>::new();
            builder.register_action(ActionSpec {
                id: ActionId::new(invalid),
                title: "Invalid",
                command: FixtureCommand::Open,
                enablement: enabled,
            });
            assert_eq!(
                build_error(builder),
                ActionRegistryError::InvalidActionId { id: invalid },
                "expected invalid ID {invalid:?}"
            );
        }
    }

    #[test]
    fn rejects_duplicate_dangling_and_ambiguous_contributions() {
        let mut duplicate = fixture_builder();
        duplicate.register_action(ActionSpec {
            id: OPEN,
            title: "Other",
            command: FixtureCommand::Open,
            enablement: enabled,
        });
        assert_eq!(build_error(duplicate), ActionRegistryError::DuplicateAction { id: OPEN });

        let missing = ActionId::new("fixture.item.missing");
        let mut dangling = fixture_builder();
        dangling.place_menu(MenuPlacement {
            menu: MENU,
            action: missing,
            group: "navigation",
            group_order: 10,
            order: 10,
            when: always,
        });
        assert_eq!(
            build_error(dangling),
            ActionRegistryError::UnknownAction { projection: "menu placement", action: missing }
        );

        let mut duplicate_menu = fixture_builder();
        for order in [10, 20] {
            duplicate_menu.place_menu(MenuPlacement {
                menu: MENU,
                action: OPEN,
                group: "navigation",
                group_order: 10,
                order,
                when: always,
            });
        }
        assert_eq!(
            build_error(duplicate_menu),
            ActionRegistryError::DuplicateMenuPlacement { menu: MENU, action: OPEN }
        );

        let chord = KeyChord::new(KeyCode::Char('X'), KeyModifiers::NONE);
        let mut duplicate_chord = fixture_builder();
        duplicate_chord
            .bind_key(KeybindingPlacement { chord, action: OPEN, when: always })
            .bind_key(KeybindingPlacement {
                chord: KeyChord::new(KeyCode::Char('x'), KeyModifiers::SHIFT),
                action: DELETE,
                when: always,
            });
        assert_eq!(
            build_error(duplicate_chord),
            ActionRegistryError::DuplicateKeyChord { chord, first: OPEN, second: DELETE }
        );
    }

    #[test]
    fn rejects_conflicting_group_orders_within_one_menu() {
        let mut conflicting = fixture_builder();
        conflicting
            .place_menu(MenuPlacement {
                menu: MENU,
                action: OPEN,
                group: "navigation",
                group_order: 10,
                order: 10,
                when: always,
            })
            .place_menu(MenuPlacement {
                menu: MENU,
                action: DETAILS,
                group: "navigation",
                group_order: 20,
                order: 20,
                when: always,
            });
        assert_eq!(
            build_error(conflicting),
            ActionRegistryError::ConflictingMenuGroupOrder {
                menu: MENU,
                group: "navigation",
                first: 10,
                second: 20,
            }
        );

        let secondary = MenuId::new("fixture.secondary.context");
        let mut independent = fixture_builder();
        independent
            .place_menu(MenuPlacement {
                menu: MENU,
                action: OPEN,
                group: "navigation",
                group_order: 10,
                order: 10,
                when: always,
            })
            .place_menu(MenuPlacement {
                menu: secondary,
                action: DETAILS,
                group: "navigation",
                group_order: 20,
                order: 10,
                when: always,
            });
        assert!(independent.build().is_ok());
    }

    #[test]
    fn resolves_menus_deterministically_with_visibility_enablement_and_hints() {
        let mut builder = fixture_builder();
        builder
            .place_menu(MenuPlacement {
                menu: MENU,
                action: DELETE,
                group: "destructive",
                group_order: 20,
                order: 10,
                when: visible,
            })
            .place_menu(MenuPlacement {
                menu: MENU,
                action: DETAILS,
                group: "navigation",
                group_order: 10,
                order: 20,
                when: always,
            })
            .place_menu(MenuPlacement {
                menu: MENU,
                action: OPEN,
                group: "navigation",
                group_order: 10,
                order: 10,
                when: always,
            })
            .bind_key(KeybindingPlacement {
                chord: KeyChord::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
                action: OPEN,
                when: always,
            });
        let registry = builder.build().unwrap();
        let context = FixtureContext { visible: true, writable: false };
        let menu = registry.resolve_menu(MENU, &context);

        assert_eq!(
            menu.items().iter().map(|item| item.id).collect::<Vec<_>>(),
            [OPEN, DETAILS, DELETE]
        );
        assert_eq!(menu.items()[0].primary_keybinding().unwrap().to_string(), "Ctrl+o");
        assert_eq!(
            menu.items()[2].state,
            ActionState::Disabled { reason: Cow::Borrowed("read only") }
        );
        assert_eq!(
            registry.command_for(&ActionInvocation::new(DELETE, context)),
            Err(ActionUnavailable::Disabled { action: DELETE, reason: Cow::Borrowed("read only") })
        );

        let hidden =
            registry.resolve_menu(MENU, &FixtureContext { visible: false, writable: true });
        assert_eq!(hidden.items().iter().map(|item| item.id).collect::<Vec<_>>(), [OPEN, DETAILS]);
    }

    #[test]
    fn build_pre_sorts_placements_and_resolution_preserves_that_order() {
        let alpha = ActionId::new("fixture.tie.alpha");
        let beta = ActionId::new("fixture.tie.beta");
        let epsilon = ActionId::new("fixture.tie.epsilon");
        let gamma = ActionId::new("fixture.tie.gamma");
        let delta = ActionId::new("fixture.tie.delta");
        let menu = MenuId::new("fixture.tie.context");
        let secondary = MenuId::new("fixture.tie.secondary");
        let mut builder = ActionRegistryBuilder::new();
        for id in [delta, gamma, epsilon, beta, alpha] {
            builder.register_action(ActionSpec {
                id,
                title: "Tie",
                command: FixtureCommand::Open,
                enablement: enabled,
            });
        }
        for (target_menu, action, group, group_order, order) in [
            (secondary, beta, "navigation", 5, 10),
            (menu, delta, "same", 20, 20),
            (menu, gamma, "same", 20, 10),
            (menu, beta, "z-group", 10, 10),
            (menu, epsilon, "same", 20, 10),
            (menu, alpha, "a-group", 10, 10),
        ] {
            builder.place_menu(MenuPlacement {
                menu: target_menu,
                action,
                group,
                group_order,
                order,
                when: always,
            });
        }

        let registry = builder.build().unwrap();
        assert_eq!(
            registry
                .menu_placements
                .iter()
                .map(|placement| {
                    (
                        placement.menu,
                        placement.group_order,
                        placement.group,
                        placement.order,
                        placement.action,
                    )
                })
                .collect::<Vec<_>>(),
            [
                (menu, 10, "a-group", 10, alpha),
                (menu, 10, "z-group", 10, beta),
                (menu, 20, "same", 10, epsilon),
                (menu, 20, "same", 10, gamma),
                (menu, 20, "same", 20, delta),
                (secondary, 5, "navigation", 10, beta),
            ]
        );

        let resolved =
            registry.resolve_menu(menu, &FixtureContext { visible: true, writable: true });
        assert_eq!(
            resolved.items().iter().map(|item| item.id).collect::<Vec<_>>(),
            [alpha, beta, epsilon, gamma, delta]
        );
    }

    #[test]
    fn key_chord_normalizes_case_modifiers_and_event_kind() {
        let lower = KeyChord::from_event(KeyEvent::new_with_kind(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
            KeyEventKind::Press,
        ))
        .unwrap();
        assert_eq!(lower.code(), KeyCode::Char('x'));
        assert_eq!(
            lower.modifiers(),
            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER
        );

        let upper = KeyChord::from_event(KeyEvent::new_with_kind(
            KeyCode::Char('x'),
            KeyModifiers::SHIFT,
            KeyEventKind::Repeat,
        ))
        .unwrap();
        assert_eq!(upper, KeyChord::new(KeyCode::Char('X'), KeyModifiers::NONE));
        assert_eq!(upper.to_string(), "X");
        assert_eq!(
            KeyChord::from_event(KeyEvent::new_with_kind(
                KeyCode::Char('X'),
                KeyModifiers::SHIFT,
                KeyEventKind::Press,
            )),
            Some(upper)
        );
        assert_eq!(
            KeyChord::from_event(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE)),
            Some(KeyChord::new(KeyCode::Delete, KeyModifiers::NONE))
        );
        assert_eq!(
            KeyChord::from_event(KeyEvent::new(KeyCode::Char('é'), KeyModifiers::SHIFT)),
            Some(KeyChord::new(KeyCode::Char('é'), KeyModifiers::SHIFT))
        );
        assert!(KeyChord::from_event(KeyEvent::new_with_kind(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ))
        .is_none());
    }

    #[test]
    fn registry_is_generic_over_an_unrelated_domain_shape() {
        #[derive(Clone, Debug, Eq, PartialEq)]
        enum DocumentCommand {
            Format,
        }

        struct DocumentContext {
            language: &'static str,
        }

        fn rust_document(context: &DocumentContext) -> bool {
            context.language == "rust"
        }

        fn available(_: &DocumentContext) -> ActionState {
            ActionState::Enabled
        }

        let action = ActionId::new("document.source.format");
        let menu = MenuId::new("document.editor.context");
        let mut builder = ActionRegistryBuilder::new();
        builder
            .register_action(ActionSpec {
                id: action,
                title: "Format document",
                command: DocumentCommand::Format,
                enablement: available,
            })
            .place_menu(MenuPlacement {
                menu,
                action,
                group: "source",
                group_order: 10,
                order: 10,
                when: rust_document,
            });
        let registry = builder.build().unwrap();
        let invocation = ActionInvocation::new(action, DocumentContext { language: "rust" });

        assert_eq!(registry.resolve_menu(menu, &invocation.context).len(), 1);
        assert_eq!(registry.command_for(&invocation), Ok(DocumentCommand::Format));
    }
}
