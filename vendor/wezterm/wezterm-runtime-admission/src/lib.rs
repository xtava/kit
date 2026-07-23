//! Process-wide admission for bounded headless mux runtimes.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use thiserror::Error;

pub const MAX_TABS: usize = 32;
pub const MAX_PANES: usize = 128;
pub const MAX_ATTACHMENTS: usize = 16;
pub const MAX_INBOUND_REQUESTS_PER_ATTACHMENT: usize = 32;
pub const MAX_INBOUND_REQUESTS_TOTAL: usize = 512;
pub const MAX_SERVER_OUTPUT_ITEMS_PER_ATTACHMENT: usize = 256;
pub const MAX_SERVER_OUTPUT_ITEMS_TOTAL: usize = 4_096;
pub const MAX_CONTROL_NOTIFICATIONS_PER_ATTACHMENT: usize = 256;
pub const MAX_CONTROL_NOTIFICATION_DELIVERIES_TOTAL: usize = 4_096;
pub const MAX_CONTROL_EVENTS_PENDING_TOTAL: usize = 256;
pub const MAX_CLIENT_REQUESTS: usize = 256;
pub const MAX_CLIENT_REQUEST_BYTES_TOTAL: usize = 67_108_864;
pub const MAX_LIFECYCLE_EVENTS: usize = 64;
pub const MAX_ADAPTER_MUX_COMMANDS: usize = 256;
pub const MAX_CLIENT_INVALIDATIONS: usize = 256;
pub const MAX_GRACE_TIMERS_TOTAL: usize = 16;
pub const MAX_REJECTION_WRITERS: usize = 16;
pub const MAX_PANE_INPUT_ITEMS_PER_PANE: usize = 64;
pub const MAX_PANE_INPUT_BYTES_PER_PANE: usize = 262_144;
pub const MAX_PANE_INPUT_BYTES_TOTAL: usize = 33_554_432;
/// Maximum PTY input represented by one parser action batch. Synchronized-output mode may defer
/// presentation, but it may not turn the parser queue into an unbounded retained-state owner.
pub const MAX_PANE_PARSER_INPUT_BYTES_PER_BATCH: usize = 1_048_576;
pub const MAX_PANE_REFRESH_JOBS_TOTAL: usize = 128;
pub const MAX_PANE_WRITE_JOBS_TOTAL: usize = 128;
pub const MAX_PANE_PUSH_JOBS_TOTAL: usize = 128;
pub const MAX_CLIENT_FETCH_JOBS_TOTAL: usize = 128;
pub const MAX_CLIENT_POLL_JOBS_TOTAL: usize = 128;
pub const CLIENT_FIXED_RUNTIME_TASKS: usize = 4;
pub const SERVER_FIXED_RUNTIME_TASKS: usize = 4;
pub const PROTOCOL_BOOTSTRAP_TIMEOUT_MS: u64 = 5_000;

pub const MAX_WIRE_FRAME_BYTES: usize = 16_777_280;
pub const MAX_DECOMPRESSED_PDU_BYTES: usize = 16_777_216;
pub const MAX_WIRE_OWNED_PAYLOAD_BYTES_PER_PDU: usize = 16_777_216;
pub const MAX_WIRE_STRING_BYTES: usize = 1_048_576;
pub const MAX_WIRE_BYTE_BUFFER_BYTES: usize = 16_777_216;
pub const MAX_WIRE_SEQUENCE_ITEMS_PER_PDU: usize = 262_144;
pub const MAX_WIRE_MAP_ENTRIES_PER_PDU: usize = 65_536;
pub const MAX_WIRE_CONTAINERS_PER_PDU: usize = 65_536;
pub const MAX_WIRE_NESTING_DEPTH: usize = 64;
pub const MAX_SCALAR_DERIVED_ALLOCATION_BYTES: usize = 16_777_216;
pub const MAX_DECODE_HEAP_ENVELOPE_BYTES_PER_PDU: usize = 67_108_864;
/// Fixed-shape protocol metadata and bounded control-state values cannot materialize terminal,
/// image, clipboard, or arbitrary collection payloads. Keep their reservation distinct so normal
/// control traffic cannot be rejected behind a small number of large render responses.
pub const MAX_DECODE_METADATA_HEAP_ENVELOPE_BYTES_PER_PDU: usize = 1_048_576;
/// Metadata notifications may own a small number of individually bounded strings or a palette.
pub const MAX_DECODE_NOTIFICATION_HEAP_ENVELOPE_BYTES_PER_PDU: usize = 4_194_304;
pub const MAX_DECODE_PDUS_IN_FLIGHT: usize = 4;
/// Four admitted decoded values may remain owned by their consumers while the reader stages the
/// wire bytes for the next value. The prior heap-only formula omitted that staging overlap and
/// rejected a legal fourth decode whenever earlier values retained even a small wire body.
pub const MAX_DECODE_WORKING_BYTES_TOTAL: usize =
    MAX_DECODE_HEAP_ENVELOPE_BYTES_PER_PDU * MAX_DECODE_PDUS_IN_FLIGHT + MAX_WIRE_FRAME_BYTES;

pub const MAX_SINGLE_RESPONSE_MATERIALIZED_BYTES: usize = 16_777_216;
pub const MAX_SINGLE_PDU_SERIALIZED_BYTES: usize = 16_777_216;
pub const MAX_SINGLE_PDU_COMPRESSED_BYTES: usize = 16_777_216;
pub const MAX_RESPONSE_MATERIALIZATION_BYTES_TOTAL: usize = 67_108_864;
pub const MAX_ENCODE_WORKING_BYTES_TOTAL: usize = 201_326_592;
pub const MAX_BLOB_READ_WORKING_BYTES_TOTAL: usize = 134_217_728;
pub const MAX_BLOB_READERS_TOTAL: usize = 32;
pub const MAX_BLOB_READER_BUFFER_BYTES: usize = 65_536;

pub const MAX_TERMINAL_STATE_BYTES_TOTAL: usize = 536_870_912;
/// Server terminals use these limits before creating a PTY or terminal allocation.
pub const MAX_SERVER_TERMINAL_ROWS: usize = 512;
pub const MAX_SERVER_TERMINAL_COLS: usize = 1_024;
pub const MAX_SERVER_TERMINAL_PIXEL_WIDTH: usize = u16::MAX as usize;
pub const MAX_SERVER_TERMINAL_PIXEL_HEIGHT: usize = u16::MAX as usize;
pub const MAX_SERVER_TERMINAL_SCROLLBACK_ROWS: usize = 10_000;
/// Conservative fixed owner cost for parser, writer, palette, maps and terminal metadata.
pub const MAX_SERVER_TERMINAL_FIXED_BYTES: usize = 1_048_576;
/// A single parser-produced action batch may not retain more source material than this.
pub const MAX_SERVER_TERMINAL_ACTION_BYTES: usize = 1_048_576;
/// Conservative retained-state growth charged for every byte of non-image action material.
pub const SERVER_TERMINAL_ACTION_AMPLIFICATION: usize = 64;
/// Peak reservation for one image mutation, including decoded and prior image coexistence.
pub const MAX_SERVER_TERMINAL_IMAGE_MUTATION_BYTES: usize = 268_435_456;
pub const MAX_CLIENT_RENDER_STATE_BYTES_TOTAL: usize = 402_653_184;
pub const MAX_CLIENT_IMAGE_CACHE_BYTES_TOTAL: usize = 134_217_728;
pub const MAX_RETAINED_STATE_BYTES_TOTAL: usize = 536_870_912;
pub const MAX_BLOB_STORE_ENTRIES: usize = 256;
pub const MAX_BLOB_STORE_BYTES_TOTAL: usize = 134_217_728;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeRole {
    Client,
    Server,
}

/// A task producer that can make a runnable reachable by the mux executor.
///
/// This is the exhaustive runtime producer census. Adding a producer requires
/// naming it here and adding it to every applicable role manifest below; there
/// is no anonymous "internal task" allowance.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum RunnableProducer {
    ClientRuntimeCommandLoop,
    ClientConnectionReader,
    ClientConnectionWriter,
    PaneLifecycleCoordinator,
    ClientRequest,
    ClientLifecycleEvent,
    PaneLifecycleEvent,
    AdapterMuxCommand,
    ClientInvalidation,
    ClientFetchJob,
    ClientPollJob,
    ServerListener,
    ServerMuxExecutor,
    ServerControlEventPublisher,
    PaneReader,
    PaneParser,
    PaneWriter,
    PaneChildWaiter,
    PaneRefreshJob,
    PaneWriteJob,
    PanePushJob,
    Attachment,
    InboundRequest,
    ServerOutput,
    ControlNotificationDelivery,
    GraceTimer,
    RejectionWriter,
}

/// One checked term in a role's reachable-runnable formula.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunnableProducerTerm {
    pub producer: RunnableProducer,
    pub maximum: usize,
}

pub const CLIENT_RUNNABLE_PRODUCERS: &[RunnableProducerTerm] = &[
    RunnableProducerTerm {
        producer: RunnableProducer::ClientRuntimeCommandLoop,
        maximum: 1,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::ClientConnectionReader,
        maximum: 1,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::ClientConnectionWriter,
        maximum: 1,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::PaneLifecycleCoordinator,
        maximum: 1,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::ClientRequest,
        maximum: MAX_CLIENT_REQUESTS,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::ClientLifecycleEvent,
        maximum: MAX_LIFECYCLE_EVENTS,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::PaneLifecycleEvent,
        maximum: MAX_PANES,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::AdapterMuxCommand,
        maximum: MAX_ADAPTER_MUX_COMMANDS,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::ClientInvalidation,
        maximum: MAX_CLIENT_INVALIDATIONS,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::ClientFetchJob,
        maximum: MAX_CLIENT_FETCH_JOBS_TOTAL,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::ClientPollJob,
        maximum: MAX_CLIENT_POLL_JOBS_TOTAL,
    },
];

pub const SERVER_RUNNABLE_PRODUCERS: &[RunnableProducerTerm] = &[
    RunnableProducerTerm {
        producer: RunnableProducer::ServerListener,
        maximum: 1,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::ServerMuxExecutor,
        maximum: 1,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::ServerControlEventPublisher,
        maximum: 1,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::PaneLifecycleCoordinator,
        maximum: 1,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::PaneLifecycleEvent,
        maximum: MAX_PANES,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::AdapterMuxCommand,
        maximum: MAX_ADAPTER_MUX_COMMANDS,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::PaneReader,
        maximum: MAX_PANES,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::PaneParser,
        maximum: MAX_PANES,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::PaneWriter,
        maximum: MAX_PANES,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::PaneChildWaiter,
        maximum: MAX_PANES,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::PaneRefreshJob,
        maximum: MAX_PANE_REFRESH_JOBS_TOTAL,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::PaneWriteJob,
        maximum: MAX_PANE_WRITE_JOBS_TOTAL,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::PanePushJob,
        maximum: MAX_PANE_PUSH_JOBS_TOTAL,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::Attachment,
        maximum: MAX_ATTACHMENTS,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::InboundRequest,
        maximum: MAX_INBOUND_REQUESTS_TOTAL,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::ServerOutput,
        maximum: MAX_SERVER_OUTPUT_ITEMS_TOTAL,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::ControlNotificationDelivery,
        maximum: MAX_CONTROL_NOTIFICATION_DELIVERIES_TOTAL,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::GraceTimer,
        maximum: MAX_GRACE_TIMERS_TOTAL,
    },
    RunnableProducerTerm {
        producer: RunnableProducer::RejectionWriter,
        maximum: MAX_REJECTION_WRITERS,
    },
];

pub const CLIENT_REACHABLE_RUNNABLES: usize =
    const_checked_runnable_capacity(CLIENT_RUNNABLE_PRODUCERS);
pub const SERVER_REACHABLE_RUNNABLES: usize =
    const_checked_runnable_capacity(SERVER_RUNNABLE_PRODUCERS);

const fn const_checked_runnable_capacity(terms: &[RunnableProducerTerm]) -> usize {
    let mut sum = 0usize;
    let mut index = 0usize;
    while index < terms.len() {
        sum = match sum.checked_add(terms[index].maximum) {
            Some(next) => next,
            None => panic!("reachable-runnable formula overflow"),
        };
        index += 1;
    }
    sum
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CountClass {
    Attachment,
    InboundRequest,
    ServerOutput,
    ControlNotificationDelivery,
    ControlEvent,
    ClientRequest,
    LifecycleEvent,
    PaneLifecycleEvent,
    AdapterMuxCommand,
    ClientInvalidation,
    GraceTimer,
    RejectionWriter,
    PaneInputItem,
    PaneRefreshJob,
    PaneWriteJob,
    PanePushJob,
    ClientFetchJob,
    ClientPollJob,
    ExecutorRunnable,
    BlobReader,
    BlobStoreEntry,
}

impl CountClass {
    const ALL: [Self; 21] = [
        Self::Attachment,
        Self::InboundRequest,
        Self::ServerOutput,
        Self::ControlNotificationDelivery,
        Self::ControlEvent,
        Self::ClientRequest,
        Self::LifecycleEvent,
        Self::PaneLifecycleEvent,
        Self::AdapterMuxCommand,
        Self::ClientInvalidation,
        Self::GraceTimer,
        Self::RejectionWriter,
        Self::PaneInputItem,
        Self::PaneRefreshJob,
        Self::PaneWriteJob,
        Self::PanePushJob,
        Self::ClientFetchJob,
        Self::ClientPollJob,
        Self::ExecutorRunnable,
        Self::BlobReader,
        Self::BlobStoreEntry,
    ];

    /// Returns this class's exact capacity for one process role.
    ///
    /// Executor runnable capacity is deliberately role-specific; callers must
    /// never substitute the larger server formula for a client runtime.
    pub const fn capacity_for_role(self, role: RuntimeRole) -> usize {
        match self {
            Self::Attachment => MAX_ATTACHMENTS,
            Self::InboundRequest => MAX_INBOUND_REQUESTS_TOTAL,
            Self::ServerOutput => MAX_SERVER_OUTPUT_ITEMS_TOTAL,
            Self::ControlNotificationDelivery => MAX_CONTROL_NOTIFICATION_DELIVERIES_TOTAL,
            Self::ControlEvent => MAX_CONTROL_EVENTS_PENDING_TOTAL,
            Self::ClientRequest => MAX_CLIENT_REQUESTS,
            Self::LifecycleEvent => MAX_LIFECYCLE_EVENTS,
            Self::PaneLifecycleEvent => MAX_PANES,
            Self::AdapterMuxCommand => MAX_ADAPTER_MUX_COMMANDS,
            Self::ClientInvalidation => MAX_CLIENT_INVALIDATIONS,
            Self::GraceTimer => MAX_GRACE_TIMERS_TOTAL,
            Self::RejectionWriter => MAX_REJECTION_WRITERS,
            Self::PaneInputItem => match MAX_PANES.checked_mul(MAX_PANE_INPUT_ITEMS_PER_PANE) {
                Some(capacity) => capacity,
                None => panic!("pane input item formula overflow"),
            },
            Self::PaneRefreshJob => MAX_PANE_REFRESH_JOBS_TOTAL,
            Self::PaneWriteJob => MAX_PANE_WRITE_JOBS_TOTAL,
            Self::PanePushJob => MAX_PANE_PUSH_JOBS_TOTAL,
            Self::ClientFetchJob => MAX_CLIENT_FETCH_JOBS_TOTAL,
            Self::ClientPollJob => MAX_CLIENT_POLL_JOBS_TOTAL,
            Self::ExecutorRunnable => match role {
                RuntimeRole::Client => CLIENT_REACHABLE_RUNNABLES,
                RuntimeRole::Server => SERVER_REACHABLE_RUNNABLES,
            },
            Self::BlobReader => MAX_BLOB_READERS_TOTAL,
            Self::BlobStoreEntry => MAX_BLOB_STORE_ENTRIES,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ByteClass {
    ClientRequest,
    DecodeWorking,
    ResponseMaterialization,
    EncodeWorking,
    PaneInput,
    BlobReadWorking,
    BlobStore,
}

impl ByteClass {
    const ALL: [Self; 7] = [
        Self::ClientRequest,
        Self::DecodeWorking,
        Self::ResponseMaterialization,
        Self::EncodeWorking,
        Self::PaneInput,
        Self::BlobReadWorking,
        Self::BlobStore,
    ];

    pub const fn capacity(self) -> usize {
        match self {
            Self::ClientRequest => MAX_CLIENT_REQUEST_BYTES_TOTAL,
            Self::DecodeWorking => MAX_DECODE_WORKING_BYTES_TOTAL,
            Self::ResponseMaterialization => MAX_RESPONSE_MATERIALIZATION_BYTES_TOTAL,
            Self::EncodeWorking => MAX_ENCODE_WORKING_BYTES_TOTAL,
            Self::PaneInput => MAX_PANE_INPUT_BYTES_TOTAL,
            Self::BlobReadWorking => MAX_BLOB_READ_WORKING_BYTES_TOTAL,
            Self::BlobStore => MAX_BLOB_STORE_BYTES_TOTAL,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum RetainedClass {
    ServerTerminal,
    ClientRender,
    ClientImage,
}

impl RetainedClass {
    const ALL: [Self; 3] = [Self::ServerTerminal, Self::ClientRender, Self::ClientImage];

    pub const fn capacity(self) -> usize {
        match self {
            Self::ServerTerminal => MAX_TERMINAL_STATE_BYTES_TOTAL,
            Self::ClientRender => MAX_CLIENT_RENDER_STATE_BYTES_TOTAL,
            Self::ClientImage => MAX_CLIENT_IMAGE_CACHE_BYTES_TOTAL,
        }
    }

    const fn role(self) -> RuntimeRole {
        match self {
            Self::ServerTerminal => RuntimeRole::Server,
            Self::ClientRender | Self::ClientImage => RuntimeRole::Client,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::ServerTerminal => "server terminal retained state",
            Self::ClientRender => "client render retained state",
            Self::ClientImage => "client image retained state",
        }
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum AdmissionError {
    #[error("runtime admission is shutting down")]
    ShuttingDown,
    #[error("{resource} capacity exceeded: requested {requested}, available {available}")]
    CapacityExceeded {
        resource: &'static str,
        requested: usize,
        available: usize,
    },
    #[error("invalid admission formula: {0}")]
    InvalidFormula(&'static str),
    #[error("{resource} is not available to the {role:?} runtime")]
    WrongRole {
        resource: &'static str,
        role: RuntimeRole,
    },
}

#[derive(Debug)]
struct Pool {
    name: &'static str,
    capacity: usize,
    used: AtomicUsize,
}

impl Pool {
    fn new(name: &'static str, capacity: usize) -> Self {
        Self {
            name,
            capacity,
            used: AtomicUsize::new(0),
        }
    }

    fn try_acquire(self: &Arc<Self>, amount: usize) -> Result<PoolPermit, AdmissionError> {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let next = used
                .checked_add(amount)
                .ok_or(AdmissionError::CapacityExceeded {
                    resource: self.name,
                    requested: amount,
                    available: self.capacity.saturating_sub(used),
                })?;
            if next > self.capacity {
                return Err(AdmissionError::CapacityExceeded {
                    resource: self.name,
                    requested: amount,
                    available: self.capacity.saturating_sub(used),
                });
            }
            match self
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    return Ok(PoolPermit {
                        pool: Arc::clone(self),
                        amount,
                    })
                }
                Err(actual) => used = actual,
            }
        }
    }
}

#[derive(Debug)]
struct PoolPermit {
    pool: Arc<Pool>,
    amount: usize,
}

impl PoolPermit {
    fn absorb(&mut self, mut additional: Self) {
        debug_assert!(Arc::ptr_eq(&self.pool, &additional.pool));
        self.amount = self
            .amount
            .checked_add(additional.amount)
            .expect("pool permits cannot exceed their pool capacity");
        additional.amount = 0;
    }

    fn release(&mut self, amount: usize) {
        assert!(
            amount <= self.amount,
            "cannot release more than a permit owns"
        );
        if amount == 0 {
            return;
        }
        let prior = self.pool.used.fetch_sub(amount, Ordering::AcqRel);
        debug_assert!(prior >= amount);
        self.amount -= amount;
    }

    fn split_off(&mut self, amount: usize) -> Self {
        assert!(
            amount <= self.amount,
            "cannot split more than a permit owns"
        );
        self.amount -= amount;
        Self {
            pool: Arc::clone(&self.pool),
            amount,
        }
    }
}

impl Drop for PoolPermit {
    fn drop(&mut self) {
        if self.amount == 0 {
            return;
        }
        let prior = self.pool.used.fetch_sub(self.amount, Ordering::AcqRel);
        debug_assert!(prior >= self.amount);
    }
}

#[derive(Debug)]
pub struct CountPermit(PoolPermit);

impl CountPermit {
    pub fn amount(&self) -> usize {
        self.0.amount
    }
}

#[derive(Debug)]
pub struct AttachmentPermit {
    _global: CountPermit,
    admission: Arc<RuntimeAdmission>,
    inbound: Arc<Pool>,
    output: Arc<Pool>,
}

impl AttachmentPermit {
    /// The sole runtime owner charged by this attachment and all of its child
    /// permits. Consumers must derive admission from here rather than accept a
    /// second, potentially mismatched runtime argument.
    pub fn admission(&self) -> &Arc<RuntimeAdmission> {
        &self.admission
    }

    pub fn try_inbound(&self) -> Result<CombinedPermit, AdmissionError> {
        self.admission.ensure_running()?;
        let local = self.inbound.try_acquire(1)?;
        let global = self.admission.try_count(CountClass::InboundRequest, 1)?;
        Ok(CombinedPermit {
            _local: local,
            _global: global,
        })
    }

    pub fn try_output(&self) -> Result<CombinedPermit, AdmissionError> {
        self.admission.ensure_running()?;
        let local = self.output.try_acquire(1)?;
        let global = self.admission.try_count(CountClass::ServerOutput, 1)?;
        Ok(CombinedPermit {
            _local: local,
            _global: global,
        })
    }
}

#[derive(Debug)]
pub struct CombinedPermit {
    _local: PoolPermit,
    _global: CountPermit,
}

#[derive(Debug)]
pub struct BytePermit(PoolPermit);

impl BytePermit {
    pub fn bytes(&self) -> usize {
        self.0.amount
    }

    /// Releases materialization headroom after a bounded value has been constructed.
    ///
    /// This can only shrink an existing charge, including during shutdown. Callers must measure
    /// the retained value before publishing it; growing requires a fresh admission decision.
    pub fn try_shrink_to(&mut self, bytes: usize) -> Result<(), AdmissionError> {
        if bytes > self.bytes() {
            return Err(AdmissionError::InvalidFormula(
                "byte-permit shrink exceeds the source permit",
            ));
        }
        self.0.release(self.bytes() - bytes);
        Ok(())
    }
}

#[derive(Debug)]
pub struct RetainedStateLease {
    admission: Arc<RuntimeAdmission>,
    class: RetainedClass,
    category: PoolPermit,
    aggregate: PoolPermit,
}

impl RetainedStateLease {
    pub fn class(&self) -> RetainedClass {
        self.class
    }

    pub fn bytes(&self) -> usize {
        debug_assert_eq!(self.category.amount, self.aggregate.amount);
        self.category.amount
    }

    /// Transfers part of this lease to a new owner without changing either
    /// admission counter or creating an uncharged retained-state window.
    ///
    /// This is intentionally allowed during shutdown: it moves an existing
    /// charge between owners and never admits additional retained state.
    pub fn try_split_off(&mut self, bytes: usize) -> Result<Self, AdmissionError> {
        if bytes > self.bytes() {
            return Err(AdmissionError::InvalidFormula(
                "retained-state split exceeds the source lease",
            ));
        }

        Ok(Self {
            admission: Arc::clone(&self.admission),
            class: self.class,
            category: self.category.split_off(bytes),
            aggregate: self.aggregate.split_off(bytes),
        })
    }

    /// Changes this lease's charge without exposing an uncharged retained-state window.
    ///
    /// Growth acquires both the category and aggregate delta before changing the
    /// existing lease. If either acquisition fails, temporary permits roll back
    /// and the lease remains unchanged. Shrink is always allowed, including
    /// during shutdown, so teardown can deterministically shed retained state.
    pub fn try_resize(&mut self, new_bytes: usize) -> Result<(), AdmissionError> {
        let current = self.bytes();
        if new_bytes == current {
            return Ok(());
        }

        if new_bytes < current {
            let released = current - new_bytes;
            self.category.release(released);
            self.aggregate.release(released);
            return Ok(());
        }

        let additional = new_bytes
            .checked_sub(current)
            .ok_or(AdmissionError::InvalidFormula(
                "retained-state resize overflow",
            ))?;
        self.admission.ensure_running()?;
        self.admission
            .ensure_role(self.class.name(), self.class.role())?;

        let category = self
            .admission
            .try_pool_running(&self.admission.retained_categories[&self.class], additional)?;
        let aggregate = match self
            .admission
            .try_pool_running(&self.admission.retained_aggregate, additional)
        {
            Ok(permit) => permit,
            Err(err) => {
                drop(category);
                return Err(err);
            }
        };

        self.category.absorb(category);
        self.aggregate.absorb(aggregate);
        Ok(())
    }
}

#[derive(Debug)]
pub struct TabAdmissionPermit(PoolPermit);

impl TabAdmissionPermit {
    pub fn amount(&self) -> usize {
        self.0.amount
    }
}

#[derive(Debug)]
pub struct PaneAdmissionPermit(PoolPermit);

impl PaneAdmissionPermit {
    pub fn amount(&self) -> usize {
        self.0.amount
    }
}

#[derive(Debug)]
pub struct RuntimeAdmission {
    role: RuntimeRole,
    shutting_down: AtomicBool,
    tabs: Arc<Pool>,
    panes: Arc<Pool>,
    counts: BTreeMap<CountClass, Arc<Pool>>,
    bytes: BTreeMap<ByteClass, Arc<Pool>>,
    retained_categories: BTreeMap<RetainedClass, Arc<Pool>>,
    retained_aggregate: Arc<Pool>,
}

impl RuntimeAdmission {
    pub fn new(role: RuntimeRole) -> Result<Arc<Self>, AdmissionError> {
        validate_formulas(role)?;
        let counts = CountClass::ALL
            .into_iter()
            .map(|class| {
                (
                    class,
                    Arc::new(Pool::new(count_name(class), class.capacity_for_role(role))),
                )
            })
            .collect();
        let bytes = ByteClass::ALL
            .into_iter()
            .map(|class| {
                (
                    class,
                    Arc::new(Pool::new(byte_name(class), class.capacity())),
                )
            })
            .collect();
        let retained_categories = RetainedClass::ALL
            .into_iter()
            .map(|class| (class, Arc::new(Pool::new(class.name(), class.capacity()))))
            .collect();
        Ok(Arc::new(Self {
            role,
            shutting_down: AtomicBool::new(false),
            tabs: Arc::new(Pool::new("tabs", MAX_TABS)),
            panes: Arc::new(Pool::new("panes", MAX_PANES)),
            counts,
            bytes,
            retained_categories,
            retained_aggregate: Arc::new(Pool::new(
                "aggregate retained state",
                MAX_RETAINED_STATE_BYTES_TOTAL,
            )),
        }))
    }

    pub fn role(&self) -> RuntimeRole {
        self.role
    }

    pub fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }

    fn ensure_running(&self) -> Result<(), AdmissionError> {
        if self.is_shutting_down() {
            Err(AdmissionError::ShuttingDown)
        } else {
            Ok(())
        }
    }

    pub fn try_tab(&self) -> Result<TabAdmissionPermit, AdmissionError> {
        self.try_pool_running(&self.tabs, 1).map(TabAdmissionPermit)
    }

    pub fn try_attachment(self: &Arc<Self>) -> Result<AttachmentPermit, AdmissionError> {
        self.ensure_running()?;
        self.ensure_role("attachments", RuntimeRole::Server)?;
        Ok(AttachmentPermit {
            _global: self.try_count(CountClass::Attachment, 1)?,
            admission: Arc::clone(self),
            inbound: Arc::new(Pool::new(
                "attachment inbound requests",
                MAX_INBOUND_REQUESTS_PER_ATTACHMENT,
            )),
            output: Arc::new(Pool::new(
                "attachment server output items",
                MAX_SERVER_OUTPUT_ITEMS_PER_ATTACHMENT,
            )),
        })
    }

    pub fn try_pane(&self) -> Result<PaneAdmissionPermit, AdmissionError> {
        self.try_pool_running(&self.panes, 1)
            .map(PaneAdmissionPermit)
    }

    pub fn try_count(
        &self,
        class: CountClass,
        amount: usize,
    ) -> Result<CountPermit, AdmissionError> {
        self.ensure_running()?;
        self.ensure_class_role(count_name(class), count_role(class))?;
        self.try_pool_running(&self.counts[&class], amount)
            .map(CountPermit)
    }

    pub fn try_bytes(&self, class: ByteClass, amount: usize) -> Result<BytePermit, AdmissionError> {
        self.ensure_running()?;
        self.ensure_class_role(byte_name(class), byte_role(class))?;
        self.try_pool_running(&self.bytes[&class], amount)
            .map(BytePermit)
    }

    pub fn try_retained(
        self: &Arc<Self>,
        class: RetainedClass,
        bytes: usize,
    ) -> Result<RetainedStateLease, AdmissionError> {
        self.ensure_running()?;
        self.ensure_role(class.name(), class.role())?;

        let category = self.try_pool_running(&self.retained_categories[&class], bytes)?;
        let aggregate = match self.try_pool_running(&self.retained_aggregate, bytes) {
            Ok(permit) => permit,
            Err(err) => {
                drop(category);
                return Err(err);
            }
        };

        Ok(RetainedStateLease {
            admission: Arc::clone(self),
            class,
            category,
            aggregate,
        })
    }

    pub fn count_usage(&self, class: CountClass) -> usize {
        self.counts[&class].used.load(Ordering::Acquire)
    }

    pub fn count_capacity(&self, class: CountClass) -> usize {
        self.counts[&class].capacity
    }

    pub fn byte_usage(&self, class: ByteClass) -> usize {
        self.bytes[&class].used.load(Ordering::Acquire)
    }

    pub fn retained_usage(&self, class: RetainedClass) -> usize {
        self.retained_categories[&class]
            .used
            .load(Ordering::Acquire)
    }

    pub fn retained_capacity(&self, class: RetainedClass) -> usize {
        self.retained_categories[&class].capacity
    }

    pub fn retained_aggregate_usage(&self) -> usize {
        self.retained_aggregate.used.load(Ordering::Acquire)
    }

    pub fn retained_aggregate_capacity(&self) -> usize {
        self.retained_aggregate.capacity
    }

    fn try_pool_running(
        &self,
        pool: &Arc<Pool>,
        amount: usize,
    ) -> Result<PoolPermit, AdmissionError> {
        self.ensure_running()?;
        let permit = pool.try_acquire(amount)?;
        // This second read closes the race with begin_shutdown: if shutdown
        // linearized before the pool CAS, the newly acquired permit is rolled
        // back; otherwise the acquisition preceded shutdown and remains valid.
        if self.is_shutting_down() {
            drop(permit);
            Err(AdmissionError::ShuttingDown)
        } else {
            Ok(permit)
        }
    }

    fn ensure_role(
        &self,
        resource: &'static str,
        required: RuntimeRole,
    ) -> Result<(), AdmissionError> {
        if self.role == required {
            Ok(())
        } else {
            Err(AdmissionError::WrongRole {
                resource,
                role: self.role,
            })
        }
    }

    fn ensure_class_role(
        &self,
        resource: &'static str,
        required: Option<RuntimeRole>,
    ) -> Result<(), AdmissionError> {
        match required {
            Some(role) => self.ensure_role(resource, role),
            None => Ok(()),
        }
    }
}

const fn count_role(class: CountClass) -> Option<RuntimeRole> {
    match class {
        CountClass::Attachment
        | CountClass::InboundRequest
        | CountClass::ServerOutput
        | CountClass::ControlNotificationDelivery
        | CountClass::ControlEvent
        | CountClass::GraceTimer
        | CountClass::RejectionWriter
        | CountClass::PanePushJob => Some(RuntimeRole::Server),
        CountClass::ClientRequest
        | CountClass::LifecycleEvent
        | CountClass::ClientInvalidation
        | CountClass::ClientFetchJob
        | CountClass::ClientPollJob => Some(RuntimeRole::Client),
        CountClass::AdapterMuxCommand
        | CountClass::ExecutorRunnable
        | CountClass::PaneLifecycleEvent
        | CountClass::PaneInputItem
        | CountClass::PaneRefreshJob
        | CountClass::PaneWriteJob
        | CountClass::BlobReader
        | CountClass::BlobStoreEntry => None,
    }
}

const fn byte_role(class: ByteClass) -> Option<RuntimeRole> {
    match class {
        ByteClass::ClientRequest => Some(RuntimeRole::Client),
        ByteClass::ResponseMaterialization => Some(RuntimeRole::Server),
        ByteClass::DecodeWorking
        | ByteClass::EncodeWorking
        | ByteClass::PaneInput
        | ByteClass::BlobReadWorking
        | ByteClass::BlobStore => None,
    }
}

fn validate_formulas(role: RuntimeRole) -> Result<(), AdmissionError> {
    if MAX_ATTACHMENTS.checked_mul(MAX_INBOUND_REQUESTS_PER_ATTACHMENT)
        != Some(MAX_INBOUND_REQUESTS_TOTAL)
    {
        return Err(AdmissionError::InvalidFormula("inbound request fanout"));
    }
    if MAX_ATTACHMENTS.checked_mul(MAX_SERVER_OUTPUT_ITEMS_PER_ATTACHMENT)
        != Some(MAX_SERVER_OUTPUT_ITEMS_TOTAL)
    {
        return Err(AdmissionError::InvalidFormula("server output fanout"));
    }
    if MAX_ATTACHMENTS.checked_mul(MAX_CONTROL_NOTIFICATIONS_PER_ATTACHMENT)
        != Some(MAX_CONTROL_NOTIFICATION_DELIVERIES_TOTAL)
    {
        return Err(AdmissionError::InvalidFormula(
            "control notification fanout",
        ));
    }
    if MAX_PANES.checked_mul(MAX_PANE_INPUT_ITEMS_PER_PANE)
        != Some(CountClass::PaneInputItem.capacity_for_role(role))
    {
        return Err(AdmissionError::InvalidFormula("pane input item fanout"));
    }
    if MAX_PANES.checked_mul(MAX_PANE_INPUT_BYTES_PER_PANE) != Some(MAX_PANE_INPUT_BYTES_TOTAL) {
        return Err(AdmissionError::InvalidFormula("pane input byte fanout"));
    }
    if [
        MAX_PANE_REFRESH_JOBS_TOTAL,
        MAX_PANE_WRITE_JOBS_TOTAL,
        MAX_PANE_PUSH_JOBS_TOTAL,
        MAX_CLIENT_FETCH_JOBS_TOTAL,
        MAX_CLIENT_POLL_JOBS_TOTAL,
    ]
    .iter()
    .any(|capacity| *capacity < MAX_PANES)
    {
        return Err(AdmissionError::InvalidFormula("per-pane job fanout"));
    }
    let retained = match role {
        RuntimeRole::Server => RetainedClass::ServerTerminal.capacity(),
        RuntimeRole::Client => RetainedClass::ClientRender
            .capacity()
            .checked_add(RetainedClass::ClientImage.capacity())
            .ok_or(AdmissionError::InvalidFormula("client retained-state sum"))?,
    };
    if retained > MAX_RETAINED_STATE_BYTES_TOTAL {
        return Err(AdmissionError::InvalidFormula(
            "retained-state role ceiling",
        ));
    }
    let client_runnables = checked_runnable_capacity(CLIENT_RUNNABLE_PRODUCERS)?;
    if client_runnables != CLIENT_REACHABLE_RUNNABLES {
        return Err(AdmissionError::InvalidFormula("client runnable census"));
    }
    let server_runnables = checked_runnable_capacity(SERVER_RUNNABLE_PRODUCERS)?;
    if server_runnables != SERVER_REACHABLE_RUNNABLES {
        return Err(AdmissionError::InvalidFormula("server runnable census"));
    }
    Ok(())
}

fn checked_runnable_capacity(terms: &[RunnableProducerTerm]) -> Result<usize, AdmissionError> {
    terms.iter().try_fold(0usize, |sum, term| {
        sum.checked_add(term.maximum)
            .ok_or(AdmissionError::InvalidFormula("runnable census overflow"))
    })
}

const fn count_name(class: CountClass) -> &'static str {
    match class {
        CountClass::Attachment => "attachments",
        CountClass::InboundRequest => "inbound requests",
        CountClass::ServerOutput => "server output items",
        CountClass::ControlNotificationDelivery => "control notification deliveries",
        CountClass::ControlEvent => "control events",
        CountClass::ClientRequest => "client requests",
        CountClass::LifecycleEvent => "lifecycle events",
        CountClass::PaneLifecycleEvent => "pane lifecycle events",
        CountClass::AdapterMuxCommand => "adapter mux commands",
        CountClass::ClientInvalidation => "client invalidations",
        CountClass::GraceTimer => "grace timers",
        CountClass::RejectionWriter => "rejection writers",
        CountClass::PaneInputItem => "pane input items",
        CountClass::PaneRefreshJob => "pane refresh jobs",
        CountClass::PaneWriteJob => "pane write jobs",
        CountClass::PanePushJob => "pane push jobs",
        CountClass::ClientFetchJob => "client fetch jobs",
        CountClass::ClientPollJob => "client poll jobs",
        CountClass::ExecutorRunnable => "executor runnables",
        CountClass::BlobReader => "blob readers",
        CountClass::BlobStoreEntry => "blob store entries",
    }
}

const fn byte_name(class: ByteClass) -> &'static str {
    match class {
        ByteClass::ClientRequest => "client request bytes",
        ByteClass::DecodeWorking => "decode working bytes",
        ByteClass::ResponseMaterialization => "response materialization bytes",
        ByteClass::EncodeWorking => "encode working bytes",
        ByteClass::PaneInput => "pane input bytes",
        ByteClass::BlobReadWorking => "blob read working bytes",
        ByteClass::BlobStore => "blob store bytes",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_and_pane_admission_are_independent() {
        let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        let tabs = (0..MAX_TABS)
            .map(|_| admission.try_tab().unwrap())
            .collect::<Vec<_>>();
        assert!(admission.try_tab().is_err());
        assert!(admission.try_pane().is_ok());
        drop(tabs);

        let panes = (0..MAX_PANES)
            .map(|_| admission.try_pane().unwrap())
            .collect::<Vec<_>>();
        assert!(admission.try_pane().is_err());
        assert!(admission.try_tab().is_ok());
        drop(panes);
    }

    #[test]
    fn permits_release_exact_capacity() {
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let permit = admission
            .try_bytes(ByteClass::ClientRequest, MAX_CLIENT_REQUEST_BYTES_TOTAL)
            .unwrap();
        assert!(admission.try_bytes(ByteClass::ClientRequest, 1).is_err());
        drop(permit);
        assert_eq!(admission.byte_usage(ByteClass::ClientRequest), 0);
    }

    #[test]
    fn byte_permit_shrink_releases_only_materialization_headroom() {
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let mut permit = admission.try_bytes(ByteClass::DecodeWorking, 64).unwrap();

        permit.try_shrink_to(7).unwrap();
        assert_eq!(permit.bytes(), 7);
        assert_eq!(admission.byte_usage(ByteClass::DecodeWorking), 7);
        assert!(matches!(
            permit.try_shrink_to(8),
            Err(AdmissionError::InvalidFormula(_))
        ));
        assert_eq!(permit.bytes(), 7);
        assert_eq!(admission.byte_usage(ByteClass::DecodeWorking), 7);

        drop(permit);
        assert_eq!(admission.byte_usage(ByteClass::DecodeWorking), 0);
    }

    #[test]
    fn attachment_local_limits_charge_and_release_the_same_runtime() {
        let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        let attachment = admission.try_attachment().unwrap();
        let inbound = (0..MAX_INBOUND_REQUESTS_PER_ATTACHMENT)
            .map(|_| attachment.try_inbound().unwrap())
            .collect::<Vec<_>>();
        assert!(matches!(
            attachment.try_inbound(),
            Err(AdmissionError::CapacityExceeded { .. })
        ));
        assert_eq!(
            admission.count_usage(CountClass::InboundRequest),
            MAX_INBOUND_REQUESTS_PER_ATTACHMENT
        );
        drop(inbound);
        assert_eq!(admission.count_usage(CountClass::InboundRequest), 0);

        let output = (0..MAX_SERVER_OUTPUT_ITEMS_PER_ATTACHMENT)
            .map(|_| attachment.try_output().unwrap())
            .collect::<Vec<_>>();
        assert!(attachment.try_output().is_err());
        drop(output);
        assert_eq!(admission.count_usage(CountClass::ServerOutput), 0);
    }

    #[test]
    fn attachment_child_admission_closes_on_runtime_shutdown() {
        let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        let attachment = admission.try_attachment().unwrap();
        admission.begin_shutdown();

        assert_eq!(
            attachment.try_inbound().unwrap_err(),
            AdmissionError::ShuttingDown
        );
        assert_eq!(
            attachment.try_output().unwrap_err(),
            AdmissionError::ShuttingDown
        );
    }

    #[test]
    fn shutdown_closes_admission() {
        let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        admission.begin_shutdown();
        assert_eq!(
            admission.try_tab().unwrap_err(),
            AdmissionError::ShuttingDown
        );
    }

    #[test]
    fn role_specific_capacity_cannot_cross_runtime_boundary() {
        let client = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        assert!(matches!(
            client.try_attachment(),
            Err(AdmissionError::WrongRole { .. })
        ));
        let server = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        assert!(matches!(
            server.try_count(CountClass::ClientRequest, 1),
            Err(AdmissionError::WrongRole { .. })
        ));
    }

    #[test]
    fn executor_runnables_are_admitted_by_client_and_server_runtimes() {
        for role in [RuntimeRole::Client, RuntimeRole::Server] {
            let admission = RuntimeAdmission::new(role).unwrap();
            assert!(admission.try_count(CountClass::ExecutorRunnable, 1).is_ok());
        }
    }

    #[test]
    fn adapter_mux_commands_are_admitted_by_client_and_server_runtimes() {
        for role in [RuntimeRole::Client, RuntimeRole::Server] {
            let admission = RuntimeAdmission::new(role).unwrap();
            assert!(admission
                .try_count(CountClass::AdapterMuxCommand, 1)
                .is_ok());
        }
    }

    #[test]
    fn executor_capacity_matches_each_role_census_and_rejects_boundary_plus_one() {
        for (role, expected) in [
            (RuntimeRole::Client, CLIENT_REACHABLE_RUNNABLES),
            (RuntimeRole::Server, SERVER_REACHABLE_RUNNABLES),
        ] {
            let admission = RuntimeAdmission::new(role).unwrap();
            assert_eq!(
                admission.count_capacity(CountClass::ExecutorRunnable),
                expected
            );
            let permit = admission
                .try_count(CountClass::ExecutorRunnable, expected)
                .unwrap();
            assert!(matches!(
                admission.try_count(CountClass::ExecutorRunnable, 1),
                Err(AdmissionError::CapacityExceeded { .. })
            ));
            drop(permit);
            assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 0);
        }
    }

    #[test]
    fn executor_census_uses_checked_arithmetic() {
        let terms = [
            RunnableProducerTerm {
                producer: RunnableProducer::ClientRequest,
                maximum: usize::MAX,
            },
            RunnableProducerTerm {
                producer: RunnableProducer::ClientLifecycleEvent,
                maximum: 1,
            },
        ];
        assert_eq!(
            checked_runnable_capacity(&terms),
            Err(AdmissionError::InvalidFormula("runnable census overflow"))
        );
    }

    #[test]
    fn producer_manifests_name_every_fixed_task_and_have_no_duplicates() {
        let client_fixed = CLIENT_RUNNABLE_PRODUCERS
            .iter()
            .filter(|term| {
                matches!(
                    term.producer,
                    RunnableProducer::ClientRuntimeCommandLoop
                        | RunnableProducer::ClientConnectionReader
                        | RunnableProducer::ClientConnectionWriter
                        | RunnableProducer::PaneLifecycleCoordinator
                )
            })
            .map(|term| term.maximum)
            .sum::<usize>();
        let server_fixed = SERVER_RUNNABLE_PRODUCERS
            .iter()
            .filter(|term| {
                matches!(
                    term.producer,
                    RunnableProducer::ServerListener
                        | RunnableProducer::ServerMuxExecutor
                        | RunnableProducer::ServerControlEventPublisher
                        | RunnableProducer::PaneLifecycleCoordinator
                )
            })
            .map(|term| term.maximum)
            .sum::<usize>();
        assert_eq!(client_fixed, CLIENT_FIXED_RUNTIME_TASKS);
        assert_eq!(server_fixed, SERVER_FIXED_RUNTIME_TASKS);

        for manifest in [CLIENT_RUNNABLE_PRODUCERS, SERVER_RUNNABLE_PRODUCERS] {
            for (index, term) in manifest.iter().enumerate() {
                assert!(
                    term.maximum > 0,
                    "zero-capacity producer: {:?}",
                    term.producer
                );
                assert!(
                    !manifest[..index]
                        .iter()
                        .any(|prior| prior.producer == term.producer),
                    "duplicate producer: {:?}",
                    term.producer
                );
            }
        }
    }

    #[test]
    fn role_neutral_executor_adapter_and_pane_lifecycle_pools_use_correct_capacity() {
        let client = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let server = RuntimeAdmission::new(RuntimeRole::Server).unwrap();

        assert_eq!(
            client.count_capacity(CountClass::ExecutorRunnable),
            CLIENT_REACHABLE_RUNNABLES
        );
        assert_eq!(
            server.count_capacity(CountClass::ExecutorRunnable),
            SERVER_REACHABLE_RUNNABLES
        );
        assert_eq!(
            client.count_capacity(CountClass::AdapterMuxCommand),
            MAX_ADAPTER_MUX_COMMANDS
        );
        assert_eq!(
            server.count_capacity(CountClass::AdapterMuxCommand),
            MAX_ADAPTER_MUX_COMMANDS
        );
        assert_eq!(
            client.count_capacity(CountClass::PaneLifecycleEvent),
            MAX_PANES
        );
        assert_eq!(
            server.count_capacity(CountClass::PaneLifecycleEvent),
            MAX_PANES
        );
        assert!(SERVER_RUNNABLE_PRODUCERS.iter().any(|term| {
            term.producer == RunnableProducer::AdapterMuxCommand
                && term.maximum == MAX_ADAPTER_MUX_COMMANDS
        }));
        for manifest in [CLIENT_RUNNABLE_PRODUCERS, SERVER_RUNNABLE_PRODUCERS] {
            assert!(manifest.iter().any(|term| {
                term.producer == RunnableProducer::PaneLifecycleCoordinator && term.maximum == 1
            }));
            assert!(manifest.iter().any(|term| {
                term.producer == RunnableProducer::PaneLifecycleEvent && term.maximum == MAX_PANES
            }));
        }
    }

    #[test]
    fn pane_lifecycle_events_are_role_neutral_bounded_and_exactly_released() {
        for role in [RuntimeRole::Client, RuntimeRole::Server] {
            let admission = RuntimeAdmission::new(role).unwrap();
            let permits = (0..MAX_PANES)
                .map(|_| {
                    admission
                        .try_count(CountClass::PaneLifecycleEvent, 1)
                        .unwrap()
                })
                .collect::<Vec<_>>();
            assert!(matches!(
                admission.try_count(CountClass::PaneLifecycleEvent, 1),
                Err(AdmissionError::CapacityExceeded { .. })
            ));
            assert_eq!(
                admission.count_usage(CountClass::PaneLifecycleEvent),
                MAX_PANES
            );
            drop(permits);
            assert_eq!(admission.count_usage(CountClass::PaneLifecycleEvent), 0);
        }
    }

    #[test]
    fn retained_classes_reject_the_wrong_runtime_role_without_charging() {
        let client = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        assert!(matches!(
            client.try_retained(RetainedClass::ServerTerminal, 1),
            Err(AdmissionError::WrongRole { .. })
        ));
        assert_eq!(client.retained_usage(RetainedClass::ServerTerminal), 0);
        assert_eq!(client.retained_aggregate_usage(), 0);

        let server = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        for class in [RetainedClass::ClientRender, RetainedClass::ClientImage] {
            assert!(matches!(
                server.try_retained(class, 1),
                Err(AdmissionError::WrongRole { .. })
            ));
            assert_eq!(server.retained_usage(class), 0);
        }
        assert_eq!(server.retained_aggregate_usage(), 0);
    }

    #[test]
    fn retained_categories_enforce_their_exact_boundary_and_release() {
        for (role, class) in [
            (RuntimeRole::Server, RetainedClass::ServerTerminal),
            (RuntimeRole::Client, RetainedClass::ClientRender),
            (RuntimeRole::Client, RetainedClass::ClientImage),
        ] {
            let admission = RuntimeAdmission::new(role).unwrap();
            let capacity = class.capacity();
            assert_eq!(admission.retained_capacity(class), capacity);
            assert_eq!(
                admission.retained_aggregate_capacity(),
                MAX_RETAINED_STATE_BYTES_TOTAL
            );

            let lease = admission.try_retained(class, capacity).unwrap();
            assert_eq!(lease.class(), class);
            assert_eq!(lease.bytes(), capacity);
            assert_eq!(admission.retained_usage(class), capacity);
            assert_eq!(admission.retained_aggregate_usage(), capacity);
            assert!(matches!(
                admission.try_retained(class, 1),
                Err(AdmissionError::CapacityExceeded { .. })
            ));

            drop(lease);
            assert_eq!(admission.retained_usage(class), 0);
            assert_eq!(admission.retained_aggregate_usage(), 0);
        }
    }

    #[test]
    fn client_render_and_image_share_one_aggregate_pool() {
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let render = admission
            .try_retained(
                RetainedClass::ClientRender,
                MAX_CLIENT_RENDER_STATE_BYTES_TOTAL,
            )
            .unwrap();
        assert_eq!(
            admission.retained_aggregate_usage(),
            MAX_CLIENT_RENDER_STATE_BYTES_TOTAL
        );

        let image = admission
            .try_retained(
                RetainedClass::ClientImage,
                MAX_CLIENT_IMAGE_CACHE_BYTES_TOTAL,
            )
            .unwrap();
        assert_eq!(
            admission.retained_aggregate_usage(),
            MAX_RETAINED_STATE_BYTES_TOTAL
        );

        drop(render);
        assert_eq!(
            admission.retained_aggregate_usage(),
            MAX_CLIENT_IMAGE_CACHE_BYTES_TOTAL
        );
        drop(image);
        assert_eq!(admission.retained_aggregate_usage(), 0);
    }

    #[test]
    fn retained_lease_resize_grows_and_shrinks_both_charges() {
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let mut lease = admission
            .try_retained(RetainedClass::ClientRender, 16)
            .unwrap();

        lease.try_resize(64).unwrap();
        assert_eq!(lease.bytes(), 64);
        assert_eq!(admission.retained_usage(RetainedClass::ClientRender), 64);
        assert_eq!(admission.retained_aggregate_usage(), 64);

        lease.try_resize(7).unwrap();
        assert_eq!(lease.bytes(), 7);
        assert_eq!(admission.retained_usage(RetainedClass::ClientRender), 7);
        assert_eq!(admission.retained_aggregate_usage(), 7);
    }

    #[test]
    fn retained_lease_split_transfers_charge_without_changing_usage() {
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let mut source = admission
            .try_retained(RetainedClass::ClientImage, 64)
            .unwrap();

        let mut transferred = source.try_split_off(24).unwrap();
        assert_eq!(source.bytes(), 40);
        assert_eq!(transferred.bytes(), 24);
        assert_eq!(admission.retained_usage(RetainedClass::ClientImage), 64);
        assert_eq!(admission.retained_aggregate_usage(), 64);

        transferred.try_resize(8).unwrap();
        assert_eq!(admission.retained_usage(RetainedClass::ClientImage), 48);
        assert_eq!(admission.retained_aggregate_usage(), 48);

        drop(source);
        assert_eq!(admission.retained_usage(RetainedClass::ClientImage), 8);
        assert_eq!(admission.retained_aggregate_usage(), 8);
        drop(transferred);
        assert_eq!(admission.retained_usage(RetainedClass::ClientImage), 0);
        assert_eq!(admission.retained_aggregate_usage(), 0);
    }

    #[test]
    fn retained_lease_split_rejects_more_than_the_source_owns() {
        let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        let mut lease = admission
            .try_retained(RetainedClass::ServerTerminal, 16)
            .unwrap();

        assert_eq!(
            lease.try_split_off(17).unwrap_err(),
            AdmissionError::InvalidFormula("retained-state split exceeds the source lease")
        );
        assert_eq!(lease.bytes(), 16);
        assert_eq!(admission.retained_usage(RetainedClass::ServerTerminal), 16);
        assert_eq!(admission.retained_aggregate_usage(), 16);
    }

    #[test]
    fn retained_lease_split_is_allowed_during_shutdown() {
        let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        let mut source = admission
            .try_retained(RetainedClass::ServerTerminal, 16)
            .unwrap();
        admission.begin_shutdown();

        let transferred = source.try_split_off(7).unwrap();
        assert_eq!(source.bytes(), 9);
        assert_eq!(transferred.bytes(), 7);
        assert_eq!(admission.retained_usage(RetainedClass::ServerTerminal), 16);
        assert_eq!(admission.retained_aggregate_usage(), 16);
    }

    #[test]
    fn failed_retained_growth_preserves_the_lease_and_both_usage_counters() {
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let mut lease = admission
            .try_retained(RetainedClass::ClientRender, 16)
            .unwrap();

        // Fault-inject aggregate saturation to prove that a successful
        // category-delta acquisition rolls back if aggregate admission fails.
        let aggregate_blocker = admission
            .retained_aggregate
            .try_acquire(MAX_RETAINED_STATE_BYTES_TOTAL - lease.bytes())
            .unwrap();
        let category_before = admission.retained_usage(RetainedClass::ClientRender);
        let aggregate_before = admission.retained_aggregate_usage();
        assert!(matches!(
            lease.try_resize(17),
            Err(AdmissionError::CapacityExceeded { .. })
        ));
        assert_eq!(lease.bytes(), 16);
        assert_eq!(
            admission.retained_usage(RetainedClass::ClientRender),
            category_before
        );
        assert_eq!(admission.retained_aggregate_usage(), aggregate_before);
        drop(aggregate_blocker);

        let category_before = admission.retained_usage(RetainedClass::ClientRender);
        let aggregate_before = admission.retained_aggregate_usage();
        assert!(matches!(
            lease.try_resize(usize::MAX),
            Err(AdmissionError::CapacityExceeded { .. })
        ));
        assert_eq!(lease.bytes(), 16);
        assert_eq!(
            admission.retained_usage(RetainedClass::ClientRender),
            category_before
        );
        assert_eq!(admission.retained_aggregate_usage(), aggregate_before);
    }

    #[test]
    fn retained_shutdown_rejects_new_and_growth_but_allows_shrink_and_drop() {
        let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        let mut lease = admission
            .try_retained(RetainedClass::ServerTerminal, 32)
            .unwrap();
        admission.begin_shutdown();

        assert_eq!(
            admission
                .try_retained(RetainedClass::ServerTerminal, 1)
                .unwrap_err(),
            AdmissionError::ShuttingDown
        );
        assert_eq!(
            lease.try_resize(33).unwrap_err(),
            AdmissionError::ShuttingDown
        );
        assert_eq!(lease.bytes(), 32);
        assert_eq!(admission.retained_usage(RetainedClass::ServerTerminal), 32);
        assert_eq!(admission.retained_aggregate_usage(), 32);

        lease.try_resize(5).unwrap();
        assert_eq!(lease.bytes(), 5);
        assert_eq!(admission.retained_usage(RetainedClass::ServerTerminal), 5);
        assert_eq!(admission.retained_aggregate_usage(), 5);

        drop(lease);
        assert_eq!(admission.retained_usage(RetainedClass::ServerTerminal), 0);
        assert_eq!(admission.retained_aggregate_usage(), 0);
    }
}
