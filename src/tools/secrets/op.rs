use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::task::JoinHandle;
use zeroize::Zeroizing;

use super::model::{
    AccountId, AccountSummary, CreateLoginRequest, ItemId, ItemRef, ItemSummary, PasswordRecipe,
    VaultId, VaultSummary,
};
use super::sensitive::{SecretBytes, SensitiveBuffer, MAX_SECRET_BYTES};

const SECRET_READ_CHUNK_BYTES: usize = 1024;
const MAX_CREATE_JSON_BYTES: usize = 1024 * 1024;
const RUN_OUTPUT_CHUNK_BYTES: usize = 8 * 1024;
const NO_MASKING_ENV: &str = "OP_RUN_NO_MASKING";

/// One validated 1Password secret reference.
///
/// The inner string is private so callers cannot construct a value-bearing request by accident.
/// `Debug` intentionally reveals only the type; explicit errors use `Display` when the reference
/// itself is required for diagnosis.
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

/// The only supported `op run` request shape.
///
/// Callers can supply the refs file and the direct child command, but cannot add flags to `op`
/// itself. In particular, there is no representation for `--no-masking`.
pub struct OpRunRequest<'a> {
    env_file: &'a Path,
    program: &'a str,
    args: &'a [String],
}

impl<'a> OpRunRequest<'a> {
    pub fn new(env_file: &'a Path, program: &'a str, args: &'a [String]) -> Self {
        Self { env_file, program, args }
    }
}

impl fmt::Debug for OpRunRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpRunRequest")
            .field("env_file", &self.env_file)
            .field("program", &self.program)
            .field("arg_count", &self.args.len())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpRunStatus {
    success: bool,
    code: Option<i32>,
}

impl OpRunStatus {
    pub fn success(self) -> bool {
        self.success
    }

    pub fn code(self) -> Option<i32> {
        self.code
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoginField {
    Username,
    Password,
}

impl LoginField {
    fn reference_name(self) -> &'static str {
        match self {
            Self::Username => "username",
            Self::Password => "password",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Username => "Username",
            Self::Password => "Password",
        }
    }
}

#[derive(Clone, Copy)]
enum StderrPolicy {
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
        S: AsRef<std::ffi::OsStr>,
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

    pub async fn accounts(&self) -> Result<Vec<AccountSummary>, OpError> {
        let raw: Vec<RawAccount> =
            self.json("list accounts", ["account", "list", "--format=json"]).await?;
        raw.into_iter().map(AccountSummary::try_from).collect()
    }

    pub async fn vaults(&self, account: &AccountId) -> Result<Vec<VaultSummary>, OpError> {
        let raw: Vec<RawVault> = self
            .json("list vaults", ["vault", "list", "--format=json", "--account", account.as_str()])
            .await?;
        raw.into_iter().map(VaultSummary::try_from).collect()
    }

    pub async fn items(&self, account: &AccountId) -> Result<Vec<ItemSummary>, OpError> {
        let raw: Vec<RawItem> = self
            .json("list items", ["item", "list", "--format=json", "--account", account.as_str()])
            .await?;
        raw.into_iter().map(|item| item.into_summary(account.clone())).collect()
    }

    /// Resolve-check one reference without retaining or printing the resolved value.
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

    /// Run one direct child command through the fixed, masking-on `op run` contract.
    ///
    /// `op run` owns secret resolution and masking. Kit pipes only its already-masked output and
    /// forwards those bytes without interpreting them.
    pub async fn run_operation(&self, request: OpRunRequest<'_>) -> Result<OpRunStatus, OpError> {
        let mut child = self
            .run_command(&request)
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
        Ok(OpRunStatus { success: status.success(), code: status.code() })
    }

    fn run_command(&self, request: &OpRunRequest<'_>) -> Command {
        let mut command = self.command();
        command
            .arg("run")
            .arg(format!("--env-file={}", request.env_file.display()))
            .arg("--")
            .arg(request.program)
            .args(request.args)
            // An inherited opt-out must not be able to weaken Kit's fixed masking contract.
            .env_remove(NO_MASKING_ENV);
        command
    }

    pub async fn field(
        &self,
        reference: &ItemRef,
        field: LoginField,
    ) -> Result<SecretBytes, OpError> {
        let secret_reference = secret_reference(reference, field);
        self.secret(
            "read login field",
            [
                "read",
                secret_reference.as_str(),
                "--account",
                reference.account_id.as_str(),
                "--no-newline",
            ],
        )
        .await
    }

    pub async fn create_login(&self, mut request: CreateLoginRequest) -> Result<(), OpError> {
        let body = create_body(&request)?;
        let args = create_args(&request);
        // The fixed JSON stdin buffer is now the sole Kit-owned copy needed by the subprocess.
        request.password = None;
        self.status_with_stdin("create login", args, body).await
    }

    pub async fn rotate_password(&self, reference: &ItemRef) -> Result<(), OpError> {
        let args = vec![
            "item".to_owned(),
            "edit".to_owned(),
            reference.item_id.as_str().to_owned(),
            "--vault".to_owned(),
            reference.vault_id.as_str().to_owned(),
            format!("--generate-password={}", PasswordRecipe::default().as_argument()),
            "--format=json".to_owned(),
            "--account".to_owned(),
            reference.account_id.as_str().to_owned(),
        ];
        self.status("rotate password", &args, StderrPolicy::Discard).await
    }

    pub async fn archive(&self, reference: &ItemRef) -> Result<(), OpError> {
        let args = vec![
            "item".to_owned(),
            "delete".to_owned(),
            reference.item_id.as_str().to_owned(),
            "--vault".to_owned(),
            reference.vault_id.as_str().to_owned(),
            "--archive".to_owned(),
            "--account".to_owned(),
            reference.account_id.as_str().to_owned(),
        ];
        self.status("archive item", &args, StderrPolicy::CaptureSanitized).await
    }

    async fn json<T, I, S>(&self, operation: &'static str, args: I) -> Result<T, OpError>
    where
        T: for<'de> Deserialize<'de>,
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
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
        S: AsRef<std::ffi::OsStr>,
    {
        let mut child = self
            .operation_command(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // A secret-returning operation never preserves stderr: an upstream diagnostic must not
            // become an accidental plaintext echo in Kit's heap or error UI.
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
        }
        drop(stdout);

        let status = child.wait().await.map_err(|source| OpError::Io { operation, source })?;
        check_status(operation, status.code(), &[], StderrPolicy::Discard)?;
        secret.into_secret().map_err(|()| OpError::InvalidResponse {
            operation,
            reason: "secret field was not valid UTF-8",
        })
    }

    async fn status_with_stdin(
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

    async fn status(
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
}

#[derive(Debug)]
pub struct ErrorDetail(Option<String>);

impl std::fmt::Display for ErrorDetail {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(detail) = self.0.as_deref() {
            write!(formatter, ": {detail}")
        } else {
            Ok(())
        }
    }
}

fn map_spawn_error(operation: &'static str, source: io::Error) -> OpError {
    if source.kind() == io::ErrorKind::NotFound {
        OpError::NotInstalled
    } else {
        OpError::Io { operation, source }
    }
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
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        match destination {
            OutputDestination::Stdout => {
                let mut writer = io::stdout().lock();
                writer.write_all(&chunk[..read])?;
                writer.flush()?;
            }
            OutputDestination::Stderr => {
                let mut writer = io::stderr().lock();
                writer.write_all(&chunk[..read])?;
                writer.flush()?;
            }
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

#[derive(Deserialize)]
struct RawAccount {
    #[serde(default, alias = "account_uuid")]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    shorthand: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    email: String,
}

impl TryFrom<RawAccount> for AccountSummary {
    type Error = OpError;

    fn try_from(raw: RawAccount) -> Result<Self, Self::Error> {
        let id = AccountId::new(raw.id).ok_or(OpError::InvalidResponse {
            operation: "list accounts",
            reason: "account omitted its id",
        })?;
        let label = [raw.name.as_str(), raw.shorthand.as_str(), raw.url.as_str()]
            .into_iter()
            .find(|value| !value.is_empty())
            .unwrap_or("1Password account")
            .to_owned();
        let selectors = [raw.shorthand, raw.url, raw.email]
            .into_iter()
            .filter(|value| !value.is_empty())
            .collect();
        Ok(Self { id, label, selectors })
    }
}

#[derive(Deserialize)]
struct RawVault {
    #[serde(default)]
    id: String,
    #[serde(default, alias = "title")]
    name: String,
}

impl TryFrom<RawVault> for VaultSummary {
    type Error = OpError;

    fn try_from(raw: RawVault) -> Result<Self, Self::Error> {
        let id = VaultId::new(raw.id).ok_or(OpError::InvalidResponse {
            operation: "list vaults",
            reason: "vault omitted its id",
        })?;
        Ok(Self { id, name: raw.name })
    }
}

#[derive(Deserialize)]
struct RawItem {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    vault: Option<RawVault>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    urls: Vec<RawUrl>,
    #[serde(default)]
    additional_information: Option<String>,
}

impl RawItem {
    fn into_summary(self, account_id: AccountId) -> Result<ItemSummary, OpError> {
        let item_id = ItemId::new(self.id).ok_or(OpError::InvalidResponse {
            operation: "list items",
            reason: "item omitted its id",
        })?;
        let vault = self.vault.ok_or(OpError::InvalidResponse {
            operation: "list items",
            reason: "item omitted its vault",
        })?;
        let vault_id = VaultId::new(vault.id).ok_or(OpError::InvalidResponse {
            operation: "list items",
            reason: "item vault omitted its id",
        })?;
        Ok(ItemSummary {
            reference: ItemRef { account_id, vault_id, item_id },
            title: self.title,
            vault_name: vault.name,
            category: self.category,
            tags: self.tags,
            urls: self.urls.into_iter().map(|url| url.href).filter(|url| !url.is_empty()).collect(),
            additional_information: self.additional_information,
        })
    }
}

#[derive(Deserialize)]
struct RawUrl {
    #[serde(default)]
    href: String,
}

#[derive(Serialize)]
struct LoginTemplate<'a> {
    title: &'a str,
    category: &'static str,
    fields: Vec<TemplateField<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    urls: Vec<TemplateUrl<'a>>,
}

#[derive(Serialize)]
struct TemplateField<'a> {
    id: &'static str,
    #[serde(rename = "type")]
    kind: &'static str,
    purpose: &'static str,
    label: &'static str,
    value: &'a str,
}

#[derive(Serialize)]
struct TemplateUrl<'a> {
    primary: bool,
    href: &'a str,
}

fn create_body(request: &CreateLoginRequest) -> Result<Zeroizing<Vec<u8>>, OpError> {
    let mut fields = Vec::new();
    if !request.username.is_empty() {
        fields.push(TemplateField {
            id: "username",
            kind: "STRING",
            purpose: "USERNAME",
            label: "username",
            value: &request.username,
        });
    }
    if let Some(password) = request.password.as_ref() {
        fields.push(TemplateField {
            id: "password",
            kind: "CONCEALED",
            purpose: "PASSWORD",
            label: "password",
            value: password.as_str(),
        });
    }
    let urls = (!request.url.is_empty())
        .then_some(TemplateUrl { primary: true, href: &request.url })
        .into_iter()
        .collect();
    let template = LoginTemplate { title: &request.title, category: "LOGIN", fields, urls };
    let serialized_len = serialized_len(&template)?;
    let mut writer = SensitiveBuffer::new(serialized_len);
    serde_json::to_writer(&mut writer, &template)
        .map_err(|source| OpError::InvalidJson { operation: "serialize login", source })?;
    Ok(writer.into_bytes())
}

fn create_args(request: &CreateLoginRequest) -> Vec<String> {
    let mut args = vec![
        "item".to_owned(),
        "create".to_owned(),
        "-".to_owned(),
        "--vault".to_owned(),
        request.vault_id.as_str().to_owned(),
        "--account".to_owned(),
        request.account_id.as_str().to_owned(),
    ];
    if request.password.is_none() {
        args.push(format!("--generate-password={}", PasswordRecipe::default().as_argument()));
    }
    args
}

struct BoundedCounter {
    length: usize,
    limit: usize,
    exceeded: bool,
}

impl BoundedCounter {
    fn new(limit: usize) -> Self {
        Self { length: 0, limit, exceeded: false }
    }
}

impl Write for BoundedCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(length) = self.length.checked_add(bytes.len()) else {
            self.exceeded = true;
            return Err(io::Error::other("serialized login exceeded its size limit"));
        };
        if length > self.limit {
            self.exceeded = true;
            return Err(io::Error::other("serialized login exceeded its size limit"));
        }
        self.length = length;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialized_len(template: &LoginTemplate<'_>) -> Result<usize, OpError> {
    let mut counter = BoundedCounter::new(MAX_CREATE_JSON_BYTES);
    let result = serde_json::to_writer(&mut counter, template);
    if counter.exceeded {
        return Err(OpError::RequestTooLarge {
            operation: "serialize login",
            limit: MAX_CREATE_JSON_BYTES,
        });
    }
    result.map_err(|source| OpError::InvalidJson { operation: "serialize login", source })?;
    Ok(counter.length)
}

fn secret_reference(reference: &ItemRef, field: LoginField) -> String {
    format!(
        "op://{}/{}/{}",
        reference.vault_id.as_str(),
        reference.item_id.as_str(),
        field.reference_name()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    struct FakeOp {
        directory: PathBuf,
        executable: PathBuf,
    }

    #[cfg(unix)]
    impl FakeOp {
        fn new(script: &str) -> Self {
            use std::io::Write as _;
            use std::os::unix::fs::PermissionsExt;
            use std::sync::atomic::{AtomicU64, Ordering};

            static NEXT_ID: AtomicU64 = AtomicU64::new(0);
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir()
                .join(format!("kit-secrets-op-test-{}-{id}", std::process::id()));
            std::fs::create_dir(&directory).unwrap();
            let executable = directory.join("op");
            // Keep the executable's write-open descriptor out of the concurrent test process so
            // another fork cannot inherit it and make the subsequent exec fail with ETXTBSY.
            let mut writer = std::process::Command::new("/bin/sh")
                .args(["-c", "umask 077; /bin/cat > \"$1\"", "kit-fake-op-writer"])
                .arg(&executable)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let mut stdin = writer.stdin.take().unwrap();
            stdin.write_all(script.as_bytes()).unwrap();
            drop(stdin);
            assert!(writer.wait().unwrap().success());
            let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
            permissions.set_mode(0o700);
            std::fs::set_permissions(&executable, permissions).unwrap();
            Self { directory, executable }
        }

        fn client(&self) -> OpClient {
            OpClient::with_executable(self.executable.clone())
        }
    }

    #[cfg(unix)]
    impl Drop for FakeOp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    fn account() -> AccountId {
        AccountId::new("account-id".to_owned()).unwrap()
    }

    fn vault() -> VaultId {
        VaultId::new("vault-id".to_owned()).unwrap()
    }

    fn item_reference() -> ItemRef {
        ItemRef {
            account_id: account(),
            vault_id: vault(),
            item_id: ItemId::new("item-id".to_owned()).unwrap(),
        }
    }

    fn synthetic_secret() -> SecretBytes {
        let mut buffer = SensitiveBuffer::new(MAX_SECRET_BYTES);
        buffer.try_extend(b"synthetic-sentinel").unwrap();
        buffer.into_secret().unwrap()
    }

    #[test]
    fn parses_item_summaries_without_secret_fields() {
        let raw: Vec<RawItem> = serde_json::from_str(
            r#"[{"id":"item-id","title":"Example","category":"LOGIN","vault":{"id":"vault-id","name":"Private"},"tags":["work"],"urls":[{"href":"https://example.test"}]}]"#,
        )
        .unwrap();
        let item = raw.into_iter().next().unwrap().into_summary(account()).unwrap();
        assert_eq!(item.title, "Example");
        assert_eq!(item.vault_name, "Private");
        assert_eq!(item.urls, vec!["https://example.test"]);
    }

    #[test]
    fn create_places_manual_password_only_in_fixed_stdin_json() {
        let request = CreateLoginRequest {
            account_id: account(),
            vault_id: vault(),
            title: "Example".to_owned(),
            username: "user".to_owned(),
            url: "https://example.test".to_owned(),
            password: Some(synthetic_secret()),
        };
        let args = create_args(&request);
        let body = create_body(&request).unwrap();

        assert!(args.iter().all(|argument| !argument.contains("synthetic-sentinel")));
        assert!(String::from_utf8_lossy(&body).contains("synthetic-sentinel"));
        assert!(body.len() <= MAX_CREATE_JSON_BYTES);
    }

    #[test]
    fn field_reference_uses_ids_and_a_fixed_built_in_name() {
        let reference = item_reference();
        assert_eq!(
            secret_reference(&reference, LoginField::Password),
            "op://vault-id/item-id/password"
        );
        assert_eq!(
            secret_reference(&reference, LoginField::Username),
            "op://vault-id/item-id/username"
        );
    }

    #[test]
    fn operation_references_are_typed_and_debug_redacted() {
        let reference = SecretReference::new("op://vault/item/password".to_owned()).unwrap();

        assert_eq!(reference.as_str(), "op://vault/item/password");
        assert!(!format!("{reference:?}").contains("password"));
        assert!(SecretReference::new("resolved-value".to_owned()).is_err());
        assert!(SecretReference::new("op://missing/components".to_owned()).is_err());
    }

    #[test]
    fn run_command_has_exact_fixed_args_and_removes_masking_opt_out() {
        let client = OpClient::with_executable(PathBuf::from("fake-op"));
        let args = vec!["deploy".to_owned(), "marketing".to_owned()];
        let request = OpRunRequest::new(Path::new("/tmp/refs.env"), "kit", &args);
        let command = client.run_command(&request);
        let argv = command
            .as_std()
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let masking_environment = command
            .as_std()
            .get_envs()
            .find(|(name, _)| *name == NO_MASKING_ENV)
            .map(|(_, value)| value);

        assert_eq!(argv, ["run", "--env-file=/tmp/refs.env", "--", "kit", "deploy", "marketing",]);
        assert_eq!(masking_environment, Some(None));
        assert!(argv.iter().all(|argument| argument != "--no-masking"));
    }

    #[test]
    fn serialized_size_counter_uses_an_explicit_limit() {
        let mut counter = BoundedCounter::new(3);
        counter.write_all(b"abc").unwrap();
        assert!(counter.write_all(b"d").is_err());
        assert!(counter.exceeded);
    }

    #[test]
    fn secret_operation_errors_do_not_include_stderr() {
        let error =
            check_status("read password", Some(1), b"synthetic-sentinel", StderrPolicy::Discard)
                .unwrap_err();
        assert!(!error.to_string().contains("synthetic-sentinel"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_op_proves_field_args_and_bounded_raw_stdout_contract() {
        let fake = FakeOp::new(
            r#"#!/bin/sh
[ "$#" -eq 6 ] || exit 90
[ "$1" = read ] || exit 91
[ "$2" = op://vault-id/item-id/password ] || exit 92
[ "$3" = --account ] && [ "$4" = account-id ] || exit 93
[ "$5" = --no-newline ] && [ "$6" = --no-color ] || exit 94
printf %s synthetic-read-value
"#,
        );

        let value = fake.client().field(&item_reference(), LoginField::Password).await.unwrap();
        assert_eq!(value.as_str(), "synthetic-read-value");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_op_proves_manual_create_secret_is_stdin_only() {
        let fake = FakeOp::new(
            r#"#!/bin/sh
[ "$1" = item ] && [ "$2" = create ] && [ "$3" = - ] || exit 90
for argument in "$@"; do
  case "$argument" in
    *synthetic-sentinel*|*Example*|*example.test*|*user*) exit 91 ;;
  esac
done
body=$(cat)
case "$body" in
  *synthetic-sentinel*) ;;
  *) exit 92 ;;
esac
printf %s 'response-output-must-not-be-read'
"#,
        );
        let request = CreateLoginRequest {
            account_id: account(),
            vault_id: vault(),
            title: "Example".to_owned(),
            username: "user".to_owned(),
            url: "https://example.test".to_owned(),
            password: Some(synthetic_secret()),
        };

        fake.client().create_login(request).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_op_output_over_limit_is_rejected() {
        let fake = FakeOp::new(
            r#"#!/bin/sh
i=0
while [ "$i" -lt 4097 ]; do
  printf x
  i=$((i + 1))
done
"#,
        );

        let result = fake.client().field(&item_reference(), LoginField::Password).await;
        let Err(error) = result else { panic!("oversized secret output unexpectedly succeeded") };
        assert!(matches!(error, OpError::ResponseTooLarge { limit: MAX_SECRET_BYTES, .. }));
    }
}
