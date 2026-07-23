use crossterm::event::{KeyCode, KeyModifiers};

use crate::tui::{
    ActionId, ActionRegistry, ActionRegistryBuilder, ActionRegistryError, ActionSpec, ActionState,
    CommandPalettePlacement, KeyChord, KeybindingPlacement, MenuId, MenuPlacement,
};

use super::model::MonitorView;

pub(super) const INSPECT: ActionId = ActionId::new("monitor.item.inspect");
pub(super) const REFRESH: ActionId = ActionId::new("monitor.scope.refresh");
pub(super) const OPEN_EXTERNAL: ActionId = ActionId::new("monitor.resource.openExternal");
pub(super) const OPEN_IN_DEPLOY: ActionId = ActionId::new("monitor.deployment.openInDeploy");
pub(super) const TOGGLE_FOLLOW: ActionId = ActionId::new("monitor.logs.toggleFollow");
pub(super) const OPEN_TRACE: ActionId = ActionId::new("monitor.correlation.openTrace");
pub(super) const OPEN_METRICS: ActionId = ActionId::new("monitor.correlation.openMetrics");
pub(super) const OPEN_DEPLOYMENT: ActionId = ActionId::new("monitor.correlation.openDeployment");

pub(super) const ITEM_CONTEXT: MenuId = ActionIdMenu::ITEM_CONTEXT;
pub(super) const ITEM_INLINE: MenuId = ActionIdMenu::ITEM_INLINE;
pub(super) const CORRELATION_INLINE: MenuId = ActionIdMenu::CORRELATION_INLINE;
pub(super) const SCOPE_INLINE: MenuId = ActionIdMenu::SCOPE_INLINE;

struct ActionIdMenu;

impl ActionIdMenu {
    const ITEM_CONTEXT: MenuId = MenuId::new("monitor.item.context");
    const ITEM_INLINE: MenuId = MenuId::new("monitor.item.inline");
    const CORRELATION_INLINE: MenuId = MenuId::new("monitor.logs.correlationInline");
    const SCOPE_INLINE: MenuId = MenuId::new("monitor.scope.inline");
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum MonitorActionTarget {
    Overview,
    Service(String),
    Metric { service_id: String, metric_id: String },
    LogEvent(String),
    Deployment(String),
    Cost(String),
    Source(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct MonitorActionContext {
    pub view: MonitorView,
    pub target: MonitorActionTarget,
    pub snapshot_generation: u64,
    pub inspectable: bool,
    pub refreshable: bool,
    pub external_open: ActionCapability,
    pub deploy_handoff: ActionCapability,
    pub follow: ActionCapability,
    pub trace_correlation: ActionCapability,
    pub metrics_correlation: ActionCapability,
    pub deployment_correlation: ActionCapability,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ActionCapability {
    Available,
    Unavailable(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MonitorCommand {
    Inspect,
    Refresh,
    OpenExternal,
    OpenInDeploy,
    ToggleLogFollow,
    OpenTraceCorrelation,
    OpenMetricsCorrelation,
    OpenDeploymentCorrelation,
}

pub(super) type MonitorActionRegistry = ActionRegistry<MonitorActionContext, MonitorCommand>;

pub(super) fn registry() -> Result<MonitorActionRegistry, ActionRegistryError> {
    let mut builder = ActionRegistryBuilder::new();
    contribute_actions(&mut builder);
    builder.build()
}

fn contribute_actions(builder: &mut ActionRegistryBuilder<MonitorActionContext, MonitorCommand>) {
    builder
        .register_action(ActionSpec {
            id: INSPECT,
            title: "Inspect",
            command: MonitorCommand::Inspect,
            enablement: inspectable,
            command_palette: CommandPalettePlacement::Visible {
                group: "Monitor",
                group_order: 10,
                order: 10,
            },
        })
        .register_action(ActionSpec {
            id: REFRESH,
            title: "Refresh",
            command: MonitorCommand::Refresh,
            enablement: refreshable,
            command_palette: CommandPalettePlacement::Visible {
                group: "Monitor",
                group_order: 10,
                order: 20,
            },
        })
        .register_action(ActionSpec {
            id: OPEN_EXTERNAL,
            title: "Open provider",
            command: MonitorCommand::OpenExternal,
            enablement: external_open,
            command_palette: CommandPalettePlacement::Visible {
                group: "Monitor",
                group_order: 10,
                order: 30,
            },
        })
        .register_action(ActionSpec {
            id: OPEN_IN_DEPLOY,
            title: "Open in kit deploy",
            command: MonitorCommand::OpenInDeploy,
            enablement: deploy_handoff,
            command_palette: CommandPalettePlacement::Visible {
                group: "Monitor",
                group_order: 10,
                order: 40,
            },
        })
        .register_action(ActionSpec {
            id: TOGGLE_FOLLOW,
            title: "Toggle log follow",
            command: MonitorCommand::ToggleLogFollow,
            enablement: follow,
            command_palette: CommandPalettePlacement::Visible {
                group: "Monitor",
                group_order: 10,
                order: 50,
            },
        })
        .register_action(ActionSpec {
            id: OPEN_TRACE,
            title: "Same trace",
            command: MonitorCommand::OpenTraceCorrelation,
            enablement: trace_correlation,
            command_palette: CommandPalettePlacement::Visible {
                group: "Correlation",
                group_order: 20,
                order: 10,
            },
        })
        .register_action(ActionSpec {
            id: OPEN_METRICS,
            title: "Nearby metrics",
            command: MonitorCommand::OpenMetricsCorrelation,
            enablement: metrics_correlation,
            command_palette: CommandPalettePlacement::Visible {
                group: "Correlation",
                group_order: 20,
                order: 20,
            },
        })
        .register_action(ActionSpec {
            id: OPEN_DEPLOYMENT,
            title: "Nearest deployment",
            command: MonitorCommand::OpenDeploymentCorrelation,
            enablement: deployment_correlation,
            command_palette: CommandPalettePlacement::Visible {
                group: "Correlation",
                group_order: 20,
                order: 30,
            },
        });

    for (action, group, group_order, order, visible) in [
        (INSPECT, "navigation", 10, 10, always as fn(&MonitorActionContext) -> bool),
        (OPEN_EXTERNAL, "navigation", 10, 20, has_external),
        (OPEN_IN_DEPLOY, "handoff", 20, 10, deployment_target),
        (OPEN_TRACE, "correlation", 30, 10, log_target),
        (OPEN_METRICS, "correlation", 30, 20, log_target),
        (OPEN_DEPLOYMENT, "correlation", 30, 30, log_target),
    ] {
        builder.place_menu(MenuPlacement {
            menu: ITEM_CONTEXT,
            action,
            group,
            group_order,
            order,
            when: visible,
        });
    }

    for (action, order, visible) in [
        (INSPECT, 10, always as fn(&MonitorActionContext) -> bool),
        (OPEN_EXTERNAL, 20, has_external),
        (OPEN_IN_DEPLOY, 30, deployment_target),
    ] {
        builder.place_menu(MenuPlacement {
            menu: ITEM_INLINE,
            action,
            group: "navigation",
            group_order: 10,
            order,
            when: visible,
        });
    }
    for (action, order) in [(OPEN_TRACE, 10), (OPEN_METRICS, 20), (OPEN_DEPLOYMENT, 30)] {
        builder.place_menu(MenuPlacement {
            menu: CORRELATION_INLINE,
            action,
            group: "correlation",
            group_order: 10,
            order,
            when: log_target,
        });
    }
    builder
        .place_menu(MenuPlacement {
            menu: SCOPE_INLINE,
            action: REFRESH,
            group: "scope",
            group_order: 10,
            order: 10,
            when: always,
        })
        .place_menu(MenuPlacement {
            menu: SCOPE_INLINE,
            action: TOGGLE_FOLLOW,
            group: "scope",
            group_order: 10,
            order: 20,
            when: logs_view,
        });

    for (code, action, visible) in [
        (KeyCode::Enter, INSPECT, always as fn(&MonitorActionContext) -> bool),
        (KeyCode::Char('r'), REFRESH, always),
        (KeyCode::Char('o'), OPEN_EXTERNAL, has_external),
        (KeyCode::Char('d'), OPEN_IN_DEPLOY, deployment_target),
        (KeyCode::Char('f'), TOGGLE_FOLLOW, logs_view),
    ] {
        builder.bind_key(KeybindingPlacement {
            binding: KeyChord::new(code, KeyModifiers::NONE).into(),
            action,
            when: visible,
        });
    }
}

fn always(_: &MonitorActionContext) -> bool {
    true
}

fn logs_view(context: &MonitorActionContext) -> bool {
    context.view == MonitorView::Logs
}

fn log_target(context: &MonitorActionContext) -> bool {
    matches!(context.target, MonitorActionTarget::LogEvent(_))
}

fn deployment_target(context: &MonitorActionContext) -> bool {
    matches!(context.target, MonitorActionTarget::Deployment(_))
}

fn has_external(context: &MonitorActionContext) -> bool {
    matches!(
        context.target,
        MonitorActionTarget::Service(_)
            | MonitorActionTarget::Deployment(_)
            | MonitorActionTarget::Source(_)
    )
}

fn inspectable(context: &MonitorActionContext) -> ActionState {
    if context.inspectable {
        ActionState::Enabled
    } else {
        ActionState::disabled("this item has no inspector projection")
    }
}

fn refreshable(context: &MonitorActionContext) -> ActionState {
    if context.refreshable {
        ActionState::Enabled
    } else {
        ActionState::disabled("this scope has no refreshable source")
    }
}

fn external_open(context: &MonitorActionContext) -> ActionState {
    capability(&context.external_open)
}

fn deploy_handoff(context: &MonitorActionContext) -> ActionState {
    capability(&context.deploy_handoff)
}

fn follow(context: &MonitorActionContext) -> ActionState {
    capability(&context.follow)
}

fn trace_correlation(context: &MonitorActionContext) -> ActionState {
    capability(&context.trace_correlation)
}

fn metrics_correlation(context: &MonitorActionContext) -> ActionState {
    capability(&context.metrics_correlation)
}

fn deployment_correlation(context: &MonitorActionContext) -> ActionState {
    capability(&context.deployment_correlation)
}

fn capability(capability: &ActionCapability) -> ActionState {
    match capability {
        ActionCapability::Available => ActionState::Enabled,
        ActionCapability::Unavailable(reason) => ActionState::disabled(*reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(view: MonitorView) -> MonitorActionContext {
        MonitorActionContext {
            view,
            target: MonitorActionTarget::Overview,
            snapshot_generation: 1,
            inspectable: true,
            refreshable: true,
            external_open: ActionCapability::Unavailable("not available"),
            deploy_handoff: ActionCapability::Unavailable("not available"),
            follow: if view == MonitorView::Logs {
                ActionCapability::Available
            } else {
                ActionCapability::Unavailable("log follow is available only in Logs")
            },
            trace_correlation: ActionCapability::Unavailable("trace is unavailable"),
            metrics_correlation: ActionCapability::Unavailable("metrics are unavailable"),
            deployment_correlation: ActionCapability::Unavailable("deployment is unavailable"),
        }
    }

    #[test]
    fn catalog_projects_one_refresh_action_and_contextual_follow() {
        let registry = registry().unwrap();
        let overview = context(MonitorView::Overview);
        let logs = context(MonitorView::Logs);
        assert!(registry
            .resolve_menu(SCOPE_INLINE, &overview)
            .items()
            .iter()
            .any(|item| item.id == REFRESH));
        assert!(!registry
            .resolve_menu(SCOPE_INLINE, &overview)
            .items()
            .iter()
            .any(|item| item.id == TOGGLE_FOLLOW));
        assert!(registry
            .resolve_menu(SCOPE_INLINE, &logs)
            .items()
            .iter()
            .any(|item| item.id == TOGGLE_FOLLOW));
    }
}
