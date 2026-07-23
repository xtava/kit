//! Declarative Stats action metadata and projection policy.

use crossterm::event::{KeyCode, KeyModifiers};

use super::app::InspectorTab;
use super::host::ProcessAction;
use super::model::{CapabilityState, HostCapabilities, ProcessIdentity};
use crate::tui::{
    ActionId, ActionRegistry, ActionRegistryBuilder, ActionRegistryError, ActionSpec, ActionState,
    KeyChord, Keybinding, KeybindingPlacement, MenuId, MenuPlacement,
};
#[cfg(test)]
use crate::tui::{KeybindingResolution, KeybindingState};

pub(super) const VIEW_COMMAND: ActionId = ActionId::new("stats.process.viewCommand");
pub(super) const OPEN_PROFILE: ActionId = ActionId::new("stats.process.openProfile");
pub(super) const TERMINATE: ActionId = ActionId::new("stats.process.terminate");
pub(super) const FORCE_TERMINATE: ActionId = ActionId::new("stats.process.forceTerminate");

pub(super) const PROCESS_CONTEXT_MENU: MenuId = MenuId::new("stats.process.context");
pub(super) const PROCESS_COMMAND_INLINE: MenuId = MenuId::new("stats.process.commandInline");
pub(super) const PROCESS_INSPECTOR_INLINE: MenuId = MenuId::new("stats.process.inspectorInline");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StatsCommand {
    ViewCommand,
    OpenProfile,
    RequestTerminate(ProcessAction),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct StatsActionContext {
    pub identity: ProcessIdentity,
    pub is_live: bool,
    pub inspector_tab: InspectorTab,
    pub host: HostCapabilities,
    pub action_running: bool,
}

pub(super) type StatsActionRegistry = ActionRegistry<StatsActionContext, StatsCommand>;

pub(super) fn registry() -> Result<StatsActionRegistry, ActionRegistryError> {
    let mut builder = ActionRegistryBuilder::new();
    contribute_actions(&mut builder);
    builder.build()
}

pub(super) fn contribute_actions(
    builder: &mut ActionRegistryBuilder<StatsActionContext, StatsCommand>,
) {
    builder
        .register_action(ActionSpec {
            id: VIEW_COMMAND,
            title: "View full command",
            command: StatsCommand::ViewCommand,
            enablement: live_process,
        })
        .register_action(ActionSpec {
            id: OPEN_PROFILE,
            title: "Profile",
            command: StatsCommand::OpenProfile,
            enablement: enabled,
        })
        .register_action(ActionSpec {
            id: TERMINATE,
            title: "End process…",
            command: StatsCommand::RequestTerminate(ProcessAction::GracefulTerminate),
            enablement: graceful_termination_available,
        })
        .register_action(ActionSpec {
            id: FORCE_TERMINATE,
            title: "Force end process…",
            command: StatsCommand::RequestTerminate(ProcessAction::ForceTerminate),
            enablement: force_termination_available,
        });

    for (action, group, group_order, order) in [
        (VIEW_COMMAND, "navigation", 10, 10),
        (OPEN_PROFILE, "navigation", 10, 20),
        (TERMINATE, "destructive", 20, 10),
        (FORCE_TERMINATE, "destructive", 20, 20),
    ] {
        builder.place_menu(MenuPlacement {
            menu: PROCESS_CONTEXT_MENU,
            action,
            group,
            group_order,
            order,
            when: always,
        });
    }

    builder
        .place_menu(MenuPlacement {
            menu: PROCESS_COMMAND_INLINE,
            action: VIEW_COMMAND,
            group: "navigation",
            group_order: 10,
            order: 10,
            when: overview,
        })
        .place_menu(MenuPlacement {
            menu: PROCESS_INSPECTOR_INLINE,
            action: OPEN_PROFILE,
            group: "navigation",
            group_order: 10,
            order: 10,
            when: always,
        })
        .place_menu(MenuPlacement {
            menu: PROCESS_INSPECTOR_INLINE,
            action: TERMINATE,
            group: "destructive",
            group_order: 20,
            order: 10,
            when: always,
        })
        .bind_key(KeybindingPlacement {
            binding: Keybinding::chord(KeyChord::new(KeyCode::Char('v'), KeyModifiers::NONE)),
            action: VIEW_COMMAND,
            when: overview,
        })
        .bind_key(KeybindingPlacement {
            binding: Keybinding::chord(KeyChord::new(KeyCode::Char('p'), KeyModifiers::NONE)),
            action: OPEN_PROFILE,
            when: always,
        })
        .bind_key(KeybindingPlacement {
            binding: Keybinding::chord(KeyChord::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            action: TERMINATE,
            when: always,
        })
        .bind_key(KeybindingPlacement {
            binding: Keybinding::chord(KeyChord::new(KeyCode::Delete, KeyModifiers::NONE)),
            action: TERMINATE,
            when: always,
        })
        .bind_key(KeybindingPlacement {
            binding: Keybinding::chord(KeyChord::new(KeyCode::Char('X'), KeyModifiers::NONE)),
            action: FORCE_TERMINATE,
            when: always,
        });
}

fn always(_: &StatsActionContext) -> bool {
    true
}

fn overview(context: &StatsActionContext) -> bool {
    context.inspector_tab == InspectorTab::Overview
}

fn enabled(_: &StatsActionContext) -> ActionState {
    ActionState::Enabled
}

fn live_process(context: &StatsActionContext) -> ActionState {
    if context.is_live {
        ActionState::Enabled
    } else {
        ActionState::disabled("process is no longer available")
    }
}

fn stable_live_process(context: &StatsActionContext) -> ActionState {
    if let state @ ActionState::Disabled { .. } = live_process(context) {
        return state;
    }
    if context.identity.stable_key().is_none() {
        return ActionState::disabled("process identity is snapshot-only");
    }
    ActionState::Enabled
}

fn capability_available(context: &StatsActionContext, capability: CapabilityState) -> ActionState {
    if let state @ ActionState::Disabled { .. } = stable_live_process(context) {
        return state;
    }
    if let Some(reason) = capability.reason() {
        return ActionState::disabled(reason);
    }
    ActionState::Enabled
}

fn process_action_available(
    context: &StatsActionContext,
    capability: CapabilityState,
) -> ActionState {
    if context.action_running {
        return ActionState::disabled("another process action is already running");
    }
    capability_available(context, capability)
}

fn graceful_termination_available(context: &StatsActionContext) -> ActionState {
    process_action_available(context, context.host.graceful_terminate)
}

fn force_termination_available(context: &StatsActionContext) -> ActionState {
    process_action_available(context, context.host.force_terminate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::stats::model::{IdentityUnavailable, ProcessKey};
    use crate::tui::{ActionInvocation, ActionUnavailable};

    fn available_host() -> HostCapabilities {
        HostCapabilities {
            last_observed_core: CapabilityState::Available,
            threads: CapabilityState::Available,
            resources: CapabilityState::Available,
            graceful_terminate: CapabilityState::Available,
            force_terminate: CapabilityState::Available,
            code_profile: CapabilityState::Available,
        }
    }

    fn fixture_context() -> StatsActionContext {
        StatsActionContext {
            identity: ProcessIdentity::stable(ProcessKey { pid: 42, start_token: 7 }),
            is_live: true,
            inspector_tab: InspectorTab::Overview,
            host: available_host(),
            action_running: false,
        }
    }

    #[test]
    fn catalog_matches_the_approved_projection_table() {
        let registry = registry().unwrap();
        let context = fixture_context();

        let process = registry.resolve_menu(PROCESS_CONTEXT_MENU, &context);
        assert_eq!(
            process.items().iter().map(|item| (item.id, item.group)).collect::<Vec<_>>(),
            [
                (VIEW_COMMAND, "navigation"),
                (OPEN_PROFILE, "navigation"),
                (TERMINATE, "destructive"),
                (FORCE_TERMINATE, "destructive"),
            ]
        );
        assert_eq!(
            process.items().iter().map(|item| item.title).collect::<Vec<_>>(),
            ["View full command", "Profile", "End process…", "Force end process…"]
        );
        assert_eq!(
            registry
                .resolve_menu(PROCESS_COMMAND_INLINE, &context)
                .items()
                .iter()
                .map(|item| (item.id, item.group))
                .collect::<Vec<_>>(),
            [(VIEW_COMMAND, "navigation")]
        );
        assert_eq!(
            registry
                .resolve_menu(PROCESS_INSPECTOR_INLINE, &context)
                .items()
                .iter()
                .map(|item| (item.id, item.group))
                .collect::<Vec<_>>(),
            [(OPEN_PROFILE, "navigation"), (TERMINATE, "destructive")]
        );

        for (code, action, command) in [
            (KeyCode::Char('v'), VIEW_COMMAND, StatsCommand::ViewCommand),
            (KeyCode::Char('p'), OPEN_PROFILE, StatsCommand::OpenProfile),
            (
                KeyCode::Char('x'),
                TERMINATE,
                StatsCommand::RequestTerminate(ProcessAction::GracefulTerminate),
            ),
            (
                KeyCode::Delete,
                TERMINATE,
                StatsCommand::RequestTerminate(ProcessAction::GracefulTerminate),
            ),
            (
                KeyCode::Char('X'),
                FORCE_TERMINATE,
                StatsCommand::RequestTerminate(ProcessAction::ForceTerminate),
            ),
        ] {
            let mut keybinding_state = KeybindingState::default();
            let invocation = registry.resolve_keybinding(
                &mut keybinding_state,
                KeyChord::new(code, KeyModifiers::NONE),
                context,
            );
            let KeybindingResolution::Invoke(invocation) = invocation else {
                panic!("approved chord must resolve");
            };
            assert_eq!(invocation.action, action);
            assert_eq!(registry.command_for(&invocation), Ok(command));
        }

        let mut non_overview = context;
        non_overview.inspector_tab = InspectorTab::Threads;
        assert!(registry.resolve_menu(PROCESS_COMMAND_INLINE, &non_overview).is_empty());
        let mut keybinding_state = KeybindingState::default();
        assert!(matches!(
            registry.resolve_keybinding(
                &mut keybinding_state,
                KeyChord::new(KeyCode::Char('v'), KeyModifiers::NONE),
                non_overview,
            ),
            KeybindingResolution::Unmatched
        ));
    }

    #[test]
    fn action_enablement_matrix_preserves_navigation_and_destructive_safety() {
        let registry = registry().unwrap();
        let mut context = fixture_context();

        context.identity = ProcessIdentity::SnapshotOnly {
            snapshot_sequence: 9,
            pid: 42,
            reason: IdentityUnavailable::PermissionDenied,
        };
        context.is_live = false;
        context.host.code_profile =
            CapabilityState::Unsupported { reason: "profiling unavailable" };
        context.action_running = true;
        assert_eq!(
            registry.command_for(&ActionInvocation::new(OPEN_PROFILE, context)),
            Ok(StatsCommand::OpenProfile),
            "Profile is navigation even when profiling and process actions are unavailable"
        );

        context = fixture_context();
        context.action_running = true;
        let invocation = ActionInvocation::new(TERMINATE, context);
        assert_eq!(
            registry.command_for(&invocation),
            Err(ActionUnavailable::Disabled {
                action: TERMINATE,
                reason: "another process action is already running".into(),
            })
        );

        context.action_running = false;
        context.is_live = false;
        let invocation = ActionInvocation::new(FORCE_TERMINATE, context);
        assert_eq!(
            registry.command_for(&invocation),
            Err(ActionUnavailable::Disabled {
                action: FORCE_TERMINATE,
                reason: "process is no longer available".into(),
            })
        );

        context.is_live = true;
        context.identity = ProcessIdentity::SnapshotOnly {
            snapshot_sequence: 10,
            pid: 42,
            reason: IdentityUnavailable::PermissionDenied,
        };
        assert_eq!(
            registry.command_for(&ActionInvocation::new(TERMINATE, context)),
            Err(ActionUnavailable::Disabled {
                action: TERMINATE,
                reason: "process identity is snapshot-only".into(),
            })
        );

        context = fixture_context();
        context.host.graceful_terminate =
            CapabilityState::Unsupported { reason: "graceful unavailable" };
        assert_eq!(
            registry.command_for(&ActionInvocation::new(TERMINATE, context)),
            Err(ActionUnavailable::Disabled {
                action: TERMINATE,
                reason: "graceful unavailable".into(),
            })
        );
        assert_eq!(
            registry.command_for(&ActionInvocation::new(FORCE_TERMINATE, context)),
            Ok(StatsCommand::RequestTerminate(ProcessAction::ForceTerminate))
        );

        context = fixture_context();
        context.host.force_terminate = CapabilityState::Unsupported { reason: "force unavailable" };
        assert_eq!(
            registry.command_for(&ActionInvocation::new(FORCE_TERMINATE, context)),
            Err(ActionUnavailable::Disabled {
                action: FORCE_TERMINATE,
                reason: "force unavailable".into(),
            })
        );
        assert_eq!(
            registry.command_for(&ActionInvocation::new(TERMINATE, context)),
            Ok(StatsCommand::RequestTerminate(ProcessAction::GracefulTerminate))
        );
    }
}
