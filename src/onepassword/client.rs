use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fmt,
    fs::OpenOptions,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{de::DeserializeOwned, de::Error as _, Deserialize, Deserializer, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    task::JoinHandle,
};
use zeroize::{Zeroize, Zeroizing};

use crate::framework::process::{
    CommandSpec, CommandSpecError, EnvironmentBase, ProcessEnvironment, ProcessEnvironmentError,
    ProcessLabel,
};

use super::sensitive::{SecretBytes, SecretBytesError, SensitiveBuffer, MAX_SECRET_BYTES};

const SECRET_READ_CHUNK_BYTES: usize = 1024;
const RUN_OUTPUT_CHUNK_BYTES: usize = 8 * 1024;
const NO_MASKING_ENV: &str = "OP_RUN_NO_MASKING";
const ENV_FILE_ATTEMPTS: usize = 32;
static NEXT_ENV_FILE: AtomicU64 = AtomicU64::new(0);

/// One validated 1Password secret reference.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SecretReference(String);

impl SecretReference {
    pub fn new(value: String) -> Result<Self, SecretReferenceError> {
        let Some(path) = value.strip_prefix("op://") else {
            return Err(SecretReferenceError);
        };
        let valid_path = path.split('/').count() >= 3
            && path.split('/').all(|segment| !segment.is_empty())
            && !value.chars().any(char::is_control);
        if !valid_path {
            return Err(SecretReferenceError);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for SecretReference {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(D::Error::custom)
    }
}

impl fmt::Debug for SecretReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretReference(<op:// reference>)")
    }
}

impl fmt::Display for SecretReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("expected an op:// reference with vault, item, and field components")]
pub struct SecretReferenceError;

#[derive(Clone, Copy)]
pub(crate) enum StderrPolicy {
    CaptureSanitized,
    Discard,
}

impl StderrPolicy {
    fn stdio(self) -> Stdio {
        match self {
            Self::CaptureSanitized => Stdio::piped(),
            Self::Discard => Stdio::null(),
        }
    }
}

#[derive(Clone)]
pub struct OpClient {
    executable: PathBuf,
}

impl OpClient {
    pub fn new() -> Self {
        Self { executable: PathBuf::from("op") }
    }

    #[cfg(test)]
    pub(crate) fn with_executable(executable: PathBuf) -> Self {
        Self { executable }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        command.kill_on_drop(true);
        command
    }

    fn operation_command<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = self.command();
        command.args(args).arg("--no-color");
        command
    }

    pub async fn version(&self) -> Result<(), OpError> {
        let output = self
            .command()
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|source| map_spawn_error("check version", source))?;
        check_status(
            "check version",
            output.status.code(),
            &output.stderr,
            StderrPolicy::CaptureSanitized,
        )
    }

    pub async fn preflight_reference(&self, reference: &SecretReference) -> Result<(), OpError> {
        let status = self
            .operation_command(["read", reference.as_str(), "--no-newline"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map_err(|source| map_spawn_error("preflight secret reference", source))?;
        if status.success() {
            Ok(())
        } else {
            Err(OpError::ReferenceFailed { reference: reference.clone(), status: status.code() })
        }
    }

    pub async fn read_reference(
        &self,
        reference: &SecretReference,
    ) -> Result<SecretBytes, OpError> {
        self.secret("read secret reference", ["read", reference.as_str(), "--no-newline"]).await
    }

    pub async fn read_reference_for_account(
        &self,
        reference: &SecretReference,
        account: &str,
    ) -> Result<SecretBytes, OpError> {
        self.secret(
            "read secret reference",
            ["read", reference.as_str(), "--account", account, "--no-newline"],
        )
        .await
    }

    pub fn prepare_run(
        &self,
        references: &BTreeMap<String, SecretReference>,
        program: OsString,
        arguments: Vec<OsString>,
    ) -> Result<PreparedOpRun, OpError> {
        PreparedOpRun::create(self.executable.clone(), references, program, arguments)
    }

    pub async fn run_operation(
        &self,
        references: &BTreeMap<String, SecretReference>,
        program: &str,
        arguments: &[String],
    ) -> Result<ExitStatus, OpError> {
        let prepared = self.prepare_run(
            references,
            OsString::from(program),
            arguments.iter().map(OsString::from).collect(),
        )?;
        let mut child = prepared
            .command()
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| map_spawn_error("run operation", source))?;
        let stdout = child.stdout.take().ok_or(OpError::Io {
            operation: "run operation",
            source: io::Error::other("op run stdout was not piped"),
        })?;
        let stderr = child.stderr.take().ok_or(OpError::Io {
            operation: "run operation",
            source: io::Error::other("op run stderr was not piped"),
        })?;
        let stdout_task = tokio::spawn(forward_output(stdout, OutputDestination::Stdout));
        let stderr_task = tokio::spawn(forward_output(stderr, OutputDestination::Stderr));

        let status =
            child.wait().await.map_err(|source| OpError::Io { operation: "run operation", source });
        let stdout_result = finish_forward(stdout_task, "stream op run stdout").await;
        let stderr_result = finish_forward(stderr_task, "stream op run stderr").await;
        let status = status?;
        stdout_result?;
        stderr_result?;
        Ok(status)
    }

    pub(crate) async fn json<T, I, S>(&self, operation: &'static str, args: I) -> Result<T, OpError>
    where
        T: DeserializeOwned,
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self
            .operation_command(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|source| map_spawn_error(operation, source))?;
        check_status(
            operation,
            output.status.code(),
            &output.stderr,
            StderrPolicy::CaptureSanitized,
        )?;
        serde_json::from_slice(&output.stdout)
            .map_err(|source| OpError::InvalidJson { operation, source })
    }

    async fn secret<I, S>(&self, operation: &'static str, args: I) -> Result<SecretBytes, OpError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut child = self
            .operation_command(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| map_spawn_error(operation, source))?;
        let mut stdout = child.stdout.take().ok_or(OpError::Io {
            operation,
            source: io::Error::other("op stdout was not piped"),
        })?;
        let mut secret = SensitiveBuffer::new(MAX_SECRET_BYTES);
        let mut chunk = Zeroizing::new([0_u8; SECRET_READ_CHUNK_BYTES]);

        loop {
            let read = stdout
                .read(&mut chunk[..])
                .await
                .map_err(|source| OpError::Io { operation, source })?;
            if read == 0 {
                break;
            }
            if secret.try_extend(&chunk[..read]).is_err() {
                terminate(&mut child).await;
                return Err(OpError::ResponseTooLarge { operation, limit: MAX_SECRET_BYTES });
            }
            chunk[..read].zeroize();
        }
        drop(stdout);

        let status = child.wait().await.map_err(|source| OpError::Io { operation, source })?;
        check_status(operation, status.code(), &[], StderrPolicy::Discard)?;
        let secret = secret.into_secret().map_err(|error| match error {
            SecretBytesError::TooLarge => {
                OpError::ResponseTooLarge { operation, limit: MAX_SECRET_BYTES }
            }
            SecretBytesError::InvalidUtf8 => {
                OpError::InvalidResponse { operation, reason: "secret field was not valid UTF-8" }
            }
        })?;
        if secret.is_empty() {
            return Err(OpError::InvalidResponse { operation, reason: "secret field was empty" });
        }
        Ok(secret)
    }

    pub(crate) async fn status_with_stdin(
        &self,
        operation: &'static str,
        args: Vec<String>,
        body: Zeroizing<Vec<u8>>,
    ) -> Result<(), OpError> {
        let mut child = self
            .operation_command(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| map_spawn_error(operation, source))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or(OpError::Io { operation, source: io::Error::other("op stdin was not piped") })?;
        if let Err(source) = stdin.write_all(&body).await {
            terminate(&mut child).await;
            return Err(OpError::Io { operation, source });
        }
        drop(stdin);
        drop(body);
        let status = child.wait().await.map_err(|source| OpError::Io { operation, source })?;
        check_status(operation, status.code(), &[], StderrPolicy::Discard)
    }

    pub(crate) async fn status(
        &self,
        operation: &'static str,
        args: &[String],
        stderr_policy: StderrPolicy,
    ) -> Result<(), OpError> {
        let output = self
            .operation_command(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr_policy.stdio())
            .output()
            .await
            .map_err(|source| map_spawn_error(operation, source))?;
        check_status(operation, output.status.code(), &output.stderr, stderr_policy)
    }
}

impl Default for OpClient {
    fn default() -> Self {
        Self::new()
    }
}

/// A masking-on `op run` command plus the scoped reference file it needs.
pub struct PreparedOpRun {
    executable: PathBuf,
    arguments: Vec<OsString>,
    environment_removals: BTreeSet<OsString>,
    env_file: ScopedEnvFile,
}

impl PreparedOpRun {
    fn create(
        executable: PathBuf,
        references: &BTreeMap<String, SecretReference>,
        program: OsString,
        arguments: Vec<OsString>,
    ) -> Result<Self, OpError> {
        let env_file = ScopedEnvFile::create(references)?;
        let mut op_arguments = vec![
            OsString::from("run"),
            OsString::from(format!("--env-file={}", env_file.path().display())),
            OsString::from("--"),
            program,
        ];
        op_arguments.extend(arguments);
        let mut environment_removals =
            references.keys().map(OsString::from).collect::<BTreeSet<_>>();
        environment_removals.insert(OsString::from(NO_MASKING_ENV));
        Ok(Self { executable, arguments: op_arguments, environment_removals, env_file })
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        command.args(&self.arguments).kill_on_drop(true);
        for name in &self.environment_removals {
            command.env_remove(name);
        }
        command
    }

    pub fn command_spec(
        &self,
        working_directory: PathBuf,
        mut values: BTreeMap<OsString, OsString>,
        label: ProcessLabel,
    ) -> Result<CommandSpec, OpError> {
        values.retain(|name, _| {
            !self.environment_removals.contains(name)
                && !name.to_string_lossy().eq_ignore_ascii_case(NO_MASKING_ENV)
        });
        let environment = ProcessEnvironment::new(
            EnvironmentBase::Inherit,
            values,
            self.environment_removals.clone(),
        )
        .map_err(OpError::PrepareEnvironment)?;
        CommandSpec::new(
            self.executable.as_os_str().to_owned(),
            self.arguments.clone(),
            working_directory,
            environment,
            label,
        )
        .map_err(OpError::PrepareCommand)
    }

    #[cfg(test)]
    pub(crate) fn arguments(&self) -> &[OsString] {
        &self.arguments
    }
}

impl fmt::Debug for PreparedOpRun {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedOpRun")
            .field("env_file", &self.env_file.path())
            .field("argument_count", &self.arguments.len())
            .finish()
    }
}

struct ScopedEnvFile {
    path: PathBuf,
}

impl ScopedEnvFile {
    fn create(references: &BTreeMap<String, SecretReference>) -> Result<Self, OpError> {
        if references.keys().any(|name| !valid_environment_name(name)) {
            return Err(OpError::InvalidEnvironmentName);
        }
        if references.keys().any(|name| name.eq_ignore_ascii_case(NO_MASKING_ENV)) {
            return Err(OpError::MaskingOverride);
        }
        for _ in 0..ENV_FILE_ATTEMPTS {
            let nonce = NEXT_ENV_FILE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("kit-op-refs-{}-{nonce}.env", std::process::id()));
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = match options.open(&path) {
                Ok(file) => file,
                Err(source) if source.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(source) => return Err(OpError::CreateEnvFile { path, source }),
            };
            let write_result = (|| {
                for (name, reference) in references {
                    writeln!(file, "{name}={}", reference.as_str())?;
                }
                Ok::<(), io::Error>(())
            })();
            if let Err(source) = write_result {
                drop(file);
                let _ = std::fs::remove_file(&path);
                return Err(OpError::WriteEnvFile { path, source });
            }
            return Ok(Self { path });
        }
        Err(OpError::AllocateEnvFile)
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

impl Drop for ScopedEnvFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Debug, Error)]
pub enum OpError {
    #[error("the official 1Password CLI (`op`) is not installed or not in PATH")]
    NotInstalled,
    #[error("1Password could not resolve reference {reference} (status {status:?})")]
    ReferenceFailed { reference: SecretReference, status: Option<i32> },
    #[error("1Password CLI failed while attempting to {operation} (status {status:?}){detail}")]
    Failed { operation: &'static str, status: Option<i32>, detail: ErrorDetail },
    #[error("1Password CLI returned invalid JSON while attempting to {operation}: {source}")]
    InvalidJson {
        operation: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "1Password CLI returned an invalid response while attempting to {operation}: {reason}"
    )]
    InvalidResponse { operation: &'static str, reason: &'static str },
    #[error(
        "1Password CLI returned more than {limit} bytes while attempting to {operation}; refusing to retain it"
    )]
    ResponseTooLarge { operation: &'static str, limit: usize },
    #[error("request was too large while attempting to {operation}; limit is {limit} bytes")]
    RequestTooLarge { operation: &'static str, limit: usize },
    #[error("I/O failure while attempting to {operation}: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("create ephemeral 1Password reference file {}: {source}", path.display())]
    CreateEnvFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("write ephemeral 1Password reference file {}: {source}", path.display())]
    WriteEnvFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not allocate a unique ephemeral 1Password reference file")]
    AllocateEnvFile,
    #[error("1Password reference environment names must be shell variable names")]
    InvalidEnvironmentName,
    #[error("the 1Password masking opt-out environment variable is reserved")]
    MaskingOverride,
    #[error("prepare masked 1Password child environment: {0}")]
    PrepareEnvironment(#[source] ProcessEnvironmentError),
    #[error("prepare masked 1Password child command: {0}")]
    PrepareCommand(#[source] CommandSpecError),
}

#[derive(Debug)]
pub struct ErrorDetail(Option<String>);

impl fmt::Display for ErrorDetail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(detail) = self.0.as_deref() {
            write!(formatter, ": {detail}")
        } else {
            Ok(())
        }
    }
}

fn check_status(
    operation: &'static str,
    status: Option<i32>,
    stderr: &[u8],
    stderr_policy: StderrPolicy,
) -> Result<(), OpError> {
    if status == Some(0) {
        return Ok(());
    }
    let detail = match stderr_policy {
        StderrPolicy::CaptureSanitized => sanitize_stderr(stderr),
        StderrPolicy::Discard => None,
    };
    Err(OpError::Failed { operation, status, detail: ErrorDetail(detail) })
}

fn map_spawn_error(operation: &'static str, source: io::Error) -> OpError {
    if source.kind() == io::ErrorKind::NotFound {
        OpError::NotInstalled
    } else {
        OpError::Io { operation, source }
    }
}

fn sanitize_stderr(stderr: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    let sanitized = text
        .chars()
        .filter(|ch| !ch.is_control() || *ch == '\n' || *ch == '\t')
        .take(600)
        .collect::<String>()
        .trim()
        .to_owned();
    (!sanitized.is_empty()).then_some(sanitized)
}

async fn terminate(child: &mut tokio::process::Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[derive(Clone, Copy)]
enum OutputDestination {
    Stdout,
    Stderr,
}

async fn forward_output<R>(mut reader: R, destination: OutputDestination) -> Result<(), io::Error>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut chunk = [0_u8; RUN_OUTPUT_CHUNK_BYTES];
    let mut write_error = None;
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return write_error.map_or(Ok(()), Err);
        }
        if write_error.is_some() {
            continue;
        }
        let result = match destination {
            OutputDestination::Stdout => {
                let mut writer = io::stdout().lock();
                writer.write_all(&chunk[..read]).and_then(|()| writer.flush())
            }
            OutputDestination::Stderr => {
                let mut writer = io::stderr().lock();
                writer.write_all(&chunk[..read]).and_then(|()| writer.flush())
            }
        };
        if let Err(error) = result {
            // Keep draining the pipe so `op run` cannot deadlock behind a closed downstream
            // consumer. The first forwarding error is returned after the child closes the pipe.
            write_error = Some(error);
        }
    }
}

async fn finish_forward(
    handle: JoinHandle<Result<(), io::Error>>,
    operation: &'static str,
) -> Result<(), OpError> {
    match handle.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(source)) => Err(OpError::Io { operation, source }),
        Err(source) => Err(OpError::Io {
            operation,
            source: io::Error::other(format!("output task failed: {source}")),
        }),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{collections::BTreeMap, ffi::OsString, os::unix::fs::PermissionsExt};

    use super::*;

    fn client() -> OpClient {
        OpClient::with_executable(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-op"),
        )
    }

    fn reference(field: &str) -> SecretReference {
        SecretReference::new(format!("op://Tests/read/{field}"))
            .expect("test reference must be valid")
    }

    #[test]
    fn secret_reference_rejects_incomplete_and_control_character_paths() {
        assert!(SecretReference::new("not-an-op-reference".to_owned()).is_err());
        assert!(SecretReference::new("op://vault/item".to_owned()).is_err());
        assert!(SecretReference::new("op://vault/item/field\nextra".to_owned()).is_err());
    }

    #[tokio::test]
    async fn read_reference_returns_bounded_utf8_without_a_trailing_newline(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let secret = client().read_reference(&reference("success")).await?;

        assert_eq!(secret.as_str(), "fixture-secret-value");
        Ok(())
    }

    #[tokio::test]
    async fn read_reference_reports_a_failed_op_status() {
        let result = client().read_reference(&reference("failure")).await;
        let Err(error) = result else { panic!("failed fake op unexpectedly returned a secret") };

        assert!(matches!(error, OpError::Failed { status: Some(23), .. }));
    }

    #[tokio::test]
    async fn read_reference_failure_never_retains_or_displays_stderr() {
        const FORBIDDEN_STDERR: &str = "fixture-stderr-must-not-leak";
        let result = client().read_reference(&reference("failure")).await;
        let Err(error) = result else { panic!("failed fake op unexpectedly returned a secret") };

        assert!(!error.to_string().contains(FORBIDDEN_STDERR));
        assert!(!format!("{error:?}").contains(FORBIDDEN_STDERR));
    }

    #[tokio::test]
    async fn read_reference_rejects_empty_output() {
        let result = client().read_reference(&reference("empty")).await;
        let Err(error) = result else { panic!("empty fake op output unexpectedly succeeded") };

        assert!(matches!(error, OpError::InvalidResponse { reason: "secret field was empty", .. }));
    }

    #[tokio::test]
    async fn read_reference_preserves_internal_newlines() -> Result<(), Box<dyn std::error::Error>>
    {
        let secret = client().read_reference(&reference("multiline")).await?;

        assert_eq!(secret.as_str(), "first line\nsecond line");
        Ok(())
    }

    #[tokio::test]
    async fn read_reference_rejects_invalid_utf8() {
        let result = client().read_reference(&reference("invalid-utf8")).await;
        let Err(error) = result else { panic!("invalid UTF-8 unexpectedly succeeded") };

        assert!(matches!(
            error,
            OpError::InvalidResponse { reason: "secret field was not valid UTF-8", .. }
        ));
    }

    #[tokio::test]
    async fn read_reference_rejects_output_over_four_kibibytes() {
        let result = client().read_reference(&reference("oversized")).await;
        let Err(error) = result else { panic!("oversized fake op output unexpectedly succeeded") };

        assert!(matches!(error, OpError::ResponseTooLarge { limit: MAX_SECRET_BYTES, .. }));
    }

    #[test]
    fn prepared_run_uses_a_mode_600_refs_file_and_deletes_it_on_drop(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let references = BTreeMap::from([(
            "TOKEN".to_owned(),
            SecretReference::new("op://Tests/run/token".to_owned())?,
        )]);
        let prepared = client().prepare_run(
            &references,
            OsString::from("printf"),
            vec![OsString::from("ok")],
        )?;
        let env_argument = prepared.arguments()[1].to_string_lossy();
        let env_path = PathBuf::from(
            env_argument
                .strip_prefix("--env-file=")
                .ok_or("prepared op run omitted its env file")?,
        );

        assert_eq!(
            prepared.arguments(),
            [
                OsString::from("run"),
                OsString::from(format!("--env-file={}", env_path.display())),
                OsString::from("--"),
                OsString::from("printf"),
                OsString::from("ok"),
            ]
        );
        assert_eq!(std::fs::read_to_string(&env_path)?, "TOKEN=op://Tests/run/token\n");
        assert_eq!(std::fs::metadata(&env_path)?.permissions().mode() & 0o777, 0o600);

        drop(prepared);
        assert!(!env_path.exists());
        Ok(())
    }

    #[test]
    fn prepared_run_rejects_reference_file_key_injection() -> Result<(), Box<dyn std::error::Error>>
    {
        let references = BTreeMap::from([(
            "TOKEN\nINJECTED".to_owned(),
            SecretReference::new("op://Tests/run/token".to_owned())?,
        )]);
        let result =
            client().prepare_run(&references, OsString::from("printf"), vec![OsString::from("ok")]);
        let Err(error) = result else { panic!("invalid environment name unexpectedly succeeded") };

        assert!(matches!(error, OpError::InvalidEnvironmentName));
        Ok(())
    }

    #[test]
    fn prepared_run_rejects_a_masking_opt_out_reference() -> Result<(), Box<dyn std::error::Error>>
    {
        let references = BTreeMap::from([(
            "op_run_no_masking".to_owned(),
            SecretReference::new("op://Tests/run/token".to_owned())?,
        )]);
        let result =
            client().prepare_run(&references, OsString::from("printf"), vec![OsString::from("ok")]);
        let Err(error) = result else { panic!("masking opt-out unexpectedly succeeded") };

        assert!(matches!(error, OpError::MaskingOverride));
        Ok(())
    }

    #[test]
    fn prepared_run_forces_the_no_masking_override_out_of_child_environment(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let references = BTreeMap::from([(
            "TOKEN".to_owned(),
            SecretReference::new("op://Tests/run/token".to_owned())?,
        )]);
        let prepared = client().prepare_run(
            &references,
            OsString::from("printf"),
            vec![OsString::from("ok")],
        )?;
        let command = prepared.command_spec(
            std::env::temp_dir().canonicalize()?,
            BTreeMap::from([
                (OsString::from("op_run_no_masking"), OsString::from("1")),
                (OsString::from("TOKEN"), OsString::from("inherited-plaintext")),
            ]),
            ProcessLabel::new("test op run".to_owned())?,
        )?;

        assert!(!command.environment.values.contains_key(OsStr::new("op_run_no_masking")));
        assert!(!command.environment.values.contains_key(OsStr::new("TOKEN")));
        assert!(command.environment.removals.contains(OsStr::new(NO_MASKING_ENV)));
        assert!(command.environment.removals.contains(OsStr::new("TOKEN")));
        Ok(())
    }

    #[test]
    fn prepared_run_removes_inherited_reference_values_from_direct_execution(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let references = BTreeMap::from([(
            "TOKEN".to_owned(),
            SecretReference::new("op://Tests/run/token".to_owned())?,
        )]);
        let prepared = client().prepare_run(
            &references,
            OsString::from("printf"),
            vec![OsString::from("ok")],
        )?;
        let command = prepared.command();
        let removals = command
            .as_std()
            .get_envs()
            .filter_map(|(name, value)| value.is_none().then_some(name))
            .collect::<BTreeSet<_>>();

        assert!(removals.contains(OsStr::new(NO_MASKING_ENV)));
        assert!(removals.contains(OsStr::new("TOKEN")));
        Ok(())
    }
}
