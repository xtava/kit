//! `update` — rebuild and reinstall Kit from the source checkout that produced this binary.

use std::{env, ffi::OsString, path::Path, process::Stdio};

use anyhow::{bail, Context as AnyhowContext, Result};
use async_trait::async_trait;
use clap::{ArgMatches, Command, CommandFactory, FromArgMatches, Parser};
use tokio::process::Command as TokioCommand;

use crate::framework::{Context, Tool, ToolMeta};

const SOURCE_DIR: &str = env!("CARGO_MANIFEST_DIR");

pub fn tool() -> UpdateTool {
    UpdateTool
}

pub struct UpdateTool;

#[derive(Parser)]
#[command(
    name = "update",
    about = "Rebuild and reinstall Kit from its source checkout",
    long_about = "Rebuilds the source checkout that produced this binary and replaces the Cargo-installed kit executable. This uses the checkout exactly as it exists; it does not pull or modify Git state."
)]
struct UpdateArgs;

#[async_trait]
impl Tool for UpdateTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "update",
            about: "Rebuild and reinstall Kit from its source checkout",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> Command {
        UpdateArgs::command()
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        UpdateArgs::from_arg_matches(matches)?;
        if cx.out.is_json() {
            bail!("kit update does not support --json because Cargo streams build output");
        }

        let source_dir = Path::new(SOURCE_DIR);
        if !source_dir.join("Cargo.toml").is_file() {
            bail!(
                "Kit source checkout is unavailable at {}; reinstall from the checkout with `cargo install --path .`",
                source_dir.display()
            );
        }

        println!("Updating Kit from {}", source_dir.display());
        let status = install_command(source_dir)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .await
            .context("run Cargo to update Kit")?;

        if !status.success() {
            bail!("Kit update failed: Cargo exited with {status}");
        }

        println!("Kit update complete");
        Ok(())
    }
}

fn install_command(source_dir: &Path) -> TokioCommand {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let mut command = TokioCommand::new(cargo);
    command.args(["install", "--locked", "--force", "--path"]).arg(source_dir);
    command
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::install_command;

    #[test]
    fn update_uses_the_locked_forced_path_install() {
        let command = install_command(Path::new("/tmp/kit source"));
        let command = command.as_std();
        let args =
            command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect::<Vec<_>>();

        assert_eq!(args, ["install", "--locked", "--force", "--path", "/tmp/kit source"]);
    }
}
