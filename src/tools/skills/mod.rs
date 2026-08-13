//! skills — one canonical Agent Skills library with explicit app availability.

mod catalog;
mod config;
mod contributions;
mod controller;
mod model;
mod projections;
mod tui;

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use clap::{ArgMatches, Command, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};

use crate::framework::{Context, SettingsSection, Tool, ToolMeta};

use controller::SkillsController;
use model::{
    DoctorIssue, DoctorReport, LibraryReport, LibrarySetReport, OperationKind, OperationReport,
    ProjectionId, ProjectionReport, ProjectionScope, ProjectionTarget, SkillsSnapshot,
};

pub fn tool() -> SkillsTool {
    SkillsTool
}

pub struct SkillsTool;

#[derive(Parser)]
#[command(name = "skills", about = "Manage one agent-skills library and app availability")]
struct SkillsArgs {
    #[command(subcommand)]
    command: Option<SkillsCommand>,
}

#[derive(Subcommand)]
enum SkillsCommand {
    /// Inspect or configure the one canonical skills library.
    Library {
        #[command(subcommand)]
        command: LibraryCommand,
    },
    /// Create a new canonical Agent Skill.
    Create {
        /// Lowercase skill name; the directory and frontmatter name are identical.
        name: String,
        /// Clear description of what the skill does and when an agent should use it.
        #[arg(long)]
        description: String,
    },
    /// Report where every skill is available and which apps can discover it.
    List {
        /// Resolve "This project" against the worktree containing this path.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Enable skill availability without overwriting foreign content.
    Enable {
        /// One or more exact canonical skill names.
        #[arg(required = true)]
        skills: Vec<String>,
        /// Make the skill available in this project or in all projects.
        #[arg(long, value_enum, default_value_t = ScopeArg::ThisProject)]
        scope: ScopeArg,
        /// Make the skill available to Claude Code, Codex, or both apps.
        #[arg(long, value_enum, default_value_t = AppArg::All)]
        app: AppArg,
        /// Resolve the project worktree from this path.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Disable only exact manager-owned skill availability.
    Disable {
        /// One or more exact canonical skill names.
        #[arg(required = true)]
        skills: Vec<String>,
        /// Remove availability from this project or from all projects.
        #[arg(long, value_enum, default_value_t = ScopeArg::ThisProject)]
        scope: ScopeArg,
        /// Remove availability from Claude Code, Codex, or both apps.
        #[arg(long, value_enum, default_value_t = AppArg::All)]
        app: AppArg,
        /// Resolve the project worktree from this path.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Diagnose the library, skill documents, repository, and availability paths.
    Doctor {
        /// Resolve "This project" against the worktree containing this path.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum LibraryCommand {
    /// Show the configured canonical library.
    Show,
    /// Set the one canonical library path.
    Set {
        path: PathBuf,
        /// Create the directory when it does not exist.
        #[arg(long)]
        create: bool,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ScopeArg {
    ThisProject,
    AllProjects,
}

impl From<ScopeArg> for ProjectionScope {
    fn from(scope: ScopeArg) -> Self {
        match scope {
            ScopeArg::ThisProject => Self::ThisProject,
            ScopeArg::AllProjects => Self::AllProjects,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum AppArg {
    ClaudeCode,
    Codex,
    All,
}

impl AppArg {
    fn projections(self, scope: ScopeArg) -> Vec<ProjectionId> {
        let scope = scope.into();
        match self {
            Self::ClaudeCode => vec![ProjectionId::new(scope, ProjectionTarget::ClaudeCode)],
            Self::Codex => vec![ProjectionId::new(scope, ProjectionTarget::Codex)],
            Self::All => vec![
                ProjectionId::new(scope, ProjectionTarget::ClaudeCode),
                ProjectionId::new(scope, ProjectionTarget::Codex),
            ],
        }
    }
}

#[async_trait]
impl Tool for SkillsTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "skills",
            about: "Manage one agent-skills library and app availability",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn settings(&self) -> Option<SettingsSection> {
        Some(config::settings())
    }

    fn command(&self) -> Command {
        SkillsArgs::command()
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        let args = SkillsArgs::from_arg_matches(matches)?;
        let working_directory = std::env::current_dir().context("resolve current directory")?;
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set; Skills cannot resolve all-project availability")?;
        let mut controller =
            SkillsController::new(cx.config.clone(), cx.repositories, working_directory, home)?;

        match args.command {
            Some(SkillsCommand::Library { command: LibraryCommand::Show }) => {
                print_library(cx, controller.library_report())?;
            }
            Some(SkillsCommand::Library { command: LibraryCommand::Set { path, create } }) => {
                print_library_set(cx, controller.set_library(&path, create)?)?;
            }
            Some(SkillsCommand::Create { name, description }) => {
                let skill = controller.create(&name, &description)?;
                if cx.out.is_json() {
                    cx.out.json(&skill)?;
                } else {
                    println!("Created {} at {}", skill.name, skill.path.display());
                }
            }
            Some(SkillsCommand::List { repo }) => {
                print_snapshot(cx, controller.snapshot(repo.as_deref())?)?;
            }
            Some(SkillsCommand::Enable { skills, scope, app, repo }) => {
                print_operation(
                    cx,
                    controller.mutate(
                        OperationKind::Enable,
                        &skills,
                        &app.projections(scope),
                        repo.as_deref(),
                    )?,
                )?;
            }
            Some(SkillsCommand::Disable { skills, scope, app, repo }) => {
                print_operation(
                    cx,
                    controller.mutate(
                        OperationKind::Disable,
                        &skills,
                        &app.projections(scope),
                        repo.as_deref(),
                    )?,
                )?;
            }
            Some(SkillsCommand::Doctor { repo }) => {
                print_doctor(cx, controller.doctor(repo.as_deref()))?;
            }
            None => {
                if cx.out.is_json() {
                    print_snapshot(cx, controller.snapshot(None)?)?;
                } else if cx.term.interactive() {
                    tui::run(controller).await?;
                } else {
                    match controller.library_report() {
                        LibraryReport::Unconfigured => {
                            print_library(cx, LibraryReport::Unconfigured)?;
                        }
                        LibraryReport::Configured { .. } => {
                            print_snapshot(cx, controller.snapshot(None)?)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn print_library(cx: &Context, report: LibraryReport) -> Result<()> {
    if cx.out.is_json() {
        return cx.out.json(&report);
    }
    match report {
        LibraryReport::Unconfigured => {
            println!("No canonical Skills library is configured.");
            println!("Set one with: kit skills library set <path> --create");
        }
        LibraryReport::Configured { path } => println!("{}", path.display()),
    }
    Ok(())
}

fn print_library_set(cx: &Context, report: LibrarySetReport) -> Result<()> {
    if cx.out.is_json() {
        return cx.out.json(&report);
    }
    let verb = if report.created { "Created and configured" } else { "Configured" };
    println!("{verb} Skills library {}", report.path.display());
    Ok(())
}

fn print_snapshot(cx: &Context, snapshot: SkillsSnapshot) -> Result<()> {
    if cx.out.is_json() {
        return cx.out.json(&snapshot);
    }
    println!("library {}", snapshot.library.display());
    match &snapshot.repository {
        model::RepositoryReport::Available { root } => println!("project {}", root.display()),
        model::RepositoryReport::Unavailable { reason } => {
            println!("project unavailable: {reason}")
        }
    }
    if snapshot.skills.is_empty() {
        println!("No valid skills.");
    } else {
        println!();
        println!("{:<28} {:^25} {:^25}", "SKILL", "THIS PROJECT", "ALL PROJECTS");
        println!(
            "{:<28} {:^12} {:^12} {:^12} {:^12}",
            "", "CLAUDE CODE", "CODEX", "CLAUDE CODE", "CODEX"
        );
        for skill in &snapshot.skills {
            let states: Vec<&str> = ProjectionId::ALL
                .iter()
                .map(|id| projection_label(&skill.projections, *id))
                .collect();
            println!(
                "{:<28} {:^12} {:^12} {:^12} {:^12}",
                skill.skill.name.as_str(),
                states[0],
                states[1],
                states[2],
                states[3]
            );
        }
    }
    if !snapshot.invalid.is_empty() {
        println!();
        println!("Invalid canonical entries:");
        for invalid in snapshot.invalid {
            println!("  {}: {}", invalid.directory, invalid.error);
        }
    }
    Ok(())
}

fn projection_label(reports: &[ProjectionReport], id: ProjectionId) -> &str {
    reports
        .iter()
        .find(|report| report.id() == id)
        .map_or("missing", |report| report.state().map_or("n/a", |state| state.short_label()))
}

fn print_operation(cx: &Context, report: OperationReport) -> Result<()> {
    if cx.out.is_json() {
        return cx.out.json(&report);
    }
    for change in report.changes {
        println!(
            "{:<16} {:<24} {:<32} {}",
            change.outcome.label(),
            change.skill,
            change.projection.label(),
            change.path.display()
        );
    }
    Ok(())
}

fn print_doctor(cx: &Context, report: DoctorReport) -> Result<()> {
    if cx.out.is_json() {
        return cx.out.json(&report);
    }
    if report.healthy() {
        println!("Skills doctor: healthy");
        return Ok(());
    }
    println!("Skills doctor: {} issue(s)", report.issues.len());
    for issue in report.issues {
        match issue {
            DoctorIssue::LibraryUnconfigured => println!("  library: not configured"),
            DoctorIssue::LibraryUnavailable { path, error } => {
                println!("  library {}: {error}", path.display());
            }
            DoctorIssue::RepositoryUnavailable { error } => {
                println!("  project: {error}");
            }
            DoctorIssue::InvalidSkill { directory, path, error } => {
                println!("  skill {directory} ({}): {error}", path.display());
            }
            DoctorIssue::ProjectionProblem { skill, projection, path, state } => {
                println!(
                    "  {} {} ({}): {}",
                    skill,
                    projection.label(),
                    path.display(),
                    state.short_label()
                );
            }
            DoctorIssue::ProjectionUnavailable { skill, projection, error } => {
                println!("  {} {}: {error}", skill, projection.label());
            }
        }
    }
    Ok(())
}
