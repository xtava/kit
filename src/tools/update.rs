//! `update` — rebuild and reinstall Kit from the source checkout that produced this binary.

use std::{env, ffi::OsStr, path::Path, process::Stdio};

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

        let executable = env::current_exe().context("resolve the running Kit executable")?;
        let install_root = install_root(&executable)?;

        println!("Updating Kit from {}", source_dir.display());
        let status = install_command(source_dir, install_root)
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

fn install_root(executable: &Path) -> Result<&Path> {
    if executable.file_stem() != Some(OsStr::new("kit")) {
        bail!(
            "cannot update Kit from executable {}; expected a Cargo-installed kit binary",
            executable.display()
        );
    }

    let Some(bin_dir) = executable.parent() else {
        bail!("cannot resolve the installation directory for {}", executable.display());
    };
    if bin_dir.file_name() != Some(OsStr::new("bin")) {
        bail!(
            "cannot update Kit from {}; expected the executable under a Cargo installation's bin directory",
            executable.display()
        );
    }

    bin_dir.parent().with_context(|| {
        format!("cannot resolve the Cargo installation root for {}", executable.display())
    })
}

fn install_command(source_dir: &Path, install_root: &Path) -> TokioCommand {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = TokioCommand::new(cargo);
    command
        .args(["install", "--locked", "--force", "--jobs", "2", "--bin", "kit", "--root"])
        .arg(install_root)
        .arg("--path")
        .arg(source_dir);
    command
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{install_command, install_root};

    #[test]
    fn update_targets_the_current_root_with_bounded_jobs() {
        let command = install_command(Path::new("/tmp/kit source"), Path::new("/tmp/cargo root"));
        let command = command.as_std();
        let args =
            command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect::<Vec<_>>();

        assert_eq!(
            args,
            [
                "install",
                "--locked",
                "--force",
                "--jobs",
                "2",
                "--bin",
                "kit",
                "--root",
                "/tmp/cargo root",
                "--path",
                "/tmp/kit source"
            ]
        );
    }

    #[test]
    fn install_root_is_the_parent_of_the_bin_directory() {
        assert_eq!(
            install_root(Path::new("/home/user/.cargo/bin/kit")).unwrap(),
            Path::new("/home/user/.cargo")
        );
    }

    #[test]
    fn install_root_rejects_non_installed_executables() {
        let error = install_root(Path::new("/workspace/kit/target/debug/kit")).unwrap_err();
        assert!(error.to_string().contains("Cargo installation's bin directory"));

        let error = install_root(Path::new("/home/user/.cargo/bin/renamed-kit")).unwrap_err();
        assert!(error.to_string().contains("expected a Cargo-installed kit binary"));
    }
}
