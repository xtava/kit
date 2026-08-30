//! Source registration and the one canonical managed Kit update transaction.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{BufReader, Read},
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use directories::{BaseDirs, ProjectDirs};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::framework::{
    process::{
        CaptureOverflow, CapturePolicy, CommandSpec, CompletionCause, ContainmentRequirement,
        EnvironmentBase, InputPolicy, LeaderExit, LeaderExitObservation, OutputPolicy,
        OutputReport, ProcessDeadline, ProcessEnvironment, ProcessLabel, ProcessSpec,
        ProcessSupervisor, TerminationPolicy,
    },
    AtomicFileTryLock, AtomicFileWriter,
};

#[path = "../source_identity.rs"]
mod source_identity;

use source_identity::SOURCE_IDENTITY_PATHS;

const STATE_SCHEMA_VERSION: u32 = 1;
const STATE_FILE: &str = "source.json";
const GIT_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const TERMINATION_GRACE: Duration = Duration::from_secs(3);
const CAPTURE_BYTES: NonZeroUsize = NonZeroUsize::new(32 * 1024 * 1024).unwrap();

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourceRegistration {
    schema_version: u32,
    checkout: PathBuf,
    branch: String,
    upstream_remote: String,
    upstream_ref: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceRegistrationReceipt {
    pub checkout: PathBuf,
    pub branch: String,
    pub upstream_remote: String,
    pub upstream_ref: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceDisposition {
    FastForwarded,
    Current,
    LocalAhead,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildIdentity {
    pub product: String,
    pub version: String,
    pub source_revision: String,
    pub source_dirty: Option<bool>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateReceipt {
    pub checkout: PathBuf,
    pub branch: String,
    pub upstream: String,
    pub before_revision: String,
    pub installed_revision: String,
    pub source_disposition: SourceDisposition,
    pub installed_executable: PathBuf,
    pub installed_sha256: String,
    pub console_status: Value,
}

pub fn build_identity() -> BuildIdentity {
    BuildIdentity {
        product: "kit".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        source_revision: env!("KIT_SOURCE_REVISION").to_owned(),
        source_dirty: match env!("KIT_SOURCE_DIRTY") {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
    }
}

#[derive(Clone)]
pub struct SourceUpdater {
    processes: ProcessSupervisor,
    state_directory: PathBuf,
    state_path: PathBuf,
    managed_executable: PathBuf,
    current_executable: PathBuf,
    git_program: OsString,
}

#[derive(Debug)]
struct CommandOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    completion: CompletionCause,
    exit: LeaderExitObservation,
}

struct InstallLock {
    directory: PathBuf,
    owner_file: PathBuf,
    owner_pid: u32,
    handoff_started: AtomicBool,
    handoff_completed: AtomicBool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Ancestry {
    Equal,
    RemoteAhead,
    LocalAhead,
    Diverged,
}

impl SourceUpdater {
    pub fn new(processes: ProcessSupervisor) -> Result<Self> {
        let project = ProjectDirs::from("", "", "kit").context("resolve Kit state directory")?;
        let state_directory =
            project.state_dir().unwrap_or_else(|| project.data_local_dir()).join("updates");
        let base = BaseDirs::new().context("resolve the local home directory")?;
        let managed_executable = base.home_dir().join(".local/bin/kit");
        let current_executable =
            std::env::current_exe().context("resolve the running Kit executable")?;
        Ok(Self {
            processes,
            state_path: state_directory.join(STATE_FILE),
            state_directory,
            managed_executable,
            current_executable,
            git_program: OsString::from("git"),
        })
    }

    pub async fn register_source(&self, checkout: &Path) -> Result<SourceRegistrationReceipt> {
        let writer =
            AtomicFileWriter::new(&self.state_directory, ".source.lock", ".source-registration");
        let _guard = match writer.try_lock().context("lock Kit source registration")? {
            AtomicFileTryLock::Acquired(guard) => guard,
            AtomicFileTryLock::Busy => bail!("another Kit source registration is in progress"),
        };
        let receipt = self.inspect_source(checkout).await?;
        let registration = SourceRegistration {
            schema_version: STATE_SCHEMA_VERSION,
            checkout: receipt.checkout.clone(),
            branch: receipt.branch.clone(),
            upstream_remote: receipt.upstream_remote.clone(),
            upstream_ref: receipt.upstream_ref.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&registration)
            .context("serialize Kit source registration")?;
        writer.replace(&self.state_path, &bytes).context("write Kit source registration")?;
        Ok(receipt)
    }

    pub async fn install_managed(&self, show_progress: bool) -> Result<UpdateReceipt> {
        self.require_managed_invocation()?;
        let install_lock = InstallLock::acquire(&self.managed_executable)?;
        let registration = self.load_registration()?;
        let inspected = self.inspect_source(&registration.checkout).await?;
        require_registration_unchanged(&registration, &inspected)?;
        self.require_supported_checkout(&registration.checkout).await?;
        let restart_console = self.require_console_idle(&registration.checkout).await?;

        let before_revision = self.head_revision(&registration.checkout).await?;
        let status_before = self.status_snapshot(&registration.checkout).await?;
        self.fetch_registered_upstream(&registration).await?;
        let fetched_revision = self
            .git_revision(&registration.checkout, "FETCH_HEAD^{commit}")
            .await
            .context("resolve the fetched Kit upstream revision")?;
        let ancestry = self
            .classify_ancestry(&registration.checkout, &before_revision, &fetched_revision)
            .await?;

        let source_disposition = match ancestry {
            Ancestry::Equal => SourceDisposition::Current,
            Ancestry::LocalAhead => SourceDisposition::LocalAhead,
            Ancestry::Diverged => bail!(
                "the registered Kit branch and {} have diverged; refusing to reset, rebase, or discard local commits",
                registration.upstream_ref
            ),
            Ancestry::RemoteAhead => {
                self.require_disjoint_fast_forward(
                    &registration.checkout,
                    &before_revision,
                    &fetched_revision,
                )
                .await?;
                self.fast_forward(&registration.checkout, &fetched_revision).await?;
                let merged_revision = self.head_revision(&registration.checkout).await?;
                if merged_revision != fetched_revision {
                    bail!(
                        "Kit fast-forward selected {fetched_revision}, but the checkout reached {merged_revision}"
                    );
                }
                let status_after = self.status_snapshot(&registration.checkout).await?;
                if status_after != status_before {
                    bail!(
                        "Kit fast-forward changed existing staged, unstaged, or untracked work; refusing to install"
                    );
                }
                SourceDisposition::FastForwarded
            }
        };

        let installed_revision = self.head_revision(&registration.checkout).await?;
        let status_at_install = self.status_snapshot(&registration.checkout).await?;
        let dirty_paths_at_install = self.dirty_source_paths(&registration.checkout).await?;
        let content_at_install =
            source_content_fingerprint(&registration.checkout, &dirty_paths_at_install)?;
        self.run_installer(&registration.checkout, show_progress, &install_lock).await?;
        let status_after_install = self.status_snapshot(&registration.checkout).await?;
        let dirty_paths_after_install = self.dirty_source_paths(&registration.checkout).await?;
        let content_after_install =
            source_content_fingerprint(&registration.checkout, &dirty_paths_after_install)?;
        if status_after_install != status_at_install
            || dirty_paths_after_install != dirty_paths_at_install
            || content_after_install != content_at_install
        {
            bail!(
                "Kit {installed_revision} was installed, but the registered source content changed during the build; refusing to attest the replacement"
            );
        }

        let identity = self.installed_identity().await.with_context(|| {
            format!(
                "install.sh completed, but the replacement at {} did not report its source identity",
                self.managed_executable.display()
            )
        })?;
        let expected_source_dirty = !dirty_paths_after_install.is_empty();
        if identity.product != "kit"
            || identity.source_revision != installed_revision
            || identity.source_dirty != Some(expected_source_dirty)
        {
            bail!(
                "install.sh completed, but {} reports product {} at revision {} with dirty={:?}; expected Kit revision {installed_revision} with dirty={expected_source_dirty}",
                self.managed_executable.display(),
                identity.product,
                identity.source_revision,
                identity.source_dirty
            );
        }
        let installed_sha256 = sha256_file(&self.managed_executable)?;
        let console_status = self
            .reconcile_console(&registration.checkout, restart_console)
            .await
            .with_context(|| {
                format!(
                    "Kit {installed_revision} was replaced and verified at {}, but Console could not be safely reconciled",
                    self.managed_executable.display()
                )
            })?;

        Ok(UpdateReceipt {
            checkout: registration.checkout,
            branch: registration.branch,
            upstream: format!("{}/{}", registration.upstream_remote, registration.upstream_ref),
            before_revision,
            installed_revision,
            source_disposition,
            installed_executable: self.managed_executable.clone(),
            installed_sha256,
            console_status,
        })
    }

    async fn inspect_source(&self, checkout: &Path) -> Result<SourceRegistrationReceipt> {
        let checkout = fs::canonicalize(checkout)
            .with_context(|| format!("resolve Kit source checkout {}", checkout.display()))?;
        let top_level = self
            .git_text(&checkout, ["rev-parse", "--show-toplevel"])
            .await
            .context("resolve the Kit Git worktree root")?;
        let top_level = fs::canonicalize(&top_level)
            .with_context(|| format!("resolve Git worktree root {top_level}"))?;
        if top_level != checkout {
            bail!(
                "{} is inside the Kit worktree at {}; register the worktree root itself",
                checkout.display(),
                top_level.display()
            );
        }
        require_kit_manifest(&checkout)?;
        require_installer(&checkout)?;

        let branch = self
            .git_text(&checkout, ["symbolic-ref", "--quiet", "--short", "HEAD"])
            .await
            .context("resolve the checked-out Kit branch")?;
        require_safe_git_name("branch", &branch)?;
        let remote_key = format!("branch.{branch}.remote");
        let merge_key = format!("branch.{branch}.merge");
        let upstream_remote = self
            .git_text(&checkout, ["config", "--get", remote_key.as_str()])
            .await
            .with_context(|| format!("resolve the configured upstream remote for {branch}"))?;
        require_safe_git_name("upstream remote", &upstream_remote)?;
        if upstream_remote == "." {
            bail!("the Kit branch upstream must use the canonical GitHub remote");
        }
        let upstream_ref = self
            .git_text(&checkout, ["config", "--get", merge_key.as_str()])
            .await
            .with_context(|| format!("resolve the configured upstream ref for {branch}"))?;
        if !upstream_ref.starts_with("refs/heads/") {
            bail!("the Kit branch upstream ref must be under refs/heads; found {upstream_ref}");
        }
        require_safe_git_name("upstream ref", &upstream_ref)?;
        self.git_bytes(&checkout, ["check-ref-format", upstream_ref.as_str()]).await.with_context(
            || format!("validate the configured upstream branch ref {upstream_ref}"),
        )?;
        let remote_url = self
            .git_text(&checkout, ["remote", "get-url", upstream_remote.as_str()])
            .await
            .with_context(|| format!("resolve remote URL for {upstream_remote}"))?;
        if !canonical_remote_url(&remote_url) {
            bail!(
                "the registered Kit upstream must be xtava/kit on GitHub; {upstream_remote} resolves to a different repository"
            );
        }

        Ok(SourceRegistrationReceipt { checkout, branch, upstream_remote, upstream_ref })
    }

    fn load_registration(&self) -> Result<SourceRegistration> {
        let bytes = fs::read(&self.state_path).with_context(|| {
            format!(
                "read Kit source registration at {}; rerun ./install.sh from the canonical Kit checkout",
                self.state_path.display()
            )
        })?;
        let registration: SourceRegistration =
            serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "parse Kit source registration at {}; rerun ./install.sh from the canonical Kit checkout",
                    self.state_path.display()
                )
            })?;
        if registration.schema_version != STATE_SCHEMA_VERSION {
            bail!(
                "unsupported Kit source registration schema {}; rerun ./install.sh from the canonical Kit checkout",
                registration.schema_version
            );
        }
        Ok(registration)
    }

    fn require_managed_invocation(&self) -> Result<()> {
        let managed_link = fs::symlink_metadata(&self.managed_executable).with_context(|| {
            format!(
                "inspect managed Kit executable {}; rerun ./install.sh first",
                self.managed_executable.display()
            )
        })?;
        if managed_link.file_type().is_symlink() || !managed_link.is_file() {
            bail!(
                "managed Kit executable {} must be a regular file, not a symlink",
                self.managed_executable.display()
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let expected_uid = unsafe { libc::geteuid() };
            if managed_link.uid() != expected_uid {
                bail!(
                    "managed Kit executable {} is owned by uid {}; expected uid {expected_uid}",
                    self.managed_executable.display(),
                    managed_link.uid()
                );
            }
        }
        let running = fs::canonicalize(&self.current_executable).with_context(|| {
            format!("resolve running Kit executable {}", self.current_executable.display())
        })?;
        let managed = fs::canonicalize(&self.managed_executable).with_context(|| {
            format!(
                "resolve managed Kit executable {}; rerun ./install.sh first",
                self.managed_executable.display()
            )
        })?;
        if running != managed {
            bail!(
                "Kit is running from {}, but updates replace {}; run the managed Kit binary or rerun ./install.sh",
                running.display(),
                managed.display()
            );
        }
        Ok(())
    }

    async fn require_supported_checkout(&self, checkout: &Path) -> Result<()> {
        let unmerged = self.git_bytes(checkout, ["ls-files", "-u", "-z"]).await?;
        if !unmerged.is_empty() {
            bail!("the registered Kit checkout has unmerged paths; resolve them before updating");
        }
        let index_flags = self.git_bytes(checkout, ["ls-files", "-v", "-z"]).await?;
        let hidden_paths = parse_hidden_index_paths(&index_flags);
        if !hidden_paths.is_empty() {
            let paths = hidden_paths
                .iter()
                .take(12)
                .map(|path| String::from_utf8_lossy(path).into_owned())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "the registered Kit checkout hides local bytes with assume-unchanged or skip-worktree flags ({paths}); clear those flags before updating"
            );
        }
        let sparse =
            self.git_allow_failure(checkout, ["config", "--bool", "core.sparseCheckout"]).await?;
        match sparse.exit {
            LeaderExitObservation::Observed(LeaderExit::Code(1)) => {}
            LeaderExitObservation::Observed(LeaderExit::Code(0)) => {
                if trim_ascii(&sparse.stdout).eq_ignore_ascii_case(b"true") {
                    bail!("sparse Kit checkouts are not supported by kit update");
                }
            }
            _ => return Err(command_failure("inspect sparse-checkout state", &sparse)),
        }
        Ok(())
    }

    async fn require_console_idle(&self, checkout: &Path) -> Result<bool> {
        let status = self
            .run_json_command(
                "inspect Console before updating Kit",
                &self.managed_executable,
                os_args(["--json", "console", "status"]),
                checkout,
                COMMAND_TIMEOUT,
            )
            .await
            .context(
                "could not prove that Console has zero active sessions; no source or binary was changed",
            )?;
        let state = status.get("state").and_then(Value::as_str).unwrap_or("unknown");
        let restart = match state {
            "ready" => {
                let sessions = status
                    .get("sessions")
                    .and_then(Value::as_u64)
                    .context("Console ready status omitted its session count")?;
                if sessions != 0 {
                    bail!(
                        "Console has {sessions} active session(s); close them before running kit update"
                    );
                }
                true
            }
            "activation-deferred" => {
                let sessions = status.get("sessions").and_then(Value::as_u64).unwrap_or(1);
                bail!(
                    "Console has {sessions} active session(s); close them before running kit update"
                );
            }
            "not-installed" | "stopped" | "socket-stale" => false,
            "socket-missing" => true,
            _ => {
                bail!("Console status is {state}; could not prove that replacing its agent is safe")
            }
        };
        Ok(restart)
    }

    async fn fetch_registered_upstream(&self, registration: &SourceRegistration) -> Result<()> {
        self.git_bytes(
            &registration.checkout,
            [
                "fetch",
                "--no-tags",
                "--no-recurse-submodules",
                "--",
                registration.upstream_remote.as_str(),
                registration.upstream_ref.as_str(),
            ],
        )
        .await
        .with_context(|| {
            format!(
                "fetch {}/{}; no source or binary was changed",
                registration.upstream_remote, registration.upstream_ref
            )
        })?;
        Ok(())
    }

    async fn head_revision(&self, checkout: &Path) -> Result<String> {
        self.git_revision(checkout, "HEAD^{commit}")
            .await
            .context("resolve the Kit checkout revision")
    }

    async fn git_revision(&self, checkout: &Path, revision: &str) -> Result<String> {
        let value = self.git_text(checkout, ["rev-parse", "--verify", revision]).await?;
        if !valid_revision(&value) {
            bail!("Git returned a malformed revision for {revision}");
        }
        Ok(value.to_ascii_lowercase())
    }

    async fn status_snapshot(&self, checkout: &Path) -> Result<Vec<u8>> {
        self.git_bytes(checkout, ["status", "--porcelain=v2", "-z", "--untracked-files=all"])
            .await
            .context("snapshot local Kit checkout changes")
    }

    async fn dirty_source_paths(&self, checkout: &Path) -> Result<BTreeSet<Vec<u8>>> {
        let mut dirty = BTreeSet::new();
        for prefix in [
            vec!["diff", "--no-ext-diff", "--no-renames", "--name-only", "-z", "--"],
            vec!["diff", "--cached", "--no-ext-diff", "--no-renames", "--name-only", "-z", "--"],
            vec!["ls-files", "--others", "--exclude-standard", "-z", "--"],
        ] {
            let mut arguments = prefix.into_iter().map(OsString::from).collect::<Vec<_>>();
            arguments.extend(SOURCE_IDENTITY_PATHS.iter().map(OsString::from));
            let output = self.git_bytes_owned(checkout, arguments).await?;
            dirty.extend(parse_nul_paths(&output));
        }
        Ok(dirty)
    }

    async fn classify_ancestry(
        &self,
        checkout: &Path,
        local: &str,
        remote: &str,
    ) -> Result<Ancestry> {
        if local == remote {
            return Ok(Ancestry::Equal);
        }
        let local_is_ancestor = self.is_ancestor(checkout, local, remote).await?;
        let remote_is_ancestor = self.is_ancestor(checkout, remote, local).await?;
        Ok(match (local_is_ancestor, remote_is_ancestor) {
            (true, false) => Ancestry::RemoteAhead,
            (false, true) => Ancestry::LocalAhead,
            _ => Ancestry::Diverged,
        })
    }

    async fn is_ancestor(&self, checkout: &Path, ancestor: &str, tip: &str) -> Result<bool> {
        let output = self
            .git_allow_failure(checkout, ["merge-base", "--is-ancestor", "--", ancestor, tip])
            .await?;
        match output.exit {
            LeaderExitObservation::Observed(LeaderExit::Code(0)) => Ok(true),
            LeaderExitObservation::Observed(LeaderExit::Code(1)) => Ok(false),
            _ => Err(command_failure("inspect Kit revision ancestry", &output)),
        }
    }

    async fn require_disjoint_fast_forward(
        &self,
        checkout: &Path,
        local: &str,
        remote: &str,
    ) -> Result<()> {
        let mut dirty = BTreeSet::new();
        for output in [
            self.git_bytes(
                checkout,
                ["diff", "--no-ext-diff", "--no-renames", "--name-only", "-z", "--"],
            )
            .await?,
            self.git_bytes(
                checkout,
                ["diff", "--cached", "--no-ext-diff", "--no-renames", "--name-only", "-z", "--"],
            )
            .await?,
            self.git_bytes(checkout, ["ls-files", "--others", "--exclude-standard", "-z", "--"])
                .await?,
        ] {
            dirty.extend(parse_nul_paths(&output));
        }
        if dirty.is_empty() {
            return Ok(());
        }
        let range = format!("{local}..{remote}");
        let incoming = self
            .git_bytes(
                checkout,
                [
                    "diff",
                    "--no-ext-diff",
                    "--no-renames",
                    "--name-only",
                    "-z",
                    range.as_str(),
                    "--",
                ],
            )
            .await?;
        let incoming = parse_nul_paths(&incoming);
        let conflicts = dirty.intersection(&incoming).take(12).collect::<Vec<_>>();
        if !conflicts.is_empty() {
            let paths = conflicts
                .iter()
                .map(|path| String::from_utf8_lossy(path).into_owned())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "upstream changes overlap local Kit work ({paths}); no merge or install was attempted"
            );
        }
        Ok(())
    }

    async fn fast_forward(&self, checkout: &Path, revision: &str) -> Result<()> {
        self.git_bytes(
            checkout,
            [
                "merge",
                "--ff-only",
                "--no-autostash",
                "--no-overwrite-ignore",
                "--no-edit",
                "--",
                revision,
            ],
        )
        .await
        .with_context(|| format!("fast-forward the Kit checkout to {revision}"))?;
        Ok(())
    }

    async fn run_installer(
        &self,
        checkout: &Path,
        show_progress: bool,
        install_lock: &InstallLock,
    ) -> Result<()> {
        let installer = checkout.join("install.sh");
        install_lock.handoff_started.store(true, Ordering::Release);
        let environment = BTreeMap::from([(
            OsString::from("KIT_INSTALL_LOCK_OWNER_PID"),
            OsString::from(install_lock.owner_pid.to_string()),
        )]);
        let output = if show_progress {
            self.run_command(
                "install updated Kit source",
                installer.as_os_str(),
                Vec::new(),
                checkout,
                environment.clone(),
                OutputPolicy::Inherit,
                INSTALL_TIMEOUT,
            )
            .await
        } else {
            self.run_command(
                "install updated Kit source",
                installer.as_os_str(),
                Vec::new(),
                checkout,
                environment,
                capture_policy(),
                INSTALL_TIMEOUT,
            )
            .await
        };
        install_lock.handoff_completed.store(true, Ordering::Release);
        let output = output?;
        require_success("install updated Kit source", &output)
    }

    async fn installed_identity(&self) -> Result<BuildIdentity> {
        let output = self
            .run_json_command(
                "verify the installed Kit source identity",
                &self.managed_executable,
                os_args(["--json", "update", "__build-identity"]),
                &self.state_directory,
                COMMAND_TIMEOUT,
            )
            .await?;
        serde_json::from_value(output).context("decode the installed Kit source identity")
    }

    async fn reconcile_console(&self, checkout: &Path, restart: bool) -> Result<Value> {
        self.run_json_command(
            if restart {
                "restart Console after updating Kit"
            } else {
                "verify Console remains inactive after updating Kit"
            },
            &self.managed_executable,
            if restart {
                os_args(["--json", "console", "restart"])
            } else {
                os_args(["--json", "console", "status"])
            },
            checkout,
            COMMAND_TIMEOUT,
        )
        .await
    }

    async fn run_json_command(
        &self,
        label: &str,
        program: &Path,
        arguments: Vec<OsString>,
        working_directory: &Path,
        timeout: Duration,
    ) -> Result<Value> {
        let output = self
            .run_command(
                label,
                program.as_os_str(),
                arguments,
                working_directory,
                BTreeMap::new(),
                capture_policy(),
                timeout,
            )
            .await?;
        require_success(label, &output)?;
        serde_json::from_slice(trim_ascii(&output.stdout))
            .with_context(|| format!("decode JSON from {label}"))
    }

    async fn git_text<const N: usize>(
        &self,
        checkout: &Path,
        arguments: [&str; N],
    ) -> Result<String> {
        let bytes = self.git_bytes(checkout, arguments).await?;
        let value =
            std::str::from_utf8(trim_ascii(&bytes)).context("Git returned non-UTF-8 metadata")?;
        Ok(value.to_owned())
    }

    async fn git_bytes<const N: usize>(
        &self,
        checkout: &Path,
        arguments: [&str; N],
    ) -> Result<Vec<u8>> {
        let label = git_label(arguments.first().copied().unwrap_or("command"));
        let output = self.git_allow_failure(checkout, arguments).await?;
        require_success(&label, &output)?;
        Ok(output.stdout)
    }

    async fn git_bytes_owned(&self, checkout: &Path, arguments: Vec<OsString>) -> Result<Vec<u8>> {
        let label = git_label(
            arguments
                .first()
                .map(|argument| argument.to_string_lossy())
                .as_deref()
                .unwrap_or("command"),
        );
        let output = self.git_allow_failure_owned(checkout, arguments, &label).await?;
        require_success(&label, &output)?;
        Ok(output.stdout)
    }

    async fn git_allow_failure<const N: usize>(
        &self,
        checkout: &Path,
        arguments: [&str; N],
    ) -> Result<CommandOutput> {
        let label = git_label(arguments.first().copied().unwrap_or("command"));
        let arguments = arguments.into_iter().map(OsString::from).collect();
        self.git_allow_failure_owned(checkout, arguments, &label).await
    }

    async fn git_allow_failure_owned(
        &self,
        checkout: &Path,
        arguments: Vec<OsString>,
        label: &str,
    ) -> Result<CommandOutput> {
        let mut command_arguments = vec![
            OsString::from("-c"),
            OsString::from("core.hooksPath=/dev/null"),
            OsString::from("-c"),
            OsString::from("core.fsmonitor=false"),
            OsString::from("-c"),
            OsString::from("submodule.recurse=false"),
            OsString::from("-C"),
            checkout.as_os_str().to_owned(),
        ];
        command_arguments.extend(arguments);
        self.run_command(
            &label,
            &self.git_program,
            command_arguments,
            checkout,
            git_environment(),
            capture_policy(),
            GIT_TIMEOUT,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_command(
        &self,
        label: &str,
        program: &OsStr,
        arguments: Vec<OsString>,
        working_directory: &Path,
        environment_overrides: BTreeMap<OsString, OsString>,
        output_policy: OutputPolicy,
        timeout: Duration,
    ) -> Result<CommandOutput> {
        let environment = ProcessEnvironment::new(
            EnvironmentBase::Inherit,
            environment_overrides,
            BTreeSet::new(),
        )?;
        let command = CommandSpec::new(
            program.to_owned(),
            arguments,
            working_directory.to_path_buf(),
            environment,
            ProcessLabel::new(label.to_owned())?,
        )?;
        let spec = ProcessSpec::new(
            command,
            InputPolicy::Closed,
            output_policy,
            output_policy,
            ContainmentRequirement::ExplicitProcessGroup,
            ProcessDeadline::After(timeout),
            TerminationPolicy::new(TERMINATION_GRACE),
        );
        let report = self
            .processes
            .spawn(spec)
            .await
            .with_context(|| format!("start {label}"))?
            .session
            .wait()
            .await
            .map_err(|failure| anyhow!("{label} supervision failed: {:?}", failure.failure))?;
        Ok(CommandOutput {
            stdout: output_bytes(report.stdout, label, "stdout")?,
            stderr: output_bytes(report.stderr, label, "stderr")?,
            completion: report.completion,
            exit: report.leader_exit,
        })
    }
}

impl InstallLock {
    fn acquire(managed_executable: &Path) -> Result<Self> {
        let managed_directory =
            managed_executable.parent().context("resolve the managed Kit executable directory")?;
        fs::create_dir_all(managed_directory).with_context(|| {
            format!("create managed Kit directory {}", managed_directory.display())
        })?;
        let directory = managed_directory.join(".kit-install.lock");
        let mut reclaimed = false;
        loop {
            match fs::create_dir(&directory) {
                Ok(()) => break,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && !reclaimed => {
                    let Some(owner_pid) = stale_install_lock_owner(&directory)? else {
                        let owner = fs::read_to_string(directory.join("pid"))
                            .unwrap_or_else(|_| "unknown".to_owned());
                        bail!(
                            "another Kit install or update owns {} (pid {}); wait for it to finish",
                            directory.display(),
                            owner.trim()
                        );
                    };
                    reclaim_install_lock(&directory, owner_pid)?;
                    reclaimed = true;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    bail!("another Kit install or update acquired {}", directory.display());
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("create Kit install lock {}", directory.display())
                    });
                }
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(error) = fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)) {
                let _ = fs::remove_dir(&directory);
                return Err(error)
                    .with_context(|| format!("secure Kit install lock {}", directory.display()));
            }
        }

        let owner_pid = std::process::id();
        let owner_file = directory.join("pid");
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        if let Err(error) = options.open(&owner_file).and_then(|mut file| {
            std::io::Write::write_all(&mut file, owner_pid.to_string().as_bytes())
        }) {
            let _ = fs::remove_file(&owner_file);
            let _ = fs::remove_dir(&directory);
            return Err(error)
                .with_context(|| format!("write Kit install lock {}", owner_file.display()));
        }
        Ok(Self {
            directory,
            owner_file,
            owner_pid,
            handoff_started: AtomicBool::new(false),
            handoff_completed: AtomicBool::new(false),
        })
    }
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        if self.handoff_started.load(Ordering::Acquire)
            && !self.handoff_completed.load(Ordering::Acquire)
        {
            return;
        }
        let owned = fs::read_to_string(&self.owner_file)
            .is_ok_and(|owner| owner.trim() == self.owner_pid.to_string());
        if !owned {
            return;
        }
        let _ = fs::remove_file(&self.owner_file);
        let _ = fs::remove_dir(&self.directory);
    }
}

fn stale_install_lock_owner(directory: &Path) -> Result<Option<u32>> {
    let metadata = fs::symlink_metadata(directory)
        .with_context(|| format!("inspect Kit install lock {}", directory.display()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("Kit install lock {} is not a regular directory", directory.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let expected_uid = unsafe { libc::geteuid() };
        if metadata.uid() != expected_uid {
            bail!(
                "Kit install lock {} is owned by uid {}; expected uid {expected_uid}",
                directory.display(),
                metadata.uid()
            );
        }
    }

    let owner_file = directory.join("pid");
    if let Ok(owner_metadata) = fs::symlink_metadata(&owner_file) {
        if !owner_metadata.is_file() || owner_metadata.file_type().is_symlink() {
            bail!("Kit install lock owner {} is not a regular file", owner_file.display());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let expected_uid = unsafe { libc::geteuid() };
            if owner_metadata.uid() != expected_uid {
                bail!(
                    "Kit install lock owner {} is owned by uid {}; expected uid {expected_uid}",
                    owner_file.display(),
                    owner_metadata.uid()
                );
            }
        }
    }
    match fs::read_to_string(&owner_file) {
        Ok(owner) => match owner.trim().parse::<u32>() {
            Ok(owner_pid) => Ok((!process_is_alive(owner_pid)).then_some(owner_pid)),
            Err(_) => Ok(lock_path_old_enough(&owner_file).then_some(0)),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(lock_path_old_enough(directory).then_some(0))
        }
        Err(error) => {
            Err(error).with_context(|| format!("read Kit install lock {}", owner_file.display()))
        }
    }
}

fn process_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = i32::try_from(pid) else {
            return true;
        };
        let result = unsafe { libc::kill(pid, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

fn reclaim_install_lock(directory: &Path, expected_owner: u32) -> Result<()> {
    let owner_file = directory.join("pid");
    if expected_owner == 0 {
        match fs::symlink_metadata(&owner_file) {
            Ok(metadata) => {
                if !metadata.is_file()
                    || metadata.file_type().is_symlink()
                    || !lock_path_old_enough(&owner_file)
                {
                    bail!(
                        "Kit install lock owner changed while reclaiming {}",
                        directory.display()
                    );
                }
                fs::remove_file(&owner_file).with_context(|| {
                    format!("remove stale Kit install lock {}", owner_file.display())
                })?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !lock_path_old_enough(directory) {
                    bail!("Kit install lock changed while reclaiming {}", directory.display());
                }
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect Kit install lock {}", owner_file.display()));
            }
        }
    } else {
        let current = fs::read_to_string(&owner_file)
            .with_context(|| format!("recheck Kit install lock {}", owner_file.display()))?;
        if current.trim() != expected_owner.to_string() {
            bail!("Kit install lock owner changed while reclaiming {}", directory.display());
        }
        fs::remove_file(&owner_file)
            .with_context(|| format!("remove stale Kit install lock {}", owner_file.display()))?;
    }
    fs::remove_dir(directory)
        .with_context(|| format!("remove stale Kit install lock {}", directory.display()))
}

fn lock_path_old_enough(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age >= Duration::from_secs(30))
}

fn require_registration_unchanged(
    registration: &SourceRegistration,
    inspected: &SourceRegistrationReceipt,
) -> Result<()> {
    if registration.checkout != inspected.checkout
        || registration.branch != inspected.branch
        || registration.upstream_remote != inspected.upstream_remote
        || registration.upstream_ref != inspected.upstream_ref
    {
        bail!(
            "the registered Kit checkout, branch, or upstream changed; rerun ./install.sh from the intended canonical checkout before updating"
        );
    }
    Ok(())
}

#[derive(Deserialize)]
struct Manifest {
    package: ManifestPackage,
}

#[derive(Deserialize)]
struct ManifestPackage {
    name: String,
    repository: Option<String>,
}

fn require_kit_manifest(checkout: &Path) -> Result<()> {
    let path = checkout.join("Cargo.toml");
    let contents = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let manifest: Manifest =
        toml::from_str(&contents).with_context(|| format!("parse {}", path.display()))?;
    if manifest.package.name != "kit" {
        bail!("{} is not the Kit package", checkout.display());
    }
    let repository = manifest
        .package
        .repository
        .as_deref()
        .context("Kit Cargo.toml does not declare its canonical repository")?;
    if !canonical_remote_url(repository) {
        bail!("Kit Cargo.toml does not identify the canonical xtava/kit repository");
    }
    Ok(())
}

fn require_installer(checkout: &Path) -> Result<()> {
    let path = checkout.join("install.sh");
    let metadata = fs::metadata(&path).with_context(|| format!("inspect {}", path.display()))?;
    if !metadata.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            bail!("{} is not executable", path.display());
        }
    }
    Ok(())
}

fn canonical_remote_url(value: &str) -> bool {
    let value = value.trim_end_matches('/');
    let value = value.strip_suffix(".git").unwrap_or(value);
    matches!(
        value,
        "https://github.com/xtava/kit"
            | "ssh://git@github.com/xtava/kit"
            | "git@github.com:xtava/kit"
    )
}

fn require_safe_git_name(kind: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.starts_with('-')
        || value.chars().any(char::is_control)
        || value.contains(char::is_whitespace)
    {
        bail!("the Kit {kind} contains unsupported characters");
    }
    Ok(())
}

fn valid_revision(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn parse_nul_paths(bytes: &[u8]) -> BTreeSet<Vec<u8>> {
    bytes.split(|byte| *byte == 0).filter(|path| !path.is_empty()).map(<[u8]>::to_vec).collect()
}

fn parse_hidden_index_paths(bytes: &[u8]) -> BTreeSet<Vec<u8>> {
    bytes
        .split(|byte| *byte == 0)
        .filter_map(|entry| {
            let (&tag, path) = entry.split_first()?;
            let path = path.strip_prefix(b" ")?;
            (tag == b'S' || tag.is_ascii_lowercase()).then(|| path.to_vec())
        })
        .collect()
}

fn source_content_fingerprint(checkout: &Path, paths: &BTreeSet<Vec<u8>>) -> Result<String> {
    let mut hasher = Sha256::new();
    for raw_path in paths {
        let relative = raw_repository_path(raw_path)?;
        let path = checkout.join(&relative);
        hasher.update((raw_path.len() as u64).to_le_bytes());
        hasher.update(raw_path);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                hasher.update(b"missing");
                continue;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("inspect {}", path.display()));
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            hasher.update(metadata.mode().to_le_bytes());
        }
        if metadata.file_type().is_symlink() {
            bail!(
                "dirty source entry {} is a symlink; restore or commit it before updating Kit",
                path.display()
            );
        } else if metadata.is_file() {
            hasher.update(b"file");
            let file = File::open(&path).with_context(|| format!("open {}", path.display()))?;
            let mut reader = BufReader::new(file);
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let count =
                    reader.read(&mut buffer).with_context(|| format!("read {}", path.display()))?;
                if count == 0 {
                    break;
                }
                hasher.update(&buffer[..count]);
            }
        } else if metadata.is_dir() {
            bail!(
                "dirty source entry {} is a directory or gitlink; restore or commit it before updating Kit",
                path.display()
            );
        } else {
            bail!("unsupported source entry {}", path.display());
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn raw_repository_path(raw: &[u8]) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(PathBuf::from(OsString::from_vec(raw.to_vec())))
    }
    #[cfg(not(unix))]
    {
        Ok(PathBuf::from(
            String::from_utf8(raw.to_vec()).context("Git returned a non-UTF-8 source path")?,
        ))
    }
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes.iter().position(|byte| !byte.is_ascii_whitespace()).unwrap_or(bytes.len());
    let end =
        bytes.iter().rposition(|byte| !byte.is_ascii_whitespace()).map_or(start, |index| index + 1);
    &bytes[start..end]
}

fn capture_policy() -> OutputPolicy {
    OutputPolicy::Capture(CapturePolicy::new(CAPTURE_BYTES, CaptureOverflow::FailAndTerminate))
}

fn git_environment() -> BTreeMap<OsString, OsString> {
    BTreeMap::from([
        (OsString::from("GIT_TERMINAL_PROMPT"), OsString::from("0")),
        (OsString::from("GCM_INTERACTIVE"), OsString::from("Never")),
        (OsString::from("GIT_CONFIG_NOSYSTEM"), OsString::from("1")),
        (OsString::from("GIT_CONFIG_GLOBAL"), OsString::from("/dev/null")),
        (OsString::from("GIT_SSH_COMMAND"), OsString::from("ssh -oBatchMode=yes")),
    ])
}

fn git_label(command: &str) -> String {
    format!("run Git {command} for Kit update")
}

fn require_success(label: &str, output: &CommandOutput) -> Result<()> {
    if output.completion != CompletionCause::Natural {
        bail!("{label} did not finish naturally ({:?})", output.completion);
    }
    if output.exit != LeaderExitObservation::Observed(LeaderExit::Code(0)) {
        return Err(command_failure(label, output));
    }
    Ok(())
}

fn command_failure(label: &str, output: &CommandOutput) -> anyhow::Error {
    let detail = String::from_utf8_lossy(trim_ascii(&output.stderr));
    if detail.is_empty() {
        anyhow!("{label} failed ({:?})", output.exit)
    } else {
        anyhow!("{label} failed: {detail}")
    }
}

fn output_bytes(output: OutputReport, label: &str, stream: &str) -> Result<Vec<u8>> {
    match output {
        OutputReport::Captured(capture) => Ok(capture.bytes.into_vec()),
        OutputReport::Inherited | OutputReport::Discarded => Ok(Vec::new()),
        _ => bail!("{label} {stream} used an unsupported output policy"),
    }
}

fn os_args<const N: usize>(values: [&str; N]) -> Vec<OsString> {
    values.into_iter().map(OsString::from).collect()
}

fn sha256_file(path: &Path) -> Result<String> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer).with_context(|| format!("read {}", path.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_remote_accepts_only_the_kit_repository() {
        for accepted in [
            "https://github.com/xtava/kit",
            "https://github.com/xtava/kit.git",
            "https://github.com/xtava/kit.git/",
            "ssh://git@github.com/xtava/kit.git",
            "git@github.com:xtava/kit.git",
        ] {
            assert!(canonical_remote_url(accepted), "{accepted}");
        }
        for rejected in [
            "https://github.com/someone/kit.git",
            "https://token@github.com/xtava/kit.git",
            "http://github.com/xtava/kit.git",
            "/tmp/kit",
            "git@evil.example:xtava/kit.git",
        ] {
            assert!(!canonical_remote_url(rejected), "{rejected}");
        }
    }

    #[test]
    fn source_state_rejects_unknown_fields_and_wrong_schema_is_visible() {
        let unknown = br#"{
            "schemaVersion": 1,
            "checkout": "/tmp/kit",
            "branch": "master",
            "upstreamRemote": "origin",
            "upstreamRef": "refs/heads/master",
            "fallback": true
        }"#;
        assert!(serde_json::from_slice::<SourceRegistration>(unknown).is_err());

        let old = br#"{
            "schemaVersion": 0,
            "checkout": "/tmp/kit",
            "branch": "master",
            "upstreamRemote": "origin",
            "upstreamRef": "refs/heads/master"
        }"#;
        let decoded: SourceRegistration = serde_json::from_slice(old).unwrap();
        assert_eq!(decoded.schema_version, 0);
    }

    #[test]
    fn nul_path_parser_preserves_raw_bytes_and_deduplicates() {
        let paths = parse_nul_paths(b"src/main.rs\0docs/\xff.md\0src/main.rs\0");
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(b"src/main.rs".as_slice()));
        assert!(paths.contains(b"docs/\xff.md".as_slice()));
    }

    #[test]
    fn hidden_index_parser_rejects_skip_worktree_and_assume_unchanged() {
        let paths =
            parse_hidden_index_paths(b"H normal.rs\0S hidden-by-skip.rs\0h hidden-by-assume.rs\0");
        assert_eq!(
            paths,
            BTreeSet::from([b"hidden-by-assume.rs".to_vec(), b"hidden-by-skip.rs".to_vec(),])
        );
    }

    #[test]
    fn install_lock_excludes_a_second_owner_and_releases_cleanly() {
        let root = std::env::temp_dir().join(format!("kit-update-lock-{}", uuid::Uuid::new_v4()));
        let managed = root.join("kit");
        let first = InstallLock::acquire(&managed).expect("acquire first install lock");
        let error =
            InstallLock::acquire(&managed).err().expect("second install lock must be refused");
        assert!(error.to_string().contains("another Kit install or update"));
        drop(first);
        let second = InstallLock::acquire(&managed).expect("reacquire released install lock");
        drop(second);
        std::fs::remove_dir(&root).expect("remove test directory");
    }

    #[test]
    fn install_lock_reclaims_a_dead_owner() {
        let root =
            std::env::temp_dir().join(format!("kit-stale-update-lock-{}", uuid::Uuid::new_v4()));
        let lock = root.join(".kit-install.lock");
        std::fs::create_dir_all(&lock).expect("create stale install lock");
        std::fs::write(lock.join("pid"), i32::MAX.to_string()).expect("write dead owner");
        let acquired = InstallLock::acquire(&root.join("kit")).expect("reclaim stale install lock");
        drop(acquired);
        std::fs::remove_dir(root).expect("remove stale lock test directory");
    }

    #[test]
    fn source_fingerprint_detects_edits_to_an_already_dirty_path() {
        let root =
            std::env::temp_dir().join(format!("kit-source-fingerprint-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).expect("create fingerprint directory");
        let path = root.join("source.rs");
        std::fs::write(&path, b"first").expect("write first source");
        let paths = BTreeSet::from([b"source.rs".to_vec()]);
        let first = source_content_fingerprint(&root, &paths).expect("hash first source");
        std::fs::write(&path, b"second").expect("write second source");
        let second = source_content_fingerprint(&root, &paths).expect("hash second source");
        assert_ne!(first, second);
        std::fs::remove_file(path).expect("remove source");
        std::fs::remove_dir(root).expect("remove fingerprint directory");
    }

    #[test]
    fn source_fingerprint_represents_a_deleted_path() {
        let root =
            std::env::temp_dir().join(format!("kit-missing-fingerprint-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).expect("create fingerprint directory");
        let paths = BTreeSet::from([b"deleted.rs".to_vec()]);
        let missing = source_content_fingerprint(&root, &paths).expect("hash missing source");
        std::fs::write(root.join("deleted.rs"), b"restored").expect("restore source");
        let restored = source_content_fingerprint(&root, &paths).expect("hash restored source");
        assert_ne!(missing, restored);
        std::fs::remove_file(root.join("deleted.rs")).expect("remove restored source");
        std::fs::remove_dir(root).expect("remove fingerprint directory");
    }

    #[test]
    fn revision_validation_requires_one_exact_object_id() {
        assert!(valid_revision("0123456789abcdef0123456789abcdef01234567"));
        assert!(!valid_revision("01234567"));
        assert!(!valid_revision("g123456789abcdef0123456789abcdef01234567"));
    }

    #[test]
    fn build_identity_exposes_compile_time_source_provenance() {
        let identity = build_identity();
        assert_eq!(identity.product, "kit");
        assert_eq!(identity.version, env!("CARGO_PKG_VERSION"));
        assert!(valid_revision(&identity.source_revision));
        assert!(identity.source_dirty.is_some());
    }
}
