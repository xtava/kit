use std::path::PathBuf;

use crate::{
    tailscale::{Node, OperatingSystem},
    tools::console::service::{ConsoleServicePlatform, ConsoleStatus},
};

use super::model::{
    ActionEffectOwner, ConnectedSessionOutcome, ConsoleProbeState, ControlCenterOutcome,
    ControlCenterState, ControlCenterStory, MachineAction, MachineConnectionRequest,
    MachineDiscoveryState, MachineRole, MachineRowWidth, MachineState,
};

fn node(id: &str, dns_name: &str, host_name: &str, online: bool) -> Node {
    Node {
        id: id.to_owned(),
        user_id: None,
        tags: Default::default(),
        dns_name: dns_name.to_owned(),
        host_name: host_name.to_owned(),
        operating_system: OperatingSystem::Macos,
        online,
        addresses: vec!["100.64.0.2".parse().unwrap()],
    }
}

fn peer(online: bool) -> Node {
    node("node-mac", "workstation.tail.example", "workstation", online)
}

fn build(version: &str, revision: &str, dirty: bool) -> wezterm_codec::BuildIdentity {
    wezterm_codec::BuildIdentity {
        product: "kit-console".to_owned(),
        version: version.to_owned(),
        source_revision: Some(revision.to_owned()),
        source_dirty: Some(dirty),
        embedded_wezterm_revision: Some("b".repeat(40)),
    }
}

fn local_machine_with(status: ConsoleStatus) -> MachineState {
    let mut machine = MachineState::from_tailnet_node(&peer(true), MachineRole::ThisMachine);
    machine.console = ConsoleProbeState::Complete(Box::new(status));
    machine
}

#[test]
fn start_screen_story_is_exhaustive_for_discovery_and_empty_states() {
    for (discovery, machine_count, expected) in [
        (MachineDiscoveryState::Discovering, 0, ControlCenterStory::Discovering),
        (
            MachineDiscoveryState::AuthenticationRequired,
            0,
            ControlCenterStory::AuthenticationRequired,
        ),
        (MachineDiscoveryState::Ready, 0, ControlCenterStory::Empty),
        (MachineDiscoveryState::Ready, 1, ControlCenterStory::Machines),
    ] {
        let machines = (0..machine_count)
            .map(|_| MachineState::from_tailnet_node(&peer(true), MachineRole::Peer))
            .collect();
        assert_eq!(
            ControlCenterState { discovery, machines, selected_machine: None }.story(),
            expected
        );
    }
    let unavailable = ControlCenterState {
        discovery: MachineDiscoveryState::Unavailable { detail: "daemon stopped".to_owned() },
        machines: Vec::new(),
        selected_machine: None,
    };
    assert_eq!(
        unavailable.story(),
        ControlCenterStory::Unavailable { detail: "daemon stopped".to_owned() }
    );
}

#[test]
fn primary_action_covers_every_local_service_state() {
    let platform = ConsoleServicePlatform::MacosLaunchAgent;
    let path = PathBuf::from("/tmp/console.sock");
    let cases = [
        (ConsoleStatus::NeedsTailscaleLogin, MachineAction::AuthenticateTailscale),
        (ConsoleStatus::TailscaleCliUnavailable { detail: String::new() }, MachineAction::Refresh),
        (
            ConsoleStatus::TailscaleDaemonUnavailable { detail: String::new() },
            MachineAction::Refresh,
        ),
        (
            ConsoleStatus::TailscalePermissionDenied { detail: String::new() },
            MachineAction::Refresh,
        ),
        (ConsoleStatus::TailscaleUnsupported { detail: String::new() }, MachineAction::Refresh),
        (ConsoleStatus::PeerOffline { machine: "workstation".to_owned() }, MachineAction::Refresh),
        (
            ConsoleStatus::TailnetEndpointUnavailable {
                machine: "workstation".to_owned(),
                detail: String::new(),
            },
            MachineAction::ShowDetails,
        ),
        (
            ConsoleStatus::TailnetAccessDenied { machine: "workstation".to_owned() },
            MachineAction::ShowDetails,
        ),
        (
            ConsoleStatus::TailnetProtocolIncompatible {
                machine: "workstation".to_owned(),
                detail: String::new(),
            },
            MachineAction::ShowDetails,
        ),
        (ConsoleStatus::NotInstalled { platform }, MachineAction::SetupOrRepair),
        (ConsoleStatus::Stopped { platform }, MachineAction::StartConsole),
        (
            ConsoleStatus::ServiceFailed { platform, detail: String::new() },
            MachineAction::SetupOrRepair,
        ),
        (
            ConsoleStatus::ServiceUnavailable { platform, detail: String::new() },
            MachineAction::SetupOrRepair,
        ),
        (
            ConsoleStatus::WrongOwner {
                platform,
                path: path.clone(),
                expected_uid: 1,
                actual_uid: 2,
            },
            MachineAction::SetupOrRepair,
        ),
        (
            ConsoleStatus::SocketMissing { platform, path: path.clone() },
            MachineAction::SetupOrRepair,
        ),
        (
            ConsoleStatus::SocketStale { platform, path: path.clone(), detail: String::new() },
            MachineAction::SetupOrRepair,
        ),
        (
            ConsoleStatus::SocketRejected { platform, path, detail: String::new() },
            MachineAction::SetupOrRepair,
        ),
        (
            ConsoleStatus::CodecIncompatible {
                platform,
                server_version: "0.1.0".to_owned(),
                server_codec: 1,
            },
            MachineAction::ShowDetails,
        ),
        (ConsoleStatus::ActivationDeferred { platform, sessions: 2 }, MachineAction::ShowDetails),
        (ConsoleStatus::RepairBusy { platform }, MachineAction::Refresh),
        (
            ConsoleStatus::MuxUnavailable { platform, detail: String::new() },
            MachineAction::SetupOrRepair,
        ),
        (
            ConsoleStatus::Ready {
                platform,
                sessions: 2,
                build: build("0.2.0", &"d".repeat(40), false),
            },
            MachineAction::Connect,
        ),
    ];
    for (status, expected) in cases {
        assert_eq!(local_machine_with(status).primary_action(), expected);
    }
}

#[test]
fn tailnet_failures_offer_details_and_refresh_without_local_lifecycle_actions() {
    let statuses = [
        ConsoleStatus::TailnetEndpointUnavailable {
            machine: "workstation".to_owned(),
            detail: "connection refused".to_owned(),
        },
        ConsoleStatus::TailnetAccessDenied { machine: "workstation".to_owned() },
        ConsoleStatus::TailnetProtocolIncompatible {
            machine: "workstation".to_owned(),
            detail: "codec 3".to_owned(),
        },
    ];

    for status in statuses {
        let machine = local_machine_with(status);
        let actions = machine.available_actions();
        assert!(actions.contains(&MachineAction::ShowDetails));
        assert!(actions.contains(&MachineAction::Refresh));
        assert!(!actions.contains(&MachineAction::StartConsole));
        assert!(!actions.contains(&MachineAction::SetupOrRepair));
        assert!(!actions.contains(&MachineAction::RestartService));
        assert!(!actions.contains(&MachineAction::StopService));
        assert!(machine.details().iter().any(|(label, _)| *label == "Next step"));
    }
}

#[test]
fn remote_actions_depend_only_on_tailnet_reachability() {
    let online = MachineState::from_tailnet_node(&peer(true), MachineRole::Peer);
    let offline = MachineState::from_tailnet_node(&peer(false), MachineRole::Peer);

    assert_eq!(online.primary_action(), MachineAction::Connect);
    assert_eq!(
        online.available_actions(),
        vec![
            MachineAction::Connect,
            MachineAction::NewSession,
            MachineAction::Refresh,
            MachineAction::ShowDetails,
        ]
    );
    assert_eq!(offline.primary_action(), MachineAction::Refresh);
    assert_eq!(
        offline.available_actions(),
        vec![MachineAction::Refresh, MachineAction::ShowDetails]
    );
}

#[test]
fn row_widths_add_information_without_changing_identity_or_action() {
    let machine = local_machine_with(ConsoleStatus::Ready {
        platform: ConsoleServicePlatform::MacosLaunchAgent,
        sessions: 3,
        build: build("0.2.0", &"a".repeat(40), true),
    });

    let compact = machine.row(MachineRowWidth::Compact);
    let normal = machine.row(MachineRowWidth::Normal);
    let wide = machine.row(MachineRowWidth::Wide);

    assert_eq!(compact.name, "workstation.tail.example");
    assert!(compact.display_name.is_none());
    assert_eq!(compact.status, "ready");
    assert_eq!(compact.primary_action, MachineAction::Connect);
    assert!(compact.role.is_none());
    assert_eq!(normal.display_name.as_deref(), Some("workstation"));
    assert_eq!(normal.operating_system.as_deref(), Some("macOS"));
    assert_eq!(normal.sessions.as_deref(), Some("3 sessions"));
    assert_eq!(wide.build.as_deref(), Some("0.2.0 aaaaaaaa dirty"));
    assert_eq!(wide.primary_action, compact.primary_action);
}

#[test]
fn duplicate_friendly_hostnames_keep_distinct_selectors_and_stable_ids() {
    let first = MachineState::from_tailnet_node(
        &node("node-a", "alpha.tail.example", "workstation", true),
        MachineRole::Peer,
    );
    let second = MachineState::from_tailnet_node(
        &node("node-b", "beta.tail.example", "workstation", true),
        MachineRole::Peer,
    );

    let first_row = first.row(MachineRowWidth::Normal);
    let second_row = second.row(MachineRowWidth::Normal);
    assert_eq!(first_row.name, "alpha.tail.example");
    assert_eq!(second_row.name, "beta.tail.example");
    assert_eq!(first_row.display_name.as_deref(), Some("workstation"));
    assert_eq!(second_row.display_name.as_deref(), Some("workstation"));
    assert_eq!(first.identity.display_name, second.identity.display_name);
    assert_ne!(first.identity.selector, second.identity.selector);
    assert_ne!(first.identity.stable_node_id, second.identity.stable_node_id);
}

#[test]
fn action_contracts_cover_every_input_surface_and_effect_owner() {
    for action in MachineAction::ALL {
        let contract = action.contract();
        assert_eq!(contract.action, action);
        assert!(!contract.title.is_empty());
        assert!(contract.mouse);
        assert!(contract.context_menu);
        assert!(contract.command_palette);
    }
    assert_eq!(MachineAction::Connect.contract().effect_owner, ActionEffectOwner::ConsoleRouter);
    assert_eq!(MachineAction::UpdateKit.contract().effect_owner, ActionEffectOwner::KitUpdater);
}

#[test]
fn routing_boundary_preserves_full_machine_identity_and_session_intent() {
    let machine = MachineState::from_tailnet_node(&peer(true), MachineRole::Peer).identity.clone();
    let connect = ControlCenterOutcome::Connect(MachineConnectionRequest::Remote {
        machine: machine.clone(),
        create_session: true,
    });
    assert_eq!(
        connect,
        ControlCenterOutcome::Connect(MachineConnectionRequest::Remote {
            machine,
            create_session: true,
        })
    );
    assert_ne!(ConnectedSessionOutcome::ReturnToControlCenter, ConnectedSessionOutcome::Quit);
}
