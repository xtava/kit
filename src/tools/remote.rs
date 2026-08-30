//! `remote` — run one command on an authenticated tailnet peer.

use std::{
    fs::File,
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Context as _, Result};
use async_trait::async_trait;
use clap::{ArgMatches, Command, CommandFactory, FromArgMatches, Parser};
use serde::Serialize;

use crate::{
    framework::{
        process::{LeaderExit, LeaderExitObservation},
        Context, Tool, ToolMeta,
    },
    remote::{RemoteExecutor, RemoteRequest, MAX_REMOTE_INPUT_BYTES},
};

pub fn tool() -> RemoteTool {
    RemoteTool
}

pub struct RemoteTool;

#[derive(Parser)]
#[command(name = "remote", about = "Run a command on another machine over your tailnet")]
struct RemoteArgs {
    /// Tailnet machine name, DNS name, stable node ID, or Tailscale IP.
    #[arg(value_name = "MACHINE")]
    machine: String,
    /// Remote Unix user. Defaults to the current local user.
    #[arg(long, value_name = "USER")]
    user: Option<String>,
    /// Send this file as the remote command's standard input.
    #[arg(long, value_name = "PATH", conflicts_with = "stdin")]
    input: Option<PathBuf>,
    /// Read the remote command's standard input from this process.
    #[arg(long)]
    stdin: bool,
    /// Maximum command duration in seconds.
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..=3600))]
    timeout: u64,
    /// Command and arguments. Place them after `--`.
    #[arg(last = true, required = true, num_args = 1.., allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonOutput {
    stdout: String,
    stderr: String,
    exit: LeaderExitObservation,
}

#[async_trait]
impl Tool for RemoteTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "remote",
            about: "Run a command on another machine over your tailnet",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> Command {
        RemoteArgs::command()
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        let args = RemoteArgs::from_arg_matches(matches)?;
        let user = args
            .user
            .or_else(|| std::env::var("USER").ok())
            .context("remote user is unavailable; pass --user")?;
        let input = read_input(args.input.as_deref(), args.stdin)?;
        let request = RemoteRequest::new(
            args.machine,
            user,
            args.command,
            input,
            Duration::from_secs(args.timeout),
        )?;
        let output = RemoteExecutor::new(cx.processes.clone(), std::env::current_dir()?)
            .execute(request)
            .await?;

        if cx.out.is_json() {
            cx.out.json(&JsonOutput {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                exit: output.exit,
            })?;
        } else {
            io::stdout().write_all(&output.stdout)?;
            io::stderr().write_all(&output.stderr)?;
        }

        match output.exit {
            LeaderExitObservation::Observed(LeaderExit::Code(0)) => Ok(()),
            LeaderExitObservation::Observed(LeaderExit::Code(code)) => {
                bail!("remote command exited with code {code}")
            }
            LeaderExitObservation::Observed(LeaderExit::Signal(signal)) => {
                bail!("remote command exited from signal {signal:?}")
            }
            LeaderExitObservation::NotObserved => bail!("remote command exit was not observed"),
        }
    }
}

fn read_input(path: Option<&Path>, force_stdin: bool) -> Result<Vec<u8>> {
    if let Some(path) = path {
        let file =
            File::open(path).with_context(|| format!("opening remote input {}", path.display()))?;
        return read_bounded(file)
            .with_context(|| format!("reading remote input {}", path.display()));
    }
    if force_stdin || !io::stdin().is_terminal() {
        return read_bounded(io::stdin().lock()).context("reading remote input from stdin");
    }
    Ok(Vec::new())
}

fn read_bounded(reader: impl Read) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take((MAX_REMOTE_INPUT_BYTES + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_REMOTE_INPUT_BYTES {
        bail!("remote input exceeds the {} MiB limit", MAX_REMOTE_INPUT_BYTES / 1024 / 1024);
    }
    Ok(bytes)
}
