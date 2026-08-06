use std::{collections::BTreeMap, ffi::OsString, path::PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use super::{
    command::{CapturedCommand, CommandRunner},
    config::Executables,
    model::{
        Diagnostic, DisplaySource, ExecutableInspection, HyprlandInspection, ServiceInspection,
        StreamInspection, WindowSource, WorkspaceSource,
    },
};

const STREAM_OUTPUT_PREFIX: &str = "KIT-STREAM-";
const SUNSHINE_PORTS: [&str; 7] =
    [":47984", ":47989", ":47990", ":47998", ":47999", ":48000", ":48010"];

pub(super) struct LinuxInspector {
    runner: CommandRunner,
    executables: Executables,
}

impl LinuxInspector {
    pub(super) fn new(
        processes: crate::framework::process::ProcessSupervisor,
        working_directory: PathBuf,
        executables: Executables,
    ) -> Self {
        Self { runner: CommandRunner::new(processes, working_directory), executables }
    }

    pub(super) async fn inspect(&self) -> StreamInspection {
        let mut inspection = StreamInspection::local();
        match self.inspect_hyprland().await {
            Ok(hyprland) => inspection.hyprland = Some(hyprland),
            Err(error) => inspection.diagnostics.push(Diagnostic::error(
                "hyprland.unavailable",
                "Hyprland inspection is unavailable",
                format!("{error:#}"),
            )),
        }

        let (sunshine, moonlight) = tokio::join!(self.inspect_sunshine(), self.inspect_moonlight());
        match sunshine {
            Ok(sunshine) => inspection.sunshine = sunshine,
            Err(_error) => inspection.diagnostics.push(Diagnostic::setup(
                "sunshine.unavailable",
                "Sunshine is unavailable",
                super::model::SetupAction {
                    id: "stream.setup.installSunshine".to_owned(),
                    label: "Install Sunshine".to_owned(),
                    kind: super::model::SetupActionKind::InstallDependency,
                    value: None,
                },
            )),
        }
        match moonlight {
            Ok(moonlight) => inspection.moonlight = moonlight,
            Err(_error) => inspection.diagnostics.push(Diagnostic::setup(
                "moonlight.unavailable",
                "Moonlight is unavailable",
                super::model::SetupAction {
                    id: "stream.setup.installMoonlight".to_owned(),
                    label: "Install Moonlight".to_owned(),
                    kind: super::model::SetupActionKind::InstallDependency,
                    value: None,
                },
            )),
        }
        inspection.refresh_readiness();
        inspection
    }

    async fn inspect_hyprland(&self) -> Result<HyprlandInspection> {
        let runtime_environment = runtime_environment();
        let instances_report = self
            .runner
            .capture_with_environment(
                self.executables.hyprctl.clone(),
                os_args(["instances", "-j"]),
                "inspect Hyprland instances",
                runtime_environment.clone(),
            )
            .await
            .context("run hyprctl instances")?;
        require_success("hyprctl instances", &instances_report)?;
        let instances: Vec<HyprlandInstanceWire> = serde_json::from_slice(&instances_report.stdout)
            .context("decode Hyprland instances")?;
        let selected = select_instance(&instances)?;
        let mut environment = runtime_environment;
        environment.insert(
            OsString::from("HYPRLAND_INSTANCE_SIGNATURE"),
            OsString::from(&selected.instance),
        );
        environment
            .insert(OsString::from("WAYLAND_DISPLAY"), OsString::from(&selected.wayland_socket));

        let version_report = self
            .runner
            .capture_with_environment(
                self.executables.hyprctl.clone(),
                os_args(["version"]),
                "inspect Hyprland version",
                environment.clone(),
            )
            .await?;
        let monitors_report = self
            .runner
            .capture_with_environment(
                self.executables.hyprctl.clone(),
                os_args(["-j", "monitors"]),
                "inspect Hyprland outputs",
                environment.clone(),
            )
            .await?;
        let workspaces_report = self
            .runner
            .capture_with_environment(
                self.executables.hyprctl.clone(),
                os_args(["-j", "workspaces"]),
                "inspect Hyprland workspaces",
                environment.clone(),
            )
            .await?;
        let windows_report = self
            .runner
            .capture_with_environment(
                self.executables.hyprctl.clone(),
                os_args(["-j", "clients"]),
                "inspect Hyprland windows",
                environment,
            )
            .await?;
        require_success("hyprctl monitors", &monitors_report)?;
        require_success("hyprctl workspaces", &workspaces_report)?;
        require_success("hyprctl clients", &windows_report)?;

        let monitors: Vec<MonitorWire> =
            serde_json::from_slice(&monitors_report.stdout).context("decode Hyprland outputs")?;
        let workspaces: Vec<WorkspaceWire> = serde_json::from_slice(&workspaces_report.stdout)
            .context("decode Hyprland workspaces")?;
        let windows: Vec<WindowWire> =
            serde_json::from_slice(&windows_report.stdout).context("decode Hyprland windows")?;

        Ok(HyprlandInspection {
            version: version_report.succeeded().then(|| first_line(&version_report)),
            instance_count: instances.len(),
            selected_instance: selected.instance.clone(),
            outputs: monitors.into_iter().map(DisplaySource::from).collect(),
            workspaces: workspaces.into_iter().map(WorkspaceSource::from).collect(),
            windows: windows
                .into_iter()
                .filter(|window| window.mapped)
                .map(WindowSource::from)
                .collect(),
        })
    }

    async fn inspect_sunshine(&self) -> Result<ExecutableInspection> {
        let version_report = self
            .runner
            .capture(
                self.executables.sunshine.clone(),
                os_args(["--version"]),
                "inspect Sunshine version",
            )
            .await
            .context("run Sunshine version probe")?;
        require_success("sunshine --version", &version_report)?;
        let service = self.inspect_sunshine_service().await.ok();
        let listeners = self.inspect_sunshine_listeners().await.unwrap_or_default();
        Ok(ExecutableInspection {
            available: true,
            version: find_version(&version_report, "Sunshine"),
            service,
            listeners,
        })
    }

    async fn inspect_moonlight(&self) -> Result<ExecutableInspection> {
        let report = self
            .runner
            .capture(
                self.executables.moonlight.clone(),
                os_args(["--version"]),
                "inspect Moonlight version",
            )
            .await
            .context("run Moonlight version probe")?;
        require_success("moonlight --version", &report)?;
        Ok(ExecutableInspection {
            available: true,
            version: find_version(&report, "Moonlight"),
            service: None,
            listeners: Vec::new(),
        })
    }

    async fn inspect_sunshine_service(&self) -> Result<ServiceInspection> {
        let report = self
            .runner
            .capture(
                self.executables.systemctl.clone(),
                os_args([
                    "--user",
                    "show",
                    "sunshine.service",
                    "-p",
                    "ActiveState",
                    "-p",
                    "SubState",
                    "-p",
                    "MainPID",
                    "--no-pager",
                ]),
                "inspect Sunshine service",
            )
            .await?;
        require_success("systemctl show sunshine.service", &report)?;
        let output = report.stdout_text();
        let fields =
            output.lines().filter_map(|line| line.split_once('=')).collect::<BTreeMap<_, _>>();
        let active_state = fields.get("ActiveState").copied().unwrap_or("unknown").to_owned();
        let sub_state = fields.get("SubState").copied().unwrap_or("unknown").to_owned();
        let main_pid = fields.get("MainPID").and_then(|value| value.parse().ok()).unwrap_or(0);
        Ok(ServiceInspection {
            active: active_state == "active",
            active_state,
            sub_state,
            main_pid,
        })
    }

    async fn inspect_sunshine_listeners(&self) -> Result<Vec<String>> {
        let report = self
            .runner
            .capture(
                self.executables.socket_inspector.clone(),
                os_args(["-H", "-lntp"]),
                "inspect Sunshine listener ports",
            )
            .await?;
        require_success("socket listener inspection", &report)?;
        Ok(report
            .stdout_text()
            .lines()
            .filter(|line| SUNSHINE_PORTS.iter().any(|port| line.contains(port)))
            .map(str::to_owned)
            .collect())
    }
}

fn runtime_environment() -> BTreeMap<OsString, OsString> {
    let mut values = BTreeMap::new();
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        values.insert(
            OsString::from("XDG_RUNTIME_DIR"),
            OsString::from(format!("/run/user/{}", rustix::process::getuid().as_raw())),
        );
    }
    values
}

fn select_instance(instances: &[HyprlandInstanceWire]) -> Result<&HyprlandInstanceWire> {
    if let Some(signature) = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE") {
        let signature = signature.to_string_lossy();
        if let Some(instance) = instances.iter().find(|instance| instance.instance == signature) {
            return Ok(instance);
        }
    }
    match instances {
        [instance] => Ok(instance),
        [] => bail!("no Hyprland instance is running for the current user"),
        _ => bail!(
            "multiple Hyprland instances are running; select one through Stream host configuration"
        ),
    }
}

fn require_success(label: &str, report: &CapturedCommand) -> Result<()> {
    if report.succeeded() {
        Ok(())
    } else {
        bail!("{label} exited with {:?}: {}", report.exit, report.detail())
    }
}

fn first_line(report: &CapturedCommand) -> String {
    let stdout = report.stdout_text();
    let stderr = report.stderr_text();
    stdout
        .lines()
        .chain(stderr.lines())
        .find(|line| !line.trim().is_empty())
        .unwrap_or("unknown")
        .trim()
        .to_owned()
}

fn find_version(report: &CapturedCommand, product: &str) -> Option<String> {
    let stdout = report.stdout_text();
    let stderr = report.stderr_text();
    stdout
        .lines()
        .chain(stderr.lines())
        .find(|line| line.to_ascii_lowercase().contains(&product.to_ascii_lowercase()))
        .map(|line| line.trim().to_owned())
        .or_else(|| Some(first_line(report)))
}

fn os_args<const N: usize>(arguments: [&str; N]) -> Vec<OsString> {
    arguments.into_iter().map(OsString::from).collect()
}

#[derive(Deserialize)]
struct HyprlandInstanceWire {
    instance: String,
    #[serde(rename = "wl_socket")]
    wayland_socket: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MonitorWire {
    name: String,
    #[serde(default)]
    description: String,
    width: i64,
    height: i64,
    refresh_rate: f64,
    x: i64,
    y: i64,
    scale: f64,
    transform: i64,
    focused: bool,
}

impl From<MonitorWire> for DisplaySource {
    fn from(value: MonitorWire) -> Self {
        let managed_by_kit = value.name.starts_with(STREAM_OUTPUT_PREFIX);
        Self {
            name: value.name,
            description: value.description,
            width: value.width,
            height: value.height,
            refresh_hz: value.refresh_rate,
            x: value.x,
            y: value.y,
            scale: value.scale,
            transform: value.transform,
            focused: value.focused,
            managed_by_kit,
        }
    }
}

#[derive(Deserialize)]
struct WorkspaceWire {
    id: i64,
    name: String,
    monitor: String,
    windows: i64,
    hasfullscreen: bool,
    #[serde(default, rename = "tiledLayout")]
    tiled_layout: String,
}

impl From<WorkspaceWire> for WorkspaceSource {
    fn from(value: WorkspaceWire) -> Self {
        Self {
            id: value.id,
            name: value.name,
            output: value.monitor,
            windows: value.windows,
            has_fullscreen: value.hasfullscreen,
            layout: value.tiled_layout,
        }
    }
}

#[derive(Deserialize)]
struct WindowWire {
    address: String,
    #[serde(default, rename = "stableId")]
    stable_id: String,
    #[serde(default)]
    class: String,
    #[serde(default)]
    title: String,
    workspace: WorkspaceIdentityWire,
    mapped: bool,
    hidden: bool,
    floating: bool,
    fullscreen: i64,
    at: [i64; 2],
    size: [i64; 2],
}

#[derive(Deserialize)]
struct WorkspaceIdentityWire {
    id: i64,
    name: String,
}

impl From<WindowWire> for WindowSource {
    fn from(value: WindowWire) -> Self {
        Self {
            address: value.address,
            stable_id: value.stable_id,
            class: value.class,
            title: value.title,
            workspace_id: value.workspace.id,
            workspace_name: value.workspace.name,
            mapped: value.mapped,
            hidden: value.hidden,
            floating: value.floating,
            fullscreen: value.fullscreen != 0,
            x: value.at[0],
            y: value.at[1],
            width: value.size[0],
            height: value.size[1],
        }
    }
}
