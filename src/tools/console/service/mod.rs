mod command;
mod model;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::framework::process::ProcessSupervisor;

use super::client::{
    local_connection_owner, probe_console_socket, remove_stale_console_socket, ConsoleClient,
    ConsoleSocketProbe,
};
use super::connection::ConnectionOwner;

pub use model::{ConsoleServicePlatform, ConsoleStatus, NativeServiceState, RemoteFailureKind};

#[cfg(target_os = "linux")]
use linux as native;
#[cfg(target_os = "macos")]
use macos as native;

const READY_TIMEOUT: Duration = Duration::from_secs(10);
const READY_INTERVAL: Duration = Duration::from_millis(100);

pub async fn status(processes: &ProcessSupervisor) -> Result<ConsoleStatus> {
    let owner = local_connection_owner()?;
    status_with_owner(processes, &owner).await
}

async fn status_with_owner(
    processes: &ProcessSupervisor,
    owner: &ConnectionOwner,
) -> Result<ConsoleStatus> {
    let platform = native::PLATFORM;
    match native::inspect(processes).await? {
        NativeServiceState::NotInstalled => inactive_status(owner, platform, true).await,
        NativeServiceState::Stopped => inactive_status(owner, platform, false).await,
        NativeServiceState::Failed { detail } => Ok(ConsoleStatus::ServiceFailed {
            platform,
            detail,
            action: "kit console setup".to_owned(),
        }),
        NativeServiceState::Unavailable { detail } => Ok(ConsoleStatus::ServiceUnavailable {
            platform,
            detail,
            action: "restore the logged-in user service manager, then run kit console status"
                .to_owned(),
        }),
        NativeServiceState::WrongOwner { path, expected_uid, actual_uid } => {
            Ok(ConsoleStatus::WrongOwner {
                platform,
                path,
                expected_uid,
                actual_uid,
                action: "remove the foreign service definition, then run kit console setup"
                    .to_owned(),
            })
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
            Ok(ConsoleStatus::NotInstalled { platform, action: "kit console setup".to_owned() })
        }
        ConsoleSocketProbe::Missing { .. } => {
            Ok(ConsoleStatus::Stopped { platform, action: "kit console setup".to_owned() })
        }
        ConsoleSocketProbe::Ready => match mux_status(owner, platform).await? {
            ConsoleStatus::Ready { sessions, .. } => Ok(ConsoleStatus::ServiceUnavailable {
                platform,
                detail: format!(
                    "a Console agent with {sessions} session(s) is running outside the native service"
                ),
                action: "close or stop that agent before running kit console setup".to_owned(),
            }),
            ConsoleStatus::MuxUnavailable { detail, .. } => Ok(ConsoleStatus::SocketStale {
                platform,
                path: super::client::console_socket_path()?,
                detail,
                action: "kit console setup".to_owned(),
            }),
            incompatible_or_rejected => Ok(incompatible_or_rejected),
        },
        ConsoleSocketProbe::WrongOwner { path, expected_uid, actual_uid } => {
            Ok(ConsoleStatus::WrongOwner {
                platform,
                path,
                expected_uid,
                actual_uid,
                action: "remove the foreign path, then run kit console setup".to_owned(),
            })
        }
        ConsoleSocketProbe::Rejected { path, detail } => Ok(ConsoleStatus::SocketRejected {
            platform,
            path,
            detail,
            action: "inspect and remove only the rejected path, then run kit console setup"
                .to_owned(),
        }),
    }
}

async fn mux_status(
    owner: &ConnectionOwner,
    platform: ConsoleServicePlatform,
) -> Result<ConsoleStatus> {
    match probe_console_socket()? {
        ConsoleSocketProbe::Missing { path } => Ok(ConsoleStatus::SocketMissing {
            platform,
            path,
            action: "inspect the Console service log, then run kit console setup".to_owned(),
        }),
        ConsoleSocketProbe::WrongOwner { path, expected_uid, actual_uid } => {
            Ok(ConsoleStatus::WrongOwner {
                platform,
                path,
                expected_uid,
                actual_uid,
                action: "stop the foreign owner and run kit console setup".to_owned(),
            })
        }
        ConsoleSocketProbe::Rejected { path, detail } => Ok(ConsoleStatus::SocketRejected {
            platform,
            path,
            detail,
            action: "stop Console, remove only the rejected owned socket, and run setup".to_owned(),
        }),
        ConsoleSocketProbe::Ready => match ConsoleClient::connect(owner).await {
            Ok(client) => {
                let sessions = client.snapshot(None).await?.sessions.len();
                Ok(ConsoleStatus::Ready { platform, sessions, build: super::build_identity()? })
            }
            Err(error) => {
                if let Some(incompatible) =
                    error.downcast_ref::<wezterm_client::client::IncompatibleVersionError>()
                {
                    return Ok(ConsoleStatus::CodecIncompatible {
                        platform,
                        server_version: incompatible.version.clone(),
                        server_codec: incompatible.codec_vers,
                        action: "kit console setup".to_owned(),
                    });
                }
                if let Some(incompatible) =
                    error.downcast_ref::<wezterm_client::client::BuildIdentityMismatch>()
                {
                    return Ok(ConsoleStatus::BuildIncompatible {
                        platform,
                        sessions: None,
                        expected: incompatible.expected.clone(),
                        actual: incompatible.actual.clone(),
                        action: "close active sessions, then run kit console setup".to_owned(),
                    });
                }
                if error.downcast_ref::<wezterm_client::client::AttachmentRejectedError>().is_some()
                {
                    return Ok(ConsoleStatus::SocketRejected {
                        platform,
                        path: super::client::console_socket_path()?,
                        detail: "the mux rejected this attachment".to_owned(),
                        action: "close another Console client and retry".to_owned(),
                    });
                }
                Ok(ConsoleStatus::MuxUnavailable {
                    platform,
                    detail: format!("{error:#}"),
                    action: "inspect the Console service log, then run kit console setup"
                        .to_owned(),
                })
            }
        },
    }
}

pub async fn setup(processes: &ProcessSupervisor) -> Result<ConsoleStatus> {
    let owner = local_connection_owner()?;
    let executable = installed_executable()?;
    let state = native::inspect(processes).await?;
    let definition_matches = native::definition_matches(processes, &executable).await?;
    let mut drain_client = None;

    if matches!(&state, NativeServiceState::Running) {
        let current = mux_status(&owner, native::PLATFORM).await?;
        if definition_matches && current.ready() {
            return Ok(current);
        }
        let client = ConsoleClient::connect_for_service_management(&owner)
            .await
            .context("attach to the running Console agent before replacing its service")?;
        client.begin_service_drain().await?;
        let sessions = client.snapshot(None).await?.sessions;
        if !sessions.is_empty() {
            let names = sessions
                .iter()
                .map(|session| format!("{} (pane {})", session.title, session.pane_id))
                .collect::<Vec<_>>()
                .join(", ");
            client.cancel_service_drain().await?;
            bail!(
                "Console still owns {} session(s): {names}; close them before replacing its native service",
                sessions.len()
            );
        }
        drain_client = Some(client);
    }

    if matches!(&state, NativeServiceState::NotInstalled | NativeServiceState::Stopped) {
        let inactive = inactive_status(
            &owner,
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
    wait_until_ready(processes, &owner).await
}

pub async fn stop(processes: &ProcessSupervisor, force: bool) -> Result<ConsoleStatus> {
    let owner = local_connection_owner()?;
    match native::inspect(processes).await? {
        NativeServiceState::NotInstalled | NativeServiceState::Stopped => {
            return status_with_owner(processes, &owner).await
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

    let client = ConsoleClient::connect_for_service_management(&owner)
        .await
        .context("attach to the Console agent before stopping it")?;
    client.begin_service_drain().await?;
    let sessions = client.snapshot(None).await?.sessions;
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
    status_with_owner(processes, &owner).await
}

async fn close_all_sessions(
    client: &ConsoleClient,
    sessions: Vec<super::client::SessionView>,
) -> Result<()> {
    for session in sessions {
        client.take_control(session.id).await.with_context(|| {
            format!("taking control of Console pane {} ({})", session.pane_id, session.title)
        })?;
        client.close_pane(session.pane_id).await.with_context(|| {
            format!("closing Console pane {} ({})", session.pane_id, session.title)
        })?;
    }

    let started = tokio::time::Instant::now();
    loop {
        let remaining = client.snapshot(None).await?.sessions;
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
