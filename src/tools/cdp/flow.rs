//! Flows — reusable verification sequences. A flow file is lines of the *exact* session grammar
//! the CLI and the interactive prompt already speak, one step per line, `#` comments, with
//! `${param}` substitution. Project flows live in `.kit/cdp/flows/` (committed, shared across
//! agent sessions); user flows in the config dir. Project shadows user on a name collision.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{bail, Context as _, Result};
use clap::Parser;

use super::protocol::{Command, Step};

/// The clap parser for one line of the session grammar — `no_binary_name` so the first token is
/// the subcommand. Shared by flow steps and the interactive prompt: one grammar, three surfaces.
#[derive(Parser)]
#[command(no_binary_name = true)]
struct StepLine {
    #[command(subcommand)]
    command: super::CdpCommand,
}

/// Parse already-split tokens of one session line into a wire [`Command`].
pub fn parse_session_tokens(tokens: &[String]) -> Result<Command> {
    let parsed = StepLine::try_parse_from(tokens)?;
    super::session_command(parsed.command)
}

/// Parse a flow script into executable steps: substitute `${params}`, skip blanks and `#`
/// comments, parse each remaining line. Steps that can't run inside a batch are rejected here,
/// so the daemon never has to refuse one mid-run.
pub fn parse_script(text: &str, params: &HashMap<String, String>) -> Result<Vec<Step>> {
    let mut steps = Vec::new();
    for (number, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = substitute(line, params).with_context(|| format!("line {}", number + 1))?;
        let tokens = shell_words::split(&line)
            .with_context(|| format!("line {}: unbalanced quotes", number + 1))?;
        let command = parse_session_tokens(&tokens)
            .with_context(|| format!("line {}: {line}", number + 1))?;
        if matches!(command, Command::Do { .. }) {
            bail!("line {}: a flow step cannot be another `do` — inline the steps", number + 1);
        }
        steps.push(Step { line, command: Box::new(command) });
    }
    if steps.is_empty() {
        bail!("no steps — the script is empty or all comments");
    }
    Ok(steps)
}

/// Replace every `${name}` with its parameter; an unknown name is an error that says what to pass.
fn substitute(line: &str, params: &HashMap<String, String>) -> Result<String> {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            bail!("unclosed ${{…}} parameter");
        };
        let name = &after[..end];
        match params.get(name) {
            Some(value) => out.push_str(value),
            None => bail!("missing parameter '{name}' — pass {name}=<value>"),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Parse trailing `key=value` arguments into a parameter map.
pub fn parse_params(raw: &[String]) -> Result<HashMap<String, String>> {
    let mut params = HashMap::new();
    for entry in raw {
        let Some((key, value)) = entry.split_once('=') else {
            bail!("bad parameter '{entry}' — expected key=value");
        };
        params.insert(key.trim().to_owned(), value.to_owned());
    }
    Ok(params)
}

/// A discovered flow: its name, where it lives, and which scope it came from.
pub struct FlowFile {
    pub name: String,
    pub path: PathBuf,
    pub scope: &'static str,
}

/// Load a flow by name — project first (the override), then user config.
pub fn load(name: &str) -> Result<(String, PathBuf)> {
    for dir in flow_dirs() {
        let path = dir.join(format!("{name}.flow"));
        if let Ok(source) = std::fs::read_to_string(&path) {
            return Ok((source, path));
        }
    }
    let known = list();
    if known.is_empty() {
        bail!("no flow '{name}' — no flows exist yet (create .kit/cdp/flows/{name}.flow)");
    }
    let names: Vec<&str> = known.iter().map(|flow| flow.name.as_str()).collect();
    bail!("no flow '{name}'\navailable: {}", names.join(", "))
}

/// Every available flow, project scope first, deduped by name (project shadows user).
pub fn list() -> Vec<FlowFile> {
    let mut flows: Vec<FlowFile> = Vec::new();
    for (dir, scope) in flow_dirs().into_iter().zip(["project", "user"]) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "flow") {
                continue;
            }
            let Some(name) = path.file_stem().map(|stem| stem.to_string_lossy().into_owned())
            else {
                continue;
            };
            if !flows.iter().any(|flow| flow.name == name) {
                flows.push(FlowFile { name, path, scope });
            }
        }
    }
    flows.sort_by(|a, b| a.name.cmp(&b.name));
    flows
}

/// `[project .kit/cdp/flows (nearest ancestor), user config flows]` — either may not exist.
fn flow_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(project) = project_dir() {
        dirs.push(project);
    }
    dirs.push(user_dir());
    dirs
}

/// Walk up from the working directory to the nearest `.kit/cdp/flows`.
fn project_dir() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join(".kit/cdp/flows");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn user_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "kit")
        .map(|dirs| dirs.config_dir().join("cdp/flows"))
        .unwrap_or_else(|| PathBuf::from("cdp/flows"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::cdp::protocol::Locator;

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(key, value)| ((*key).to_owned(), (*value).to_owned())).collect()
    }

    /// A flow script is the session grammar verbatim: comments and blanks vanish, quotes work,
    /// and each line becomes the same wire command the CLI would send.
    #[test]
    fn scripts_parse_through_the_session_grammar() {
        let script = "\
# checkout smoke
click 'button:Save settings'

expect text 'Saved'
verify
";
        let steps = parse_script(script, &HashMap::new()).unwrap();
        assert_eq!(steps.len(), 3);
        assert!(matches!(
            &*steps[0].command,
            Command::Click { locator: Locator::Query { role: Some(role), name }, settle: Some(_), .. }
                if role == "button" && name == "Save settings"
        ));
        assert!(matches!(&*steps[2].command, Command::Verify { window: None, .. }));
    }

    #[test]
    fn parameters_substitute_and_missing_ones_name_themselves() {
        let steps =
            parse_script("fill textbox:Name '${user}'", &params(&[("user", "Ada")])).unwrap();
        assert!(steps[0].line.contains("Ada"));

        let missing = parse_script("fill textbox:Name '${user}'", &HashMap::new()).unwrap_err();
        assert!(missing.root_cause().to_string().contains("user=<value>"), "{missing:#}");
    }

    #[test]
    fn a_flow_cannot_nest_do() {
        let error = parse_script("do 'verify'", &HashMap::new()).unwrap_err();
        assert!(format!("{error:#}").contains("cannot be another"), "{error:#}");
    }

    #[test]
    fn bad_lines_report_their_number_and_text() {
        let error = parse_script("# fine\nclick", &HashMap::new()).unwrap_err();
        assert!(format!("{error:#}").contains("line 2"), "{error:#}");
    }
}
