mod command;
mod model;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::framework::{process::ProcessSupervisor, AtomicFileTryLock, AtomicFileWriter};

use super::client::{
    connection_owner, probe_console_socket, remove_stale_console_socket, ConsoleClient,
    ConsoleSocketProbe,
};
use super::connection::ConnectionOwner;

pub(crate) use model::ConsoleRecovery;
pub use model::{
    ConsoleServicePlatform, ConsoleStage, ConsoleStatus, NativeServiceState, RemoteFailureKind,
};

#[cfg(target_os = "linux")]
use linux as native;
#[cfg(target_os = "macos")]
use macos as native;

const READY_TIMEOUT: Duration = Duration::from_secs(10);
const READY_INTERVAL: Duration = Duration::from_millis(100);

pub async fn status(processes: &ProcessSupervisor) -> Result<ConsoleStatus> {
    let owner = connection_owner()?;
    status_with_owner(processes, &owner).await
}

pub(crate) async fn status_with_owner(
    processes: &ProcessSupervisor,
    owner: &ConnectionOwner,
) -> Result<ConsoleStatus> {
    let platform = native::PLATFORM;
    match native::inspect(processes).await? {
        NativeServiceState::NotInstalled => inactive_status(owner, platform, true).await,
        NativeServiceState::Stopped => inactive_status(owner, platform, false).await,
        NativeServiceState::Failed { detail } => {
            Ok(ConsoleStatus::ServiceFailed { platform, detail })
        }
        NativeServiceState::Unavailable { detail } => {
            Ok(ConsoleStatus::ServiceUnavailable { platform, detail })
        }
        NativeServiceState::WrongOwner { path, expected_uid, actual_uid } => {
            Ok(ConsoleStatus::WrongOwner { platform, path, expected_uid, actual_uid })
        }
        NativeServiceState::Running => mux_status(owner, platform).await,
    }
}

async fn inactive_status(
    owner: &ConnectionOwner,
    platform: ConsoleServicePlatform,
    not_installed: bool,
) -> Result<ConsoleStatus> {
    match probe_console_socket()? {
        ConsoleSocketProbe::Missing { .. } if not_installed => {
            Ok(ConsoleStatus::NotInstalled { platform })
        }
        ConsoleSocketProbe::Missing { .. } => {
            Ok(ConsoleStatus::Stopped { platform })
        }
        ConsoleSocketProbe::Ready => match mux_status(owner, platform).await? {
            ConsoleStatus::Ready { sessions, .. } => Ok(ConsoleStatus::ServiceUnavailable {
                platform,
                detail: format!(
                    "a Console agent with {sessions} session(s) is running outside the native service"
                ),
            }),
            ConsoleStatus::MuxUnavailable { detail, .. } => Ok(ConsoleStatus::SocketStale {
                platform,
                path: super::client::console_socket_path()?,
                detail,
            }),
            incompatible_or_rejected => Ok(incompatible_or_rejected),
        },
        ConsoleSocketProbe::WrongOwner { path, expected_uid, actual_uid } => {
            Ok(ConsoleStatus::WrongOwner {
                platform,
                path,
                expected_uid,
                actual_uid,
            })
        }
        ConsoleSocketProbe::Rejected { path, detail } => Ok(ConsoleStatus::SocketRejected {
            platform,
            path,
            detail,
        }),
    }
}

async fn mux_status(
    owner: &ConnectionOwner,
    platform: ConsoleServicePlatform,
) -> Result<ConsoleStatus> {
    match probe_console_socket()? {
        ConsoleSocketProbe::Missing { path } => Ok(ConsoleStatus::SocketMissing { platform, path }),
        ConsoleSocketProbe::WrongOwner { path, expected_uid, actual_uid } => {
            Ok(ConsoleStatus::WrongOwner { platform, path, expected_uid, actual_uid })
        }
        ConsoleSocketProbe::Rejected { path, detail } => {
            Ok(ConsoleStatus::SocketRejected { platform, path, detail })
        }
        ConsoleSocketProbe::Ready => match ConsoleClient::connect(owner).await {
            Ok(client) => {
                let sessions = client.snapshot().await?.sessions.len();
                let build = client.server_build_identity()?;
                Ok(ConsoleStatus::Ready { platform, sessions, build })
            }
            Err(error) => {
                if let Some(incompatible) =
                    error.downcast_ref::<wezterm_client::client::IncompatibleVersionError>()
                {
                    return Ok(ConsoleStatus::CodecIncompatible {
                        platform,
                        server_version: incompatible.version.clone(),
                        server_codec: incompatible.codec_vers,
                    });
                }
                if error.downcast_ref::<wezterm_client::client::AttachmentRejectedError>().is_some()
                {
                    return Ok(ConsoleStatus::SocketRejected {
                        platform,
                        path: super::client::console_socket_path()?,
                        detail: "the mux rejected this attachment".to_owned(),
                    });
                }
                Ok(ConsoleStatus::MuxUnavailable { platform, detail: format!("{error:#}") })
            }
        },
    }
}

pub async fn setup(processes: &ProcessSupervisor) -> Result<ConsoleStatus> {
    let owner = connection_owner()?;
    setup_with_owner(processes, &owner).await
}

pub(crate) async fn setup_with_owner(
    processes: &ProcessSupervisor,
    owner: &ConnectionOwner,
) -> Result<ConsoleStatus> {
    let runtime_dir = super::runtime::directory()?;
    super::runtime::prepare(&runtime_dir)?;
    let operation_lock =
        AtomicFileWriter::new(&runtime_dir, ".service-operation.lock", ".service-operation");
    let _operation = match operation_lock.try_lock().context("serialize Console service repair")? {
        AtomicFileTryLock::Acquired(operation) => operation,
        AtomicFileTryLock::Busy => {
            return Ok(ConsoleStatus::RepairBusy { platform: native::PLATFORM });
        }
    };

    let executable = installed_executable()?;
    let state = native::inspect(processes).await?;
    let definition_matches = native::definition_matches(processes, &executable).await?;
    let mut drain_client = None;

    if matches!(probe_console_socket()?, ConsoleSocketProbe::Ready) {
        let current = mux_status(owner, native::PLATFORM).await?;
        if matches!(&state, NativeServiceState::Running) && definition_matches && current.ready() {
            return Ok(current);
        }
        let client = ConsoleClient::connect_for_service_management(owner)
            .await
            .context("attach to the live Console agent before repairing its service")?;
        client.begin_service_drain().await?;
        let sessions = client.snapshot().await?.sessions;
        if !sessions.is_empty() {
            client.cancel_service_drain().await?;
            return Ok(ConsoleStatus::ActivationDeferred {
                platform: native::PLATFORM,
                sessions: sessions.len(),
            });
        }
        drain_client = Some(client);
    }

    if matches!(&state, NativeServiceState::NotInstalled | NativeServiceState::Stopped) {
        let inactive = inactive_status(
            owner,
            native::PLATFORM,
            matches!(&state, NativeServiceState::NotInstalled),
        )
        .await?;
        match inactive {
            ConsoleStatus::NotInstalled { .. }
            | ConsoleStatus::Stopped { .. }
            | ConsoleStatus::SocketStale { .. } => remove_stale_console_socket()?,
            blocked => {
                bail!("refusing to replace an unmanaged Console agent: {}", blocked.text())
            }
        }
    }

    if definition_matches && matches!(&state, NativeServiceState::Stopped) {
        native::start(processes).await?;
    } else {
        native::install_and_start(processes, &executable).await?;
    }
    drop(drain_client);
    wait_until_ready(processes, owner).await
}

pub async fn stop(processes: &ProcessSupervisor, force: bool) -> Result<ConsoleStatus> {
    let owner = connection_owner()?;
    stop_with_owner(processes, &owner, force).await
}

pub async fn restart(processes: &ProcessSupervisor, force: bool) -> Result<ConsoleStatus> {
    let owner = connection_owner()?;
    restart_with_owner(processes, &owner, force).await
}

pub(crate) async fn restart_with_owner(
    processes: &ProcessSupervisor,
    owner: &ConnectionOwner,
    force: bool,
) -> Result<ConsoleStatus> {
    stop_with_owner(processes, owner, force).await?;
    setup_with_owner(processes, owner).await
}

pub(crate) async fn stop_with_owner(
    processes: &ProcessSupervisor,
    owner: &ConnectionOwner,
    force: bool,
) -> Result<ConsoleStatus> {
    match native::inspect(processes).await? {
        NativeServiceState::NotInstalled | NativeServiceState::Stopped => {
            return status_with_owner(processes, owner).await
        }
        NativeServiceState::WrongOwner { path, expected_uid, actual_uid } => bail!(
            "refusing to stop foreign Console service definition {} owned by uid {}; expected uid {}",
            path.display(),
            actual_uid,
            expected_uid
        ),
        NativeServiceState::Unavailable { detail } => bail!("Console service unavailable: {detail}"),
        NativeServiceState::Failed { .. } | NativeServiceState::Running => {}
    }

    let client = ConsoleClient::connect_for_service_management(owner)
        .await
        .context("attach to the Console agent before stopping it")?;
    client.begin_service_drain().await?;
    let sessions = client.snapshot().await?.sessions;
    if !sessions.is_empty() && !force {
        let names = sessions
            .iter()
            .map(|session| format!("{} (pane {})", session.title, session.pane_id))
            .collect::<Vec<_>>()
            .join(", ");
        client.cancel_service_drain().await?;
        bail!(
            "Console still owns {} session(s): {names}; rerun with --force to close them",
            sessions.len()
        );
    }
    if force {
        if let Err(error) = close_all_sessions(&client, sessions).await {
            if let Err(cancel_error) = client.cancel_service_drain().await {
                return Err(error.context(format!(
                    "also failed to cancel the Console service drain: {cancel_error:#}"
                )));
            }
            return Err(error);
        }
    }
    native::stop(processes).await?;
    drop(client);
    remove_stale_console_socket()?;
    status_with_owner(processes, owner).await
}

async fn close_all_sessions(
    client: &ConsoleClient,
    sessions: Vec<super::client::SessionView>,
) -> Result<()> {
    for session in sessions {
        client.close_pane(session.pane_id).await.with_context(|| {
            format!("closing Console pane {} ({})", session.pane_id, session.title)
        })?;
    }

    let started = tokio::time::Instant::now();
    loop {
        let remaining = client.snapshot().await?.sessions;
        if remaining.is_empty() {
            return Ok(());
        }
        if started.elapsed() >= READY_TIMEOUT {
            let names = remaining
                .iter()
                .map(|session| format!("{} (pane {})", session.title, session.pane_id))
                .collect::<Vec<_>>()
                .join(", ");
            bail!("Console panes did not close within the service deadline: {names}");
        }
        tokio::time::sleep(READY_INTERVAL).await;
    }
}

fn installed_executable() -> Result<std::path::PathBuf> {
    let executable = std::env::current_exe().context("resolving the installed Kit executable")?;
    executable
        .canonicalize()
        .with_context(|| format!("resolving Kit executable {}", executable.display()))
}

async fn wait_until_ready(
    processes: &ProcessSupervisor,
    owner: &ConnectionOwner,
) -> Result<ConsoleStatus> {
    let started = tokio::time::Instant::now();
    loop {
        let current = status_with_owner(processes, owner).await?;
        if current.ready() {
            return Ok(current);
        }
        if started.elapsed() >= READY_TIMEOUT {
            bail!("Console service did not become ready: {}", current.text());
        }
        tokio::time::sleep(READY_INTERVAL).await;
    }
}
