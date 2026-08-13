use std::{path::PathBuf, time::Duration};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tokio::time::sleep;

use crate::framework::process::ProcessSupervisor;

use super::{
    command::{os_args, CommandRunner},
    model::WindowFrame,
    window,
};

pub(super) const DISPLAY_NAME: &str = "TV";
const BETTERDISPLAY_APP: &str = "/Applications/BetterDisplay.app";
const CONNECT_ATTEMPTS: usize = 20;

#[derive(Clone)]
pub(super) struct DisplayController {
    runner: CommandRunner,
}

#[derive(Clone, Debug)]
pub(super) struct DisplayStatus {
    pub exists: bool,
    pub display_id: Option<u32>,
    pub frame: Option<WindowFrame>,
}

#[derive(Clone, Debug)]
pub(super) struct ConnectedDisplay {
    pub display_id: u32,
    pub frame: WindowFrame,
    pub connected_by_kit: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DisplayIdentifier {
    device_type: String,
    name: String,
    #[serde(default)]
    #[serde(rename = "displayID")]
    display_id: Option<String>,
}

impl DisplayController {
    pub(super) fn new(processes: ProcessSupervisor, working_directory: PathBuf) -> Self {
        Self { runner: CommandRunner::new(processes, working_directory) }
    }

    pub(super) async fn inspect(&self) -> Result<DisplayStatus> {
        let displays = self.identifiers().await?;
        let display = displays
            .iter()
            .find(|display| display.device_type == "VirtualScreen" && display.name == DISPLAY_NAME);
        let display_id = display
            .and_then(|display| display.display_id.as_deref())
            .map(str::parse)
            .transpose()
            .context("parse BetterDisplay TV display ID")?;
        let frame = match display_id {
            Some(display_id) => window::online_display_bounds(display_id)?,
            None => None,
        };
        Ok(DisplayStatus { exists: display.is_some(), display_id, frame })
    }

    pub(super) async fn connect(&self) -> Result<ConnectedDisplay> {
        let before = self.inspect().await?;
        if let (Some(display_id), Some(frame)) = (before.display_id, before.frame) {
            return Ok(ConnectedDisplay { display_id, frame, connected_by_kit: false });
        }
        if !std::path::Path::new(BETTERDISPLAY_APP).is_dir() {
            bail!("BetterDisplay is not installed at {BETTERDISPLAY_APP}");
        }
        let executable = betterdisplay_executable();
        if !before.exists {
            let create = self
                .runner
                .capture(
                    executable.clone(),
                    os_args([
                        "create",
                        "--type=VirtualScreen",
                        "--virtualScreenName=TV",
                        "--aspectWidth=16",
                        "--aspectHeight=9",
                        "--resolution=1920x1080",
                    ]),
                    "create Stream virtual display",
                )
                .await?;
            require_success("create the TV virtual display", &create)?;
        } else {
            let connect = self
                .runner
                .capture(
                    executable,
                    os_args(["set", "--name=TV", "--connected=on"]),
                    "connect Stream virtual display",
                )
                .await?;
            require_success("connect the TV virtual display", &connect)?;
        }
        for _ in 0..CONNECT_ATTEMPTS {
            let current = self.inspect().await?;
            if let (Some(display_id), Some(frame)) = (current.display_id, current.frame) {
                return Ok(ConnectedDisplay { display_id, frame, connected_by_kit: true });
            }
            sleep(Duration::from_millis(250)).await;
        }
        bail!("the TV virtual display did not become available to macOS")
    }

    pub(super) async fn disconnect(&self) -> Result<()> {
        let report = self
            .runner
            .capture(
                betterdisplay_executable(),
                os_args(["set", "--name=TV", "--connected=off"]),
                "disconnect Stream virtual display",
            )
            .await?;
        require_success("disconnect the TV virtual display", &report)
    }

    async fn identifiers(&self) -> Result<Vec<DisplayIdentifier>> {
        let report = self
            .runner
            .capture(
                betterdisplay_executable(),
                os_args(["get", "--identifiers"]),
                "inspect BetterDisplay identifiers",
            )
            .await
            .context("query BetterDisplay")?;
        require_success("query BetterDisplay", &report)?;
        parse_identifiers(&report.stdout_text())
    }
}

fn betterdisplay_executable() -> PathBuf {
    for candidate in [
        "/opt/homebrew/bin/betterdisplaycli",
        "/usr/local/bin/betterdisplaycli",
        "/Applications/BetterDisplay.app/Contents/MacOS/BetterDisplay",
    ] {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return path;
        }
    }
    PathBuf::from("betterdisplaycli")
}

fn parse_identifiers(raw: &str) -> Result<Vec<DisplayIdentifier>> {
    let trimmed = raw.trim().trim_end_matches(',');
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&format!("[{trimmed}]")).context("parse BetterDisplay identifiers")
}

fn require_success(label: &str, report: &super::command::CapturedCommand) -> Result<()> {
    if report.succeeded() {
        Ok(())
    } else {
        bail!("{label} failed: {}", report.detail())
    }
}
