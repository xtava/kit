use crate::dispatch::{
    AttachmentConnection, AttachmentFence, AttachmentRegistry, EstablishedAttachment,
};
use codec::{
    AttachmentIdentity, AttachmentResumeToken, BuildIdentity, DecodeReservation, Pdu,
    RequestOperation, ServiceDrainAction, ServiceDrainResult,
};
use mux::client::ClientId;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wezterm_runtime_admission::{CombinedPermit, RuntimeAdmission};

/// An identity admitted and issued by the server for one established session.
///
/// The wire-level ClientId is presentation metadata. Keeping construction of
/// this type private prevents ordinary authorization from accepting those raw,
/// caller-controlled claims as proof that bootstrap completed.
#[derive(Clone, Debug)]
pub struct ServerIssuedIdentity {
    fence: AttachmentFence,
    client_id: Arc<ClientId>,
}

impl ServerIssuedIdentity {
    pub fn attachment(&self) -> AttachmentIdentity {
        self.fence.identity
    }

    pub fn client_id(&self) -> &Arc<ClientId> {
        &self.client_id
    }
}

pub(crate) struct EstablishedServerIdentity {
    pub(crate) identity: ServerIssuedIdentity,
    pub(crate) resume_token: AttachmentResumeToken,
    pub(crate) connection: AttachmentConnection,
    pub(crate) is_new: bool,
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
    attachments: Arc<AttachmentRegistry>,
    drain: Mutex<ServiceDrainState>,
}

#[derive(Default)]
struct ServiceDrainState {
    owner: Option<AttachmentIdentity>,
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
        let policy = Arc::new(Self {
            authorizer,
            build_identity,
            attachments: AttachmentRegistry::new(),
            drain: Mutex::new(ServiceDrainState::default()),
        });
        policy.attachments.bind_policy(Arc::downgrade(&policy));
        policy
    }

    pub fn bind_admission(&self, admission: &Arc<RuntimeAdmission>) -> anyhow::Result<()> {
        self.attachments.bind_admission(admission)
    }

    pub fn authorize_proxy_registration(&self, client_id: &ClientId) -> anyhow::Result<()> {
        self.authorizer
            .authorize_registration(None, client_id, true)
    }

    pub(crate) fn establish_identity(
        &self,
        proxy: Option<&ClientId>,
        client_id: ClientId,
        resume_token: Option<AttachmentResumeToken>,
    ) -> anyhow::Result<EstablishedServerIdentity> {
        self.authorizer
            .authorize_registration(proxy, &client_id, false)?;
        let resume_token = resume_token.ok_or_else(|| {
            anyhow::anyhow!("attachment registration requires a resume capability")
        })?;
        let EstablishedAttachment {
            fence,
            client_id,
            resume_token,
            connection,
            is_new,
        } = self
            .attachments
            .establish(Arc::new(client_id), resume_token)?;
        Ok(EstablishedServerIdentity {
            identity: ServerIssuedIdentity { fence, client_id },
            resume_token,
            connection,
            is_new,
        })
    }

    pub(crate) fn ensure_current(&self, identity: &ServerIssuedIdentity) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.attachments.is_current(identity.fence),
            "attachment connection was superseded"
        );
        Ok(())
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
        self.ensure_current(identity)?;
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
                (ServiceDrainAction::Begin, None) => drain.owner = Some(identity.attachment()),
                (ServiceDrainAction::Begin, Some(current)) if current == identity.attachment() => {}
                (ServiceDrainAction::Begin, Some(_)) => {
                    anyhow::bail!("another connection owns the Console service drain")
                }
                (ServiceDrainAction::Cancel, Some(current)) if current == identity.attachment() => {
                }
                (ServiceDrainAction::Cancel, _) => {
                    anyhow::bail!("this connection does not own the Console service drain")
                }
            }
        }
        Ok(())
    }

    pub(crate) fn attachment_expired(&self, identity: AttachmentIdentity) {
        let mut drain = self.drain.lock().unwrap();
        if drain.owner == Some(identity) {
            drain.owner = None;
        }
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
                self.ensure_current(identity)?;
                let spawns_in_flight = {
                    let drain = self.drain.lock().unwrap();
                    if drain.owner != Some(identity.attachment()) {
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
                self.ensure_current(identity)?;
                let mut drain = self.drain.lock().unwrap();
                if drain.owner != Some(identity.attachment()) {
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
    use wezterm_runtime_admission::{RuntimeAdmission, RuntimeRole};

    fn identity(policy: &ServerPolicy, client_id: ClientId) -> ServerIssuedIdentity {
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT_TEST_TOKEN: AtomicU64 = AtomicU64::new(1);
        let sequence = NEXT_TEST_TOKEN.fetch_add(1, Ordering::Relaxed);
        let mut token = [0u8; 32];
        token[..8].copy_from_slice(&sequence.to_le_bytes());
        policy
            .establish_identity(
                None,
                client_id,
                Some(AttachmentResumeToken::from_random_bytes(token)),
            )
            .unwrap()
            .identity
    }

    #[test]
    fn caller_metadata_cannot_forge_the_server_attachment_identity() {
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

        assert_ne!(first.attachment(), second.attachment());
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
}
