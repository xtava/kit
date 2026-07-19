//! ops — typed, refs-only operation orchestration through the official `op run` command.

mod config;
mod journal;
mod runner;

use std::{path::PathBuf, time::SystemTime};

use anyhow::{anyhow, bail, Context as _, Result};
use async_trait::async_trait;
use clap::{ArgMatches, Command, CommandFactory, FromArgMatches, Parser};

use crate::{
    framework::{Context, Tool, ToolMeta},
    onepassword::OpClient,
};

use config::LoadedConfig;
use journal::{machine_name, JournalEntry, JournalOutcome, JournalStore};
use runner::OpsRunner;

pub fn tool() -> OpsTool {
    OpsTool
}

pub struct OpsTool;

#[derive(Parser)]
#[command(
    name = "ops",
    about = "Run a named operation with masked 1Password secrets",
    long_about = "Run a refs-only, config-defined operation through the official op run command. 1Password owns secret resolution and output masking; Kit owns selection, ref scoping, streaming, and the ref-only journal."
)]
struct OpsArgs {
    /// Stable operation ID from ops.toml.
    #[arg(value_name = "OPERATION")]
    operation: String,

    /// Load this operation catalog instead of project-local or XDG configuration.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

#[async_trait]
impl Tool for OpsTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "ops",
            about: "Run a named operation with masked 1Password secrets",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> Command {
        OpsArgs::command()
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        let args = OpsArgs::from_arg_matches(matches)?;
        if cx.out.is_json() {
            bail!("kit ops streams command output and does not support --json");
        }

        let project_dir = std::env::current_dir().context("resolve current directory")?;
        let loaded = LoadedConfig::load(args.config, project_dir, cx.config.path("ops"))?;
        let operation = loaded.config.operation(&args.operation).ok_or_else(|| {
            anyhow!("operation '{}' was not found in {}", args.operation, loaded.path.display())
        })?;
        let journal_store = JournalStore::bootstrap()?;
        journal_store.load()?;
        let machine = machine_name()?;
        let timestamp_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .context("system clock is before the Unix epoch")?
            .as_secs();
        let started = std::time::Instant::now();
        let execution = OpsRunner::new(OpClient::new()).run(operation).await;
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let outcome = match &execution {
            Ok(status) if status.success() => JournalOutcome::Success,
            Ok(_) | Err(_) => JournalOutcome::Failed,
        };
        let journal_result = journal_store.record(JournalEntry {
            operation_id: operation.id.clone(),
            references: operation.refs.clone(),
            machine,
            timestamp_secs,
            outcome,
            duration_ms,
        });

        match (execution, journal_result) {
            (Ok(status), Ok(())) if status.success() => Ok(()),
            (Ok(status), Ok(())) => match status.code() {
                Some(code) => bail!("operation '{}' failed with status {code}", operation.id),
                None => bail!("operation '{}' terminated by signal", operation.id),
            },
            (Err(error), Ok(())) => {
                Err(error).with_context(|| format!("run operation '{}'", operation.id))
            }
            (Ok(_), Err(journal_error)) => Err(journal_error).context("record ops journal"),
            (Err(run_error), Err(journal_error)) => Err(anyhow!(
                "operation '{}' failed: {run_error}; recording the failed outcome also failed: {journal_error}",
                operation.id
            )),
        }
    }
}
