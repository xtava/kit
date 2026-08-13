use crate::client::{
    Client, HeadlessConnectionFailure, HeadlessConnectionLifecycle, HeadlessLifecycleError,
};
use crate::pane::ClientPane;
use anyhow::{anyhow, bail};
use async_trait::async_trait;
use codec::{
    AdmittedRpcResponse, EnvironmentFreeCommand, ListPanesResponse, SpawnV2, SplitPane,
    SplitSpawnDomain, SplitSpawnSource, TabSpawnDomain, TabSpawnPlacement,
};
use config::{SshDomain, TlsDomainClient, UnixDomain};
use lru::LruCache;
use mux::client::ClientId;
use mux::connui::{ConnectionUI, ConnectionUIParams};
use mux::domain::{alloc_domain_id, Domain, DomainId, DomainState, SplitSource};
use mux::pane::{Pane, PaneId};
use mux::tab::{SplitRequest, Tab, TabId};
use mux::window::WindowId;
use mux::{Mux, MuxNotification};
use portable_pty::CommandBuilder;
use promise::spawn::spawn_into_new_thread;
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use termwiz::image::ImageData;
use wezterm_runtime_admission::RetainedClass;
use wezterm_term::TerminalSize;

const CLIENT_IMAGE_CACHE_ENTRIES: usize = 128;

fn environment_free_command(
    command: Option<CommandBuilder>,
) -> anyhow::Result<(EnvironmentFreeCommand, Option<String>)> {
    let command_dir = command
        .as_ref()
        .and_then(CommandBuilder::get_cwd)
        .map(|cwd| {
            cwd.to_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow!("spawn command cwd is not valid UTF-8"))
        })
        .transpose()?;
    let command = match command {
        None => EnvironmentFreeCommand::DefaultLoginShell,
        Some(command) if command.is_default_prog() => EnvironmentFreeCommand::DefaultLoginShell,
        Some(command) => EnvironmentFreeCommand::try_from_argv(command.get_argv())
            .map_err(anyhow::Error::from)?,
    };
    Ok((command, command_dir))
}

pub struct ClientInner {
    pub client: Client,
    pub local_domain_id: DomainId,
    pub local_echo_threshold_ms: Option<u64>,
    pub overlay_lag_indicator: bool,
    remote_to_local_window: Mutex<HashMap<WindowId, WindowId>>,
    remote_to_local_tab: Mutex<HashMap<TabId, TabId>>,
    remote_to_local_pane: Mutex<HashMap<PaneId, PaneId>>,
    pub focused_remote_pane_id: Mutex<Option<PaneId>>,
    image_cache: Mutex<LruCache<[u8; 32], Arc<ImageData>>>,
}

impl ClientInner {
    pub(crate) fn cached_image(&self, hash: &[u8; 32]) -> Option<Arc<ImageData>> {
        self.image_cache.lock().unwrap().get(hash).map(Arc::clone)
    }

    /// Admit a newly decoded image before making it reachable from either the
    /// client cache or a rendered line. The lease is anchored in ImageData so
    /// cache eviction cannot release it while ImageCell aliases remain alive.
    pub(crate) fn retain_image(&self, data: Arc<ImageData>) -> anyhow::Result<Arc<ImageData>> {
        let hash = data.hash();
        let mut cache = self.image_cache.lock().unwrap();
        if let Some(cached) = cache.get(&hash) {
            return Ok(Arc::clone(cached));
        }

        let retained_bytes = data.retained_size();
        let lease = self
            .client
            .admission()
            .try_retained(RetainedClass::ClientImage, retained_bytes)?;
        if let Err(lease) = data.try_set_retention_guard(lease) {
            drop(lease);
            bail!("newly decoded image already has a retained-memory guard");
        }

        let evicted = cache.push(hash, Arc::clone(&data));
        drop(cache);
        drop(evicted);
        Ok(data)
    }

    fn remote_to_local_window(&self, remote_window_id: WindowId) -> Option<WindowId> {
        let map = self.remote_to_local_window.lock().unwrap();
        map.get(&remote_window_id).cloned()
    }

    pub(crate) fn expire_stale_mappings(&self) {
        let mux = Mux::get();

        self.remote_to_local_pane
            .lock()
            .unwrap()
            .retain(|_remote_pane_id, local_pane_id| mux.get_pane(*local_pane_id).is_some());

        self.remote_to_local_tab
            .lock()
            .unwrap()
            .retain(
                |remote_tab_id, local_tab_id| match mux.get_tab(*local_tab_id) {
                    Some(tab) => {
                        for pos in tab.iter_panes_ignoring_zoom() {
                            if pos.pane.domain_id() == self.local_domain_id {
                                return true;
                            }
                        }
                        log::trace!(
                            "expire_stale_mappings: domain: {}. will remove \
                            {remote_tab_id} -> {local_tab_id} tab mapping \
                            because tab contains no panes from this domain",
                            self.local_domain_id,
                        );
                        false
                    }
                    None => false,
                },
            );

        self.remote_to_local_window
            .lock()
            .unwrap()
            .retain(
                |_remote_window_id, local_window_id| match mux.get_window(*local_window_id) {
                    Some(w) => {
                        for tab in w.iter() {
                            for pos in tab.iter_panes_ignoring_zoom() {
                                if pos.pane.domain_id() == self.local_domain_id {
                                    return true;
                                }
                            }
                        }
                        false
                    }
                    None => false,
                },
            );
    }

    fn record_remote_to_local_window_mapping(
        &self,
        remote_window_id: WindowId,
        local_window_id: WindowId,
    ) {
        let mut map = self.remote_to_local_window.lock().unwrap();
        map.insert(remote_window_id, local_window_id);
        log::trace!(
            "record_remote_to_local_window_mapping: {} -> {}",
            remote_window_id,
            local_window_id
        );
    }

    fn local_to_remote_tab(&self, local_tab_id: TabId) -> Option<TabId> {
        let map = self.remote_to_local_tab.lock().unwrap();
        for (remote, local) in map.iter() {
            if *local == local_tab_id {
                return Some(*remote);
            }
        }
        None
    }

    fn local_to_remote_window(&self, local_window_id: WindowId) -> Option<WindowId> {
        let map = self.remote_to_local_window.lock().unwrap();
        for (remote, local) in map.iter() {
            if *local == local_window_id {
                return Some(*remote);
            }
        }
        None
    }

    pub fn remote_to_local_pane_id(&self, remote_pane_id: PaneId) -> Option<TabId> {
        let mut pane_map = self.remote_to_local_pane.lock().unwrap();

        if let Some(id) = pane_map.get(&remote_pane_id) {
            return Some(*id);
        }

        let mux = Mux::get();

        for pane in mux.iter_panes() {
            if pane.domain_id() != self.local_domain_id {
                continue;
            }
            if let Some(pane) = pane.downcast_ref::<ClientPane>() {
                if pane.remote_pane_id() == remote_pane_id {
                    let local_pane_id = pane.pane_id();
                    pane_map.insert(remote_pane_id, local_pane_id);
                    return Some(local_pane_id);
                }
            }
        }
        None
    }
    pub fn remove_old_pane_mapping(&self, remote_pane_id: PaneId) {
        let mut pane_map = self.remote_to_local_pane.lock().unwrap();
        pane_map.remove(&remote_pane_id);
    }

    pub fn remove_old_tab_mapping(&self, remote_tab_id: TabId) {
        let mut tab_map = self.remote_to_local_tab.lock().unwrap();
        let old = tab_map.remove(&remote_tab_id);
        log::trace!("remove_old_tab_mapping: {remote_tab_id} -> {old:?}");
    }

    fn record_remote_to_local_tab_mapping(&self, remote_tab_id: TabId, local_tab_id: TabId) {
        let mut map = self.remote_to_local_tab.lock().unwrap();
        let prior = map.insert(remote_tab_id, local_tab_id);
        log::trace!(
            "record_remote_to_local_tab_mapping: {} -> {} \
             (prior={prior:?}, domain={})",
            remote_tab_id,
            local_tab_id,
            self.local_domain_id,
        );
    }

    pub fn remote_to_local_tab_id(&self, remote_tab_id: TabId) -> Option<TabId> {
        let map = self.remote_to_local_tab.lock().unwrap();
        map.get(&remote_tab_id).copied()
    }

    pub fn is_local(&self) -> bool {
        self.client.is_local
    }
}

#[derive(Clone, Debug)]
pub enum ClientDomainConfig {
    Unix(UnixDomain),
    Tls(TlsDomainClient),
    Ssh(SshDomain),
}

impl ClientDomainConfig {
    pub fn name(&self) -> &str {
        match self {
            ClientDomainConfig::Unix(unix) => &unix.name,
            ClientDomainConfig::Tls(tls) => &tls.name,
            ClientDomainConfig::Ssh(ssh) => &ssh.name,
        }
    }

    pub fn local_echo_threshold_ms(&self) -> Option<u64> {
        match self {
            ClientDomainConfig::Unix(unix) => unix.local_echo_threshold_ms,
            ClientDomainConfig::Tls(tls) => tls.local_echo_threshold_ms,
            ClientDomainConfig::Ssh(ssh) => ssh.local_echo_threshold_ms,
        }
    }

    pub fn overlay_lag_indicator(&self) -> bool {
        match self {
            ClientDomainConfig::Unix(unix) => unix.overlay_lag_indicator,
            ClientDomainConfig::Tls(tls) => tls.overlay_lag_indicator,
            ClientDomainConfig::Ssh(ssh) => ssh.overlay_lag_indicator,
        }
    }

    pub fn label(&self) -> String {
        match self {
            ClientDomainConfig::Unix(unix) => format!("unix mux {}", unix.socket_path().display()),
            ClientDomainConfig::Tls(tls) => format!("TLS mux {}", tls.remote_address),
            ClientDomainConfig::Ssh(ssh) => {
                if let Some(user) = &ssh.username {
                    format!("SSH mux {}@{}", user, ssh.remote_address)
                } else {
                    format!("SSH mux {}", ssh.remote_address)
                }
            }
        }
    }

    pub fn connect_automatically(&self) -> bool {
        match self {
            ClientDomainConfig::Unix(unix) => unix.connect_automatically,
            ClientDomainConfig::Tls(tls) => tls.connect_automatically,
            ClientDomainConfig::Ssh(ssh) => ssh.connect_automatically,
        }
    }
}

impl ClientInner {
    pub fn new(
        local_domain_id: DomainId,
        client: Client,
        local_echo_threshold_ms: Option<u64>,
        overlay_lag_indicator: bool,
    ) -> Self {
        Self {
            client,
            local_domain_id,
            local_echo_threshold_ms,
            overlay_lag_indicator,
            remote_to_local_window: Mutex::new(HashMap::new()),
            remote_to_local_tab: Mutex::new(HashMap::new()),
            remote_to_local_pane: Mutex::new(HashMap::new()),
            focused_remote_pane_id: Mutex::new(None),
            image_cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(CLIENT_IMAGE_CACHE_ENTRIES).unwrap(),
            )),
        }
    }
}

pub struct ClientDomain {
    config: ClientDomainConfig,
    label: String,
    inner: Mutex<Option<Arc<ClientInner>>>,
    local_domain_id: DomainId,
}

async fn update_remote_workspace(
    local_domain_id: DomainId,
    pdu: codec::SetWindowWorkspace,
) -> anyhow::Result<()> {
    let inner = ClientDomain::get_client_inner_for_domain(local_domain_id)?;
    inner.client.set_window_workspace(pdu).await?;
    Ok(())
}

fn mux_notify_client_domain(local_domain_id: DomainId, notif: MuxNotification) -> bool {
    let mux = Mux::get();
    let domain = match mux.get_domain(local_domain_id) {
        Some(domain) => domain,
        None => return false,
    };
    let client_domain = match domain.downcast_ref::<ClientDomain>() {
        Some(c) => c,
        None => return false,
    };

    match notif {
        MuxNotification::ActiveWorkspaceChanged(_client_id) => {
            // TODO: advice remote host of interesting workspaces
        }
        MuxNotification::WorkspaceRenamed {
            old_workspace,
            new_workspace,
        } => {
            if let Some(inner) = client_domain.inner() {
                let workspaces = Mux::get().iter_workspaces();
                if workspaces.contains(&old_workspace) {
                    let future = async move {
                        inner
                            .client
                            .rename_workspace(codec::RenameWorkspace {
                                old_workspace,
                                new_workspace,
                            })
                            .await?;
                        Ok(())
                    };
                    if let Err(err) = mux.try_spawn_client_invalidation(future) {
                        log::error!("unable to schedule remote workspace rename: {err:#}");
                    }
                }
            }
        }
        MuxNotification::WindowWorkspaceChanged(window_id) => {
            // Mux::get_window() may trigger a borrow error if called
            // immediately; defer the bulk of this work.
            // <https://github.com/wezterm/wezterm/issues/2638>
            let future = async move {
                let request = {
                    let mux = Mux::get();
                    let domain = match mux.get_domain(local_domain_id) {
                        Some(domain) => domain,
                        None => return Ok(()),
                    };
                    let domain = match domain.downcast_ref::<ClientDomain>() {
                        Some(domain) => domain,
                        None => return Ok(()),
                    };
                    if let Some(remote_window_id) = domain.local_to_remote_window_id(window_id) {
                        mux.get_window(window_id)
                            .map(|window| codec::SetWindowWorkspace {
                                window_id: remote_window_id,
                                workspace: window.get_workspace().to_string(),
                            })
                    } else {
                        log::debug!(
                            "local window id {window_id} has no known remote window \
                            id while reconciling a local WindowWorkspaceChanged event"
                        );
                        None
                    }
                };
                if let Some(request) = request {
                    update_remote_workspace(local_domain_id, request).await?;
                }
                Ok(())
            };
            if let Err(err) = mux.try_spawn_client_invalidation(future) {
                log::error!("unable to schedule remote window workspace update: {err:#}");
            }
        }
        MuxNotification::TabTitleChanged { tab_id, title } => {
            if let Some(remote_tab_id) = client_domain.local_to_remote_tab_id(tab_id) {
                if let Some(inner) = client_domain.inner() {
                    let future = async move {
                        inner
                            .client
                            .set_tab_title(codec::TabTitleChanged {
                                tab_id: remote_tab_id,
                                title,
                            })
                            .await?;
                        Ok(())
                    };
                    if let Err(err) = mux.try_spawn_client_invalidation(future) {
                        log::error!("unable to schedule remote tab title update: {err:#}");
                    }
                }
            }
        }
        MuxNotification::WindowTitleChanged {
            window_id,
            title: _,
        } => {
            if let Some(remote_window_id) = client_domain.local_to_remote_window_id(window_id) {
                if let Some(inner) = client_domain.inner() {
                    let future = async move {
                        // De-bounce the title propagation.
                        // There is a bit of a race condition with these async
                        // updates that can trigger a cycle of WindowTitleChanged
                        // PDUs being exchanged between client and server if the
                        // title is changed twice in quick succession.
                        // To avoid that, here on the client, we wait a second
                        // and then report the now-current name of the window, rather
                        // than propagating the title encoded in the MuxNotification.
                        smol::Timer::after(std::time::Duration::from_secs(1)).await;
                        if let Some(mux) = Mux::try_get() {
                            let title = mux
                                .get_window(window_id)
                                .map(|win| win.get_title().to_string());
                            if let Some(title) = title {
                                inner
                                    .client
                                    .set_window_title(codec::WindowTitleChanged {
                                        window_id: remote_window_id,
                                        title,
                                    })
                                    .await?;
                            }
                        }
                        anyhow::Result::<()>::Ok(())
                    };
                    if let Err(err) = mux.try_spawn_client_invalidation(future) {
                        log::error!("unable to schedule remote window title update: {err:#}");
                    }
                }
            }
        }
        _ => {}
    }
    true
}

impl ClientDomain {
    pub fn new(config: ClientDomainConfig) -> Self {
        let local_domain_id = alloc_domain_id();
        let label = config.label();
        Mux::get().subscribe(move |notif| mux_notify_client_domain(local_domain_id, notif));
        Self {
            config,
            label,
            inner: Mutex::new(None),
            local_domain_id,
        }
    }

    fn inner(&self) -> Option<Arc<ClientInner>> {
        self.inner.lock().unwrap().as_ref().map(Arc::clone)
    }

    pub fn connect_automatically(&self) -> bool {
        self.config.connect_automatically()
    }

    fn lifecycle_failure(error: &HeadlessLifecycleError) -> HeadlessConnectionFailure {
        match error {
            HeadlessLifecycleError::Saturated => HeadlessConnectionFailure::LifecycleSaturated,
            HeadlessLifecycleError::Closed | HeadlessLifecycleError::Empty => {
                HeadlessConnectionFailure::LifecycleClosed
            }
            HeadlessLifecycleError::Admission(_) => HeadlessConnectionFailure::Runtime,
        }
    }

    fn headless_attach_error(
        lifecycle: &HeadlessConnectionLifecycle,
        failure: HeadlessConnectionFailure,
        error: anyhow::Error,
    ) -> anyhow::Error {
        match lifecycle.publish_failed(failure) {
            Ok(()) => error,
            Err(publish_error) => error.context(format!(
                "failed to publish terminal headless lifecycle state: {publish_error}"
            )),
        }
    }

    /// Attach without constructing a terminal-backed connection UI.
    ///
    /// The supplied lifecycle must be backed by this mux's admission owner;
    /// it remains the sole observer for the initial connection and all later
    /// reconnects.
    pub async fn attach_with_lifecycle(
        &self,
        window_id: Option<WindowId>,
        lifecycle: &HeadlessConnectionLifecycle,
        client_id: ClientId,
    ) -> anyhow::Result<()> {
        let mux = Mux::get();
        let admission = mux.admission().clone();
        if !lifecycle.uses_admission(&admission) {
            return Err(Self::headless_attach_error(
                lifecycle,
                HeadlessConnectionFailure::Runtime,
                anyhow!("headless lifecycle does not belong to this mux runtime"),
            ));
        }

        if self.state() == DomainState::Attached {
            if let Err(error) = lifecycle.publish_ready() {
                return Err(Self::headless_attach_error(
                    lifecycle,
                    Self::lifecycle_failure(&error),
                    error.into(),
                ));
            }
            return Ok(());
        }

        if let Err(error) = lifecycle.publish_attaching() {
            return Err(Self::headless_attach_error(
                lifecycle,
                Self::lifecycle_failure(&error),
                error.into(),
            ));
        }
        let client = match Client::new_headless(
            Some(self.local_domain_id),
            self.config.clone(),
            admission,
            lifecycle,
            client_id,
            true,
            false,
        )
        .await
        {
            Ok(client) => client,
            // The headless constructor owns initial-connection failure
            // publication and joins its runtime before returning an error.
            Err(error) => return Err(error),
        };

        let panes = match client.list_panes().await {
            Ok(panes) => panes,
            Err(error) => {
                let outcome = client.shutdown_and_join();
                log::debug!("headless pane-list cleanup outcome: {outcome:?}");
                return Err(Self::headless_attach_error(
                    lifecycle,
                    HeadlessConnectionFailure::Runtime,
                    error,
                ));
            }
        };
        let executor = match mux.headless_executor() {
            Ok(executor) => executor,
            Err(error) => {
                return Err(Self::headless_attach_error(
                    lifecycle,
                    HeadlessConnectionFailure::Runtime,
                    error.context("headless attachment requires the mux owner executor"),
                ));
            }
        };
        let domain_id = self.local_domain_id;
        let finish = match executor
            .try_spawn(async move { Self::finish_attach(domain_id, client, panes, window_id) })
        {
            Ok(finish) => finish,
            Err(error) => {
                return Err(Self::headless_attach_error(
                    lifecycle,
                    HeadlessConnectionFailure::Runtime,
                    anyhow::Error::new(error)
                        .context("schedule pane projection on the mux owner executor"),
                ));
            }
        };
        let finish_result = match finish.await {
            Ok(result) => result,
            Err(error) => {
                return Err(Self::headless_attach_error(
                    lifecycle,
                    HeadlessConnectionFailure::Runtime,
                    anyhow::Error::new(error)
                        .context("join pane projection on the mux owner executor"),
                ));
            }
        };
        if let Err(error) = finish_result {
            let inner = self.inner.lock().unwrap().take();
            if let Some(inner) = inner {
                let outcome = inner.client.shutdown_and_join();
                log::debug!("headless attach cleanup outcome: {outcome:?}");
            }
            mux.domain_was_detached(self.local_domain_id);
            return Err(Self::headless_attach_error(
                lifecycle,
                HeadlessConnectionFailure::Runtime,
                error,
            ));
        }

        if let Err(error) = lifecycle.publish_ready() {
            let failure = Self::lifecycle_failure(&error);
            self.perform_detach();
            return Err(Self::headless_attach_error(
                lifecycle,
                failure,
                error.into(),
            ));
        }

        Ok(())
    }

    pub fn perform_detach(&self) {
        log::info!("detached domain {}", self.local_domain_id);
        // Retain the projection mux across client shutdown. Headless owners keep the global slot
        // active for this explicit detach phase, so projected tasks can finish before the domain is
        // removed without racing a second global lookup.
        let mux = Mux::try_get();
        let inner = self.inner.lock().unwrap().take();
        if let Some(inner) = inner {
            let outcome = inner.client.shutdown_and_join();
            log::debug!("client runtime detach outcome: {outcome:?}");
        }
        if let Some(mux) = mux {
            mux.domain_was_detached(self.local_domain_id);
        }
    }

    pub fn attached_client(&self) -> Option<Client> {
        self.inner().map(|inner| inner.client.clone())
    }

    pub fn remote_to_local_pane_id(&self, remote_pane_id: TabId) -> Option<TabId> {
        let inner = self.inner()?;
        inner.remote_to_local_pane_id(remote_pane_id)
    }

    pub fn remote_to_local_window_id(&self, remote_window_id: WindowId) -> Option<WindowId> {
        let inner = self.inner()?;
        inner.remote_to_local_window(remote_window_id)
    }

    pub fn local_to_remote_window_id(&self, local_window_id: WindowId) -> Option<WindowId> {
        let inner = self.inner()?;
        inner.local_to_remote_window(local_window_id)
    }

    pub fn local_to_remote_tab_id(&self, local_tab_id: TabId) -> Option<TabId> {
        let inner = self.inner()?;
        inner.local_to_remote_tab(local_tab_id)
    }

    pub fn get_client_inner_for_domain(domain_id: DomainId) -> anyhow::Result<Arc<ClientInner>> {
        let mux = Mux::get();
        let domain = mux
            .get_domain(domain_id)
            .ok_or_else(|| anyhow!("invalid domain id {}", domain_id))?;
        let domain = domain
            .downcast_ref::<Self>()
            .ok_or_else(|| anyhow!("domain {} is not a ClientDomain", domain_id))?;

        if let Some(inner) = domain.inner() {
            Ok(inner)
        } else {
            bail!("domain has no assigned client");
        }
    }

    /// The reader in the mux may have decided to give up on one or
    /// more tabs at the time that a disconnect was detected, and
    /// it's also possible that another client connected and adjusted
    /// the set of tabs since we were connected, so we need to re-sync.
    pub async fn reattach(domain_id: DomainId) -> anyhow::Result<()> {
        let inner = Self::get_client_inner_for_domain(domain_id)?;

        let panes = inner.client.list_panes().await?;
        Self::process_pane_list(Arc::clone(&inner), panes, None)?;
        inner.client.publish_ready()?;
        Ok(())
    }

    pub async fn resync(&self) -> anyhow::Result<()> {
        if let Some(inner) = self.inner() {
            let panes = inner.client.list_panes().await?;
            Self::process_pane_list(inner, panes, None)?;
        }
        Ok(())
    }

    pub fn process_remote_window_title_change(&self, remote_window_id: WindowId, title: String) {
        if let Some(inner) = self.inner() {
            if let Some(local_window_id) = inner.remote_to_local_window(remote_window_id) {
                if let Some(mut window) = Mux::get().get_window_mut(local_window_id) {
                    window.set_title(&title);
                }
            }
        }
    }

    pub fn process_remote_tab_title_change(&self, remote_tab_id: TabId, title: String) {
        if let Some(inner) = self.inner() {
            if let Some(local_tab_id) = inner.remote_to_local_tab_id(remote_tab_id) {
                if let Some(tab) = Mux::get().get_tab(local_tab_id) {
                    tab.set_title(&title);
                }
            }
        }
    }

    fn process_pane_list(
        inner: Arc<ClientInner>,
        panes: AdmittedRpcResponse<ListPanesResponse>,
        mut primary_window_id: Option<WindowId>,
    ) -> anyhow::Result<()> {
        let (_serial, panes, _reservation) = panes.into_parts();
        let mux = Mux::get();
        log::debug!(
            "domain {}: ListPanes result {:#?}",
            inner.local_domain_id,
            panes
        );

        // "Mark" the current set of known remote ids, so that we can "Sweep"
        // any unreferenced ids at the bottom, garbage collection style
        let mut remote_windows_to_forget: HashSet<WindowId> = inner
            .remote_to_local_window
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect();
        let mut remote_tabs_to_forget: HashSet<WindowId> = inner
            .remote_to_local_tab
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect();
        let mut remote_panes_to_forget: HashSet<WindowId> = inner
            .remote_to_local_pane
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect();

        for (tabroot, tab_title) in panes.tabs.into_iter().zip(panes.tab_titles.iter()) {
            let root_size = match tabroot.root_size() {
                Some(size) => size,
                None => continue,
            };

            if let Some((remote_window_id, remote_tab_id)) = tabroot.window_and_tab_ids() {
                let tab;

                remote_windows_to_forget.remove(&remote_window_id);
                remote_tabs_to_forget.remove(&remote_tab_id);

                if let Some(tab_id) = inner.remote_to_local_tab_id(remote_tab_id) {
                    match mux.get_tab(tab_id) {
                        Some(t) => tab = t,
                        None => {
                            // We likely decided that we hit EOF on the tab and
                            // removed it from the mux.  Let's add it back, but
                            // with a new id.
                            log::trace!(
                                "we had remote_to_local_tab_id mapping of \
                                 {remote_tab_id} -> {tab_id}, but the local \
                                 tab is not in the mux, make a new tab"
                            );
                            inner.remove_old_tab_mapping(remote_tab_id);
                            tab = Arc::new(Tab::new(&root_size));
                            inner.record_remote_to_local_tab_mapping(remote_tab_id, tab.tab_id());
                            mux.add_tab_no_panes(&tab)?;
                        }
                    };
                } else {
                    tab = Arc::new(Tab::new(&root_size));
                    mux.add_tab_no_panes(&tab)?;
                    inner.record_remote_to_local_tab_mapping(remote_tab_id, tab.tab_id());
                }
                tab.set_title(tab_title);

                log::debug!("domain: {} tree: {:#?}", inner.local_domain_id, tabroot);
                let mut workspace = None;
                let mut pane_registration_error = None;
                tab.sync_with_pane_tree(root_size, tabroot, |entry| {
                    workspace.replace(entry.workspace.clone());
                    remote_panes_to_forget.remove(&entry.pane_id);
                    if let Some(pane_id) = inner.remote_to_local_pane_id(entry.pane_id) {
                        match mux.get_pane(pane_id) {
                            Some(pane) => pane,
                            None => {
                                // We likely decided that we hit EOF on the tab and
                                // removed it from the mux.  Let's add it back, but
                                // with a new id.
                                inner.remove_old_pane_mapping(entry.pane_id);
                                let client_pane = Arc::new(ClientPane::new(
                                    &inner,
                                    entry.tab_id,
                                    entry.pane_id,
                                    entry.size,
                                    &entry.title,
                                ));
                                let pane: Arc<dyn Pane> = client_pane.clone();
                                if let Err(err) =
                                    mux.add_pane(&pane).and_then(|_| client_pane.start())
                                {
                                    pane_registration_error.get_or_insert(err);
                                }
                                pane
                            }
                        }
                    } else {
                        let client_pane = Arc::new(ClientPane::new(
                            &inner,
                            entry.tab_id,
                            entry.pane_id,
                            entry.size,
                            &entry.title,
                        ));
                        let pane: Arc<dyn Pane> = client_pane.clone();
                        log::debug!(
                            "domain: {} attaching to remote pane {:?} -> local pane_id {}",
                            inner.local_domain_id,
                            entry,
                            pane.pane_id()
                        );
                        if let Err(err) = mux.add_pane(&pane).and_then(|_| client_pane.start()) {
                            pane_registration_error.get_or_insert(err);
                        }
                        pane
                    }
                });
                if let Some(err) = pane_registration_error {
                    return Err(err);
                }

                if let Some(local_window_id) = inner.remote_to_local_window(remote_window_id) {
                    let mut window = mux
                        .get_window_mut(local_window_id)
                        .expect("no such window!?");
                    log::debug!(
                        "domain: {} adding tab to existing local window {}",
                        inner.local_domain_id,
                        local_window_id
                    );
                    if window.idx_by_id(tab.tab_id()).is_none() {
                        window.push(&tab);
                    }
                    continue;
                }

                if let Some(local_window_id) = primary_window_id {
                    // Verify that the workspace is consistent between the local and remote
                    // windows
                    if Some(
                        mux.get_window(local_window_id)
                            .expect("primary window to be valid")
                            .get_workspace(),
                    ) == workspace.as_deref()
                    {
                        // Yes! We can use this window
                        log::debug!(
                            "adding remote window {} as tab to local window {}",
                            remote_window_id,
                            local_window_id
                        );
                        inner.record_remote_to_local_window_mapping(
                            remote_window_id,
                            local_window_id,
                        );
                        mux.add_tab_to_window(&tab, local_window_id)?;
                        primary_window_id.take();
                        continue;
                    }
                }
                log::debug!(
                    "making new local window for remote {} in workspace {:?}",
                    remote_window_id,
                    workspace
                );
                let position = None;
                let local_window_id = mux.new_empty_window(workspace.take(), position);
                inner.record_remote_to_local_window_mapping(remote_window_id, *local_window_id);
                mux.add_tab_to_window(&tab, *local_window_id)?;
            }
        }

        for (remote_window_id, window_title) in panes.window_titles {
            if let Some(local_window_id) = inner.remote_to_local_window(remote_window_id) {
                let mut window = mux
                    .get_window_mut(local_window_id)
                    .expect("no such window!?");
                window.set_title(&window_title);
            }
        }

        // "Sweep" away our mapping for ids that are no longer present in the
        // latest sync
        log::debug!(
            "after sync, remote_windows_to_forget={remote_windows_to_forget:?}, \
                    remote_tabs_to_forget={remote_tabs_to_forget:?}, \
                    remote_panes_to_forget={remote_panes_to_forget:?}"
        );
        if !remote_windows_to_forget.is_empty() {
            let mut windows = inner.remote_to_local_window.lock().unwrap();
            for w in remote_windows_to_forget {
                windows.remove(&w);
            }
        }
        if !remote_tabs_to_forget.is_empty() {
            let mut tabs = inner.remote_to_local_tab.lock().unwrap();
            for t in remote_tabs_to_forget {
                tabs.remove(&t);
            }
        }
        if !remote_panes_to_forget.is_empty() {
            for remote_pane_id in remote_panes_to_forget {
                let local_pane_id = {
                    inner
                        .remote_to_local_pane
                        .lock()
                        .unwrap()
                        .remove(&remote_pane_id)
                };
                if let Some(local_pane_id) = local_pane_id {
                    mux.unregister_pane(local_pane_id);
                }
            }
        }

        Ok(())
    }

    fn finish_attach(
        domain_id: DomainId,
        client: Client,
        panes: AdmittedRpcResponse<ListPanesResponse>,
        primary_window_id: Option<WindowId>,
    ) -> anyhow::Result<()> {
        let mux = Mux::get();
        if mux.is_headless_runtime() && !mux.is_main_thread() {
            bail!("headless pane projection must run on the mux owner thread");
        }
        let domain = mux
            .get_domain(domain_id)
            .ok_or_else(|| anyhow!("invalid domain id {}", domain_id))?;
        let domain = domain
            .downcast_ref::<Self>()
            .ok_or_else(|| anyhow!("domain {} is not a ClientDomain", domain_id))?;
        let threshold = domain.config.local_echo_threshold_ms();
        let overlay_lag_indicator = domain.config.overlay_lag_indicator();

        let inner = Arc::new(ClientInner::new(
            domain_id,
            client,
            threshold,
            overlay_lag_indicator,
        ));
        *domain.inner.lock().unwrap() = Some(Arc::clone(&inner));

        Self::process_pane_list(inner, panes, primary_window_id)?;

        Ok(())
    }
}

#[async_trait(?Send)]
impl Domain for ClientDomain {
    fn domain_id(&self) -> DomainId {
        self.local_domain_id
    }

    fn domain_name(&self) -> &str {
        self.config.name()
    }

    async fn domain_label(&self) -> String {
        self.label.to_string()
    }

    async fn spawn_pane(
        &self,
        _size: TerminalSize,
        _command: Option<CommandBuilder>,
        _command_dir: Option<String>,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        anyhow::bail!("spawn_pane not implemented for ClientDomain")
    }

    /// Forward the request to the remote; we need to translate the local ids
    /// to those that match the remote for the request, resync the changed
    /// structure, and then translate the results back to local
    async fn move_pane_to_new_tab(
        &self,
        pane_id: PaneId,
        window_id: Option<WindowId>,
        workspace_for_new_window: Option<String>,
    ) -> anyhow::Result<Option<(Arc<Tab>, WindowId)>> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;

        let local_pane = Mux::get()
            .get_pane(pane_id)
            .ok_or_else(|| anyhow!("pane_id {} is invalid", pane_id))?;
        let pane = local_pane
            .downcast_ref::<ClientPane>()
            .ok_or_else(|| anyhow!("pane_id {} is not a ClientPane", pane_id))?;

        let remote_window_id =
            window_id.and_then(|local_window| self.local_to_remote_window_id(local_window));

        let result = inner
            .client
            .move_pane_to_new_tab(codec::MovePaneToNewTab {
                pane_id: pane.remote_pane_id,
                window_id: remote_window_id,
                workspace_for_new_window,
            })
            .await?
            .into_inner();

        self.resync().await?;

        let local_tab_id = inner
            .remote_to_local_tab_id(result.tab_id)
            .ok_or_else(|| anyhow!("remote tab {} didn't resolve after resync", result.tab_id))?;

        let local_win_id = self
            .remote_to_local_window_id(result.window_id)
            .ok_or_else(|| {
                anyhow!(
                    "remote window {} didn't resolve after resync",
                    result.window_id
                )
            })?;

        let tab = Mux::get()
            .get_tab(local_tab_id)
            .ok_or_else(|| anyhow!("local tab {local_tab_id} is invalid"))?;

        Ok(Some((tab, local_win_id)))
    }

    async fn spawn(
        &self,
        size: TerminalSize,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
        window: WindowId,
    ) -> anyhow::Result<Arc<Tab>> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;

        let placement = match inner.local_to_remote_window(window) {
            Some(window_id) => TabSpawnPlacement::ExistingWindow { window_id },
            None => TabSpawnPlacement::NewWindow {
                size,
                workspace: Mux::get().active_workspace(),
            },
        };
        let (command, builder_command_dir) = environment_free_command(command)?;

        let result = inner
            .client
            .spawn_v2(SpawnV2 {
                domain: TabSpawnDomain::DefaultDomain,
                placement,
                command,
                command_dir: command_dir.or(builder_command_dir),
            })
            .await?
            .into_inner();

        inner.record_remote_to_local_window_mapping(result.window_id, window);

        let client_pane = Arc::new(ClientPane::new(
            &inner,
            result.tab_id,
            result.pane_id,
            result.size,
            "wezterm",
        ));
        let pane: Arc<dyn Pane> = client_pane.clone();
        let tab = Arc::new(Tab::new(&result.size));
        tab.assign_pane(&pane);
        inner.remove_old_tab_mapping(result.tab_id);
        inner.record_remote_to_local_tab_mapping(result.tab_id, tab.tab_id());

        let mux = Mux::get();
        mux.add_tab_and_active_pane(&tab)?;
        client_pane.start()?;
        mux.add_tab_to_window(&tab, window)?;

        Ok(tab)
    }

    async fn split_pane(
        &self,
        source: SplitSource,
        tab_id: TabId,
        pane_id: PaneId,
        split_request: SplitRequest,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;

        let mux = Mux::get();

        let tab = mux
            .get_tab(tab_id)
            .ok_or_else(|| anyhow!("tab_id {} is invalid", tab_id))?;
        let local_pane = mux
            .get_pane(pane_id)
            .ok_or_else(|| anyhow!("pane_id {} is invalid", pane_id))?;
        let pane = local_pane
            .downcast_ref::<ClientPane>()
            .ok_or_else(|| anyhow!("pane_id {} is not a ClientPane", pane_id))?;

        let source = match source {
            SplitSource::Spawn {
                command,
                command_dir,
            } => {
                let (command, builder_command_dir) = environment_free_command(command)?;
                SplitSpawnSource::Spawn {
                    command,
                    command_dir: command_dir.or(builder_command_dir),
                }
            }
            SplitSource::MovePane(pane_id) => SplitSpawnSource::MovePane { pane_id },
        };

        let result = inner
            .client
            .split_pane(SplitPane {
                domain: SplitSpawnDomain::TargetPaneDomain,
                target_pane_id: pane.remote_pane_id,
                split_request,
                source,
            })
            .await?
            .into_inner();

        let client_pane = Arc::new(ClientPane::new(
            &inner,
            result.tab_id,
            result.pane_id,
            result.size,
            "wezterm",
        ));
        let pane: Arc<dyn Pane> = client_pane.clone();

        let pane_index = match tab
            .iter_panes()
            .iter()
            .find(|p| p.pane.pane_id() == pane_id)
        {
            Some(p) => p.index,
            None => anyhow::bail!("invalid pane id {}", pane_id),
        };

        tab.split_and_insert(pane_index, split_request, Arc::clone(&pane))
            .ok();

        mux.add_pane(&pane)?;
        client_pane.start()?;

        Ok(pane)
    }

    async fn attach(&self, window_id: Option<WindowId>) -> anyhow::Result<()> {
        if self.state() == DomainState::Attached {
            // Already attached
            return Ok(());
        }

        let domain_id = self.local_domain_id;
        let config = self.config.clone();
        let admission = Mux::get().admission().clone();

        let activity = mux::activity::Activity::new();
        let ui = ConnectionUI::with_params(ConnectionUIParams {
            window_id,
            ..Default::default()
        });
        ui.title("wezterm: Connecting...");

        ui.async_run_and_log_error({
            let ui = ui.clone();
            async move {
                let mut cloned_ui = ui.clone();
                let client = spawn_into_new_thread(move || match &config {
                    ClientDomainConfig::Unix(unix) => {
                        let initial = true;
                        let no_auto_start = false;
                        Client::new_unix_domain(
                            admission,
                            Some(domain_id),
                            unix,
                            initial,
                            &mut cloned_ui,
                            no_auto_start,
                        )
                    }
                    ClientDomainConfig::Tls(tls) => {
                        Client::new_tls(admission, domain_id, tls, &mut cloned_ui)
                    }
                    ClientDomainConfig::Ssh(ssh) => {
                        Client::new_ssh(admission, domain_id, ssh, &mut cloned_ui)
                    }
                })
                .await?;

                ui.output_str("Requesting pane list...\n");
                let panes = client.list_panes().await?;
                ui.output_str(&format!(
                    "Server has {} tabs.  Attaching to local UI...\n",
                    panes.value().tabs.len()
                ));
                ClientDomain::finish_attach(domain_id, client, panes, window_id)
            }
        })
        .await
        .map_err(|e| {
            ui.output_str(&format!("Error during attach: {:#}\n", e));
            e
        })?;

        ui.output_str("Attached!\n");
        drop(activity);
        ui.close();
        Ok(())
    }

    fn detachable(&self) -> bool {
        true
    }

    fn detach(&self) -> anyhow::Result<()> {
        self.perform_detach();
        Ok(())
    }

    fn state(&self) -> DomainState {
        if self.inner.lock().unwrap().is_some() {
            DomainState::Attached
        } else {
            DomainState::Detached
        }
    }
}
