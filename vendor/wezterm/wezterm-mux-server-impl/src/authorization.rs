use crate::dispatch::{ControlPublisher, ControlSubscription};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use codec::{
    BuildIdentity, ConnectionIdentity, ControlLeaseAction, ControlLeaseResult, DecodeReservation,
    Pdu, RequestAuthority, RequestOperation, ServiceDrainAction, ServiceDrainResult,
};
use mux::client::ClientId;
use mux::pane::PaneId;
use wezterm_runtime_admission::{CombinedPermit, RuntimeAdmission};

/// An identity admitted and issued by the server for one established session.
///
/// The wire-level ClientId is presentation metadata. Keeping construction of
/// this type private prevents ordinary authorization from accepting those raw,
/// caller-controlled claims as proof that bootstrap completed.
#[derive(Clone, Debug)]
pub struct ServerIssuedIdentity {
    connection: ConnectionIdentity,
    client_id: Arc<ClientId>,
}

impl ServerIssuedIdentity {
    pub fn connection(&self) -> ConnectionIdentity {
        self.connection
    }

    pub fn client_id(&self) -> &Arc<ClientId> {
        &self.client_id
    }
}

pub trait RequestAuthorizer: Send + Sync + 'static {
    fn authorize_registration(
        &self,
        proxy: Option<&ClientId>,
        client_id: &ClientId,
        is_proxy: bool,
    ) -> anyhow::Result<()>;

    fn authorize_bootstrap(&self, operation: RequestOperation, request: &Pdu)
        -> anyhow::Result<()>;

    fn authorize(
        &self,
        identity: &ServerIssuedIdentity,
        operation: RequestOperation,
        request: &Pdu,
    ) -> anyhow::Result<()>;
}

pub struct AllowAllRequests;

impl RequestAuthorizer for AllowAllRequests {
    fn authorize_registration(
        &self,
        _proxy: Option<&ClientId>,
        _client_id: &ClientId,
        _is_proxy: bool,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn authorize_bootstrap(
        &self,
        _operation: RequestOperation,
        _request: &Pdu,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn authorize(
        &self,
        _identity: &ServerIssuedIdentity,
        _operation: RequestOperation,
        _request: &Pdu,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Closed baseline for hosts that have not installed an ordinary-request policy.
pub struct DenyOrdinaryRequests;

impl RequestAuthorizer for DenyOrdinaryRequests {
    fn authorize_registration(
        &self,
        _proxy: Option<&ClientId>,
        _client_id: &ClientId,
        _is_proxy: bool,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn authorize_bootstrap(
        &self,
        _operation: RequestOperation,
        _request: &Pdu,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn authorize(
        &self,
        _identity: &ServerIssuedIdentity,
        operation: RequestOperation,
        _request: &Pdu,
    ) -> anyhow::Result<()> {
        anyhow::bail!("ordinary request {operation:?} is denied by host policy")
    }
}

pub struct ServerPolicy {
    authorizer: Arc<dyn RequestAuthorizer>,
    build_identity: BuildIdentity,
    control: Arc<ControlPublisher>,
    drain: Mutex<ServiceDrainState>,
}

#[derive(Default)]
struct ServiceDrainState {
    owner: Option<ConnectionIdentity>,
    spawns_in_flight: usize,
}

pub struct SpawnDispatchPermit {
    policy: Arc<ServerPolicy>,
}

impl Drop for SpawnDispatchPermit {
    fn drop(&mut self) {
        let mut drain = self.policy.drain.lock().unwrap();
        drain.spawns_in_flight = drain.spawns_in_flight.saturating_sub(1);
    }
}

impl ServerPolicy {
    pub fn new(authorizer: Arc<dyn RequestAuthorizer>, build_identity: BuildIdentity) -> Arc<Self> {
        Arc::new(Self {
            authorizer,
            build_identity,
            control: ControlPublisher::new(),
            drain: Mutex::new(ServiceDrainState::default()),
        })
    }

    pub fn bind_admission(&self, admission: &Arc<RuntimeAdmission>) -> anyhow::Result<()> {
        self.control.bind_admission(admission)
    }

    pub fn authorize_proxy_registration(&self, client_id: &ClientId) -> anyhow::Result<()> {
        self.authorizer
            .authorize_registration(None, client_id, true)
    }

    pub fn issue_identity(
        &self,
        proxy: Option<&ClientId>,
        client_id: ClientId,
    ) -> anyhow::Result<ServerIssuedIdentity> {
        self.authorizer
            .authorize_registration(proxy, &client_id, false)?;
        let connection = self.control.issue_identity()?;
        Ok(ServerIssuedIdentity {
            connection,
            client_id: Arc::new(client_id),
        })
    }

    pub fn authorize_bootstrap(
        &self,
        operation: RequestOperation,
        request: &Pdu,
    ) -> anyhow::Result<()> {
        self.authorizer.authorize_bootstrap(operation, request)
    }

    pub fn authorize(
        &self,
        identity: &ServerIssuedIdentity,
        operation: RequestOperation,
        request: &Pdu,
    ) -> anyhow::Result<()> {
        self.authorizer.authorize(identity, operation, request)?;
        if operation == RequestOperation::Spawn && self.drain.lock().unwrap().owner.is_some() {
            anyhow::bail!("Console service shutdown is draining new sessions");
        }
        if operation == RequestOperation::ServiceDrain {
            let Pdu::ServiceDrainRequest(request) = request else {
                anyhow::bail!("service drain operation requires a service drain request");
            };
            let mut drain = self.drain.lock().unwrap();
            match (request.action, drain.owner) {
                (ServiceDrainAction::Begin, None) => drain.owner = Some(identity.connection()),
                (ServiceDrainAction::Begin, Some(current)) if current == identity.connection() => {}
                (ServiceDrainAction::Begin, Some(_)) => {
                    anyhow::bail!("another connection owns the Console service drain")
                }
                (ServiceDrainAction::Cancel, Some(current)) if current == identity.connection() => {
                }
                (ServiceDrainAction::Cancel, _) => {
                    anyhow::bail!("this connection does not own the Console service drain")
                }
            }
        }
        match request.request_authority()? {
            RequestAuthority::PaneControl(targets) => {
                for pane_id in
                    IntoIterator::into_iter([Some(targets.primary), targets.secondary]).flatten()
                {
                    if !self.control.is_controller(pane_id, identity.connection()) {
                        anyhow::bail!(
                            "connection {} is an observer for pane {}",
                            identity.connection().get(),
                            pane_id
                        );
                    }
                }
            }
            RequestAuthority::ControlLease(_)
            | RequestAuthority::Observe
            | RequestAuthority::Bootstrap
            | RequestAuthority::UntargetedMutation
            | RequestAuthority::HostSensitive => {}
        }
        Ok(())
    }

    pub fn subscribe_control(
        self: &Arc<Self>,
        identity: &ServerIssuedIdentity,
    ) -> anyhow::Result<ControlSubscription> {
        self.control.subscribe(identity.connection())
    }

    pub fn apply_control(
        &self,
        identity: &ServerIssuedIdentity,
        pane_id: PaneId,
        action: ControlLeaseAction,
    ) -> ControlLeaseResult {
        self.control.apply(pane_id, identity.connection(), action)
    }

    pub fn remove_controlled_pane(&self, pane_id: PaneId) -> bool {
        self.control.remove_pane(pane_id)
    }

    pub fn expire_control_grace(&self, identity: ConnectionIdentity) -> bool {
        let changed = self.control.expire_grace(identity);
        let mut drain = self.drain.lock().unwrap();
        if drain.owner == Some(identity) {
            drain.owner = None;
        }
        changed
    }

    pub fn reserve_spawn(self: &Arc<Self>) -> anyhow::Result<SpawnDispatchPermit> {
        let mut drain = self.drain.lock().unwrap();
        if drain.owner.is_some() {
            anyhow::bail!("Console service shutdown is draining new sessions");
        }
        drain.spawns_in_flight = drain
            .spawns_in_flight
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Console spawn accounting overflow"))?;
        Ok(SpawnDispatchPermit {
            policy: Arc::clone(self),
        })
    }

    pub async fn apply_service_drain(
        &self,
        identity: &ServerIssuedIdentity,
        action: ServiceDrainAction,
    ) -> anyhow::Result<ServiceDrainResult> {
        match action {
            ServiceDrainAction::Begin => loop {
                let spawns_in_flight = {
                    let drain = self.drain.lock().unwrap();
                    if drain.owner != Some(identity.connection()) {
                        anyhow::bail!("this connection does not own the Console service drain");
                    }
                    drain.spawns_in_flight
                };
                if spawns_in_flight == 0 {
                    return Ok(ServiceDrainResult { draining: true });
                }
                smol::Timer::after(Duration::from_millis(1)).await;
            },
            ServiceDrainAction::Cancel => {
                let mut drain = self.drain.lock().unwrap();
                if drain.owner != Some(identity.connection()) {
                    anyhow::bail!("this connection does not own the Console service drain");
                }
                drain.owner = None;
                Ok(ServiceDrainResult { draining: false })
            }
        }
    }

    pub fn build_identity(&self) -> &BuildIdentity {
        &self.build_identity
    }
}

/// A decoded request that has crossed bootstrap, semantic target, and host authorization.
pub struct AdmittedAuthorizedRequest {
    pub(crate) serial: u64,
    pub(crate) pdu: Pdu,
    pub(crate) operation: RequestOperation,
    pub(crate) identity: Option<ServerIssuedIdentity>,
    pub(crate) split_domain_id: Option<usize>,
    pub(crate) decode_reservation: DecodeReservation,
    pub(crate) inbound: CombinedPermit,
}

impl AdmittedAuthorizedRequest {
    pub(crate) fn new(
        serial: u64,
        pdu: Pdu,
        operation: RequestOperation,
        identity: Option<ServerIssuedIdentity>,
        split_domain_id: Option<usize>,
        decode_reservation: DecodeReservation,
        inbound: CombinedPermit,
    ) -> Self {
        Self {
            serial,
            pdu,
            operation,
            identity,
            split_domain_id,
            decode_reservation,
            inbound,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codec::{
        ControlLeaseRequest, Resize, SplitPane, SplitSpawnDomain, SplitSpawnSource, WriteToPane,
    };
    use mux::tab::{SplitDirection, SplitRequest, SplitSize};
    use wezterm_runtime_admission::{RuntimeAdmission, RuntimeRole};
    use wezterm_term::TerminalSize;

    fn identity(policy: &ServerPolicy, client_id: ClientId) -> ServerIssuedIdentity {
        policy.issue_identity(None, client_id).unwrap()
    }

    #[test]
    fn caller_metadata_cannot_forge_the_server_connection_identity() {
        let policy = ServerPolicy::new(
            Arc::new(AllowAllRequests),
            BuildIdentity {
                product: "test".to_string(),
                version: "test".to_string(),
                source_revision: None,
                source_dirty: None,
                embedded_wezterm_revision: None,
            },
        );
        let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        policy.bind_admission(&admission).unwrap();
        let metadata = ClientId::new();
        let first = identity(&policy, metadata.clone());
        let second = identity(&policy, metadata);

        assert_ne!(first.connection(), second.connection());
        assert_eq!(first.client_id().hostname, second.client_id().hostname);
    }

    #[test]
    fn service_drain_closes_spawn_admission_until_cancelled() {
        let policy = ServerPolicy::new(
            Arc::new(AllowAllRequests),
            BuildIdentity {
                product: "test".to_string(),
                version: "test".to_string(),
                source_revision: None,
                source_dirty: None,
                embedded_wezterm_revision: None,
            },
        );
        let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        policy.bind_admission(&admission).unwrap();
        let manager = identity(&policy, ClientId::new());
        let in_flight = policy.reserve_spawn().unwrap();
        let begin = Pdu::ServiceDrainRequest(codec::ServiceDrainRequest {
            action: ServiceDrainAction::Begin,
        });

        policy
            .authorize(&manager, RequestOperation::ServiceDrain, &begin)
            .unwrap();
        assert!(policy.reserve_spawn().is_err());
        drop(in_flight);
        let result =
            smol::block_on(policy.apply_service_drain(&manager, ServiceDrainAction::Begin))
                .unwrap();
        assert!(result.draining);

        let cancel = Pdu::ServiceDrainRequest(codec::ServiceDrainRequest {
            action: ServiceDrainAction::Cancel,
        });
        policy
            .authorize(&manager, RequestOperation::ServiceDrain, &cancel)
            .unwrap();
        let result =
            smol::block_on(policy.apply_service_drain(&manager, ServiceDrainAction::Cancel))
                .unwrap();
        assert!(!result.draining);
        assert!(policy.reserve_spawn().is_ok());
    }

    #[test]
    fn observers_cannot_send_input_or_resize_and_takeover_is_atomic() {
        let policy = ServerPolicy::new(
            Arc::new(AllowAllRequests),
            BuildIdentity {
                product: "test".to_string(),
                version: "test".to_string(),
                source_revision: None,
                source_dirty: None,
                embedded_wezterm_revision: None,
            },
        );
        let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        policy.bind_admission(&admission).unwrap();
        let controller = identity(&policy, ClientId::new());
        let observer = identity(&policy, ClientId::new());
        assert!(matches!(
            policy.apply_control(&controller, 9, ControlLeaseAction::Acquire),
            ControlLeaseResult::Acquired(_)
        ));

        let input = Pdu::WriteToPane(WriteToPane {
            pane_id: 9,
            data: b"input".to_vec(),
        });
        assert!(policy
            .authorize(&controller, RequestOperation::WriteToPane, &input)
            .is_ok());
        assert!(policy
            .authorize(&observer, RequestOperation::WriteToPane, &input)
            .is_err());
        let resize = Pdu::Resize(Resize {
            containing_tab_id: 1,
            pane_id: 9,
            size: TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
                dpi: 96,
            },
        });
        assert!(policy
            .authorize(&observer, RequestOperation::Resize, &resize)
            .is_err());

        assert!(matches!(
            policy.apply_control(&observer, 9, ControlLeaseAction::Take),
            ControlLeaseResult::Taken(_)
        ));
        assert!(policy
            .authorize(&controller, RequestOperation::WriteToPane, &input)
            .is_err());
        assert!(policy
            .authorize(&observer, RequestOperation::WriteToPane, &input)
            .is_ok());

        let lease = Pdu::ControlLeaseRequest(ControlLeaseRequest {
            pane_id: 9,
            action: ControlLeaseAction::Release,
        });
        assert!(policy
            .authorize(&observer, RequestOperation::ControlLease, &lease)
            .is_ok());
    }

    #[test]
    fn deny_by_default_host_policy_rejects_sensitive_requests() {
        let policy = ServerPolicy::new(
            Arc::new(DenyOrdinaryRequests),
            BuildIdentity {
                product: "test".to_string(),
                version: "test".to_string(),
                source_revision: None,
                source_dirty: None,
                embedded_wezterm_revision: None,
            },
        );
        let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        policy.bind_admission(&admission).unwrap();
        let identity = identity(&policy, ClientId::new());

        for (operation, request) in [
            (
                RequestOperation::GetTlsCredentials,
                Pdu::GetTlsCreds(codec::GetTlsCreds {}),
            ),
            (
                RequestOperation::GetClientList,
                Pdu::GetClientList(codec::GetClientList),
            ),
            (
                RequestOperation::SetPalette,
                Pdu::SetPalette(codec::SetPalette {
                    pane_id: 1,
                    palette: Box::new(wezterm_term::color::ColorPalette::default()),
                }),
            ),
        ] {
            assert!(policy.authorize(&identity, operation, &request).is_err());
        }
    }

    #[test]
    fn moving_a_split_source_requires_control_of_both_panes() {
        let policy = ServerPolicy::new(
            Arc::new(AllowAllRequests),
            BuildIdentity {
                product: "test".to_string(),
                version: "test".to_string(),
                source_revision: None,
                source_dirty: None,
                embedded_wezterm_revision: None,
            },
        );
        let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        policy.bind_admission(&admission).unwrap();
        let identity = identity(&policy, ClientId::new());
        assert!(matches!(
            policy.apply_control(&identity, 1, ControlLeaseAction::Acquire),
            ControlLeaseResult::Acquired(_)
        ));
        let request = Pdu::SplitPane(SplitPane {
            target_pane_id: 1,
            split_request: SplitRequest {
                direction: SplitDirection::Horizontal,
                target_is_second: true,
                size: SplitSize::Percent(50),
                top_level: false,
            },
            domain: SplitSpawnDomain::TargetPaneDomain,
            source: SplitSpawnSource::MovePane { pane_id: 2 },
        });
        assert!(policy
            .authorize(&identity, RequestOperation::SplitPane, &request)
            .is_err());
        assert!(matches!(
            policy.apply_control(&identity, 2, ControlLeaseAction::Acquire),
            ControlLeaseResult::Acquired(_)
        ));
        assert!(policy
            .authorize(&identity, RequestOperation::SplitPane, &request)
            .is_ok());
    }
}
