use std::{net::IpAddr, path::PathBuf};

use anyhow::{bail, Context, Result};

use crate::{
    framework::process::ProcessSupervisor,
    framework::{start_external, ExternalTarget},
    tailscale::{
        LoginEvent, Node, OperatingSystem, Readiness, RemoteCommand, Status, TailscaleClient,
        TailscaleSshTarget,
    },
};

use super::{
    command::CommandRunner,
    config::Config,
    linux::LinuxInspector,
    model::{
        Diagnostic, DiagnosticSeverity, DoctorCheck, DoctorCheckStatus, HostTarget, HostTargetKind,
        SetupAction, SetupActionKind, StreamDoctorReport, StreamInspection, StreamReadiness,
        StreamSessionState, StreamSetupReport, StreamSetupState, StreamStatusReport,
        TailscaleReadiness, STREAM_SCHEMA_VERSION,
    },
};

pub(super) struct StreamController {
    processes: ProcessSupervisor,
    working_directory: PathBuf,
    config: Config,
}

impl StreamController {
    pub(super) fn new(
        processes: ProcessSupervisor,
        working_directory: PathBuf,
        config: Config,
    ) -> Self {
        Self { processes, working_directory, config }
    }

    pub(super) async fn inspect(&self, cli_host: Option<&str>) -> Result<StreamInspection> {
        let selector = cli_host.or_else(|| self.config.preferred_host());
        if selector.is_none() || selector == Some("local") {
            return Ok(self.inspect_local().await);
        }
        let selector = selector.expect("checked Stream host selector");
        let tailscale =
            TailscaleClient::new(self.processes.clone(), self.working_directory.clone());
        let status = match tailscale.readiness().await? {
            Readiness::Ready(status) => status,
            Readiness::NeedsLogin => {
                return Ok(unresolved_inspection(
                    selector,
                    Diagnostic::setup(
                        "tailscale.loginRequired",
                        "Tailscale authentication is required",
                        SetupAction {
                            id: "stream.setup.authenticateTailscale".to_owned(),
                            label: "Authenticate Tailscale".to_owned(),
                            kind: SetupActionKind::AuthenticateTailscale,
                            value: None,
                        },
                    ),
                    TailscaleReadiness::LoginRequired,
                ));
            }
            readiness => {
                return Ok(unresolved_inspection(
                    selector,
                    Diagnostic::error(
                        "tailscale.unavailable",
                        "Tailscale is unavailable",
                        readiness_detail(readiness),
                    ),
                    TailscaleReadiness::Unavailable,
                ));
            }
        };
        let resolved = match resolve_node(&status, selector) {
            Ok(resolved) => resolved,
            Err(error) => {
                return Ok(unresolved_inspection(
                    selector,
                    Diagnostic::error(
                        "stream.host.notResolved",
                        "The selected host could not be resolved exactly",
                        format!("{error:#}"),
                    ),
                    TailscaleReadiness::Ready,
                ));
            }
        };
        match resolved {
            ResolvedNode::Local(node) => {
                let mut inspection = self.inspect_local().await;
                inspection.target = host_target(HostTargetKind::Local, node);
                Ok(inspection)
            }
            ResolvedNode::Peer(node) => self.inspect_remote(node).await,
        }
    }

    pub(super) async fn status(&self, cli_host: Option<&str>) -> Result<StreamStatusReport> {
        Ok(StreamStatusReport {
            schema_version: STREAM_SCHEMA_VERSION,
            session: StreamSessionState::Inactive,
            inspection: self.inspect(cli_host).await?,
        })
    }

    pub(super) async fn doctor(&self, cli_host: Option<&str>) -> Result<StreamDoctorReport> {
        let inspection = self.inspect(cli_host).await?;
        let (preset_name, preset) = self.config.default_preset();
        let mut checks = vec![
            DoctorCheck {
                id: "stream.host.hyprland".to_owned(),
                status: if inspection.hyprland.is_some() {
                    DoctorCheckStatus::Pass
                } else {
                    DoctorCheckStatus::Fail
                },
                summary: if inspection.hyprland.is_some() {
                    "Hyprland sources are readable".to_owned()
                } else {
                    "Hyprland sources are unavailable".to_owned()
                },
                action: None,
            },
            dependency_check(
                "stream.host.sunshine",
                "Sunshine",
                inspection.sunshine.available,
                "stream.setup.installSunshine",
            ),
            dependency_check(
                "stream.client.moonlight",
                "Moonlight",
                inspection.moonlight.available,
                "stream.setup.installMoonlight",
            ),
            DoctorCheck {
                id: "stream.config.defaultPreset".to_owned(),
                status: DoctorCheckStatus::Pass,
                summary: format!(
                    "Default preset {preset_name} is {}x{} at {} FPS",
                    preset.width, preset.height, preset.fps
                ),
                action: None,
            },
        ];
        if inspection.sunshine.service.as_ref().is_some_and(|service| service.active)
            || !inspection.sunshine.listeners.is_empty()
        {
            checks.push(DoctorCheck {
                id: "stream.host.sunshineBusy".to_owned(),
                status: DoctorCheckStatus::Attention,
                summary: "Sunshine is already active; Stream will not displace it".to_owned(),
                action: None,
            });
        } else {
            checks.push(DoctorCheck {
                id: "stream.host.sunshineAvailable".to_owned(),
                status: DoctorCheckStatus::Pass,
                summary: "No unrelated Sunshine session was detected".to_owned(),
                action: None,
            });
        }
        checks.extend(inspection.diagnostics.iter().map(|diagnostic| DoctorCheck {
            id: diagnostic.id.clone(),
            status: match diagnostic.severity {
                DiagnosticSeverity::Info => DoctorCheckStatus::Pass,
                DiagnosticSeverity::Warning => DoctorCheckStatus::Attention,
                DiagnosticSeverity::Error => DoctorCheckStatus::Fail,
            },
            summary: diagnostic.summary.clone(),
            action: diagnostic.action.clone(),
        }));
        let ready = inspection.readiness == StreamReadiness::Ready
            && checks.iter().all(|check| check.status != DoctorCheckStatus::Fail);
        Ok(StreamDoctorReport {
            schema_version: STREAM_SCHEMA_VERSION,
            ready,
            target: inspection.target,
            checks,
        })
    }

    pub(super) async fn authenticate_tailscale(&self) -> Result<StreamSetupReport> {
        let client = TailscaleClient::new(self.processes.clone(), self.working_directory.clone());
        if let Readiness::Ready(status) = client.readiness().await? {
            return Ok(StreamSetupReport {
                schema_version: STREAM_SCHEMA_VERSION,
                action: "authenticate_tailscale".to_owned(),
                state: StreamSetupState::Ready,
                target: Some(host_target(HostTargetKind::Local, &status.local)),
            });
        }
        let (mut events, _cancel, owner) = client.start_login();
        let mut ready = None;
        while let Some(event) = events.recv().await {
            match event {
                LoginEvent::Url(url) => {
                    start_external(&self.processes, ExternalTarget::Url(url.as_str().to_owned()))?
                        .completion()
                        .await
                        .context("open Tailscale authentication")?;
                }
                LoginEvent::Ready(status) => {
                    ready = Some(status);
                    break;
                }
                LoginEvent::Failed(detail) => bail!("Tailscale authentication failed: {detail}"),
                LoginEvent::Cancelled => bail!("Tailscale authentication was cancelled"),
            }
        }
        owner.await.context("join Tailscale authentication owner")?;
        let status = ready.context("Tailscale authentication ended without a ready status")?;
        Ok(StreamSetupReport {
            schema_version: STREAM_SCHEMA_VERSION,
            action: "authenticate_tailscale".to_owned(),
            state: StreamSetupState::Ready,
            target: Some(host_target(HostTargetKind::Local, &status.local)),
        })
    }

    pub(super) async fn configure_host(
        &mut self,
        selector: &str,
        user: &str,
        preferred: bool,
    ) -> Result<StreamSetupReport> {
        let client = TailscaleClient::new(self.processes.clone(), self.working_directory.clone());
        let status = match client.readiness().await? {
            Readiness::Ready(status) => status,
            Readiness::NeedsLogin => bail!("Tailscale authentication is required"),
            readiness => bail!("Tailscale is unavailable: {}", readiness_detail(readiness)),
        };
        let node = match resolve_node(&status, selector)? {
            ResolvedNode::Peer(node) => node,
            ResolvedNode::Local(_) => bail!("the local host does not require an SSH user"),
        };
        if !node.online {
            bail!("the selected host is offline");
        }
        if node.operating_system != OperatingSystem::Linux {
            bail!("Stream hosting currently requires Linux with Hyprland");
        }
        let address =
            node.addresses.first().copied().context("selected host has no Tailscale IP")?;
        TailscaleSshTarget::new(node.id.clone(), user, address)?;
        self.config.set_ssh_user(&node.id, user)?;
        if preferred {
            self.config.set_preferred_host(&node.id)?;
        }
        Ok(StreamSetupReport {
            schema_version: STREAM_SCHEMA_VERSION,
            action: "configure_host".to_owned(),
            state: StreamSetupState::Ready,
            target: Some(host_target(HostTargetKind::Remote, node)),
        })
    }

    async fn inspect_local(&self) -> StreamInspection {
        let inspector = LinuxInspector::new(
            self.processes.clone(),
            self.working_directory.clone(),
            self.config.executables().clone(),
        );
        let mut inspection = inspector.inspect().await;
        let tailscale =
            TailscaleClient::new(self.processes.clone(), self.working_directory.clone());
        match tailscale.readiness().await {
            Ok(Readiness::Ready(status)) => {
                inspection.tailscale = TailscaleReadiness::Ready;
                inspection.target = host_target(HostTargetKind::Local, &status.local);
            }
            Ok(Readiness::NeedsLogin) => {
                inspection.tailscale = TailscaleReadiness::LoginRequired;
                inspection.diagnostics.push(Diagnostic::setup(
                    "tailscale.loginRequired",
                    "Tailscale authentication is required for remote hosts",
                    SetupAction {
                        id: "stream.setup.authenticateTailscale".to_owned(),
                        label: "Authenticate Tailscale".to_owned(),
                        kind: SetupActionKind::AuthenticateTailscale,
                        value: None,
                    },
                ));
            }
            Ok(readiness) => {
                inspection.tailscale = TailscaleReadiness::Unavailable;
                inspection.diagnostics.push(Diagnostic::warning(
                    "tailscale.unavailable",
                    format!("Remote hosts are unavailable: {}", readiness_detail(readiness)),
                ));
            }
            Err(error) => {
                inspection.tailscale = TailscaleReadiness::Unavailable;
                inspection.diagnostics.push(Diagnostic::warning(
                    "tailscale.unavailable",
                    format!("Remote hosts are unavailable: {error:#}"),
                ));
            }
        }
        inspection.refresh_readiness();
        inspection
    }

    async fn inspect_remote(&self, node: &Node) -> Result<StreamInspection> {
        let target = host_target(HostTargetKind::Remote, node);
        if !node.online {
            return Ok(targeted_inspection(
                target,
                Diagnostic::warning("stream.host.offline", "The selected host is offline"),
            ));
        }
        if node.operating_system != OperatingSystem::Linux {
            return Ok(targeted_inspection(
                target,
                Diagnostic::error(
                    "stream.host.unsupported",
                    "The selected host cannot run Stream",
                    "Stream hosting currently requires Linux with Hyprland",
                ),
            ));
        }
        let Some(user) = self.config.ssh_user(&node.id) else {
            return Ok(targeted_inspection(
                target,
                Diagnostic::setup(
                    "stream.host.sshUserRequired",
                    "An SSH user is required for the selected host",
                    SetupAction {
                        id: "stream.setup.configureSshUser".to_owned(),
                        label: "Configure SSH user".to_owned(),
                        kind: SetupActionKind::ConfigureSshUser,
                        value: Some(node.id.clone()),
                    },
                ),
            ));
        };
        let address =
            node.addresses.first().copied().context("selected host has no Tailscale IP")?;
        let ssh_target = TailscaleSshTarget::new(node.id.clone(), user, address)?;
        let remote_command = RemoteCommand::from_arguments([
            self.config.executables().remote_kit.as_str(),
            "--json",
            "stream",
            "__host-inspect",
        ])?;
        let spec = ssh_target.captured_command_process_spec(
            &remote_command,
            &self.working_directory,
            format!("inspect Stream host {}", node.display_name()),
        )?;
        let runner = CommandRunner::new(self.processes.clone(), self.working_directory.clone());
        let report = runner.capture_spec(spec).await;
        let report = match report {
            Ok(report) => report,
            Err(error) => {
                return Ok(targeted_inspection(
                    target,
                    Diagnostic::error(
                        "stream.host.transportFailed",
                        "The selected host could not be inspected",
                        format!("{error:#}"),
                    ),
                ));
            }
        };
        if !report.succeeded() {
            return Ok(targeted_inspection(
                target,
                Diagnostic::error(
                    "stream.host.commandFailed",
                    "The selected host rejected Stream inspection",
                    format!("{:?}: {}", report.exit, report.detail()),
                ),
            ));
        }
        let mut inspection: StreamInspection =
            serde_json::from_slice(&report.stdout).context("decode remote Stream inspection")?;
        inspection.target = target;
        inspection.tailscale = TailscaleReadiness::Ready;
        inspection.refresh_readiness();
        Ok(inspection)
    }
}

fn resolve_node<'a>(status: &'a Status, selector: &str) -> Result<ResolvedNode<'a>> {
    let selector = selector.trim();
    if selector.is_empty() {
        bail!("Stream host selector cannot be empty");
    }
    let mut exact = status
        .peers
        .iter()
        .filter(|node| exact_node_match(node, selector))
        .map(ResolvedNode::Peer)
        .collect::<Vec<_>>();
    if exact_node_match(&status.local, selector) {
        exact.push(ResolvedNode::Local(&status.local));
    }
    match exact.as_slice() {
        [node] => return Ok(*node),
        nodes if nodes.len() > 1 => return ambiguous(selector, nodes),
        _ => {}
    }

    let mut labels = status
        .peers
        .iter()
        .filter(|node| label_node_match(node, selector))
        .map(ResolvedNode::Peer)
        .collect::<Vec<_>>();
    if label_node_match(&status.local, selector) {
        labels.push(ResolvedNode::Local(&status.local));
    }
    match labels.as_slice() {
        [node] => Ok(*node),
        [] => bail!("no Tailscale node exactly matches {selector:?}"),
        nodes => ambiguous(selector, nodes),
    }
}

fn exact_node_match(node: &Node, selector: &str) -> bool {
    let address = selector.parse::<IpAddr>().ok();
    node.id == selector
        || node.dns_name.eq_ignore_ascii_case(selector.trim_end_matches('.'))
        || address.is_some_and(|address| node.addresses.contains(&address))
}

fn label_node_match(node: &Node, selector: &str) -> bool {
    node.host_name.eq_ignore_ascii_case(selector)
        || node.dns_name.split('.').next().is_some_and(|label| label.eq_ignore_ascii_case(selector))
}

fn ambiguous<'a>(selector: &str, nodes: &[ResolvedNode<'a>]) -> Result<ResolvedNode<'a>> {
    let mut candidates =
        nodes.iter().map(|node| node.node().display_name().to_owned()).collect::<Vec<_>>();
    candidates.sort();
    candidates.dedup();
    bail!("Tailscale node selector {selector:?} is ambiguous: {candidates:?}")
}

#[derive(Clone, Copy)]
enum ResolvedNode<'a> {
    Local(&'a Node),
    Peer(&'a Node),
}

impl<'a> ResolvedNode<'a> {
    fn node(self) -> &'a Node {
        match self {
            Self::Local(node) | Self::Peer(node) => node,
        }
    }
}

fn host_target(kind: HostTargetKind, node: &Node) -> HostTarget {
    HostTarget {
        kind,
        display_name: node.display_name().to_owned(),
        stable_node_id: Some(node.id.clone()),
        operating_system: node.operating_system.label().to_owned(),
        online: node.online,
    }
}

fn unresolved_inspection(
    selector: &str,
    diagnostic: Diagnostic,
    tailscale: TailscaleReadiness,
) -> StreamInspection {
    let mut inspection = targeted_inspection(
        HostTarget {
            kind: HostTargetKind::Remote,
            display_name: selector.to_owned(),
            stable_node_id: None,
            operating_system: "unknown".to_owned(),
            online: false,
        },
        diagnostic,
    );
    inspection.tailscale = tailscale;
    inspection
}

fn targeted_inspection(target: HostTarget, diagnostic: Diagnostic) -> StreamInspection {
    let mut inspection = StreamInspection::local();
    inspection.target = target;
    inspection.diagnostics.push(diagnostic);
    inspection.refresh_readiness();
    inspection
}

fn readiness_detail(readiness: Readiness) -> String {
    match readiness {
        Readiness::Ready(_) => "ready".to_owned(),
        Readiness::NeedsLogin => "authentication required".to_owned(),
        Readiness::CliUnavailable(detail)
        | Readiness::DaemonUnavailable(detail)
        | Readiness::PermissionDenied(detail)
        | Readiness::Unsupported(detail) => detail,
    }
}

fn dependency_check(id: &str, product: &str, available: bool, action_id: &str) -> DoctorCheck {
    DoctorCheck {
        id: id.to_owned(),
        status: if available { DoctorCheckStatus::Pass } else { DoctorCheckStatus::Fail },
        summary: if available {
            format!("{product} is available")
        } else {
            format!("{product} is unavailable")
        },
        action: (!available).then(|| SetupAction {
            id: action_id.to_owned(),
            label: format!("Install {product}"),
            kind: SetupActionKind::InstallDependency,
            value: None,
        }),
    }
}
