//! domain — authoritative domain registration checker.
//!
//! DNS delegation → registry RDAP → registry WHOIS. "Available" is reported only when a registry
//! source confirms the name is unregistered. Headless when given names or piped; otherwise a TUI.

mod config;
mod engine;
mod report;
mod tui;

use std::io;
use std::sync::OnceLock;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use clap::{ArgMatches, Command, CommandFactory, FromArgMatches, Parser};

use crate::framework::{Context, Tool, ToolMeta};
use config::Config;
use engine::{canonicalize_suffix, expand_domains, CheckClient, Verdict};

pub const DEFAULT_TLDS: &[&str] = &["com", "ai", "io", "studio"];

fn default_tlds_hint() -> &'static str {
    static HINT: OnceLock<String> = OnceLock::new();
    HINT.get_or_init(|| DEFAULT_TLDS.join(",")).as_str()
}

pub fn tool() -> DomainTool {
    DomainTool
}

pub struct DomainTool;

#[derive(Parser)]
#[command(
    name = "domain",
    about = "Authoritative domain registration checker",
    long_about = "Checks whether domains are registered using DNS delegation, registry RDAP, then registry WHOIS. Available is only reported when a registry source confirms it."
)]
struct DomainArgs {
    /// Names or full domains to check. Bare names expand across the active TLD set.
    #[arg(value_name = "NAME_OR_DOMAIN")]
    names: Vec<String>,

    /// Comma-separated TLDs used to expand bare names.
    #[arg(short, long, value_name = default_tlds_hint())]
    tlds: Option<String>,

    /// Only print domains confirmed available by registry sources.
    #[arg(short, long)]
    available: bool,
}

#[async_trait]
impl Tool for DomainTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "domain",
            about: "Authoritative domain registration checker",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> Command {
        DomainArgs::command()
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        let args = DomainArgs::from_arg_matches(matches)?;

        if args.names.is_empty() && cx.term.interactive() && !cx.out.is_json() {
            let config = Config::load(cx.config.clone())?;
            return tui::run(config).await;
        }

        let mut tokens = args.names;
        if tokens.is_empty() {
            tokens.extend(read_stdin_tokens()?);
        }
        if tokens.is_empty() {
            DomainArgs::command().print_help()?;
            println!();
            return Ok(());
        }

        let tlds = match args.tlds.as_deref() {
            Some(raw) => parse_tlds(raw)?,
            None => DEFAULT_TLDS.iter().map(|tld| (*tld).to_owned()).collect(),
        };

        let domains = expand_domains(tokens.iter().map(String::as_str), &tlds);
        let client = CheckClient::new()?;
        let mut results = client.check_many(domains, 8).await;

        if args.available {
            results.retain(|result| result.verdict == Verdict::Available);
        }

        if cx.out.is_json() {
            cx.out.json(&results)?;
        } else {
            report::print_results(&results);
        }
        Ok(())
    }
}

fn read_stdin_tokens() -> Result<Vec<String>> {
    let input = io::read_to_string(io::stdin())?;
    Ok(input.split_whitespace().map(ToOwned::to_owned).collect())
}

fn parse_tlds(raw: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for item in raw.split(',') {
        let Some(tld) = canonicalize_suffix(item) else {
            continue;
        };
        if !out.iter().any(|existing| existing == &tld) {
            out.push(tld);
        }
    }

    if out.is_empty() {
        Err(anyhow!("--tlds must contain at least one valid suffix"))
    } else {
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tlds_as_canonical_suffixes() -> Result<()> {
        assert_eq!(
            parse_tlds(" .COM.,co.uk,com ")?,
            vec!["com".to_owned(), "co.uk".to_owned()]
        );
        Ok(())
    }

    #[test]
    fn rejects_explicit_tld_sets_without_valid_suffixes() {
        let error = parse_tlds("bad suffix,").expect_err("invalid TLD set must fail");
        assert!(error.to_string().contains("at least one valid suffix"));
    }
}
