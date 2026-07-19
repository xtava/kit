use std::{
    ffi::OsString,
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context, Result};

pub enum ExternalTarget<'a> {
    Url(&'a str),
    Path(&'a Path),
}

impl ExternalTarget<'_> {
    fn argument(&self) -> OsString {
        match self {
            Self::Url(url) => OsString::from(url),
            Self::Path(path) => path.as_os_str().to_owned(),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct OpenCommand {
    program: &'static str,
    args: Vec<OsString>,
}

pub fn open_external(target: ExternalTarget<'_>) -> Result<()> {
    let command = command_for(std::env::consts::OS, target.argument())?;
    Command::new(command.program)
        .args(command.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("start {}", command.program))?;
    Ok(())
}

fn command_for(platform: &str, target: OsString) -> Result<OpenCommand> {
    match platform {
        "linux" => Ok(OpenCommand { program: "xdg-open", args: vec![target] }),
        "macos" => Ok(OpenCommand { program: "open", args: vec![target] }),
        "windows" => Ok(OpenCommand {
            program: "rundll32.exe",
            args: vec![OsString::from("url.dll,FileProtocolHandler"), target],
        }),
        other => anyhow::bail!("opening external targets is unsupported on {other}"),
    }
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
}
