use std::{net::IpAddr, sync::Arc, time::Duration};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use tokio::{sync::watch, task::JoinHandle};

use crate::{
    framework::{process::ProcessSupervisor, start_external, ExternalTarget},
    tailscale::{LoginEvent, Node, Readiness, Status, TailscaleClient},
};

use super::{
    service::ConsoleStatus,
    transport::{
        PreparedRelayEpoch, RelayEpochFailure, RelayEpochOutcomeKind, RelayEpochProvider,
        TailnetRelay, TailnetRelayError, CONSOLE_GATEWAY_PORT,
    },
};

#[derive(Clone)]
pub(crate) struct RemoteTarget {
    client: TailscaleClient,
    machine: String,
    stable_node_id: String,
}

pub(crate) struct RemoteRelay {
    relay: TailnetRelay,
    status: watch::Receiver<Option<ConsoleStatus>>,
    status_owner: JoinHandle<()>,
}

struct RemoteEpochProvider {
    client: TailscaleClient,
    machine: String,
    stable_node_id: String,
    status: watch::Sender<Option<ConsoleStatus>>,
}

pub(crate) enum Resolution {
    Ready(RemoteTarget),
    Status(ConsoleStatus),
}

pub(crate) async fn resolve(processes: &ProcessSupervisor, selector: &str) -> Result<Resolution> {
    let client = TailscaleClient::new(processes.clone(), std::env::current_dir()?);
    let status = match tailnet_status(&client).await? {
        Ok(status) => status,
        Err(status) => return Ok(Resolution::Status(status)),
    };
    let node = status.resolve_peer(selector)?;
    resolve_node(client, node)
}

pub(crate) async fn resolve_identity(
    processes: &ProcessSupervisor,
    stable_node_id: &str,
) -> Result<Resolution> {
    let client = TailscaleClient::new(processes.clone(), std::env::current_dir()?);
    let status = match tailnet_status(&client).await? {
        Ok(status) => status,
        Err(status) => return Ok(Resolution::Status(status)),
    };
    let node = status.resolve_peer_by_id(stable_node_id)?;
    resolve_node(client, node)
}

fn resolve_node(client: TailscaleClient, node: &Node) -> Result<Resolution> {
    let machine = if node.dns_name.is_empty() {
        node.display_name().to_owned()
    } else {
        node.dns_name.clone()
    };
    if !node.online {
        return Ok(Resolution::Status(ConsoleStatus::PeerOffline { machine }));
    }
    preferred_address(node).context("the selected Tailscale node has no routable address")?;
    Ok(Resolution::Ready(RemoteTarget { client, machine, stable_node_id: node.id.clone() }))
}

async fn tailnet_status(client: &TailscaleClient) -> Result<Result<Status, ConsoleStatus>> {
    Ok(match client.readiness().await? {
        Readiness::Ready(status) => Ok(status),
        Readiness::NeedsLogin => Err(ConsoleStatus::NeedsTailscaleLogin),
        Readiness::CliUnavailable(detail) => Err(ConsoleStatus::TailscaleCliUnavailable { detail }),
        Readiness::DaemonUnavailable(detail) => {
            Err(ConsoleStatus::TailscaleDaemonUnavailable { detail })
        }
        Readiness::PermissionDenied(detail) => {
            Err(ConsoleStatus::TailscalePermissionDenied { detail })
        }
        Readiness::Unsupported(detail) => Err(ConsoleStatus::TailscaleUnsupported { detail }),
    })
}

pub(crate) async fn login(processes: &ProcessSupervisor) -> Result<()> {
    let client = TailscaleClient::new(processes.clone(), std::env::current_dir()?);
    let (mut events, _cancel, owner) = client.start_login();
    let mut outcome = Ok(false);
    while let Some(event) = events.recv().await {
        match event {
            LoginEvent::Url(url) => {
                println!("Authenticate Tailscale: {url}");
                if let Err(error) = async {
                    start_external(processes, ExternalTarget::Url(url.as_str().to_owned()))?
                        .completion()
                        .await
                }
                .await
                {
                    eprintln!("Could not open the Tailscale authentication link: {error:#}");
                }
            }
            LoginEvent::Ready(_) => {
                outcome = Ok(true);
                break;
            }
            LoginEvent::Failed(detail) => {
                outcome = Err(anyhow::anyhow!("Tailscale login failed: {detail}"));
                break;
            }
            LoginEvent::Cancelled => {
                outcome = Err(anyhow::anyhow!("Tailscale login was cancelled"));
                break;
            }
        }
    }
    owner.await.context("joining the Tailscale login owner")?;
    if !outcome? {
        bail!("Tailscale login ended before the device became ready");
    }
    Ok(())
}

pub(crate) async fn start_relay(
    processes: &ProcessSupervisor,
    target: &RemoteTarget,
) -> Result<RemoteRelay> {
    let (status_sender, status) = watch::channel(None);
    let provider: Arc<dyn RelayEpochProvider> = Arc::new(RemoteEpochProvider {
        client: target.client.clone(),
        machine: target.machine.clone(),
        stable_node_id: target.stable_node_id.clone(),
        status: status_sender.clone(),
    });
    let relay = match TailnetRelay::start(processes, provider).await {
        Ok(relay) => relay,
        Err(error) => return Err(initial_relay_error(&status, error)),
    };
    let mut outcomes = relay.outcome_receiver();
    let machine = target.machine.clone();
    let status_owner = tokio::spawn(async move {
        loop {
            if outcomes.changed().await.is_err() {
                return;
            }
            let Some(outcome) = outcomes.borrow_and_update().clone() else {
                continue;
            };
            let next = status_for_outcome(&machine, &outcome.kind);
            if !matches!(&outcome.kind, RelayEpochOutcomeKind::Failed(RelayEpochFailure::Preflight))
            {
                status_sender.send_replace(next);
            }
        }
    });
    Ok(RemoteRelay { relay, status, status_owner })
}

fn initial_relay_error(
    status: &watch::Receiver<Option<ConsoleStatus>>,
    error: TailnetRelayError,
) -> anyhow::Error {
    match status.borrow().clone() {
        Some(status) => anyhow::anyhow!(status.text()),
        None => error.into(),
    }
}

impl RemoteRelay {
    pub(crate) fn socket_path(&self) -> &std::path::Path {
        self.relay.socket_path()
    }

    pub(crate) fn status_receiver(&self) -> watch::Receiver<Option<ConsoleStatus>> {
        self.status.clone()
    }

    pub(crate) async fn failure_status(&self) -> Option<ConsoleStatus> {
        let mut status = self.status.clone();
        if status.borrow().is_none() {
            let _ = tokio::time::timeout(Duration::from_millis(250), status.changed()).await;
        }
        let failure = status.borrow().clone();
        failure
    }

    pub(crate) fn gateway_build(&self) -> Option<wezterm_codec::BuildIdentity> {
        self.relay.gateway_build()
    }

    pub(crate) async fn shutdown(self) -> Result<()> {
        let relay_result = self.relay.shutdown().await;
        self.status_owner.await.context("joining the Console tailnet status owner")?;
        relay_result.map_err(Into::into)
    }
}

#[async_trait]
impl RelayEpochProvider for RemoteEpochProvider {
    async fn prepare(&self) -> Result<PreparedRelayEpoch, RelayEpochFailure> {
        match self.prepare_status().await {
            Ok(prepared) => {
                self.status.send_replace(None);
                Ok(prepared)
            }
            Err(status) => {
                self.status.send_replace(Some(status));
                Err(RelayEpochFailure::Preflight)
            }
        }
    }
}

impl RemoteEpochProvider {
    async fn prepare_status(&self) -> Result<PreparedRelayEpoch, ConsoleStatus> {
        let status = tailnet_status(&self.client).await.map_err(|error| {
            unavailable(&self.machine, format!("refreshing Tailscale status: {error:#}"))
        })??;
        let node = status.resolve_peer_by_id(&self.stable_node_id).map_err(|error| {
            unavailable(&self.machine, format!("resolving the stable node identity: {error}"))
        })?;
        if !node.online {
            return Err(ConsoleStatus::PeerOffline { machine: self.machine.clone() });
        }
        let address = preferred_address(node)
            .ok_or_else(|| unavailable(&self.machine, "no Tailscale address was advertised"))?;
        let command = self
            .client
            .nc_command(address, CONSOLE_GATEWAY_PORT, "connect Console over Tailscale")
            .map_err(|error| {
                unavailable(&self.machine, format!("preparing the Tailscale connection: {error:#}"))
            })?;
        Ok(PreparedRelayEpoch::new(command))
    }
}

fn preferred_address(node: &Node) -> Option<IpAddr> {
    node.addresses.iter().copied().find(IpAddr::is_ipv4).or_else(|| node.addresses.first().copied())
}

fn status_for_outcome(machine: &str, kind: &RelayEpochOutcomeKind) -> Option<ConsoleStatus> {
    match kind {
        RelayEpochOutcomeKind::Ready { .. } | RelayEpochOutcomeKind::LocalDisconnected => None,
        RelayEpochOutcomeKind::Cancelled => None,
        RelayEpochOutcomeKind::Failed(RelayEpochFailure::GatewayDenied) => {
            Some(ConsoleStatus::TailnetAccessDenied { machine: machine.to_owned() })
        }
        RelayEpochOutcomeKind::Failed(RelayEpochFailure::Protocol) => {
            Some(ConsoleStatus::TailnetProtocolIncompatible {
                machine: machine.to_owned(),
                detail: "the target returned an unsupported Console gateway response".to_owned(),
            })
        }
        RelayEpochOutcomeKind::Failed(RelayEpochFailure::Preflight) => None,
        RelayEpochOutcomeKind::Failed(failure) => {
            Some(unavailable(machine, format!("tailnet relay failed: {failure:?}")))
        }
        RelayEpochOutcomeKind::TransportExited { exit } => {
            Some(unavailable(machine, format!("tailscale nc exited: {exit:?}")))
        }
    }
}

fn unavailable(machine: &str, detail: impl Into<String>) -> ConsoleStatus {
    ConsoleStatus::TailnetEndpointUnavailable { machine: machine.to_owned(), detail: detail.into() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_outcomes_keep_authentication_and_transport_recovery_distinct() {
        assert!(matches!(
            status_for_outcome(
                "peer",
                &RelayEpochOutcomeKind::Failed(RelayEpochFailure::GatewayDenied)
            ),
            Some(ConsoleStatus::TailnetAccessDenied { .. })
        ));
        assert!(matches!(
            status_for_outcome("peer", &RelayEpochOutcomeKind::Failed(RelayEpochFailure::Start)),
            Some(ConsoleStatus::TailnetEndpointUnavailable { .. })
        ));
    }
}
