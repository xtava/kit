use crossterm::event::{KeyCode, KeyModifiers};

use crate::tui::{
    ActionId, ActionRegistry, ActionRegistryBuilder, ActionRegistryError, ActionSpec, ActionState,
    CommandPalettePlacement, KeyChord, KeybindingPlacement, MenuId, MenuPlacement,
};

use super::controller::DashboardStatus;
use super::model::SlotPhase;

pub(super) const DASHBOARD_ACTIONS: MenuId = MenuId::new("stream.dashboard.actions");

const RECALL: ActionId = ActionId::new("stream.slot.recall");
const RECOVER: ActionId = ActionId::new("stream.slot.recover");
const INSTALL_SHORTCUT: ActionId = ActionId::new("stream.shortcut.install");
const REFRESH: ActionId = ActionId::new("stream.refresh");
const OPEN_COMMAND_PALETTE: ActionId = ActionId::new("stream.commandPalette.open");
const QUIT: ActionId = ActionId::new("stream.quit");

#[derive(Clone)]
pub(super) struct StreamActionContext {
    pub status: DashboardStatus,
    pub busy: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StreamAction {
    Recall,
    Recover,
    InstallShortcut,
    Refresh,
    OpenCommandPalette,
    Quit,
}

impl StreamAction {
    const ALL: [Self; 6] = [
        Self::Recall,
        Self::Recover,
        Self::InstallShortcut,
        Self::Refresh,
        Self::OpenCommandPalette,
        Self::Quit,
    ];

    const fn id(self) -> ActionId {
        match self {
            Self::Recall => RECALL,
            Self::Recover => RECOVER,
            Self::InstallShortcut => INSTALL_SHORTCUT,
            Self::Refresh => REFRESH,
            Self::OpenCommandPalette => OPEN_COMMAND_PALETTE,
            Self::Quit => QUIT,
        }
    }

    const fn title(self) -> &'static str {
        match self {
            Self::Recall => "Recall streamed window",
            Self::Recover => "Recover interrupted Stream Slot",
            Self::InstallShortcut => "Install Cmd+Shift+M shortcut",
            Self::Refresh => "Refresh status",
            Self::OpenCommandPalette => "Show command palette",
            Self::Quit => "Quit",
        }
    }

    fn chord(self) -> KeyChord {
        match self {
            Self::Recall => KeyChord::new(KeyCode::Char('s'), KeyModifiers::NONE),
            Self::Recover => KeyChord::new(KeyCode::Char('e'), KeyModifiers::NONE),
            Self::InstallShortcut => KeyChord::new(KeyCode::Char('i'), KeyModifiers::NONE),
            Self::Refresh => KeyChord::new(KeyCode::Char('r'), KeyModifiers::NONE),
            Self::OpenCommandPalette => KeyChord::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            Self::Quit => KeyChord::new(KeyCode::Char('q'), KeyModifiers::NONE),
        }
    }
}

pub(super) type StreamActionRegistry = ActionRegistry<StreamActionContext, StreamAction>;

pub(super) fn registry() -> Result<StreamActionRegistry, ActionRegistryError> {
    let mut builder = ActionRegistryBuilder::new();
    for action in StreamAction::ALL {
        let enablement = match action {
            StreamAction::Recall => recallable,
            StreamAction::Refresh | StreamAction::Quit => idle,
            StreamAction::Recover => recoverable,
            StreamAction::InstallShortcut => shortcut_missing,
            StreamAction::OpenCommandPalette => always_enabled,
        };
        let (group, group_order, order) = match action {
            StreamAction::Recall => ("Stream Slot", 10, 10),
            StreamAction::Recover => ("Stream Slot", 10, 20),
            StreamAction::InstallShortcut => ("Setup", 20, 10),
            StreamAction::Refresh => ("Dashboard", 30, 10),
            StreamAction::OpenCommandPalette => ("Dashboard", 30, 20),
            StreamAction::Quit => ("Dashboard", 30, 30),
        };
        builder.register_action(ActionSpec {
            id: action.id(),
            title: action.title(),
            command: action,
            enablement,
            command_palette: CommandPalettePlacement::Visible { group, group_order, order },
        });
        builder.bind_key(KeybindingPlacement {
            action: action.id(),
            binding: action.chord().into(),
            when: always,
        });
    }
    for (action, group, group_order, order) in [
        (StreamAction::Recall, "slot", 10, 10),
        (StreamAction::Recover, "slot", 10, 20),
        (StreamAction::InstallShortcut, "setup", 20, 10),
        (StreamAction::Refresh, "dashboard", 30, 10),
    ] {
        builder.place_menu(MenuPlacement {
            menu: DASHBOARD_ACTIONS,
            action: action.id(),
            group,
            group_order,
            order,
            when: always,
        });
    }
    builder.build()
}

fn always(_: &StreamActionContext) -> bool {
    true
}

fn always_enabled(_: &StreamActionContext) -> ActionState {
    ActionState::Enabled
}

fn idle(context: &StreamActionContext) -> ActionState {
    if context.busy {
        ActionState::disabled("another Stream operation is running")
    } else {
        ActionState::Enabled
    }
}

fn recallable(context: &StreamActionContext) -> ActionState {
    if context.busy {
        ActionState::disabled("another Stream operation is running")
    } else if context.status.slot.phase != Some(SlotPhase::Active) {
        ActionState::disabled("the Stream Slot is empty")
    } else {
        ActionState::Enabled
    }
}

fn recoverable(context: &StreamActionContext) -> ActionState {
    if context.busy {
        ActionState::disabled("another Stream operation is running")
    } else if !matches!(
        context.status.slot.phase,
        Some(SlotPhase::Preparing | SlotPhase::Restoring)
    ) {
        ActionState::disabled("there is no interrupted Stream Slot")
    } else {
        ActionState::Enabled
    }
}

fn shortcut_missing(context: &StreamActionContext) -> ActionState {
    if context.busy {
        ActionState::disabled("another Stream operation is running")
    } else if context.status.shortcut_installed {
        ActionState::disabled("Cmd+Shift+M is already installed")
    } else {
        ActionState::Enabled
    }
}
