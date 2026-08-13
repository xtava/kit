use std::{
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::{bail, Context, Result};
use thiserror::Error;
use tokio::task::JoinHandle;

use super::process::{
    leader_exit, tokio_command, CommandSpec, EnvironmentBase, LeaderExit, ProcessEnvironment,
    ProcessLabel, ProcessRunId, ProcessStartError, ProcessSupervisor,
};

/// An operating-system file action Kit can hand off without owning the launched application.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalFileAction {
    Open,
    Reveal,
    Preview,
}

impl fmt::Display for ExternalFileAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Open => "open",
            Self::Reveal => "reveal",
            Self::Preview => "preview",
        })
    }
}

/// File handoffs supported by the current operating system integration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExternalFileCapabilities {
    open: bool,
    reveal: bool,
    preview: bool,
}

impl ExternalFileCapabilities {
    pub const fn supports(self, action: ExternalFileAction) -> bool {
        match action {
            ExternalFileAction::Open => self.open,
            ExternalFileAction::Reveal => self.reveal,
            ExternalFileAction::Preview => self.preview,
        }
    }

    fn for_platform(platform: &str) -> Self {
        match platform {
            "linux" | "windows" => Self { open: true, reveal: true, preview: false },
            "macos" => Self { open: true, reveal: true, preview: true },
            _ => Self { open: false, reveal: false, preview: false },
        }
    }
}

/// Returns the file handoffs that can be planned on this host.
pub fn external_file_capabilities() -> ExternalFileCapabilities {
    ExternalFileCapabilities::for_platform(std::env::consts::OS)
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ExternalFileActionError {
    #[error("{action} file handoff is unsupported on {platform}")]
    Unsupported { action: ExternalFileAction, platform: String },
}

impl ProcessSupervisor {
    /// Starts the platform launcher without exposing an uncontained process API to tools.
    fn start_external_handoff(
        &self,
        program: OsString,
        command: CommandSpec,
    ) -> Result<ExternalOpenReceipt, ProcessStartError> {
        if !command.working_directory.is_dir() {
            return Err(ProcessStartError::WorkingDirectoryUnavailable);
        }

        let run_id = ProcessRunId::new();
        let mut command = tokio_command(&command);
        command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let mut child = command
            .spawn()
            .map_err(|source| ProcessStartError::SpawnFailed { message: source.to_string() })?;
        let reaper = tokio::spawn(async move { child.wait().await.map(leader_exit) });

        Ok(ExternalOpenReceipt { program, run_id, reaper })
    }
}

/// A caller-owned launcher command that hands an application to the operating system.
pub struct ExternalCommand {
    pub program: OsString,
    pub arguments: Vec<OsString>,
    pub working_directory: PathBuf,
}

/// An operating-system-owned destination or application launcher.
pub enum ExternalTarget {
    Url(String),
    Path(PathBuf),
    File { action: ExternalFileAction, path: PathBuf },
    Command(ExternalCommand),
}

#[derive(Debug, Eq, PartialEq)]
struct OpenCommand {
    program: OsString,
    args: Vec<OsString>,
}

/// Completion observation for an external launcher.
///
/// Dropping this receipt does not terminate the launcher or the application it opened.
pub struct ExternalOpenReceipt {
    program: OsString,
    run_id: ProcessRunId,
    reaper: JoinHandle<std::io::Result<LeaderExit>>,
}

impl ExternalOpenReceipt {
    /// Waits for and classifies the launcher leader's exit.
    pub async fn completion(self) -> Result<()> {
        let exit = self
            .reaper
            .await
            .with_context(|| format!("external opener owner stopped for {}", self.run_id))?
            .with_context(|| format!("reap external opener {}", self.run_id))?;
        match exit {
            LeaderExit::Code(0) => Ok(()),
            exit => {
                bail!("external opener {} exited with {exit:?}", self.program.to_string_lossy())
            }
        }
    }
}

/// Starts an OS handoff and returns immediately after the launcher is created.
pub fn start_external(
    processes: &ProcessSupervisor,
    target: ExternalTarget,
) -> Result<ExternalOpenReceipt> {
    let (command, working_directory) = match target {
        ExternalTarget::Url(url) => (
            command_for(std::env::consts::OS, OsString::from(url))?,
            std::env::current_dir().context("resolve working directory")?,
        ),
        ExternalTarget::Path(path) => (
            command_for(std::env::consts::OS, path.into_os_string())?,
            std::env::current_dir().context("resolve working directory")?,
        ),
        ExternalTarget::File { action, path } => (
            file_command_for(std::env::consts::OS, action, &path)?,
            std::env::current_dir().context("resolve working directory")?,
        ),
        ExternalTarget::Command(command) => (
            OpenCommand { program: command.program, args: command.arguments },
            command.working_directory,
        ),
    };
    start_open_command(processes, command, working_directory)
}

fn start_open_command(
    processes: &ProcessSupervisor,
    command: OpenCommand,
    working_directory: PathBuf,
) -> Result<ExternalOpenReceipt> {
    let program_label = command.program.to_string_lossy();
    let label = ProcessLabel::new(format!("open external target with {program_label}"))
        .context("construct external-open process label")?;
    let environment =
        ProcessEnvironment::new(EnvironmentBase::Inherit, Default::default(), Default::default())
            .context("construct external-open environment")?;
    let command_spec = CommandSpec::new(
        command.program.clone(),
        command.args,
        working_directory,
        environment,
        label,
    )
    .context("construct external-open command")?;
    let receipt = processes
        .start_external_handoff(command.program.clone(), command_spec)
        .with_context(|| format!("start {program_label}"))?;
    Ok(receipt)
}

fn command_for(platform: &str, target: OsString) -> Result<OpenCommand> {
    match platform {
        "linux" => Ok(OpenCommand { program: OsString::from("xdg-open"), args: vec![target] }),
        "macos" => Ok(OpenCommand { program: OsString::from("open"), args: vec![target] }),
        "windows" => Ok(OpenCommand {
            program: OsString::from("rundll32.exe"),
            args: vec![OsString::from("url.dll,FileProtocolHandler"), target],
        }),
        other => anyhow::bail!("opening external targets is unsupported on {other}"),
    }
}

/// Purely maps a typed file intent to the platform launcher command. Process creation remains in
/// [`start_open_command`], so callers cannot accidentally bypass Kit's process ownership.
fn file_command_for(
    platform: &str,
    action: ExternalFileAction,
    target: &Path,
) -> std::result::Result<OpenCommand, ExternalFileActionError> {
    let reveal_directory = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .as_os_str()
        .to_owned();
    let target = target.as_os_str().to_owned();
    let command = match (platform, action) {
        ("linux", ExternalFileAction::Open) => {
            OpenCommand { program: OsString::from("xdg-open"), args: vec![target] }
        }
        ("linux", ExternalFileAction::Reveal) => {
            OpenCommand { program: OsString::from("xdg-open"), args: vec![reveal_directory] }
        }
        ("macos", ExternalFileAction::Open) => {
            OpenCommand { program: OsString::from("open"), args: vec![target] }
        }
        ("macos", ExternalFileAction::Reveal) => OpenCommand {
            program: OsString::from("open"),
            args: vec![OsString::from("-R"), target],
        },
        ("macos", ExternalFileAction::Preview) => OpenCommand {
            program: OsString::from("qlmanage"),
            args: vec![OsString::from("-p"), target],
        },
        ("windows", ExternalFileAction::Open) => OpenCommand {
            program: OsString::from("rundll32.exe"),
            args: vec![OsString::from("url.dll,FileProtocolHandler"), target],
        },
        ("windows", ExternalFileAction::Reveal) => {
            let mut selection = OsString::from("/select,");
            selection.push(target);
            OpenCommand { program: OsString::from("explorer.exe"), args: vec![selection] }
        }
        _ => {
            return Err(ExternalFileActionError::Unsupported {
                action,
                platform: platform.to_owned(),
            })
        }
    };
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs_linux_command() {
        let command = command_for("linux", OsString::from("https://example.com/a b")).unwrap();
        assert_eq!(command.program, "xdg-open");
        assert_eq!(command.args, [OsString::from("https://example.com/a b")]);
    }

    #[test]
    fn constructs_macos_command() {
        let command = command_for("macos", OsString::from("https://example.com")).unwrap();
        assert_eq!(command.program, "open");
        assert_eq!(command.args, [OsString::from("https://example.com")]);
    }

    #[test]
    fn constructs_windows_command_without_a_command_string() {
        let target = OsString::from("https://example.com/a b");
        let command = command_for("windows", target.clone()).unwrap();
        assert_eq!(command.program, "rundll32.exe");
        assert_eq!(command.args, [OsString::from("url.dll,FileProtocolHandler"), target]);
    }

    #[test]
    fn rejects_unsupported_platforms() {
        let error = command_for("freebsd", OsString::from("https://example.com")).unwrap_err();
        assert_eq!(error.to_string(), "opening external targets is unsupported on freebsd");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn supervised_opener_reaps_repeated_processes() {
        let state_root = std::env::temp_dir().join(format!(
            "kit-external-processes-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let working_directory = std::env::current_dir().unwrap();
        let processes = ProcessSupervisor::for_test(state_root.clone()).unwrap();

        for _ in 0..8 {
            let receipt = start_open_command(
                &processes,
                OpenCommand {
                    program: OsString::from("/bin/sh"),
                    args: vec![OsString::from("-c"), OsString::from("exit 0")],
                },
                working_directory.clone(),
            )
            .unwrap();
            receipt.completion().await.unwrap();
        }

        assert_eq!(std::fs::read_dir(&state_root).unwrap().count(), 0);

        std::fs::remove_dir_all(state_root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reports_nonzero_launcher_exit() {
        let state_root = std::env::temp_dir().join(format!(
            "kit-external-nonzero-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let processes = ProcessSupervisor::for_test(state_root.clone()).unwrap();
        let receipt = start_open_command(
            &processes,
            OpenCommand {
                program: OsString::from("/bin/sh"),
                args: vec![OsString::from("-c"), OsString::from("exit 7")],
            },
            std::env::current_dir().unwrap(),
        )
        .unwrap();

        let error = receipt.completion().await.unwrap_err();
        assert!(error.to_string().contains("Code(7)"));

        std::fs::remove_dir_all(state_root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn receipt_drop_and_launcher_exit_do_not_terminate_handoff_descendants() {
        let state_root = std::env::temp_dir().join(format!(
            "kit-external-handoff-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let marker = state_root.join("descendant-completed");
        let working_directory = std::env::current_dir().unwrap();
        let processes = ProcessSupervisor::for_test(state_root.clone()).unwrap();
        let receipt = start_open_command(
            &processes,
            OpenCommand {
                program: OsString::from("/bin/sh"),
                args: vec![
                    OsString::from("-c"),
                    OsString::from("(sleep 0.05; printf complete > \"$1\") &"),
                    OsString::from("kit-external-handoff"),
                    marker.as_os_str().to_owned(),
                ],
            },
            working_directory,
        )
        .unwrap();
        drop(receipt);

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !marker.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "complete");

        std::fs::remove_dir_all(state_root).unwrap();
    }
}
