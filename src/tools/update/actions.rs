use crossterm::event::{KeyCode, KeyModifiers};

use crate::tui::{
    ActionId, ActionRegistry, ActionRegistryBuilder, ActionRegistryError, ActionSpec, ActionState,
    CommandPalettePlacement, KeyChord, KeybindingPlacement, MenuId, MenuPlacement,
};

pub(super) const CHOICES: MenuId = MenuId::new("update.prompt.choices");
const CHOOSE_NOW: ActionId = ActionId::new("update.prompt.chooseNow");
const CHOOSE_LATER: ActionId = ActionId::new("update.prompt.chooseLater");
const CHOOSE_SKIP: ActionId = ActionId::new("update.prompt.chooseSkip");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PromptActionContext {
    pub selected: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PromptCommand {
    MoveUp,
    MoveDown,
    ChooseNow,
    ChooseLater,
    ChooseSkip,
    Activate,
    Dismiss,
}

pub(super) type PromptActionRegistry = ActionRegistry<PromptActionContext, PromptCommand>;

pub(super) fn registry() -> Result<PromptActionRegistry, ActionRegistryError> {
    let mut builder = ActionRegistryBuilder::new();
    for (id, title, command, visible, order) in [
        (ActionId::new("update.prompt.moveUp"), "Move up", PromptCommand::MoveUp, true, 10),
        (ActionId::new("update.prompt.moveDown"), "Move down", PromptCommand::MoveDown, true, 20),
        (CHOOSE_NOW, "Update now", PromptCommand::ChooseNow, false, 30),
        (CHOOSE_LATER, "Later", PromptCommand::ChooseLater, false, 40),
        (CHOOSE_SKIP, "Skip this version", PromptCommand::ChooseSkip, false, 50),
        (ActionId::new("update.prompt.activate"), "Confirm", PromptCommand::Activate, true, 60),
        (ActionId::new("update.prompt.dismiss"), "Later", PromptCommand::Dismiss, true, 70),
    ] {
        builder.register_action(ActionSpec {
            id,
            title,
            command,
            enablement: enabled,
            command_palette: if visible {
                CommandPalettePlacement::Visible { group: "Update", group_order: 10, order }
            } else {
                CommandPalettePlacement::Hidden
            },
        });
    }
    for (action, order) in [(CHOOSE_NOW, 10), (CHOOSE_LATER, 20), (CHOOSE_SKIP, 30)] {
        builder.place_menu(MenuPlacement {
            menu: CHOICES,
            action,
            group: "choices",
            group_order: 10,
            order,
            when: always,
        });
    }
    for (code, modifiers, action) in [
        (KeyCode::Up, KeyModifiers::NONE, "update.prompt.moveUp"),
        (KeyCode::Char('k'), KeyModifiers::NONE, "update.prompt.moveUp"),
        (KeyCode::Down, KeyModifiers::NONE, "update.prompt.moveDown"),
        (KeyCode::Char('j'), KeyModifiers::NONE, "update.prompt.moveDown"),
        (KeyCode::Char('1'), KeyModifiers::NONE, "update.prompt.chooseNow"),
        (KeyCode::Char('2'), KeyModifiers::NONE, "update.prompt.chooseLater"),
        (KeyCode::Char('3'), KeyModifiers::NONE, "update.prompt.chooseSkip"),
        (KeyCode::Enter, KeyModifiers::NONE, "update.prompt.activate"),
        (KeyCode::Esc, KeyModifiers::NONE, "update.prompt.dismiss"),
        (KeyCode::Char('c'), KeyModifiers::CONTROL, "update.prompt.dismiss"),
        (KeyCode::Char('d'), KeyModifiers::CONTROL, "update.prompt.dismiss"),
    ] {
        builder.bind_key(KeybindingPlacement {
            binding: KeyChord::new(code, modifiers).into(),
            action: ActionId::new(action),
            when: always,
        });
    }
    builder.build()
}

fn enabled(_: &PromptActionContext) -> ActionState {
    ActionState::Enabled
}

fn always(_: &PromptActionContext) -> bool {
    true
}
