use std::{
    num::NonZeroU32,
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use tokio::sync::Mutex as AsyncMutex;
use wezterm_client::{
    client::{
        Client, HeadlessConnectionLifecycle, HeadlessConnectionState, HeadlessLifecycleError,
    },
    domain::{ClientDomain, ClientDomainConfig},
};
use wezterm_codec::BuildIdentity;
use wezterm_mux::{client::ClientId, domain::Domain, Mux, RuntimeAdmission, RuntimeRole};
use wezterm_promise::spawn::{SimpleExecutor, SimpleExecutorHandle};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionState {
    Attaching,
    Reconnecting { attempt: u32 },
    Ready,
    Failed,
    RetryExhausted,
    Detached,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectionHealth {
    pub generation: u64,
    pub state: ConnectionState,
}

#[derive(Clone)]
pub(crate) struct AttachmentPolicy {
    expected_build_identity: Option<BuildIdentity>,
    timeout: Duration,
    reconnect_attempt_limit: Option<NonZeroU32>,
}

impl AttachmentPolicy {
    pub(crate) fn new(
        expected_build_identity: Option<BuildIdentity>,
        timeout: Duration,
        reconnect_attempt_limit: Option<NonZeroU32>,
    ) -> Self {
        Self { expected_build_identity, timeout, reconnect_attempt_limit }
    }
}

/// The sole process-global mux projection and serialized attachment-generation owner.
///
/// This bootstrap owner is intentionally not cloneable. An attached handle retains its state so a
/// usable client can never outlive and tear down the mux projection it depends on.
pub(crate) struct ConnectionOwner {
    state: Arc<OwnerState>,
}

#[derive(Clone)]
pub(crate) struct ConnectionHandle {
    state: Arc<OwnerState>,
}

struct OwnerState {
    projection: ClientProjection,
    generation: AsyncMutex<Generation>,
    client: Mutex<Option<Client>>,
    lifecycle: Mutex<Option<LifecycleGeneration>>,
}

struct Generation {
    number: u64,
    policy: Option<AttachmentPolicy>,
}

struct LifecycleGeneration {
    number: u64,
    lifecycle: Arc<HeadlessConnectionLifecycle>,
}

struct ClientProjection {
    domain: Arc<ClientDomain>,
    mux: Arc<Mux>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    executor: SimpleExecutorHandle,
    owner: Mutex<Option<JoinHandle<Result<()>>>>,
}

impl ConnectionOwner {
    pub(crate) fn start(config: ClientDomainConfig) -> Result<Self> {
        wezterm_config::designate_this_as_the_main_thread();
        wezterm_config::common_init(None, &[], true)
            .context("initializing headless WezTerm config")?;
        Ok(Self {
            state: Arc::new(OwnerState {
                projection: ClientProjection::start(config)?,
                generation: AsyncMutex::new(Generation { number: 0, policy: None }),
                client: Mutex::new(None),
                lifecycle: Mutex::new(None),
            }),
        })
    }

    pub(crate) async fn attach(&self, policy: AttachmentPolicy) -> Result<ConnectionHandle> {
        self.state.reconstruct(policy).await?;
        Ok(ConnectionHandle { state: Arc::clone(&self.state) })
    }
}

impl ConnectionHandle {
    pub(crate) fn client(&self) -> Result<Client> {
        let client = self.state.client.lock().unwrap().clone();
        client.context("Console connection is not attached")
    }

    pub(crate) fn domain(&self) -> Result<Arc<ClientDomain>> {
        Ok(Arc::clone(&self.state.projection.domain))
    }

    pub(crate) fn drain_health(&self) -> Result<Option<ConnectionHealth>> {
        let lifecycle = self.state.lifecycle.lock().unwrap();
        let Some(active) = lifecycle.as_ref() else { return Ok(None) };
        let mut latest = None;
        loop {
            match active.lifecycle.try_recv() {
                Ok(connection) => {
                    latest = Some(ConnectionHealth {
                        generation: active.number,
                        state: map_connection_state(connection),
                    });
                }
                Err(HeadlessLifecycleError::Empty) => return Ok(latest),
                Err(HeadlessLifecycleError::Closed) => {
                    return Ok(latest.or(Some(ConnectionHealth {
                        generation: active.number,
                        state: ConnectionState::Detached,
                    })))
                }
                Err(error) => return Err(error).context("reading Console connection lifecycle"),
            }
        }
    }

    pub(crate) async fn retry(&self) -> Result<()> {
        let policy = {
            let generation = self.state.generation.lock().await;
            generation.policy.clone().context("Console connection has no attachment policy")?
        };
        self.state.reconstruct(policy).await
    }
}

impl OwnerState {
    async fn reconstruct(&self, policy: AttachmentPolicy) -> Result<()> {
        let mut generation = self.generation.lock().await;

        self.client.lock().unwrap().take();
        self.lifecycle.lock().unwrap().take();
        self.projection.domain.perform_detach();
        self.projection.quiesce().await?;

        generation.number = generation
            .number
            .checked_add(1)
            .context("Console connection generation space exhausted")?;
        generation.policy = Some(policy.clone());
        let number = generation.number;
        let admission = Arc::clone(self.projection.mux.admission());
        let lifecycle = Arc::new(match policy.reconnect_attempt_limit {
            Some(limit) => HeadlessConnectionLifecycle::with_reconnect_attempt_limit(
                Arc::clone(&admission),
                Some(limit),
            ),
            None => HeadlessConnectionLifecycle::new(Arc::clone(&admission)),
        });
        *self.lifecycle.lock().unwrap() =
            Some(LifecycleGeneration { number, lifecycle: Arc::clone(&lifecycle) });

        let client_id = ClientId { ssh_auth_sock: None, ..ClientId::new() };
        let attach = self.projection.domain.attach_with_lifecycle(
            None,
            &lifecycle,
            policy.expected_build_identity,
            client_id,
        );
        tokio::time::timeout(policy.timeout, attach)
            .await
            .context("timed out connecting to the Console agent")??;
        let client = self
            .projection
            .domain
            .attached_client()
            .context("Console client domain attached without a client")?;
        *self.client.lock().unwrap() = Some(client);
        Ok(())
    }
}

impl ClientProjection {
    fn start(config: ClientDomainConfig) -> Result<Self> {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc::sync_channel;

        let admission = RuntimeAdmission::new(RuntimeRole::Client)?;
        let executor = Arc::new(SimpleExecutor::new(Arc::clone(&admission)));
        let executor_handle = executor.handle();
        let shutdown = Arc::new(AtomicBool::new(false));
        let owner_shutdown = Arc::clone(&shutdown);
        let (ready_tx, ready_rx) = sync_channel(1);
        let owner = std::thread::Builder::new()
            .name("kit-console-client-mux".to_owned())
            .spawn(move || {
                let mux = Arc::new(Mux::new_headless(None, admission, executor));
                if let Err(error) = Mux::set_mux(&mux) {
                    let _ = ready_tx.send(Err(format!("{error:#}")));
                    return Err(error);
                }
                ready_tx
                    .send(Ok(Arc::clone(&mux)))
                    .map_err(|_| anyhow!("Console client abandoned its mux during startup"))?;

                let result = loop {
                    if owner_shutdown.load(Ordering::Acquire) {
                        break Ok(());
                    }
                    if let Err(error) = mux.tick_headless() {
                        eprintln!("Console client projection stopped: {error:#}");
                        break Err(error.context("ticking Console client projection"));
                    }
                };
                Mux::shutdown();
                result
            })
            .context("starting the Console client projection")?;
        let mux = match ready_rx.recv().context("waiting for the Console client projection")? {
            Ok(mux) => mux,
            Err(error) => {
                let _ = owner.join();
                bail!("initializing the Console client projection: {error}")
            }
        };
        let domain = Arc::new(ClientDomain::new(config));
        let mux_domain: Arc<dyn Domain> = domain.clone();
        mux.add_domain(&mux_domain);
        Ok(Self {
            domain,
            mux,
            shutdown,
            executor: executor_handle,
            owner: Mutex::new(Some(owner)),
        })
    }

    async fn quiesce(&self) -> Result<()> {
        self.executor
            .try_spawn(async {})
            .context("scheduling Console connection cleanup barrier")?
            .await
            .context("joining Console connection cleanup barrier")?;
        Ok(())
    }
}

impl Drop for ClientProjection {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;

        if Mux::try_get().is_some() {
            self.domain.perform_detach();
        }
        self.shutdown.store(true, Ordering::Release);
        let wake = self.executor.try_spawn(async {});
        if let Some(owner) = self.owner.lock().unwrap().take() {
            let _ = owner.join();
        }
        drop(wake);
    }
}

fn map_connection_state(state: HeadlessConnectionState) -> ConnectionState {
    match state {
        HeadlessConnectionState::Attaching => ConnectionState::Attaching,
        HeadlessConnectionState::Reconnecting { attempt } => {
            ConnectionState::Reconnecting { attempt }
        }
        HeadlessConnectionState::Ready => ConnectionState::Ready,
        HeadlessConnectionState::Failed(
            wezterm_client::client::HeadlessConnectionFailure::RetryExhausted,
        ) => ConnectionState::RetryExhausted,
        HeadlessConnectionState::Failed(_) => ConnectionState::Failed,
        HeadlessConnectionState::Detached => ConnectionState::Detached,
    }
}

#[cfg(test)]
mod tests {
    use super::{map_connection_state, ConnectionState};
    use wezterm_client::client::HeadlessConnectionState;

    #[test]
    fn connection_health_preserves_reconnect_attempts() {
        assert_eq!(
            map_connection_state(HeadlessConnectionState::Reconnecting { attempt: 3 }),
            ConnectionState::Reconnecting { attempt: 3 }
        );
        assert_eq!(map_connection_state(HeadlessConnectionState::Ready), ConnectionState::Ready);
    }
}
