//! Immutable, typed action contribution mechanics for interactive tools.
//!
//! Tools own action vocabulary and execution. This module validates stable identities and projects
//! caller-owned action data into menus and keybindings without storing handlers or mutable state.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
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
    pub binding: Keybinding,
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
        let mut display_modifiers = self.modifiers;
        if self.modifiers != KeyModifiers::NONE
            && matches!(self.code, KeyCode::Char(character) if character.is_ascii_uppercase())
        {
            display_modifiers.insert(KeyModifiers::SHIFT);
        }
        for (modifier, label) in [
            (KeyModifiers::CONTROL, "Ctrl"),
            (KeyModifiers::ALT, "Alt"),
            (KeyModifiers::SUPER, "Super"),
            (KeyModifiers::HYPER, "Hyper"),
            (KeyModifiers::META, "Meta"),
            (KeyModifiers::SHIFT, "Shift"),
        ] {
            if display_modifiers.contains(modifier) {
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
            KeyCode::Char(character)
                if self.modifiers != KeyModifiers::NONE && character.is_ascii_alphabetic() =>
            {
                write!(formatter, "{}", character.to_ascii_uppercase())
            }
            KeyCode::Char(character) => write!(formatter, "{character}"),
            KeyCode::F(number) => write!(formatter, "F{number}"),
            other => write!(formatter, "{other:?}"),
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("invalid key chord '{value}': {reason}")]
pub struct KeyChordParseError {
    value: String,
    reason: &'static str,
}

impl FromStr for KeyChord {
    type Err = KeyChordParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = |reason| KeyChordParseError { value: value.to_owned(), reason };
        let mut parts = value.split('+').peekable();
        let mut modifiers = KeyModifiers::NONE;
        let mut explicit_shift = false;
        let mut key = None;

        while let Some(part) = parts.next() {
            let part = part.trim();
            if part.is_empty() {
                return Err(invalid("empty key component"));
            }
            if parts.peek().is_some() {
                let modifier = match part.to_ascii_lowercase().as_str() {
                    "ctrl" | "control" => KeyModifiers::CONTROL,
                    "alt" => KeyModifiers::ALT,
                    "shift" => {
                        explicit_shift = true;
                        KeyModifiers::SHIFT
                    }
                    "super" => KeyModifiers::SUPER,
                    "hyper" => KeyModifiers::HYPER,
                    "meta" => KeyModifiers::META,
                    _ => return Err(invalid("unknown modifier")),
                };
                modifiers.insert(modifier);
                continue;
            }
            let mut code = parse_key_code(part).ok_or_else(|| invalid("unknown key"))?;
            if !explicit_shift && modifiers != KeyModifiers::NONE {
                if let KeyCode::Char(character) = code {
                    if character.is_ascii_uppercase() {
                        code = KeyCode::Char(character.to_ascii_lowercase());
                    }
                }
            }
            key = Some(code);
        }

        key.map(|code| Self::new(code, modifiers)).ok_or_else(|| invalid("missing key"))
    }
}

impl Serialize for KeyChord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for KeyChord {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?.parse().map_err(de::Error::custom)
    }
}

fn parse_key_code(value: &str) -> Option<KeyCode> {
    let lower = value.to_ascii_lowercase();
    let named = match lower.as_str() {
        "backspace" => Some(KeyCode::Backspace),
        "enter" | "return" => Some(KeyCode::Enter),
        "left" => Some(KeyCode::Left),
        "right" => Some(KeyCode::Right),
        "up" => Some(KeyCode::Up),
        "down" => Some(KeyCode::Down),
        "home" => Some(KeyCode::Home),
        "end" => Some(KeyCode::End),
        "pageup" | "page_up" => Some(KeyCode::PageUp),
        "pagedown" | "page_down" => Some(KeyCode::PageDown),
        "tab" => Some(KeyCode::Tab),
        "backtab" | "back_tab" => Some(KeyCode::BackTab),
        "delete" | "del" => Some(KeyCode::Delete),
        "insert" | "ins" => Some(KeyCode::Insert),
        "escape" | "esc" => Some(KeyCode::Esc),
        "space" => Some(KeyCode::Char(' ')),
        "plus" => Some(KeyCode::Char('+')),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    if let Some(number) = lower.strip_prefix('f').and_then(|value| value.parse::<u8>().ok()) {
        return (1..=24).contains(&number).then_some(KeyCode::F(number));
    }
    let mut characters = value.chars();
    let character = characters.next()?;
    characters.next().is_none().then_some(KeyCode::Char(character))
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

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Keybinding {
    Chord(KeyChord),
    Sequence { prefix: KeyChord, chord: KeyChord },
}

impl Keybinding {
    pub const fn chord(chord: KeyChord) -> Self {
        Self::Chord(chord)
    }

    pub const fn sequence(prefix: KeyChord, chord: KeyChord) -> Self {
        Self::Sequence { prefix, chord }
    }

    pub const fn direct_chord(self) -> Option<KeyChord> {
        match self {
            Self::Chord(chord) => Some(chord),
            Self::Sequence { .. } => None,
        }
    }
}

impl From<KeyChord> for Keybinding {
    fn from(chord: KeyChord) -> Self {
        Self::Chord(chord)
    }
}

impl fmt::Display for Keybinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Chord(chord) => chord.fmt(formatter),
            Self::Sequence { prefix, chord } => write!(formatter, "{prefix} {chord}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KeybindingState {
    pending_prefix: Option<KeyChord>,
}

impl KeybindingState {
    pub fn cancel(&mut self) {
        self.pending_prefix = None;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeybindingResolution<C> {
    Unmatched,
    Pending,
    UnmatchedSequence { prefix: KeyChord, chord: KeyChord },
    Invoke(ActionInvocation<C>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedAction {
    pub id: ActionId,
    pub title: &'static str,
    pub group: &'static str,
    pub state: ActionState,
    pub keybindings: Vec<Keybinding>,
}

impl ResolvedAction {
    pub fn primary_keybinding(&self) -> Option<Keybinding> {
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
    #[error("keybinding {binding} is bound to both {first} and {second}")]
    DuplicateKeybinding { binding: Keybinding, first: ActionId, second: ActionId },
    #[error("key chord {chord} is both a direct binding and a sequence prefix")]
    AmbiguousSequencePrefix { chord: KeyChord },
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

        let mut binding_actions = HashMap::with_capacity(self.keybindings.len());
        let mut direct_chords = HashSet::new();
        let mut prefixes = HashSet::new();
        for placement in &self.keybindings {
            if !action_indices.contains_key(&placement.action) {
                return Err(ActionRegistryError::UnknownAction {
                    projection: "keybinding",
                    action: placement.action,
                });
            }
            if let Some(first) = binding_actions.insert(placement.binding, placement.action) {
                return Err(ActionRegistryError::DuplicateKeybinding {
                    binding: placement.binding,
                    first,
                    second: placement.action,
                });
            }
            match placement.binding {
                Keybinding::Chord(chord) => {
                    direct_chords.insert(chord);
                }
                Keybinding::Sequence { prefix, .. } => {
                    prefixes.insert(prefix);
                }
            }
        }
        if let Some(chord) = direct_chords.intersection(&prefixes).next().copied() {
            return Err(ActionRegistryError::AmbiguousSequencePrefix { chord });
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
                        .map(|binding| binding.binding)
                        .collect(),
                }
            })
            .collect::<Vec<_>>();
        ResolvedMenu { items }
    }

    pub fn resolve_keybinding(
        &self,
        state: &mut KeybindingState,
        chord: KeyChord,
        context: C,
    ) -> KeybindingResolution<C> {
        if let Some(prefix) = state.pending_prefix.take() {
            let action = self.keybindings.iter().find_map(|binding| {
                (binding.binding == Keybinding::Sequence { prefix, chord }
                    && (binding.when)(&context))
                .then_some(binding.action)
            });
            return action
                .map_or(KeybindingResolution::UnmatchedSequence { prefix, chord }, |action| {
                    KeybindingResolution::Invoke(ActionInvocation::new(action, context))
                });
        }
        if let Some(action) = self.keybindings.iter().find_map(|binding| {
            (binding.binding == Keybinding::Chord(chord) && (binding.when)(&context))
                .then_some(binding.action)
        }) {
            return KeybindingResolution::Invoke(ActionInvocation::new(action, context));
        }
        if self.keybindings.iter().any(|binding| {
            matches!(binding.binding, Keybinding::Sequence { prefix, .. } if prefix == chord)
                && (binding.when)(&context)
        }) {
            state.pending_prefix = Some(chord);
            return KeybindingResolution::Pending;
        }
        KeybindingResolution::Unmatched
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
    fn key_chords_round_trip_through_the_configuration_syntax() {
        for chord in [
            KeyChord::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            KeyChord::new(KeyCode::Char('B'), KeyModifiers::CONTROL),
            KeyChord::new(KeyCode::Char('?'), KeyModifiers::NONE),
            KeyChord::new(KeyCode::Left, KeyModifiers::CONTROL | KeyModifiers::SHIFT),
            KeyChord::new(KeyCode::F(12), KeyModifiers::ALT),
            KeyChord::new(KeyCode::Char('+'), KeyModifiers::NONE),
        ] {
            let encoded = if chord.code() == KeyCode::Char('+') {
                "Plus".to_owned()
            } else {
                chord.to_string()
            };
            assert_eq!(encoded.parse(), Ok(chord));
        }
    }

    #[test]
    fn key_chord_parser_rejects_unknown_or_incomplete_chords() {
        for invalid in ["", "Ctrl", "Ctrl+", "Banana+N", "F25"] {
            assert!(invalid.parse::<KeyChord>().is_err(), "{invalid:?} should be rejected");
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
            .bind_key(KeybindingPlacement { binding: chord.into(), action: OPEN, when: always })
            .bind_key(KeybindingPlacement {
                binding: KeyChord::new(KeyCode::Char('x'), KeyModifiers::SHIFT).into(),
                action: DELETE,
                when: always,
            });
        assert_eq!(
            build_error(duplicate_chord),
            ActionRegistryError::DuplicateKeybinding {
                binding: chord.into(),
                first: OPEN,
                second: DELETE
            }
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
                binding: KeyChord::new(KeyCode::Char('o'), KeyModifiers::CONTROL).into(),
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
        assert_eq!(menu.items()[0].primary_keybinding().unwrap().to_string(), "Ctrl+O");
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
    fn one_resolver_handles_direct_and_prefixed_bindings() {
        let prefix = KeyChord::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        let suffix = KeyChord::new(KeyCode::Char('n'), KeyModifiers::NONE);
        let direct = KeyChord::new(KeyCode::Enter, KeyModifiers::NONE);
        let mut builder = fixture_builder();
        builder
            .bind_key(KeybindingPlacement {
                binding: Keybinding::sequence(prefix, suffix),
                action: OPEN,
                when: always,
            })
            .bind_key(KeybindingPlacement {
                binding: direct.into(),
                action: DETAILS,
                when: always,
            });
        let registry = builder.build().unwrap();
        let context = FixtureContext { visible: true, writable: true };
        let mut state = KeybindingState::default();

        assert!(matches!(
            registry.resolve_keybinding(&mut state, prefix, context),
            KeybindingResolution::Pending
        ));
        let KeybindingResolution::Invoke(invocation) =
            registry.resolve_keybinding(&mut state, suffix, context)
        else {
            panic!("prefix suffix must resolve");
        };
        assert_eq!(registry.command_for(&invocation), Ok(FixtureCommand::Open));

        let KeybindingResolution::Invoke(invocation) =
            registry.resolve_keybinding(&mut state, direct, context)
        else {
            panic!("direct chord must resolve");
        };
        assert_eq!(registry.command_for(&invocation), Ok(FixtureCommand::Details));
    }

    #[test]
    fn unmatched_sequences_reset_the_shared_prefix_state() {
        let prefix = KeyChord::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        let suffix = KeyChord::new(KeyCode::Char('n'), KeyModifiers::NONE);
        let unknown = KeyChord::new(KeyCode::Char('x'), KeyModifiers::NONE);
        let mut builder = fixture_builder();
        builder.bind_key(KeybindingPlacement {
            binding: Keybinding::sequence(prefix, suffix),
            action: OPEN,
            when: always,
        });
        let registry = builder.build().unwrap();
        let context = FixtureContext { visible: true, writable: true };
        let mut state = KeybindingState::default();

        assert!(matches!(
            registry.resolve_keybinding(&mut state, prefix, context),
            KeybindingResolution::Pending
        ));
        assert!(matches!(
            registry.resolve_keybinding(&mut state, unknown, context),
            KeybindingResolution::UnmatchedSequence {
                prefix: actual_prefix,
                chord: actual_chord,
            } if actual_prefix == prefix && actual_chord == unknown
        ));
        assert!(matches!(
            registry.resolve_keybinding(&mut state, suffix, context),
            KeybindingResolution::Unmatched
        ));
    }

    #[test]
    fn rejects_a_direct_chord_that_is_also_a_sequence_prefix() {
        let prefix = KeyChord::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        let suffix = KeyChord::new(KeyCode::Char('n'), KeyModifiers::NONE);
        let mut builder = fixture_builder();
        builder
            .bind_key(KeybindingPlacement { binding: prefix.into(), action: DETAILS, when: always })
            .bind_key(KeybindingPlacement {
                binding: Keybinding::sequence(prefix, suffix),
                action: OPEN,
                when: always,
            });

        assert_eq!(
            build_error(builder),
            ActionRegistryError::AmbiguousSequencePrefix { chord: prefix }
        );
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
