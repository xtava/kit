use anyhow::Result;

use crate::tui::{
    ActionId, ActionRegistry, ActionRegistryBuilder, ActionSpec, ActionState,
    CommandPalettePlacement, MenuId, MenuPlacement,
};

use super::model::MachineAction;

pub(super) const MACHINE_CONTEXT_MENU: MenuId = MenuId::new("console.machine.context");

#[derive(Clone)]
pub(super) struct ControlCenterActionContext {
    pub(super) available_actions: Vec<MachineAction>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ControlCenterCommand {
    Machine(MachineAction),
    OpenSettings,
    Quit,
}

pub(super) fn control_center_actions(
) -> Result<ActionRegistry<ControlCenterActionContext, ControlCenterCommand>> {
    let mut builder = ActionRegistryBuilder::new();
    for (order, action) in MachineAction::ALL.into_iter().enumerate() {
        let contract = action.contract();
        builder.register_action(ActionSpec {
            id: action.id(),
            title: contract.title,
            command: ControlCenterCommand::Machine(action),
            enablement: enablement_for(action),
            command_palette: CommandPalettePlacement::Visible {
                group: "Machines",
                group_order: 10,
                order: order as i16,
            },
        });
        builder.place_menu(MenuPlacement {
            menu: MACHINE_CONTEXT_MENU,
            action: action.id(),
            group: "Machine",
            group_order: 10,
            order: order as i16,
            when: always,
        });
    }
    builder.register_action(ActionSpec {
        id: ActionId::new("console.settings.open"),
        title: "Open Console settings",
        command: ControlCenterCommand::OpenSettings,
        enablement: enabled,
        command_palette: CommandPalettePlacement::Visible {
            group: "Console",
            group_order: 20,
            order: 10,
        },
    });
    builder.register_action(ActionSpec {
        id: ActionId::new("console.quit"),
        title: "Quit Console",
        command: ControlCenterCommand::Quit,
        enablement: enabled,
        command_palette: CommandPalettePlacement::Visible {
            group: "Console",
            group_order: 20,
            order: 20,
        },
    });
    Ok(builder.build()?)
}

fn always(_: &ControlCenterActionContext) -> bool {
    true
}

fn enabled(_: &ControlCenterActionContext) -> ActionState {
    ActionState::Enabled
}

fn enablement_for(action: MachineAction) -> fn(&ControlCenterActionContext) -> ActionState {
    match action {
        MachineAction::Connect => connect_enabled,
        MachineAction::NewSession => new_session_enabled,
        MachineAction::Refresh => refresh_enabled,
        MachineAction::AuthenticateTailscale => authenticate_tailscale_enabled,
        MachineAction::StartConsole => start_console_enabled,
        MachineAction::SetupOrRepair => setup_or_repair_enabled,
        MachineAction::UpdateKit => update_kit_enabled,
        MachineAction::RestartService => restart_service_enabled,
        MachineAction::StopService => stop_service_enabled,
        MachineAction::ShowDetails => show_details_enabled,
        MachineAction::CancelOperation => cancel_operation_enabled,
    }
}

fn action_enabled(context: &ControlCenterActionContext, action: MachineAction) -> ActionState {
    if context.available_actions.contains(&action) {
        ActionState::Enabled
    } else {
        ActionState::disabled("not available for the selected machine")
    }
}

macro_rules! action_enablement {
    ($name:ident, $action:expr) => {
        fn $name(context: &ControlCenterActionContext) -> ActionState {
            action_enabled(context, $action)
        }
    };
}

action_enablement!(connect_enabled, MachineAction::Connect);
action_enablement!(new_session_enabled, MachineAction::NewSession);
action_enablement!(refresh_enabled, MachineAction::Refresh);
action_enablement!(authenticate_tailscale_enabled, MachineAction::AuthenticateTailscale);
action_enablement!(start_console_enabled, MachineAction::StartConsole);
action_enablement!(setup_or_repair_enabled, MachineAction::SetupOrRepair);
action_enablement!(update_kit_enabled, MachineAction::UpdateKit);
action_enablement!(restart_service_enabled, MachineAction::RestartService);
action_enablement!(stop_service_enabled, MachineAction::StopService);
action_enablement!(show_details_enabled, MachineAction::ShowDetails);
action_enablement!(cancel_operation_enabled, MachineAction::CancelOperation);
