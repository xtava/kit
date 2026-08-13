use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const REGISTRY_SCHEMA: u32 = 1;
pub const SERVICE_PROTOCOL_VERSION: u32 = 8;
pub const LOCUS_SCHEMA_VERSION: u32 = 2;
pub const DIAGNOSTIC_SCHEMA_VERSION: u32 = 2;
pub const MAX_TRACE_DEPTH: u32 = 64;
pub const MAX_TRACE_NODES: usize = 4_096;
pub const MAX_TRACE_NATIVE_VARIANTS: usize = 8_192;
pub const MAX_TRACE_PROJECT_CONTEXTS: usize = 64;
pub const MAX_LOCUS_LOCATIONS: usize = 4_096;
pub const MAX_LOCUS_CASE_ITEMS: usize = 256;
pub const MAX_LOCUS_CANDIDATES: usize = 128;
pub const MAX_LOCUS_REQUIREMENTS: usize = 512;
pub const MAX_LOCUS_TOTAL_EVIDENCE: usize = 2_048;
pub const MAX_LOCUS_TOTAL_CALL_SITES: usize = 4_096;
pub const MAX_LOCUS_TOTAL_AMBIGUITY_CANDIDATES: usize = 512;
pub const MAX_LOCUS_MATRIX_CELLS: usize = 4_096;
pub const MAX_LOCUS_WITNESSES_PER_REQUIREMENT: usize = 16;
pub const MAX_LOCUS_OBSERVED_FILES: usize = 1_024;
pub const MAX_LOCUS_TEXT_BYTES: usize = 4_096;
pub const MAX_LOCUS_LABEL_BYTES: usize = 256;
pub const MAX_LOCUS_ID_BYTES: usize = 128;
pub const MAX_LOCUS_SOURCE_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_LOCUS_TOTAL_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_DIAGNOSE_FILES: usize = 64;
pub const MAX_DIAGNOSTIC_ITEMS: usize = 512;
pub const MAX_DIAGNOSTIC_RELATED_INFORMATION: usize = 64;
pub const MAX_DIAGNOSTIC_MESSAGE_BYTES: usize = 16 * 1024;
pub const MAX_DIAGNOSE_TOTAL_SOURCE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceIdentity {
    pub key: String,
    pub workspace: PathBuf,
    pub launcher: PathBuf,
    pub server_version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryRecord {
    pub schema: u32,
    pub identity: ServiceIdentity,
    pub socket_path: PathBuf,
    pub daemon_receipt: String,
    pub token: String,
    pub published_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TraceDirection {
    Callers,
    Callees,
}

impl TraceDirection {
    pub fn label(self) -> &'static str {
        match self {
            Self::Callers => "Callers",
            Self::Callees => "Callees",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "selector", rename_all = "kebab-case", deny_unknown_fields)]
pub enum TraceSelector {
    Position { file: PathBuf, line: u32, character: u32 },
    Symbol { query: String, scope: Option<PathBuf> },
}

impl TraceSelector {
    pub fn display_name(&self) -> String {
        match self {
            Self::Position { file, line, character } => {
                format!("{}:{}:{}", file.display(), line + 1, character + 1)
            }
            Self::Symbol { query, .. } => query.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceLimits {
    pub max_depth: u32,
    pub max_nodes: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceScope {
    pub source_roots: Vec<PathBuf>,
    pub stop_at_package_boundary: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ServiceCommand {
    Ping,
    Inspect,
    Trace {
        selector: TraceSelector,
        direction: TraceDirection,
        limits: TraceLimits,
        scope: TraceScope,
    },
    Locus {
        request: LocusRequest,
    },
    Diagnose {
        request: DiagnoseRequest,
    },
    Stop,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceRequest {
    pub token: String,
    pub request_id: String,
    pub command: ServiceCommand,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildIdentity {
    pub run_id: String,
    pub generation: u64,
    pub started_at_ms: u64,
    pub launcher: PathBuf,
    pub server_version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceInfo {
    pub key: String,
    pub protocol_version: u32,
    pub instance_id: String,
    pub started_at_ms: u64,
    pub request_count: u64,
    pub state: String,
    pub workspace: PathBuf,
    pub child: ChildIdentity,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceReply {
    pub request_id: String,
    pub ok: bool,
    pub fatal: bool,
    pub service: Option<ServiceInfo>,
    pub result: Option<Value>,
    pub error: Option<String>,
}

impl ServiceReply {
    pub fn success(request_id: String, service: ServiceInfo, result: Value) -> Self {
        Self {
            request_id,
            ok: true,
            fatal: false,
            service: Some(service),
            result: Some(result),
            error: None,
        }
    }

    pub fn error(request_id: String, error: impl Into<String>, fatal: bool) -> Self {
        Self {
            request_id,
            ok: false,
            fatal,
            service: None,
            result: None,
            error: Some(error.into()),
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceLocation {
    pub file: PathBuf,
    pub line: u32,
    pub character: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceCandidate {
    pub name: String,
    pub detail: Option<String>,
    pub kind: u64,
    pub location: TraceLocation,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceNode {
    pub id: String,
    pub name: String,
    pub detail: Option<String>,
    pub kind: u64,
    pub definition: TraceLocation,
    pub generated_aliases: Vec<TraceLocation>,
    pub external: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceEdge {
    pub caller: String,
    pub callee: String,
    pub call_sites: Vec<TraceLocation>,
    pub cycle: bool,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TraceBoundaryKind {
    External,
    SourceRoot,
    Package,
    MaxDepth,
    MaxNodes,
    MaxNativeVariants,
    MaxRelations,
    MaxCallSites,
}

impl TraceBoundaryKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::External => "external",
            Self::SourceRoot => "source-root",
            Self::Package => "package",
            Self::MaxDepth => "max-depth",
            Self::MaxNodes => "max-nodes",
            Self::MaxNativeVariants => "max-native-variants",
            Self::MaxRelations => "max-relations",
            Self::MaxCallSites => "max-call-sites",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceBoundary {
    pub node: String,
    pub kind: TraceBoundaryKind,
    pub omitted_relations: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceSummary {
    pub observed_leaves: usize,
    pub nodes: usize,
    pub edges: usize,
    pub cycle_components: usize,
    pub boundaries: usize,
    pub truncated: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceTiming {
    pub elapsed_ms: u64,
    pub native_requests: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceDiscovery {
    pub scanned_files: usize,
    pub activated_files: usize,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TraceStatus {
    CompleteWithinCapture,
    Cut,
    Ambiguous,
    NotFound,
}

impl TraceStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::CompleteWithinCapture => "complete-within-capture",
            Self::Cut => "cut",
            Self::Ambiguous => "ambiguous",
            Self::NotFound => "not-found",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TraceDocumentSync {
    Opened,
    Refreshed,
    Reused,
}

impl TraceDocumentSync {
    pub fn label(self) -> &'static str {
        match self {
            Self::Opened => "opened",
            Self::Refreshed => "refreshed",
            Self::Reused => "reused",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum TraceProjectContext {
    Configured { config: PathBuf },
    Inferred,
    NotQueried { reason: TraceProjectOmissionReason },
    Unavailable { detail: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TraceProjectOmissionReason {
    ProjectContextLimit,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceCoveredDocument {
    pub file: PathBuf,
    pub sync: TraceDocumentSync,
    pub project: TraceProjectContext,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TraceWorkspaceCoverage {
    ProjectFilesNotEnumerated,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceCoverage {
    pub documents: Vec<TraceCoveredDocument>,
    pub omitted_project_contexts: usize,
    pub workspace: TraceWorkspaceCoverage,
}

impl Default for TraceCoverage {
    fn default() -> Self {
        Self {
            documents: Vec::new(),
            omitted_project_contexts: 0,
            workspace: TraceWorkspaceCoverage::ProjectFilesNotEnumerated,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum TracePackageScope {
    Disabled,
    Unresolved,
    Enabled { root: PathBuf },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceScopeReceipt {
    pub source_roots: Vec<PathBuf>,
    pub package: TracePackageScope,
}

impl Default for TraceScopeReceipt {
    fn default() -> Self {
        Self { source_roots: Vec::new(), package: TracePackageScope::Disabled }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TraceCallerGapReason {
    NativeCallHierarchyIsNotAbsenceProof,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "reason", deny_unknown_fields)]
pub enum TraceIdentityGapReason {
    SourceDefinitionUnsupported,
    NoSourceDefinition,
    AmbiguousSourceDefinition { observed: usize },
    SourceOutsideWorkspace,
    SourceMapMissing,
    SourceMapInvalid { detail: String },
    SourcePositionUnmapped,
    SourcePreparationNotUnique { observed: usize },
    NativeRequestFailed { detail: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum TraceGap {
    CallerAbsenceUnproven {
        node: String,
        reason: TraceCallerGapReason,
    },
    GeneratedIdentityUnresolved {
        node: String,
        declaration: TraceLocation,
        reason: TraceIdentityGapReason,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TraceAdviceReason {
    BroadExpansion,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceAdvice {
    pub suggested_max_depth: u32,
    pub reason: TraceAdviceReason,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceResult {
    pub status: TraceStatus,
    pub selector: String,
    pub direction: TraceDirection,
    pub target: Option<String>,
    pub candidates: Vec<TraceCandidate>,
    pub nodes: BTreeMap<String, TraceNode>,
    pub edges: Vec<TraceEdge>,
    pub observed_leaves: Vec<String>,
    pub cycle_components: Vec<Vec<String>>,
    pub boundaries: Vec<TraceBoundary>,
    pub summary: TraceSummary,
    pub timing: TraceTiming,
    pub discovery: TraceDiscovery,
    pub coverage: TraceCoverage,
    pub scope: TraceScopeReceipt,
    pub gaps: Vec<TraceGap>,
    pub advice: Vec<TraceAdvice>,
    pub truncation_reasons: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TraceOutput {
    pub action: &'static str,
    pub service: ServiceInfo,
    pub result: TraceResult,
    pub ascii: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocusStatement {
    pub id: String,
    pub statement: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocusObligation {
    pub id: String,
    pub statement: String,
    pub acquisition_ids: Vec<String>,
    pub gap_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocusSeed {
    pub id: String,
    pub label: String,
    pub selector: TraceSelector,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocusInputPosition {
    pub file: PathBuf,
    pub line: u32,
    pub character: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocusSuppliedCandidate {
    pub id: String,
    pub label: String,
    pub position: LocusInputPosition,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum LocusOperation {
    Definition { max_results: usize },
    References { include_declaration: bool, max_results: usize },
    Implementations { max_results: usize },
    IncomingCalls { limits: TraceLimits },
    OutgoingCalls { limits: TraceLimits },
}

impl LocusOperation {
    pub fn relation(&self) -> LocusRelationKind {
        match self {
            Self::Definition { .. } => LocusRelationKind::Definition,
            Self::References { .. } => LocusRelationKind::Reference,
            Self::Implementations { .. } => LocusRelationKind::Implementation,
            Self::IncomingCalls { .. } => LocusRelationKind::IncomingCall,
            Self::OutgoingCalls { .. } => LocusRelationKind::OutgoingCall,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocusAcquisition {
    pub id: String,
    pub seed_id: String,
    pub required: bool,
    pub accept_no_call_item: bool,
    pub operation: LocusOperation,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "strategy", rename_all = "kebab-case", deny_unknown_fields)]
pub enum LocusDiscoveryStrategy {
    SuppliedAnchors {
        candidate_ids: Vec<String>,
    },
    SeedDefinitions {
        seed_ids: Vec<String>,
    },
    ReturnedImplementations {
        seed_ids: Vec<String>,
    },
    CallWitnessIntersection {
        seed_ids: Vec<String>,
        direction: TraceDirection,
        require_complete: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocusDiscoveryRule {
    pub id: String,
    pub strategy: LocusDiscoveryStrategy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocusGapFamily {
    EventReducer,
    DependencyInjectionRegistration,
    RuntimeObservation,
    ResourceFlow,
    UpstreamPolicy,
    OtherLanguage,
    DynamicDispatch,
    GeneratedCode,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocusDeclaredGap {
    pub id: String,
    pub family: LocusGapFamily,
    pub statement: String,
    pub required: bool,
    pub obligation_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocusRequest {
    pub schema_version: u32,
    pub goal: String,
    pub seeds: Vec<LocusSeed>,
    pub obligations: Vec<LocusObligation>,
    pub non_goals: Vec<LocusStatement>,
    pub assumptions: Vec<LocusStatement>,
    pub acquisitions: Vec<LocusAcquisition>,
    pub supplied_candidates: Vec<LocusSuppliedCandidate>,
    pub discovery: Vec<LocusDiscoveryRule>,
    pub declared_gaps: Vec<LocusDeclaredGap>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocusAnchor {
    pub label: String,
    pub location: TraceLocation,
    pub external: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocusSeedCandidate {
    pub label: String,
    pub anchor: LocusAnchor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocusSessionIntegrity {
    Preserved,
    Lost,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
pub enum LocusSeedResult {
    Resolved {
        seed_id: String,
        label: String,
        anchor: LocusAnchor,
        discovery: TraceDiscovery,
    },
    Ambiguous {
        seed_id: String,
        label: String,
        candidates: Vec<LocusSeedCandidate>,
        observed: usize,
        discovery: TraceDiscovery,
    },
    NotFound {
        seed_id: String,
        label: String,
        discovery: TraceDiscovery,
    },
    Failed {
        seed_id: String,
        label: String,
        reason: String,
        session_integrity: LocusSessionIntegrity,
        discovery: TraceDiscovery,
    },
    AmbiguousCallItem {
        seed_id: String,
        label: String,
        anchor: LocusAnchor,
        acquisition_id: String,
        candidates: Vec<LocusSeedCandidate>,
        observed: usize,
        discovery: TraceDiscovery,
    },
}

impl LocusSeedResult {
    pub fn seed_id(&self) -> &str {
        match self {
            Self::Resolved { seed_id, .. }
            | Self::Ambiguous { seed_id, .. }
            | Self::NotFound { seed_id, .. }
            | Self::Failed { seed_id, .. }
            | Self::AmbiguousCallItem { seed_id, .. } => seed_id,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocusRelationKind {
    Definition,
    Reference,
    Implementation,
    IncomingCall,
    OutgoingCall,
}

impl LocusRelationKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Definition => "definition",
            Self::Reference => "reference",
            Self::Implementation => "implementation",
            Self::IncomingCall => "incoming call",
            Self::OutgoingCall => "outgoing call",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocusEvidenceCapture {
    CompleteWithinCapture,
    RetainedBeforeCut,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocusEvidence {
    pub id: String,
    pub acquisition_id: String,
    pub seed_id: String,
    pub relation: LocusRelationKind,
    pub source: LocusAnchor,
    pub target: LocusAnchor,
    pub call_sites: Vec<TraceLocation>,
    pub capture: LocusEvidenceCapture,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocusCutReason {
    MaxResults,
    MaxDepth,
    MaxNodes,
    ExternalBoundary,
    DiscoveryLimit,
    MaxCallSites,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "knowledge", rename_all = "kebab-case", deny_unknown_fields)]
pub enum LocusOmission {
    Known { count: usize },
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocusCaptureCut {
    pub reason: LocusCutReason,
    pub omission: LocusOmission,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
pub enum LocusAcquisitionState {
    CompleteWithinCapture { retained: usize },
    Cut { retained: usize, cuts: Vec<LocusCaptureCut> },
    Unsupported { reason: String },
    NoCallItem,
    AmbiguousCallItem { candidates: Vec<LocusSeedCandidate>, observed: usize },
    Failed { reason: String, session_integrity: LocusSessionIntegrity },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocusPrepareReceipt {
    pub query_anchor: LocusAnchor,
    pub semantic_root: LocusAnchor,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocusAcquisitionResult {
    pub id: String,
    pub seed_id: String,
    pub required: bool,
    pub accept_no_call_item: bool,
    pub operation: LocusOperation,
    pub prepare: Option<LocusPrepareReceipt>,
    pub state: LocusAcquisitionState,
    pub evidence_ids: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocusCapturedFile {
    pub file: PathBuf,
    pub sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
pub enum LocusRecheckValue {
    Sha256 { sha256: String },
    Unavailable { reason: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocusChangedFile {
    pub file: PathBuf,
    pub before_sha256: String,
    pub after: LocusRecheckValue,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
pub enum LocusFreshness {
    Checked {
        files: Vec<LocusCapturedFile>,
    },
    ChangedObservedInput {
        unchanged_files: Vec<LocusCapturedFile>,
        changed_files: Vec<LocusChangedFile>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocusCapturedCandidate {
    pub request_id: String,
    pub label: String,
    pub anchor: LocusAnchor,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
pub enum LocusDiscoveryState {
    Applied,
    NoMatch,
    IncompleteEvidence,
    Cut { omission: LocusOmission },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocusDiscoveryReceipt {
    pub rule_id: String,
    pub strategy: LocusDiscoveryStrategy,
    pub state: LocusDiscoveryState,
    pub candidate_ids: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
pub enum LocusRequirementState {
    Witnessed { evidence_ids: Vec<String>, observed: usize },
    WitnessedBeforeCut { evidence_ids: Vec<String>, observed: usize },
    NotObservedWithinCompleteAcquisition,
    OpenCut,
    OpenUnsupported,
    OpenFailed,
    AcceptedNoCallItem,
    OpenNoCallItem,
    OpenDeclaredGap { gap_id: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocusRequirementResult {
    pub acquisition_id: String,
    pub state: LocusRequirementState,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocusCandidateObligation {
    pub obligation_id: String,
    pub requirements: Vec<LocusRequirementResult>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocusCandidate {
    pub id: String,
    pub label: String,
    pub anchor: LocusAnchor,
    pub discovered_by: String,
    pub obligations: Vec<LocusCandidateObligation>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocusObligationState {
    ClosedWithinDeclaredCapture,
    Open,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocusObligationResult {
    pub id: String,
    pub statement: String,
    pub state: LocusObligationState,
    pub acquisition_ids: Vec<String>,
    pub gap_ids: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocusGapProvenance {
    DeclaredByCase,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocusGap {
    pub id: String,
    pub family: LocusGapFamily,
    pub statement: String,
    pub required: bool,
    pub obligation_ids: Vec<String>,
    pub provenance: LocusGapProvenance,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "kebab-case", deny_unknown_fields)]
pub enum LocusBlock {
    AmbiguousSeed { seed_id: String },
    SeedNotFound { seed_id: String },
    SeedFailure { seed_id: String, session_integrity: LocusSessionIntegrity },
    ChangedObservedInput,
    LostSessionDuringAcquisition { acquisition_id: String },
    AmbiguousCallItem { acquisition_id: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocusStatus {
    Blocked,
    InvestigationRequired,
    NoCandidate,
    EvidenceReady,
}

impl LocusStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::InvestigationRequired => "investigation-required",
            Self::NoCandidate => "no-candidate",
            Self::EvidenceReady => "evidence-ready",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocusTiming {
    pub elapsed_ms: u64,
    pub native_requests: u64,
}

#[derive(Clone, Debug)]
pub struct LocusCapture {
    pub seeds: Vec<LocusSeedResult>,
    pub acquisitions: Vec<LocusAcquisitionResult>,
    pub evidence: Vec<LocusEvidence>,
    pub supplied_candidates: Vec<LocusCapturedCandidate>,
    pub freshness: LocusFreshness,
    pub fingerprint: String,
    pub timing: LocusTiming,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocusResult {
    pub goal: String,
    pub status: LocusStatus,
    pub blocks: Vec<LocusBlock>,
    pub seeds: Vec<LocusSeedResult>,
    pub obligations: Vec<LocusObligationResult>,
    pub assumptions: Vec<LocusStatement>,
    pub non_goals: Vec<LocusStatement>,
    pub acquisitions: Vec<LocusAcquisitionResult>,
    pub evidence: Vec<LocusEvidence>,
    pub candidates: Vec<LocusCandidate>,
    pub discovery_receipts: Vec<LocusDiscoveryReceipt>,
    pub gaps: Vec<LocusGap>,
    pub freshness: LocusFreshness,
    pub fingerprint: String,
    pub timing: LocusTiming,
}

#[derive(Clone, Debug, Serialize)]
pub struct LocusOutput {
    pub action: &'static str,
    pub service: ServiceInfo,
    pub result: LocusResult,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnoseRequest {
    pub files: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiagnosticAuthority {
    LanguageService,
    Compiler,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticPosition {
    /// One-based line number.
    pub line: u32,
    /// One-based UTF-16 character offset.
    pub character: u32,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticRange {
    pub start: DiagnosticPosition,
    pub end: DiagnosticPosition,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DiagnosticLocation {
    SourceRange { file: PathBuf, range: DiagnosticRange },
    SourcePoint { file: PathBuf, position: DiagnosticPosition },
    Project { config: PathBuf },
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
    Unspecified,
    Unknown { value: u64 },
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DiagnosticCode {
    Absent,
    Number { value: i64 },
    Text { value: String },
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DiagnosticSource {
    Absent,
    Named { name: String },
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DiagnosticTag {
    Unnecessary,
    Deprecated,
    Unknown { value: u64 },
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticRelatedInformation {
    pub location: DiagnosticLocation,
    pub message: String,
    pub message_truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diagnostic {
    pub id: String,
    pub authority: DiagnosticAuthority,
    pub location: DiagnosticLocation,
    pub severity: DiagnosticSeverity,
    pub code: DiagnosticCode,
    pub source: DiagnosticSource,
    pub message: String,
    pub message_truncated: bool,
    pub tags: Vec<DiagnosticTag>,
    pub related: Vec<DiagnosticRelatedInformation>,
    pub related_omitted: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticSummary {
    pub total: usize,
    pub returned: usize,
    pub omitted: usize,
    pub errors: usize,
    pub warnings: usize,
    pub information: usize,
    pub hints: usize,
    pub unspecified: usize,
    pub unknown: usize,
    pub truncated_details: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DiagnoseProject {
    Configured { config: PathBuf },
    Inferred,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosedDocument {
    pub file: PathBuf,
    pub sha256: String,
    pub server_document_version: i64,
    pub selected_project: DiagnoseProject,
    pub diagnostics: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DiagnosticRecheckValue {
    Present { sha256: String },
    Missing,
    Unreadable { detail: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangedDiagnosticDocument {
    pub file: PathBuf,
    pub before_sha256: String,
    pub after: DiagnosticRecheckValue,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
pub enum RequestedDocumentFreshness {
    Verified,
    Changed { files: Vec<ChangedDiagnosticDocument> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DiagnosticDependencyFreshness {
    Unchecked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DiagnosticProjectContexts {
    SelectedOnly,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DiagnoseIncompleteReason {
    ChangedInput,
    DiagnosticLimit { observed: usize, retained: usize },
    DiagnosticDetailLimit { omitted: usize },
    UnspecifiedSeverity { diagnostics: usize },
    UnknownSeverity { diagnostics: usize },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DiagnoseVerdict {
    NoLocalDiagnostics,
    LocalDiagnostics,
    Incomplete { reasons: Vec<DiagnoseIncompleteReason> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnoseCompleteness {
    pub requested_documents: usize,
    pub completed_documents: usize,
    pub inter_file_dependencies: bool,
    pub workspace_diagnostics: bool,
    pub project_contexts: DiagnosticProjectContexts,
    pub dependency_freshness: DiagnosticDependencyFreshness,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnoseTiming {
    pub elapsed_ms: u64,
    pub native_requests: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnoseResult {
    pub schema: u32,
    pub authority: DiagnosticAuthority,
    pub verdict: DiagnoseVerdict,
    pub documents: Vec<DiagnosedDocument>,
    pub diagnostics: Vec<Diagnostic>,
    pub summary: DiagnosticSummary,
    pub completeness: DiagnoseCompleteness,
    pub requested_document_freshness: RequestedDocumentFreshness,
    pub timing: DiagnoseTiming,
}

impl DiagnoseResult {
    pub fn exit_code(&self) -> i32 {
        match self.verdict {
            DiagnoseVerdict::Incomplete { .. } => 2,
            DiagnoseVerdict::LocalDiagnostics => 1,
            DiagnoseVerdict::NoLocalDiagnostics => 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum DiagnosticCommandFailure {
    Operational { detail: String },
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum DiagnoseOutput {
    Result { action: &'static str, service: ServiceInfo, result: DiagnoseResult, text: String },
    OperationalFailure { action: &'static str, failure: DiagnosticCommandFailure, text: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckProject {
    pub config: PathBuf,
    pub entry_config_sha256: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckCoverage {
    pub root_files: usize,
    pub project_references: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompilerInvocation {
    pub launcher: PathBuf,
    pub server_version: String,
    pub arguments: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum CompilerExit {
    Code { code: i32 },
    Signal { signal: i32 },
    NotObserved,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
pub enum CheckEntryConfigFreshness {
    Verified,
    Changed { before_sha256: String, after_sha256: String },
    Unreadable { detail: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
pub enum CheckInputFreshness {
    Unchecked,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum CompilerOutputEvidence {
    Classified,
    Unclassified {
        stdout: String,
        stderr: String,
    },
    Truncated {
        stdout: String,
        stderr: String,
        stdout_observed_bytes: u64,
        stderr_observed_bytes: u64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum CheckIncompleteReason {
    EntryConfigChanged,
    EntryConfigUnreadable,
    OutputTruncated,
    UnclassifiedOutput,
    DiagnosticLimit,
    DiagnosticDetailLimit,
    NoRootFiles,
    ProjectReferencesNotChecked { references: usize },
    ProjectDiagnostic { diagnostics: usize },
    InconsistentCompilerResult,
    UnexpectedExit,
    DeadlineExceeded,
    Cancelled,
    ExternalTermination,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum CheckVerdict {
    CompilerReportedNoDiagnostics,
    DiagnosticsPresent,
    Incomplete { reasons: Vec<CheckIncompleteReason> },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckTiming {
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckResult {
    pub schema: u32,
    pub authority: DiagnosticAuthority,
    pub workspace: PathBuf,
    pub project: CheckProject,
    pub coverage: CheckCoverage,
    pub invocation: CompilerInvocation,
    pub verdict: CheckVerdict,
    pub diagnostics: Vec<Diagnostic>,
    pub summary: DiagnosticSummary,
    pub output: CompilerOutputEvidence,
    pub exit: CompilerExit,
    pub entry_config_freshness: CheckEntryConfigFreshness,
    pub input_freshness: CheckInputFreshness,
    pub timing: CheckTiming,
}

impl CheckResult {
    pub fn exit_code(&self) -> i32 {
        match self.verdict {
            CheckVerdict::CompilerReportedNoDiagnostics => 0,
            CheckVerdict::DiagnosticsPresent => 1,
            CheckVerdict::Incomplete { .. } => 2,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum CheckOutput {
    Result { action: &'static str, result: CheckResult, text: String },
    OperationalFailure { action: &'static str, failure: DiagnosticCommandFailure, text: String },
}

#[derive(Clone, Debug, Serialize)]
pub struct InspectEntry {
    pub identity: ServiceIdentity,
    pub status: String,
    pub service: Option<ServiceInfo>,
    pub daemon_run_id: Option<String>,
    pub result: Option<Value>,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ManagementOutput {
    pub action: &'static str,
    pub matched: usize,
    pub changed: usize,
    pub services: Vec<InspectEntry>,
}
