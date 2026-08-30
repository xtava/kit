use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use futures_util::{stream::FuturesUnordered, StreamExt};
use tokio::{
    io::{copy_bidirectional, AsyncWriteExt},
    net::{TcpListener, TcpStream, UnixStream},
    sync::watch,
    task::{JoinHandle, JoinSet},
};
use wezterm_codec::BuildIdentity;

use crate::tailscale::{is_tailscale_address, Readiness, TailscaleClient};

use super::protocol::{
    read_bounded_line, ClientRequest, GatewayErrorCode, ServerResponse, MAX_REQUEST_LINE_BYTES,
};

/// Stable node-scoped Console endpoint in the IANA dynamic/private port range.
pub(crate) const CONSOLE_GATEWAY_PORT: u16 = 57_483;

const CONSOLE_CAPABILITY: &str = "github.com/xtava/kit/cap/console";
const RECONCILE_INTERVAL: Duration = Duration::from_secs(3);
const CONNECT_HELLO_TIMEOUT: Duration = Duration::from_secs(2);
const WHOIS_TIMEOUT: Duration = Duration::from_secs(4);
const RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CONNECTIONS: usize = 32;

type ReadinessFuture = Pin<Box<dyn Future<Output = Result<Readiness>> + Send>>;

#[derive(Clone, Debug, Eq, PartialEq)]
struct BindingIdentity {
    stable_node_id: String,
    user_id: Option<u64>,
    tagged: bool,
    addresses: BTreeSet<IpAddr>,
}

impl BindingIdentity {
    fn same_authority(&self, other: &Self) -> bool {
        self.stable_node_id == other.stable_node_id
            && self.user_id == other.user_id
            && self.tagged == other.tagged
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum BindingUpdate {
    Replace(Option<BindingIdentity>),
    Retain,
}

pub(crate) struct PreparedGateway {
    client: TailscaleClient,
    agent_socket: PathBuf,
    ready_line: Arc<[u8]>,
}

pub(crate) struct GatewayControl {
    shutdown: watch::Sender<bool>,
}

impl PreparedGateway {
    pub(crate) fn new(
        client: TailscaleClient,
        agent_socket: PathBuf,
        build: BuildIdentity,
    ) -> Result<Self> {
        let ready_line = ServerResponse::Ready { build }
            .encode()
            .context("encode the Console gateway ready response")?;
        Ok(Self { client, agent_socket, ready_line: Arc::from(ready_line) })
    }

    pub(crate) fn start(self) -> (GatewayControl, JoinHandle<Result<()>>) {
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let owner = tokio::spawn(run_owner(self, shutdown_receiver));
        (GatewayControl { shutdown }, owner)
    }
}

impl GatewayControl {
    pub(crate) fn request_shutdown(&self) {
        let _ = self.shutdown.send(true);
    }
}

async fn run_owner(gateway: PreparedGateway, mut shutdown: watch::Receiver<bool>) -> Result<()> {
    let PreparedGateway { client, agent_socket, ready_line } = gateway;
    let mut readiness = readiness_after(client.clone(), Duration::ZERO);
    let mut binding = None;
    let mut listeners = BTreeMap::<SocketAddr, Arc<TcpListener>>::new();
    let mut bind_failures = BTreeMap::<SocketAddr, String>::new();
    let (mut generation_shutdown, _) = watch::channel(false);
    let mut connections = JoinSet::new();

    loop {
        let mut accepts = listeners
            .iter()
            .map(|(address, listener)| accept_one(*address, Arc::clone(listener)))
            .collect::<FuturesUnordered<_>>();
        let can_accept = !accepts.is_empty() && connections.len() < MAX_CONNECTIONS;
        let has_connections = !connections.is_empty();
        tokio::select! {
            biased;
            _ = cancelled(&mut shutdown) => {
                generation_shutdown.send_replace(true);
                listeners.clear();
                join_connections(&mut connections).await?;
                return Ok(());
            }
            result = &mut readiness => {
                if let BindingUpdate::Replace(desired) = binding_update(result) {
                    reconcile(
                        desired,
                        &mut binding,
                        &mut listeners,
                        &mut bind_failures,
                        &mut generation_shutdown,
                        &mut connections,
                    ).await?;
                }
                readiness = readiness_after(client.clone(), RECONCILE_INTERVAL);
            }
            accepted = accepts.next(), if can_accept => {
                let Some((local_address, accepted)) = accepted else {
                    continue;
                };
                let (stream, peer_address) = match accepted {
                    Ok(accepted) => accepted,
                    Err(_) => {
                        listeners.remove(&local_address);
                        continue;
                    }
                };
                reap_connections(&mut connections)?;
                let Some(destination) = binding.as_ref() else {
                    continue;
                };
                let destination = destination.clone();
                let client = client.clone();
                let agent_socket = agent_socket.clone();
                let ready_line = Arc::clone(&ready_line);
                let connection_shutdown = generation_shutdown.subscribe();
                connections.spawn(async move {
                    serve_connection(
                        stream,
                        peer_address,
                        destination,
                        client,
                        agent_socket,
                        ready_line,
                        connection_shutdown,
                    ).await;
                });
            }
            joined = connections.join_next(), if has_connections => {
                check_connection_join(joined.expect("a non-empty set has one join result"))?;
            }
        }
    }
}

fn readiness_after(client: TailscaleClient, delay: Duration) -> ReadinessFuture {
    Box::pin(async move {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        client.readiness().await
    })
}

fn binding_update(readiness: Result<Readiness>) -> BindingUpdate {
    match readiness {
        Ok(Readiness::Ready(status)) => {
            let addresses = status
                .local
                .addresses
                .into_iter()
                .filter(|address| is_bindable_address(*address))
                .collect();
            BindingUpdate::Replace(Some(BindingIdentity {
                stable_node_id: status.local.id,
                user_id: status.local.user_id,
                tagged: !status.local.tags.is_empty(),
                addresses,
            }))
        }
        Ok(Readiness::NeedsLogin) => BindingUpdate::Replace(None),
        Ok(Readiness::CliUnavailable(_))
        | Ok(Readiness::DaemonUnavailable(_))
        | Ok(Readiness::PermissionDenied(_))
        | Ok(Readiness::Unsupported(_))
        | Err(_) => BindingUpdate::Retain,
    }
}

async fn reconcile(
    desired: Option<BindingIdentity>,
    current: &mut Option<BindingIdentity>,
    listeners: &mut BTreeMap<SocketAddr, Arc<TcpListener>>,
    bind_failures: &mut BTreeMap<SocketAddr, String>,
    generation_shutdown: &mut watch::Sender<bool>,
    connections: &mut JoinSet<()>,
) -> Result<()> {
    let authority_changed = match (current.as_ref(), desired.as_ref()) {
        (Some(current), Some(desired)) => !current.same_authority(desired),
        (None, None) => false,
        (Some(_), None) | (None, Some(_)) => true,
    };
    if authority_changed {
        generation_shutdown.send_replace(true);
        listeners.clear();
        bind_failures.clear();
        join_connections(connections).await?;
        let (next_shutdown, _) = watch::channel(false);
        *generation_shutdown = next_shutdown;
        *current = desired;
    } else if let (Some(current), Some(desired)) = (current.as_mut(), desired) {
        current.addresses = desired.addresses;
    }

    let Some(identity) = current else { return Ok(()) };
    let desired_endpoints = identity
        .addresses
        .iter()
        .copied()
        .map(|address| SocketAddr::new(address, CONSOLE_GATEWAY_PORT))
        .collect::<BTreeSet<_>>();
    listeners.retain(|endpoint, _| desired_endpoints.contains(endpoint));
    bind_failures.retain(|endpoint, _| desired_endpoints.contains(endpoint));
    for endpoint in desired_endpoints {
        if listeners.contains_key(&endpoint) {
            continue;
        }
        match TcpListener::bind(endpoint).await {
            Ok(listener) => {
                bind_failures.remove(&endpoint);
                listeners.insert(endpoint, Arc::new(listener));
            }
            Err(error) => {
                let detail = format!(
                    "Console gateway could not claim {endpoint}; one trusted local Console owner must own port {CONSOLE_GATEWAY_PORT}: {error}"
                );
                if bind_failures.get(&endpoint) != Some(&detail) {
                    eprintln!("{detail}");
                }
                bind_failures.insert(endpoint, detail);
            }
        }
    }
    Ok(())
}

fn is_bindable_address(address: IpAddr) -> bool {
    is_tailscale_address(address)
        && !address.is_unspecified()
        && !address.is_loopback()
        && !address.is_multicast()
}

async fn accept_one(
    local_address: SocketAddr,
    listener: Arc<TcpListener>,
) -> (SocketAddr, std::io::Result<(TcpStream, SocketAddr)>) {
    (local_address, listener.accept().await)
}

async fn serve_connection(
    mut stream: TcpStream,
    peer_address: SocketAddr,
    destination: BindingIdentity,
    client: TailscaleClient,
    agent_socket: PathBuf,
    ready_line: Arc<[u8]>,
    mut shutdown: watch::Receiver<bool>,
) {
    let _ = stream.set_nodelay(true);
    let request = tokio::select! {
        result = tokio::time::timeout(
            CONNECT_HELLO_TIMEOUT,
            read_bounded_line(&mut stream, MAX_REQUEST_LINE_BYTES),
        ) => result,
        _ = cancelled(&mut shutdown) => return,
    };
    let Ok(Ok(line)) = request else {
        send_error(&mut stream, GatewayErrorCode::Protocol, &mut shutdown).await;
        return;
    };
    if !matches!(ClientRequest::parse_line(&line), Ok(ClientRequest::Connect)) {
        send_error(&mut stream, GatewayErrorCode::Protocol, &mut shutdown).await;
        return;
    }

    let whois = tokio::select! {
        result = tokio::time::timeout(WHOIS_TIMEOUT, client.whois(peer_address)) => {
            match result {
                Ok(result) => result,
                Err(_) => {
                    send_error(&mut stream, GatewayErrorCode::Unavailable, &mut shutdown).await;
                    return;
                }
            }
        },
        _ = cancelled(&mut shutdown) => return,
    };
    let Ok(source) = whois else {
        send_error(&mut stream, GatewayErrorCode::Unavailable, &mut shutdown).await;
        return;
    };
    if !is_authorized(
        &destination.stable_node_id,
        destination.user_id,
        destination.tagged,
        source.user_id,
        !source.tags.is_empty(),
        &source.stable_node_id,
        source.has_capability(CONSOLE_CAPABILITY),
    ) {
        send_error(&mut stream, GatewayErrorCode::Auth, &mut shutdown).await;
        return;
    }

    let mut agent = tokio::select! {
        result = UnixStream::connect(&agent_socket) => match result {
            Ok(agent) => agent,
            Err(_) => {
                send_error(&mut stream, GatewayErrorCode::Unavailable, &mut shutdown).await;
                return;
            }
        },
        _ = cancelled(&mut shutdown) => return,
    };
    if !write_response(&mut stream, ready_line.as_ref(), &mut shutdown).await {
        return;
    }

    tokio::select! {
        _ = copy_bidirectional(&mut stream, &mut agent) => {}
        _ = cancelled(&mut shutdown) => {
            let _ = stream.shutdown().await;
            let _ = agent.shutdown().await;
        }
    }
}

fn is_authorized(
    destination_stable_node_id: &str,
    destination_user_id: Option<u64>,
    destination_tagged: bool,
    source_user_id: Option<u64>,
    source_tagged: bool,
    stable_source_id: &str,
    has_console_capability: bool,
) -> bool {
    if destination_stable_node_id.trim().is_empty()
        || stable_source_id.trim().is_empty()
        || destination_stable_node_id == stable_source_id
    {
        return false;
    }
    let Some(destination_user_id) = destination_user_id else { return false };
    let same_user = !destination_tagged
        && !source_tagged
        && source_user_id.is_some_and(|source| destination_user_id == source);
    same_user || has_console_capability
}

async fn send_error(
    stream: &mut TcpStream,
    code: GatewayErrorCode,
    shutdown: &mut watch::Receiver<bool>,
) {
    let line = ServerResponse::Error(code)
        .encode()
        .expect("a fixed Console gateway error line is bounded");
    let _ = write_response(stream, &line, shutdown).await;
    let _ = stream.shutdown().await;
}

async fn write_response(
    stream: &mut TcpStream,
    line: &[u8],
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    tokio::select! {
        result = tokio::time::timeout(RESPONSE_WRITE_TIMEOUT, stream.write_all(line)) => {
            matches!(result, Ok(Ok(())))
        }
        _ = cancelled(shutdown) => false,
    }
}

fn reap_connections(connections: &mut JoinSet<()>) -> Result<()> {
    while let Some(joined) = connections.try_join_next() {
        check_connection_join(joined)?;
    }
    Ok(())
}

async fn join_connections(connections: &mut JoinSet<()>) -> Result<()> {
    while let Some(joined) = connections.join_next().await {
        check_connection_join(joined)?;
    }
    Ok(())
}

fn check_connection_join(joined: Result<(), tokio::task::JoinError>) -> Result<()> {
    joined.map_err(|error| anyhow::anyhow!("Console gateway connection owner failed: {error}"))
}

async fn cancelled(receiver: &mut watch::Receiver<bool>) {
    loop {
        if *receiver.borrow() {
            return;
        }
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_never_selects_wildcard_loopback_or_multicast_addresses() {
        for rejected in ["0.0.0.0", "::", "127.0.0.1", "::1", "192.168.1.2", "224.0.0.1", "ff02::1"]
        {
            assert!(!is_bindable_address(rejected.parse().unwrap()));
        }
        assert!(is_bindable_address("100.64.0.1".parse().unwrap()));
        assert!(is_bindable_address("fd7a:115c:a1e0::1".parse().unwrap()));
    }

    #[test]
    fn authorization_requires_stable_source_and_same_user_or_exact_capability() {
        assert_eq!(CONSOLE_CAPABILITY, "github.com/xtava/kit/cap/console");
        assert!(is_authorized(
            "node-target",
            Some(42),
            false,
            Some(42),
            false,
            "node-source",
            false,
        ));
        assert!(
            is_authorized("node-target", Some(42), false, Some(7), false, "node-source", true,)
        );
        for (destination_user_id, destination_tagged, source_user_id, source_tagged, capability) in [
            (Some(42), false, Some(7), false, false),
            (None, false, Some(42), false, false),
            (None, false, Some(42), false, true),
            (Some(42), false, None, false, false),
            (Some(42), true, Some(42), false, false),
            (Some(42), false, Some(42), true, false),
        ] {
            assert!(!is_authorized(
                "node-target",
                destination_user_id,
                destination_tagged,
                source_user_id,
                source_tagged,
                "node-source",
                capability,
            ));
        }
        assert!(is_authorized("node-target", Some(42), true, Some(42), true, "node-source", true,));
        for (destination, source, capability) in [
            ("node-target", "", false),
            ("node-target", "   ", true),
            ("", "node-source", false),
            ("node-source", "node-source", false),
            ("node-source", "node-source", true),
        ] {
            assert!(!is_authorized(
                destination,
                Some(42),
                false,
                Some(42),
                false,
                source,
                capability,
            ));
        }
    }

    #[test]
    fn gateway_port_is_one_stable_high_nonzero_endpoint() {
        assert!((49_152..=u16::MAX).contains(&CONSOLE_GATEWAY_PORT));
    }

    #[test]
    fn transient_readiness_failures_retain_established_data_plane_state() {
        for observation in [
            Ok(Readiness::CliUnavailable("missing".to_owned())),
            Ok(Readiness::DaemonUnavailable("restarting".to_owned())),
            Ok(Readiness::PermissionDenied("temporary".to_owned())),
            Ok(Readiness::Unsupported("new schema".to_owned())),
            Err(anyhow::anyhow!("malformed status")),
        ] {
            assert_eq!(binding_update(observation), BindingUpdate::Retain);
        }
        assert_eq!(binding_update(Ok(Readiness::NeedsLogin)), BindingUpdate::Replace(None));
    }

    #[test]
    fn address_churn_does_not_change_the_authenticated_gateway_authority() {
        let identity = |address: &str| BindingIdentity {
            stable_node_id: "node".to_owned(),
            user_id: Some(42),
            tagged: false,
            addresses: [address.parse().unwrap()].into_iter().collect(),
        };
        assert!(identity("100.64.0.1").same_authority(&identity("100.64.0.2")));

        let mut other_user = identity("100.64.0.1");
        other_user.user_id = Some(7);
        assert!(!identity("100.64.0.1").same_authority(&other_user));
    }

    #[test]
    fn server_work_deadline_fits_inside_the_client_handshake_deadline() {
        assert!(
            CONNECT_HELLO_TIMEOUT + WHOIS_TIMEOUT + RESPONSE_WRITE_TIMEOUT
                < super::super::tailnet::HANDSHAKE_TIMEOUT
        );
    }
}
