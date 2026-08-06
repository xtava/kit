use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _, Result};
use serde::Serialize;

use crate::{
    framework::{process::ProcessSupervisor, ConfigStore},
    tailscale::{
        prepare_mutagen_ssh_transport, Node, OperatingSystem, Readiness, Status, TailscaleClient,
        TailscaleSshTarget,
    },
};

use super::{
    config::Config,
    engine::{
        EndpointPlatform, MutagenClient, Session, SessionHealth, SessionMonitor, SUPPORTED_VERSION,
    },
    model::{LocalEndpoint, ProjectLifecycle, RemoteEndpoint, SourcePolicy, SyncedProject},
    remote::{RemoteProbe, RemoteProbeError},
};

#[derive(Clone)]
pub(super) struct SyncController {
    processes: ProcessSupervisor,
    config_store: ConfigStore,
    working_directory: PathBuf,
    engine: MutagenClient,
}

pub(super) struct AddRequest {
    pub name: String,
    pub machine: String,
    pub remote_root: PathBuf,
    pub user: String,
    pub local_root: Option<PathBuf>,
    pub excludes: Vec<String>,
    pub includes: Vec<String>,
}

impl SyncController {
    pub(super) fn new(
        processes: ProcessSupervisor,
        config_store: ConfigStore,
        working_directory: PathBuf,
    ) -> Result<Self> {
        let engine = MutagenClient::new(processes.clone(), working_directory.clone())?;
        Ok(Self { processes, config_store, working_directory, engine })
    }

    pub(super) async fn add(&self, request: AddRequest) -> Result<ProjectReport> {
        self.engine.verify_installation().await?;
        let node = self.resolve_peer(&request.machine).await?;
        let remote_platform = ensure_supported_peer(&node)?;
        let local_root = canonical_local_root(
            &request.local_root.unwrap_or_else(|| self.working_directory.clone()),
        )?;
        let source = if request.excludes.is_empty() && request.includes.is_empty() {
            SourcePolicy::default()
        } else {
            SourcePolicy::new(request.excludes, request.includes)?
        };
        let project = SyncedProject::new(
            request.name,
            LocalEndpoint::new(local_root)?,
            RemoteEndpoint::new(node.id.clone(), request.user, request.remote_root)?,
            source,
        )?;
        let mut config = Config::load(self.config_store.clone())?;
        config.ensure_addable(&project)?;
        RemoteProbe::new(self.processes.clone(), self.working_directory.clone())
            .require_directory(&node, project.remote())
            .await?;
        let address = node.addresses.first().copied().context("remote peer has no Tailscale IP")?;
        let ssh_target =
            TailscaleSshTarget::new(node.id.clone(), project.remote().unix_user(), address)?;
        let ssh_transport = prepare_mutagen_ssh_transport(&ssh_target, &node.dns_name)?;
        config.add(project.clone())?;
        self.engine.create(&project, ssh_transport.host(), remote_platform).await.with_context(
            || {
                format!(
                    "create runtime for Synced Project {:?}; durable lifecycle remains `creating`",
                    project.name()
                )
            },
        )?;
        let sessions = self.engine.project_sessions(project.id()).await?;
        if !matches!(sessions.as_slice(), [session] if session.matches_project(&project)) {
            bail!(
                "Mutagen created an invalid runtime for Synced Project {:?} ({} matching sessions); \
                 durable lifecycle remains `creating`",
                project.name(),
                sessions.len(),
            );
        }
        config.set_lifecycle(project.id(), ProjectLifecycle::Active)?;
        let project = config
            .project(project.id())
            .context("Synced Project disappeared while completing creation")?
            .clone();
        Ok(ProjectReport::from_project(project, sessions))
    }

    pub(super) async fn status(&self, selector: Option<&str>) -> Result<Vec<ProjectReport>> {
        self.engine.verify_installation().await?;
        let config = Config::load(self.config_store.clone())?;
        let projects = selected_projects(&config, selector)?.into_iter().cloned().collect();
        reports(&self.engine, projects).await
    }

    pub(super) async fn pause(&self, selector: &str) -> Result<ProjectReport> {
        let mut config = Config::load(self.config_store.clone())?;
        let id = config.resolve(selector)?.id();
        ensure_lifecycle(
            config.resolve(selector)?,
            "pause",
            &[ProjectLifecycle::Active, ProjectLifecycle::Paused],
        )?;
        config.set_lifecycle(id, ProjectLifecycle::Paused)?;
        let project =
            config.project(id).context("Synced Project disappeared while pausing")?.clone();
        let mut session = require_one_session(&self.engine, &project).await?;
        self.engine.pause(project.id()).await.with_context(|| {
            format!(
                "apply saved `paused` lifecycle for Synced Project {:?}; retry `kit sync pause {}`",
                project.name(),
                project.name()
            )
        })?;
        session.mark_paused();
        Ok(ProjectReport::from_project(project, vec![session]))
    }

    pub(super) async fn resume(&self, selector: &str) -> Result<ProjectReport> {
        let mut config = Config::load(self.config_store.clone())?;
        let id = config.resolve(selector)?.id();
        ensure_lifecycle(
            config.resolve(selector)?,
            "resume",
            &[ProjectLifecycle::Active, ProjectLifecycle::Paused],
        )?;
        config.set_lifecycle(id, ProjectLifecycle::Active)?;
        let project =
            config.project(id).context("Synced Project disappeared while resuming")?.clone();
        let mut session = require_one_session(&self.engine, &project).await?;
        self.engine.resume(project.id()).await.with_context(|| {
            format!(
                "apply saved `active` lifecycle for Synced Project {:?}; retry `kit sync resume {}`",
                project.name(),
                project.name()
            )
        })?;
        self.engine.flush(project.id()).await.with_context(|| {
            format!(
                "flush resumed Synced Project {:?}; durable lifecycle is `active`",
                project.name()
            )
        })?;
        session.mark_flushed();
        Ok(ProjectReport::from_project(project, vec![session]))
    }

    pub(super) async fn flush(&self, selector: &str) -> Result<ProjectReport> {
        let config = Config::load(self.config_store.clone())?;
        let project = config.resolve(selector)?.clone();
        ensure_lifecycle(&project, "flush", &[ProjectLifecycle::Active])?;
        let mut session = require_one_session(&self.engine, &project).await?;
        self.engine.flush(project.id()).await?;
        session.mark_flushed();
        Ok(ProjectReport::from_project(project, vec![session]))
    }

    pub(super) async fn remove(&self, selector: &str) -> Result<SyncedProject> {
        let mut config = Config::load(self.config_store.clone())?;
        let id = config.resolve(selector)?.id();
        config.set_lifecycle(id, ProjectLifecycle::Removing)?;
        let project =
            config.project(id).context("Synced Project disappeared while removing")?.clone();
        let sessions = self.engine.project_sessions(project.id()).await?;
        if !sessions.is_empty() {
            self.engine.terminate(project.id()).await.with_context(|| {
                format!(
                    "apply saved `removing` lifecycle for Synced Project {:?}; retry `kit sync remove {}`",
                    project.name(),
                    project.name()
                )
            })?;
        }
        let remaining = self.engine.project_sessions(project.id()).await?;
        if !remaining.is_empty() {
            bail!(
                "Mutagen session for Synced Project {:?} still exists; configuration was preserved",
                project.name()
            );
        }
        config
            .remove(project.id())?
            .context("Synced Project disappeared before removal completed")?;
        Ok(project)
    }

    pub(super) async fn doctor(&self, selector: Option<&str>) -> Result<DoctorReport> {
        let config = Config::load(self.config_store.clone())?;
        let (mutagen, mut next_action) = match self.engine.verify_installation().await {
            Ok(()) => (
                Check {
                    status: CheckStatus::Ready,
                    detail: format!("Mutagen {SUPPORTED_VERSION} is ready"),
                },
                None,
            ),
            Err(error) => (
                Check { status: CheckStatus::ActionRequired, detail: error.to_string() },
                Some(format!("install Mutagen {SUPPORTED_VERSION} and retry")),
            ),
        };
        let tailscale_client =
            TailscaleClient::new(self.processes.clone(), self.working_directory.clone());
        let (tailscale, tailnet) = match tailscale_client.readiness().await? {
            Readiness::Ready(status) => (
                Check { status: CheckStatus::Ready, detail: "Tailscale is ready".to_owned() },
                Some(status),
            ),
            Readiness::NeedsLogin => {
                next_action.get_or_insert_with(|| "run `tailscale login` and retry".to_owned());
                (
                    Check {
                        status: CheckStatus::ActionRequired,
                        detail: "Tailscale authentication is required".to_owned(),
                    },
                    None,
                )
            }
            readiness => {
                next_action.get_or_insert_with(|| "repair Tailscale and retry".to_owned());
                (
                    Check {
                        status: CheckStatus::ActionRequired,
                        detail: readiness_detail(readiness),
                    },
                    None,
                )
            }
        };
        let selected = selector.map(|selector| config.resolve(selector)).transpose()?;
        let remote = match (selected, tailnet.as_ref()) {
            (Some(project), Some(status)) => {
                let (check, action) = self.inspect_remote(status, project).await;
                if let Some(action) = action {
                    next_action.get_or_insert(action);
                }
                Some(check)
            }
            (Some(_), None) => Some(Check {
                status: CheckStatus::ActionRequired,
                detail: "remote access cannot be checked until Tailscale is ready".to_owned(),
            }),
            (None, _) => None,
        };
        let project = match selected {
            Some(project) => Some(match self.engine.project_sessions(project.id()).await {
                Ok(sessions) => {
                    let state = ProjectReport::from_project(project.clone(), sessions).state;
                    project_check(project, state, &mut next_action)
                }
                Err(error) => {
                    next_action.get_or_insert_with(|| "repair Mutagen and retry".to_owned());
                    Check { status: CheckStatus::ActionRequired, detail: error.to_string() }
                }
            }),
            None => None,
        };
        Ok(DoctorReport { mutagen, tailscale, remote, project, next_action })
    }

    pub(super) async fn monitor(&self, selector: &str) -> Result<ProjectMonitor> {
        self.engine.verify_installation().await?;
        let project = Config::load(self.config_store.clone())?.resolve(selector)?.clone();
        ensure_lifecycle(
            &project,
            "monitor",
            &[ProjectLifecycle::Active, ProjectLifecycle::Paused],
        )?;
        require_one_session(&self.engine, &project).await?;
        let monitor = self.engine.monitor(project.id()).await?;
        Ok(ProjectMonitor { project, monitor })
    }

    pub(super) fn load_config(&self) -> Result<Config> {
        Config::load(self.config_store.clone())
    }

    async fn resolve_peer(&self, selector: &str) -> Result<Node> {
        let client = TailscaleClient::new(self.processes.clone(), self.working_directory.clone());
        let Readiness::Ready(status) = client.readiness().await? else {
            bail!("Tailscale is not ready; run `kit sync doctor`")
        };
        Ok(status.resolve_peer(selector)?.clone())
    }

    async fn inspect_remote(
        &self,
        status: &Status,
        project: &SyncedProject,
    ) -> (Check, Option<String>) {
        let node = match status.resolve_peer(project.remote().stable_node_id()) {
            Ok(node) => node,
            Err(error) => {
                return (
                    Check { status: CheckStatus::ActionRequired, detail: error.to_string() },
                    Some("restore the configured Tailscale machine and retry".to_owned()),
                );
            }
        };
        if let Err(error) = ensure_supported_peer(node) {
            return (
                Check { status: CheckStatus::ActionRequired, detail: error.to_string() },
                Some("bring the configured Linux or macOS machine online and retry".to_owned()),
            );
        }
        match RemoteProbe::new(self.processes.clone(), self.working_directory.clone())
            .require_directory(node, project.remote())
            .await
        {
            Ok(()) => (
                Check {
                    status: CheckStatus::Ready,
                    detail: format!("OpenSSH and {} are ready", project.remote().root().display()),
                },
                None,
            ),
            Err(RemoteProbeError::AuthenticationRequired { machine, url }) => (
                Check {
                    status: CheckStatus::ActionRequired,
                    detail: format!("OpenSSH authentication is required for {machine}"),
                },
                Some(format!("open {url} and retry")),
            ),
            Err(RemoteProbeError::DirectoryMissing { machine, path }) => (
                Check {
                    status: CheckStatus::ActionRequired,
                    detail: format!("{} does not exist on {machine}", path.display()),
                },
                Some(format!("create {} on {machine} and retry", path.display())),
            ),
            Err(error) => (
                Check { status: CheckStatus::ActionRequired, detail: error.to_string() },
                Some("repair OpenSSH access over Tailscale and retry".to_owned()),
            ),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ProjectReport {
    pub project: SyncedProject,
    pub state: ProjectState,
    pub sessions: Vec<Session>,
}

impl ProjectReport {
    pub(super) fn from_project(project: SyncedProject, sessions: Vec<Session>) -> Self {
        let state = match sessions.as_slice() {
            _ if project.lifecycle() == ProjectLifecycle::Removing => ProjectState::Removing,
            [] if project.lifecycle() == ProjectLifecycle::Creating => ProjectState::Creating,
            [] => ProjectState::Missing,
            [session] if session.creating_version != SUPPORTED_VERSION => {
                ProjectState::Incompatible
            }
            [session] if !session.matches_project(&project) => ProjectState::Stale,
            [session] if project.lifecycle() == ProjectLifecycle::Creating => {
                ProjectState::Creating
            }
            [session] if project.lifecycle() == ProjectLifecycle::Active && session.paused => {
                ProjectState::NeedsResume
            }
            [session] if project.lifecycle() == ProjectLifecycle::Paused && !session.paused => {
                ProjectState::NeedsPause
            }
            [session] => ProjectState::Session(session.health()),
            _ => ProjectState::Duplicate,
        };
        Self { project, state, sessions }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum ProjectState {
    Creating,
    Removing,
    Missing,
    Duplicate,
    Incompatible,
    Stale,
    NeedsPause,
    NeedsResume,
    Session(SessionHealth),
}

impl ProjectState {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Removing => "removing",
            Self::Missing => "missing",
            Self::Duplicate => "duplicate",
            Self::Incompatible => "incompatible",
            Self::Stale => "stale",
            Self::NeedsPause => "needs pause",
            Self::NeedsResume => "needs resume",
            Self::Session(health) => health.label(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DoctorReport {
    pub mutagen: Check,
    pub tailscale: Check,
    pub remote: Option<Check>,
    pub project: Option<Check>,
    pub next_action: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Check {
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum CheckStatus {
    Ready,
    ActionRequired,
}

async fn reports(
    engine: &MutagenClient,
    projects: Vec<SyncedProject>,
) -> Result<Vec<ProjectReport>> {
    if projects.is_empty() {
        return Ok(Vec::new());
    }
    let mut inventory = engine.session_inventory().await?;
    let mut reports = Vec::with_capacity(projects.len());
    for project in projects {
        let sessions = inventory.remove(&project.id()).unwrap_or_default();
        reports.push(ProjectReport::from_project(project, sessions));
    }
    Ok(reports)
}

async fn require_one_session(engine: &MutagenClient, project: &SyncedProject) -> Result<Session> {
    let mut sessions = engine.project_sessions(project.id()).await?;
    match sessions.as_slice() {
        [session] if session.creating_version != SUPPORTED_VERSION => bail!(
            "Synced Project {:?} uses incompatible Mutagen session version {:?}; run `kit sync doctor {}`",
            project.name(),
            session.creating_version,
            project.name()
        ),
        [session] if !session.matches_project(project) => bail!(
            "Synced Project {:?} has stale runtime state; run `kit sync doctor {}`",
            project.name(),
            project.name()
        ),
        [_] => Ok(sessions.pop().expect("exactly one session checked above")),
        [] => bail!(
            "Synced Project {:?} has no Mutagen session; run `kit sync doctor {}`",
            project.name(),
            project.name()
        ),
        sessions => bail!(
            "Synced Project {:?} has {count} Mutagen sessions; run `kit sync doctor {}`",
            project.name(),
            project.name(),
            count = sessions.len()
        ),
    }
}

fn ensure_lifecycle(
    project: &SyncedProject,
    operation: &str,
    allowed: &[ProjectLifecycle],
) -> Result<()> {
    if allowed.contains(&project.lifecycle()) {
        Ok(())
    } else {
        bail!(
            "cannot {operation} Synced Project {:?} while its durable lifecycle is `{}`",
            project.name(),
            project.lifecycle().label()
        )
    }
}

fn selected_projects<'a>(
    config: &'a Config,
    selector: Option<&str>,
) -> Result<Vec<&'a SyncedProject>> {
    match selector {
        Some(selector) => Ok(vec![config.resolve(selector)?]),
        None => Ok(config.projects().iter().collect()),
    }
}

fn ensure_supported_peer(node: &Node) -> Result<EndpointPlatform> {
    if !node.online {
        bail!("Tailscale machine {:?} is offline", node.display_name());
    }
    let platform = match &node.operating_system {
        OperatingSystem::Linux => EndpointPlatform::Linux,
        OperatingSystem::Macos => EndpointPlatform::Macos,
        OperatingSystem::Unsupported(label) => bail!(
            "Tailscale machine {:?} runs unsupported operating system {:?}",
            node.display_name(),
            label
        ),
        OperatingSystem::Unknown => {
            bail!("Tailscale machine {:?} has no operating system", node.display_name())
        }
    };
    if node.dns_name.is_empty() {
        bail!("Tailscale machine {:?} has no MagicDNS name", node.display_name());
    }
    Ok(platform)
}

fn canonical_local_root(path: &Path) -> Result<PathBuf> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("inspect local synchronization root {}", path.display()))?;
    if !metadata.is_dir() {
        bail!("local synchronization root {} is not a directory", path.display());
    }
    path.canonicalize()
        .with_context(|| format!("canonicalize local synchronization root {}", path.display()))
}

fn project_check(
    project: &SyncedProject,
    state: ProjectState,
    next_action: &mut Option<String>,
) -> Check {
    let (status, detail, action) = match state {
        ProjectState::Creating => (
            CheckStatus::ActionRequired,
            "project creation was interrupted".to_owned(),
            Some(format!("remove and recreate Synced Project {:?}", project.name())),
        ),
        ProjectState::Removing => (
            CheckStatus::ActionRequired,
            "project removal is incomplete".to_owned(),
            Some(format!("retry `kit sync remove {}`", project.name())),
        ),
        ProjectState::Missing => (
            CheckStatus::ActionRequired,
            "the configured Mutagen session is missing".to_owned(),
            Some(format!("remove and recreate Synced Project {:?}", project.name())),
        ),
        ProjectState::Duplicate => (
            CheckStatus::ActionRequired,
            "multiple Mutagen sessions claim this project".to_owned(),
            Some(format!("remove duplicate sessions for {:?}", project.name())),
        ),
        ProjectState::Incompatible => (
            CheckStatus::ActionRequired,
            "the Mutagen session was created by an unsupported version".to_owned(),
            Some(format!("remove and recreate Synced Project {:?}", project.name())),
        ),
        ProjectState::Stale => (
            CheckStatus::ActionRequired,
            "the Mutagen session does not match configured project intent".to_owned(),
            Some(format!("remove and recreate Synced Project {:?}", project.name())),
        ),
        ProjectState::NeedsPause => (
            CheckStatus::ActionRequired,
            "runtime is active but durable lifecycle is paused".to_owned(),
            Some(format!("retry `kit sync pause {}`", project.name())),
        ),
        ProjectState::NeedsResume => (
            CheckStatus::ActionRequired,
            "runtime is paused but durable lifecycle is active".to_owned(),
            Some(format!("retry `kit sync resume {}`", project.name())),
        ),
        ProjectState::Session(SessionHealth::Conflicted) => (
            CheckStatus::ActionRequired,
            "the Mutagen session has unresolved file conflicts".to_owned(),
            Some(format!(
                "inspect `kit sync status {}` and resolve the conflicted files",
                project.name()
            )),
        ),
        ProjectState::Session(SessionHealth::Offline) => (
            CheckStatus::ActionRequired,
            "the Mutagen session cannot reach one of its endpoints".to_owned(),
            Some(
                "restore endpoint connectivity; synchronization will catch up automatically"
                    .to_owned(),
            ),
        ),
        ProjectState::Session(SessionHealth::Error) => (
            CheckStatus::ActionRequired,
            "the Mutagen session reports an error".to_owned(),
            Some(format!("inspect `kit sync status {}`", project.name())),
        ),
        ProjectState::Session(health) => {
            (CheckStatus::Ready, format!("the Mutagen session is {}", health.label()), None)
        }
    };
    if let Some(action) = action {
        next_action.get_or_insert(action);
    }
    Check { status, detail }
}

pub(super) struct ProjectMonitor {
    project: SyncedProject,
    monitor: SessionMonitor,
}

impl ProjectMonitor {
    pub(super) async fn next(&mut self) -> Result<Option<ProjectReport>> {
        Ok(self
            .monitor
            .next()
            .await?
            .map(|sessions| ProjectReport::from_project(self.project.clone(), sessions)))
    }

    pub(super) async fn stop(self) -> Result<()> {
        self.monitor.stop().await?;
        Ok(())
    }
}

fn readiness_detail(readiness: Readiness) -> String {
    match readiness {
        Readiness::Ready(_) => "Tailscale is ready".to_owned(),
        Readiness::NeedsLogin => "Tailscale authentication is required".to_owned(),
        Readiness::CliUnavailable(detail)
        | Readiness::DaemonUnavailable(detail)
        | Readiness::PermissionDenied(detail)
        | Readiness::Unsupported(detail) => detail,
    }
}
