use crossterm::event::{KeyCode, KeyModifiers};

use crate::tui::{
    ActionId, ActionRegistry, ActionRegistryBuilder, ActionRegistryError, ActionSpec, ActionState,
    CommandPalettePlacement, KeyChord, KeybindingPlacement,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ScoutCommand {
    Quit,
    Refresh,
    Previous,
    Next,
    Toggle,
    OpenCommandPalette,
}

pub(super) type ScoutActionRegistry = ActionRegistry<(), ScoutCommand>;

pub(super) fn registry() -> Result<ScoutActionRegistry, ActionRegistryError> {
    let mut builder = ActionRegistryBuilder::new();
    for (order, (id, title, command)) in [
        ("scout.quit", "Quit", ScoutCommand::Quit),
        ("scout.refresh", "Refresh", ScoutCommand::Refresh),
        ("scout.previous", "Move up", ScoutCommand::Previous),
        ("scout.next", "Move down", ScoutCommand::Next),
        ("scout.toggle", "Expand or collapse", ScoutCommand::Toggle),
        ("scout.commandPalette.open", "Show command palette", ScoutCommand::OpenCommandPalette),
    ]
    .into_iter()
    .enumerate()
    {
        builder.register_action(ActionSpec {
            id: ActionId::new(id),
            title,
            command,
            enablement: enabled,
            command_palette: match command {
                ScoutCommand::OpenCommandPalette => CommandPalettePlacement::Hidden,
                _ => CommandPalettePlacement::Visible {
                    group: "Scout",
                    group_order: 10,
                    order: order as i16,
                },
            },
        });
    }
    for (code, modifiers, action) in [
        (KeyCode::Char('q'), KeyModifiers::NONE, "scout.quit"),
        (KeyCode::Esc, KeyModifiers::NONE, "scout.quit"),
        (KeyCode::Char('c'), KeyModifiers::CONTROL, "scout.quit"),
        (KeyCode::Char('r'), KeyModifiers::NONE, "scout.refresh"),
        (KeyCode::Up, KeyModifiers::NONE, "scout.previous"),
        (KeyCode::Char('k'), KeyModifiers::NONE, "scout.previous"),
        (KeyCode::Down, KeyModifiers::NONE, "scout.next"),
        (KeyCode::Char('j'), KeyModifiers::NONE, "scout.next"),
        (KeyCode::Enter, KeyModifiers::NONE, "scout.toggle"),
        (KeyCode::Char(' '), KeyModifiers::NONE, "scout.toggle"),
        (KeyCode::Char('p'), KeyModifiers::CONTROL, "scout.commandPalette.open"),
    ] {
        builder.bind_key(KeybindingPlacement {
            binding: KeyChord::new(code, modifiers).into(),
            action: ActionId::new(action),
            when: always,
        });
    }
    builder.build()
}

fn enabled(_: &()) -> ActionState {
    ActionState::Enabled
}

fn always(_: &()) -> bool {
    true
}
