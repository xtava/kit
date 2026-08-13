use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use directories::BaseDirs;

use crate::framework::process::ProcessSupervisor;

use super::command::{os_args, os_args_owned, CommandRunner};

const SERVICE_LABEL: &str = "dev.kit.stream.sunshine";

#[derive(Clone)]
pub(super) struct SunshineController {
    runner: CommandRunner,
}

#[derive(Clone, Debug)]
pub(super) struct SunshinePlan {
    pub started_by_kit: bool,
    pub previous_output_name: Option<String>,
    pub output_name_changed: bool,
    selected_output_name: String,
}

impl SunshineController {
    pub(super) fn new(processes: ProcessSupervisor, working_directory: PathBuf) -> Self {
        Self { runner: CommandRunner::new(processes, working_directory) }
    }

    pub(super) async fn running(&self) -> Result<bool> {
        let report = self
            .runner
            .capture("/usr/bin/pgrep", os_args(["-x", "sunshine"]), "inspect Sunshine process")
            .await?;
        Ok(report.succeeded())
    }

    pub(super) async fn plan(&self, display_id: u32) -> Result<SunshinePlan> {
        let config = sunshine_config_path()?;
        let previous_output_name = read_output_name(&config)?;
        let selected = display_id.to_string();
        let output_name_changed = previous_output_name.as_deref() != Some(selected.as_str());
        let running = self.running().await?;
        if running && output_name_changed {
            bail!(
                "Sunshine is already running for output_name={}; stop it or select display {} before using Stream Slot",
                previous_output_name.as_deref().unwrap_or("<automatic>"),
                selected
            );
        }
        Ok(SunshinePlan {
            started_by_kit: !running,
            previous_output_name,
            output_name_changed,
            selected_output_name: selected,
        })
    }

    pub(super) async fn apply(&self, plan: &SunshinePlan) -> Result<()> {
        if !plan.started_by_kit {
            return Ok(());
        }
        if self.running().await? {
            bail!("Sunshine started outside Kit while Stream Slot was preparing; try again");
        }
        let config = sunshine_config_path()?;
        if plan.output_name_changed {
            write_output_name(
                &config,
                plan.previous_output_name.as_deref(),
                &plan.selected_output_name,
            )?;
        }
        if self.running().await? {
            bail!("Sunshine started outside Kit while Stream Slot was preparing; try again");
        }
        if self.owned_service_loaded().await? {
            self.stop_owned().await?;
        }
        let sunshine = sunshine_executable()?;
        let log_directory = stream_state_directory()?;
        std::fs::create_dir_all(&log_directory).with_context(|| {
            format!("create Stream state directory {}", log_directory.display())
        })?;
        let stdout = log_directory.join("sunshine.log");
        let stderr = log_directory.join("sunshine-error.log");
        let arguments = vec![
            OsString::from("submit"),
            OsString::from("-l"),
            OsString::from(SERVICE_LABEL),
            OsString::from("-p"),
            sunshine.into_os_string(),
            OsString::from("-o"),
            stdout.into_os_string(),
            OsString::from("-e"),
            stderr.into_os_string(),
            OsString::from("--"),
            OsString::from("sunshine"),
            config.into_os_string(),
        ];
        let report =
            self.runner.capture("/bin/launchctl", arguments, "start Kit-owned Sunshine").await?;
        if report.succeeded() {
            Ok(())
        } else {
            bail!("start Sunshine failed: {}", report.detail())
        }
    }

    pub(super) async fn stop_owned(&self) -> Result<()> {
        let report = self
            .runner
            .capture(
                "/bin/launchctl",
                os_args(["remove", SERVICE_LABEL]),
                "stop Kit-owned Sunshine",
            )
            .await?;
        if report.succeeded() || report.detail().contains("Could not find service") {
            Ok(())
        } else {
            bail!("stop Kit-owned Sunshine failed: {}", report.detail())
        }
    }

    pub(super) async fn owned_service_loaded(&self) -> Result<bool> {
        let domain = format!("gui/{}/{}", rustix::process::getuid().as_raw(), SERVICE_LABEL);
        let report = self
            .runner
            .capture(
                "/bin/launchctl",
                os_args_owned([OsString::from("print"), OsString::from(domain)]),
                "inspect Kit-owned Sunshine",
            )
            .await?;
        Ok(report.succeeded())
    }
}

pub(super) fn restore_output_name(previous: Option<&str>, selected: &str) -> Result<()> {
    let config = sunshine_config_path()?;
    update_output_name(&config, Some(selected), previous, true)
}

fn sunshine_executable() -> Result<PathBuf> {
    ["/opt/homebrew/bin/sunshine", "/usr/local/bin/sunshine"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .context("Sunshine is not installed in /opt/homebrew/bin or /usr/local/bin")
}

fn sunshine_config_path() -> Result<PathBuf> {
    Ok(BaseDirs::new()
        .context("resolve home directory")?
        .home_dir()
        .join(".config/sunshine/sunshine.conf"))
}

fn stream_state_directory() -> Result<PathBuf> {
    Ok(BaseDirs::new()
        .context("resolve home directory")?
        .home_dir()
        .join("Library/Application Support/kit/stream"))
}

fn read_output_name(path: &Path) -> Result<Option<String>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    Ok(raw.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim() == "output_name").then(|| value.trim().to_owned())
    }))
}

fn write_output_name(path: &Path, previous: Option<&str>, display_id: &str) -> Result<()> {
    update_output_name(path, previous, Some(display_id), false)
}

fn replace_output_name(raw: &str, value: Option<&str>) -> String {
    let mut found = false;
    let mut lines = raw
        .lines()
        .filter_map(|line| {
            let is_output =
                line.split_once('=').is_some_and(|(key, _)| key.trim() == "output_name");
            if !is_output {
                return Some(line.to_owned());
            }
            found = true;
            value.map(|value| format!("output_name = {value}"))
        })
        .collect::<Vec<_>>();
    if !found {
        if let Some(value) = value {
            lines.push(format!("output_name = {value}"));
        }
    }
    let mut updated = lines.join("\n");
    if !updated.is_empty() {
        updated.push('\n');
    }
    updated
}

fn update_output_name(
    path: &Path,
    expected: Option<&str>,
    value: Option<&str>,
    preserve_external_change: bool,
) -> Result<()> {
    let directory = path.parent().context("Sunshine config has no parent directory")?;
    let writer = crate::framework::AtomicFileWriter::new(
        directory,
        ".kit-stream-sunshine.lock",
        ".kit-stream-sunshine",
    );
    let _lock = writer.lock()?;
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let current = raw.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim() == "output_name").then(|| value.trim())
    });
    if current != expected {
        if preserve_external_change {
            return Ok(());
        }
        bail!(
            "Sunshine output_name changed while Stream Slot was preparing (expected {}, found {})",
            expected.unwrap_or("<automatic>"),
            current.unwrap_or("<automatic>")
        );
    }
    let updated = replace_output_name(&raw, value);
    writer.replace(path, updated.as_bytes())?;
    Ok(())
}
