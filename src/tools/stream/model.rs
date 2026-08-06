use serde::{Deserialize, Serialize};

pub(super) const STREAM_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct StreamInspection {
    pub schema_version: u32,
    pub target: HostTarget,
    pub readiness: StreamReadiness,
    pub tailscale: TailscaleReadiness,
    pub hyprland: Option<HyprlandInspection>,
    pub sunshine: ExecutableInspection,
    pub moonlight: ExecutableInspection,
    pub diagnostics: Vec<Diagnostic>,
}

impl StreamInspection {
    pub(super) fn local() -> Self {
        Self {
            schema_version: STREAM_SCHEMA_VERSION,
            target: HostTarget::local(),
            readiness: StreamReadiness::Unavailable,
            tailscale: TailscaleReadiness::Unavailable,
            hyprland: None,
            sunshine: ExecutableInspection::unavailable(),
            moonlight: ExecutableInspection::unavailable(),
            diagnostics: Vec::new(),
        }
    }

    pub(super) fn refresh_readiness(&mut self) {
        self.readiness = if self.hyprland.is_some()
            && self.sunshine.available
            && self.moonlight.available
            && !self
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error)
        {
            StreamReadiness::Ready
        } else if self.diagnostics.iter().any(|diagnostic| diagnostic.action.is_some()) {
            StreamReadiness::SetupRequired
        } else {
            StreamReadiness::Unavailable
        };
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct StreamStatusReport {
    pub schema_version: u32,
    pub session: StreamSessionState,
    pub inspection: StreamInspection,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct StreamDoctorReport {
    pub schema_version: u32,
    pub ready: bool,
    pub target: HostTarget,
    pub checks: Vec<DoctorCheck>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct StreamSetupReport {
    pub schema_version: u32,
    pub action: String,
    pub state: StreamSetupState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<HostTarget>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum StreamSetupState {
    Ready,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct HostTarget {
    pub kind: HostTargetKind,
    pub display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stable_node_id: Option<String>,
    pub operating_system: String,
    pub online: bool,
}

impl HostTarget {
    fn local() -> Self {
        Self {
            kind: HostTargetKind::Local,
            display_name: "local".to_owned(),
            stable_node_id: None,
            operating_system: "linux".to_owned(),
            online: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum HostTargetKind {
    Local,
    Remote,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum StreamReadiness {
    Ready,
    SetupRequired,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TailscaleReadiness {
    Ready,
    LoginRequired,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum StreamSessionState {
    Inactive,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct HyprlandInspection {
    pub version: Option<String>,
    pub instance_count: usize,
    pub selected_instance: String,
    pub outputs: Vec<DisplaySource>,
    pub workspaces: Vec<WorkspaceSource>,
    pub windows: Vec<WindowSource>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct DisplaySource {
    pub name: String,
    pub description: String,
    pub width: i64,
    pub height: i64,
    pub refresh_hz: f64,
    pub x: i64,
    pub y: i64,
    pub scale: f64,
    pub transform: i64,
    pub focused: bool,
    pub managed_by_kit: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct WorkspaceSource {
    pub id: i64,
    pub name: String,
    pub output: String,
    pub windows: i64,
    pub has_fullscreen: bool,
    pub layout: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct WindowSource {
    pub address: String,
    pub stable_id: String,
    pub class: String,
    pub title: String,
    pub workspace_id: i64,
    pub workspace_name: String,
    pub mapped: bool,
    pub hidden: bool,
    pub floating: bool,
    pub fullscreen: bool,
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct ExecutableInspection {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<ServiceInspection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub listeners: Vec<String>,
}

impl ExecutableInspection {
    pub(super) fn unavailable() -> Self {
        Self { available: false, version: None, service: None, listeners: Vec::new() }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct ServiceInspection {
    pub active: bool,
    pub active_state: String,
    pub sub_state: String,
    pub main_pid: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Diagnostic {
    pub id: String,
    pub severity: DiagnosticSeverity,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<SetupAction>,
}

impl Diagnostic {
    pub(super) fn warning(id: &str, summary: impl Into<String>) -> Self {
        Self {
            id: id.to_owned(),
            severity: DiagnosticSeverity::Warning,
            summary: summary.into(),
            detail: None,
            action: None,
        }
    }

    pub(super) fn error(id: &str, summary: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            id: id.to_owned(),
            severity: DiagnosticSeverity::Error,
            summary: summary.into(),
            detail: Some(detail.into()),
            action: None,
        }
    }

    pub(super) fn setup(id: &str, summary: impl Into<String>, action: SetupAction) -> Self {
        Self {
            id: id.to_owned(),
            severity: DiagnosticSeverity::Warning,
            summary: summary.into(),
            detail: None,
            action: Some(action),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum DiagnosticSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct SetupAction {
    pub id: String,
    pub label: String,
    pub kind: SetupActionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SetupActionKind {
    AuthenticateTailscale,
    ConfigureSshUser,
    InstallDependency,
    StartService,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct DoctorCheck {
    pub id: String,
    pub status: DoctorCheckStatus,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<SetupAction>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum DoctorCheckStatus {
    Pass,
    Attention,
    Fail,
}
