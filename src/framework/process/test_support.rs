//! Test-only command recording and replay through the real process supervisor boundary.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::{
    CaptureOverflow, CapturePolicy, CommandSpec, ContainmentRequirement, InputPolicy, LeaderExit,
    LeaderExitObservation, OutputPolicy, OutputReport, ProcessDeadline, ProcessReport, ProcessSpec,
    ProcessSupervisor, TerminationPolicy,
};

const SCENARIO_VERSION: u32 = 1;
const CAPTURE_LIMIT: usize = 8 * 1024 * 1024;
const RECORD_DEADLINE: Duration = Duration::from_secs(30);
const TERMINATION_GRACE: Duration = Duration::from_secs(2);
const RUNTIME_SOURCE: &str = include_str!("command_fixture_runtime.rs");

static RUNTIME_EXECUTABLE: OnceLock<Result<PathBuf, String>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum FixtureStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OutputEvent {
    stream: FixtureStream,
    #[serde(with = "readable_bytes")]
    bytes: Vec<u8>,
    delay_ms: u64,
}

impl OutputEvent {
    pub(crate) fn stdout(bytes: impl Into<Vec<u8>>) -> Self {
        Self { stream: FixtureStream::Stdout, bytes: bytes.into(), delay_ms: 0 }
    }

    pub(crate) fn stderr(bytes: impl Into<Vec<u8>>) -> Self {
        Self { stream: FixtureStream::Stderr, bytes: bytes.into(), delay_ms: 0 }
    }

    pub(crate) fn after(mut self, delay: Duration) -> Self {
        self.delay_ms = delay.as_millis().try_into().unwrap_or(u64::MAX);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "code", rename_all = "kebab-case")]
enum FixtureBehavior {
    Exit(i32),
    Hang,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommandResponse {
    events: Vec<OutputEvent>,
    behavior: FixtureBehavior,
    expected_stdin: Option<Vec<u8>>,
}

impl CommandResponse {
    pub(crate) fn success() -> Self {
        Self { events: Vec::new(), behavior: FixtureBehavior::Exit(0), expected_stdin: None }
    }

    pub(crate) fn exit(code: i32) -> Self {
        Self { events: Vec::new(), behavior: FixtureBehavior::Exit(code), expected_stdin: None }
    }

    pub(crate) fn hang() -> Self {
        Self { events: Vec::new(), behavior: FixtureBehavior::Hang, expected_stdin: None }
    }

    pub(crate) fn event(mut self, event: OutputEvent) -> Self {
        self.events.push(event);
        self
    }

    pub(crate) fn stdout(self, bytes: impl Into<Vec<u8>>) -> Self {
        self.event(OutputEvent::stdout(bytes))
    }

    pub(crate) fn stderr(self, bytes: impl Into<Vec<u8>>) -> Self {
        self.event(OutputEvent::stderr(bytes))
    }

    pub(crate) fn expect_stdin(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.expected_stdin = Some(bytes.into());
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Invocation {
    pub(crate) arguments: Vec<OsString>,
    pub(crate) stdin: Vec<u8>,
    pub(crate) pid: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedExchange {
    arguments: Vec<String>,
    response: CommandResponse,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedScenario {
    version: u32,
    exchanges: Vec<SavedExchange>,
}

pub(crate) struct CommandFixture {
    root: PathBuf,
    rules: Vec<Vec<Vec<u8>>>,
}

impl CommandFixture {
    pub(crate) fn new() -> Result<Self> {
        let root =
            std::env::temp_dir().join(format!("kit-command-fixture-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).with_context(|| format!("create {}", root.display()))?;
        let executable = root.join(runtime_name());
        fs::copy(runtime_executable()?, &executable)
            .with_context(|| format!("install fixture runtime at {}", executable.display()))?;
        fs::create_dir(root.join("rules"))?;
        fs::create_dir(root.join("observed"))?;
        Ok(Self { root, rules: Vec::new() })
    }

    pub(crate) fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let scenario: SavedScenario =
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
        if scenario.version != SCENARIO_VERSION {
            bail!(
                "unsupported command fixture version {}; expected {SCENARIO_VERSION}",
                scenario.version
            );
        }
        let mut fixture = Self::new()?;
        for exchange in scenario.exchanges {
            fixture.respond(exchange.arguments, exchange.response)?;
        }
        Ok(fixture)
    }

    pub(crate) fn respond<I, S>(&mut self, arguments: I, response: CommandResponse) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let arguments = arguments.into_iter().map(Into::into).collect::<Vec<_>>();
        let encoded = arguments.iter().map(|argument| encode_os(argument)).collect::<Vec<_>>();
        let rule_index = match self.rules.iter().position(|rule| rule == &encoded) {
            Some(index) => index,
            None => {
                let index = self.rules.len();
                let rule = self.root.join("rules").join(format!("{index:08}"));
                write_arguments(&rule.join("arguments"), &arguments)?;
                fs::create_dir_all(rule.join("responses"))?;
                self.rules.push(encoded);
                index
            }
        };
        let responses = self.root.join("rules").join(format!("{rule_index:08}")).join("responses");
        let response_index = fs::read_dir(&responses)?.count();
        write_response(&responses.join(format!("{response_index:08}")), &response)
    }

    pub(crate) fn executable(&self) -> PathBuf {
        self.root.join(runtime_name())
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn invocations(&self) -> Result<Vec<Invocation>> {
        let mut directories = directories(&self.root.join("observed"))?;
        directories.retain(|directory| directory.join("ready").is_file());
        directories.sort();
        directories.into_iter().map(|path| read_invocation(&path)).collect()
    }

    pub(crate) async fn wait_for_invocation<I, S>(
        &self,
        arguments: I,
        timeout: Duration,
    ) -> Result<Invocation>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let expected = arguments.into_iter().map(Into::into).collect::<Vec<_>>();
        tokio::time::timeout(timeout, async {
            loop {
                if let Some(invocation) = self
                    .invocations()?
                    .into_iter()
                    .find(|invocation| invocation.arguments == expected)
                {
                    return Ok(invocation);
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .context("wait for command fixture invocation")?
    }
}

impl Drop for CommandFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RecordingPolicy {
    redactions: Vec<(Vec<u8>, Vec<u8>)>,
    forbidden: Vec<Vec<u8>>,
}

impl RecordingPolicy {
    pub(crate) fn strict() -> Self {
        let mut forbidden = vec![
            b"tskey-".to_vec(),
            b"https://login.tailscale.com/".to_vec(),
            b"op://".to_vec(),
            b"\"access_token\"".to_vec(),
            b"\"refresh_token\"".to_vec(),
            b"\"password\"".to_vec(),
        ];
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            let encoded = encode_os(&home);
            if !encoded.is_empty() {
                forbidden.push(encoded);
            }
        }
        Self { redactions: Vec::new(), forbidden }
    }

    pub(crate) fn redact(
        mut self,
        actual: impl Into<Vec<u8>>,
        placeholder: impl Into<Vec<u8>>,
    ) -> Result<Self> {
        let actual = actual.into();
        let placeholder = placeholder.into();
        if actual.is_empty() {
            bail!("recording redaction cannot match an empty value");
        }
        if placeholder.is_empty() {
            bail!("recording redaction placeholder cannot be empty");
        }
        self.forbidden.push(actual.clone());
        self.redactions.push((actual, placeholder));
        Ok(self)
    }

    pub(crate) fn forbid(mut self, value: impl Into<Vec<u8>>) -> Result<Self> {
        let value = value.into();
        if value.is_empty() {
            bail!("recording forbidden value cannot be empty");
        }
        self.forbidden.push(value);
        Ok(self)
    }

    fn sanitize(&self, bytes: &[u8]) -> Result<Vec<u8>> {
        let mut sanitized = bytes.to_vec();
        for (actual, placeholder) in &self.redactions {
            sanitized = replace_bytes(&sanitized, actual, placeholder);
        }
        let normalized = ascii_lowercase(&sanitized);
        if let Some(forbidden) = self.forbidden.iter().find(|forbidden| {
            !forbidden.is_empty() && contains_bytes(&normalized, &ascii_lowercase(forbidden))
        }) {
            bail!(
                "sanitized recording still contains a forbidden value ({} bytes)",
                forbidden.len()
            );
        }
        Ok(sanitized)
    }
}

pub(crate) async fn record_commands(
    processes: &ProcessSupervisor,
    commands: Vec<CommandSpec>,
    policy: &RecordingPolicy,
    destination: &Path,
) -> Result<PathBuf> {
    if commands.is_empty() {
        bail!("record at least one command");
    }
    let raw_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("command-recordings")
        .join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&raw_root)
        .with_context(|| format!("create raw recording at {}", raw_root.display()))?;

    let mut exchanges = Vec::with_capacity(commands.len());
    for (index, command) in commands.into_iter().enumerate() {
        let raw_command = raw_root.join(format!("{index:08}"));
        fs::create_dir(&raw_command)?;
        write_raw_command(&raw_command, &command)?;
        let arguments = command
            .arguments
            .iter()
            .map(|argument| {
                argument
                    .to_str()
                    .map(ToOwned::to_owned)
                    .context("recorded command arguments must be valid UTF-8")
            })
            .collect::<Result<Vec<_>>>()?;
        let report = run_recording(processes, command).await?;
        let stdout = captured(&report.stdout, "stdout")?;
        let stderr = captured(&report.stderr, "stderr")?;
        fs::write(raw_command.join("stdout"), stdout)?;
        fs::write(raw_command.join("stderr"), stderr)?;
        fs::write(raw_command.join("completion"), format!("{:?}", report.completion))?;
        fs::write(raw_command.join("leader-exit"), format!("{:?}", report.leader_exit))?;
        let code = match report.leader_exit {
            LeaderExitObservation::Observed(LeaderExit::Code(code)) => code,
            exit => bail!("recorded command did not exit with a code: {exit:?}"),
        };
        let sanitized_arguments = arguments
            .into_iter()
            .map(|argument| {
                let bytes = policy.sanitize(argument.as_bytes())?;
                String::from_utf8(bytes).context("sanitized argument is not valid UTF-8")
            })
            .collect::<Result<Vec<_>>>()?;
        let mut response = CommandResponse::exit(code);
        let stdout = policy.sanitize(stdout)?;
        let stderr = policy.sanitize(stderr)?;
        if !stdout.is_empty() {
            response.events.push(OutputEvent::stdout(stdout));
        }
        if !stderr.is_empty() {
            response.events.push(OutputEvent::stderr(stderr));
        }
        exchanges.push(SavedExchange { arguments: sanitized_arguments, response });
    }

    let scenario = SavedScenario { version: SCENARIO_VERSION, exchanges };
    let encoded = serde_json::to_vec_pretty(&scenario)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(destination, encoded)
        .with_context(|| format!("write sanitized fixture to {}", destination.display()))?;
    Ok(raw_root)
}

async fn run_recording(
    processes: &ProcessSupervisor,
    command: CommandSpec,
) -> Result<ProcessReport> {
    let capture = OutputPolicy::Capture(CapturePolicy::new(
        NonZeroUsize::new(CAPTURE_LIMIT).unwrap(),
        CaptureOverflow::FailAndTerminate,
    ));
    let spec = ProcessSpec::new(
        command,
        InputPolicy::Closed,
        capture,
        capture,
        ContainmentRequirement::ExplicitProcessGroup,
        ProcessDeadline::After(RECORD_DEADLINE),
        TerminationPolicy::new(TERMINATION_GRACE),
    );
    processes
        .spawn(spec)
        .await?
        .session
        .wait()
        .await
        .map_err(|failure| anyhow::anyhow!("recorded command failed: {:?}", failure.failure))
}

fn write_raw_command(root: &Path, command: &CommandSpec) -> Result<()> {
    fs::write(root.join("program"), encode_os(&command.program))?;
    write_arguments(&root.join("arguments"), &command.arguments)
}

fn captured<'a>(report: &'a OutputReport, stream: &str) -> Result<&'a [u8]> {
    match report {
        OutputReport::Captured(capture) => Ok(capture.bytes.as_ref()),
        _ => bail!("recorded command {stream} was not captured"),
    }
}

fn runtime_executable() -> Result<&'static Path> {
    match RUNTIME_EXECUTABLE.get_or_init(|| compile_runtime().map_err(|error| format!("{error:#}")))
    {
        Ok(path) => Ok(path.as_path()),
        Err(error) => bail!("compile command fixture runtime: {error}"),
    }
}

fn compile_runtime() -> Result<PathBuf> {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("command-fixture-runtime")
        .join(std::process::id().to_string());
    fs::create_dir_all(&directory)?;
    let source = directory.join("main.rs");
    let executable = directory.join(runtime_name());
    fs::write(&source, RUNTIME_SOURCE)?;
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
    let output = Command::new(rustc)
        .arg("--edition=2021")
        .arg("--crate-name=kit_command_fixture_runtime")
        .arg("-o")
        .arg(&executable)
        .arg(&source)
        .output()
        .context("launch rustc for command fixture runtime")?;
    if !output.status.success() {
        bail!(
            "rustc failed with {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(executable)
}

fn runtime_name() -> &'static str {
    if cfg!(windows) {
        "command-fixture.exe"
    } else {
        "command-fixture"
    }
}

fn write_arguments(root: &Path, arguments: &[OsString]) -> Result<()> {
    fs::create_dir_all(root)?;
    for (index, argument) in arguments.iter().enumerate() {
        fs::write(root.join(format!("{index:08}")), encode_os(argument))?;
    }
    Ok(())
}

fn write_response(root: &Path, response: &CommandResponse) -> Result<()> {
    fs::create_dir_all(root.join("events"))?;
    if let Some(stdin) = &response.expected_stdin {
        fs::write(root.join("expected-stdin"), stdin)?;
    }
    for (index, event) in response.events.iter().enumerate() {
        let event_root = root.join("events").join(format!("{index:08}"));
        fs::create_dir(&event_root)?;
        fs::write(
            event_root.join("stream"),
            match event.stream {
                FixtureStream::Stdout => "stdout",
                FixtureStream::Stderr => "stderr",
            },
        )?;
        fs::write(event_root.join("delay-ms"), event.delay_ms.to_string())?;
        fs::write(event_root.join("bytes"), &event.bytes)?;
    }
    fs::write(
        root.join("behavior"),
        match response.behavior {
            FixtureBehavior::Exit(code) => format!("exit:{code}"),
            FixtureBehavior::Hang => "hang".to_owned(),
        },
    )?;
    Ok(())
}

fn read_invocation(root: &Path) -> Result<Invocation> {
    let arguments = read_arguments(&root.join("arguments"))?;
    let stdin = fs::read(root.join("stdin"))?;
    let pid = fs::read_to_string(root.join("pid"))?.trim().parse()?;
    Ok(Invocation { arguments, stdin, pid })
}

fn read_arguments(root: &Path) -> Result<Vec<OsString>> {
    let mut paths = files(root)?;
    paths.sort();
    paths.into_iter().map(|path| decode_os(&fs::read(path)?)).collect()
}

fn directories(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    Ok(fs::read_dir(root)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect())
}

fn files(root: &Path) -> Result<Vec<PathBuf>> {
    Ok(fs::read_dir(root)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect())
}

fn replace_bytes(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(haystack.len());
    let mut cursor = 0;
    while let Some(offset) = find_bytes(&haystack[cursor..], needle) {
        let found = cursor + offset;
        output.extend_from_slice(&haystack[cursor..found]);
        output.extend_from_slice(replacement);
        cursor = found + needle.len();
    }
    output.extend_from_slice(&haystack[cursor..]);
    output
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    find_bytes(haystack, needle).is_some()
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    (!needle.is_empty())
        .then(|| haystack.windows(needle.len()).position(|window| window == needle))?
}

fn ascii_lowercase(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().map(u8::to_ascii_lowercase).collect()
}

mod readable_bytes {
    use std::fmt;

    use serde::{
        de::{SeqAccess, Visitor},
        Deserializer, Serializer,
    };

    pub(super) fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match std::str::from_utf8(bytes) {
            Ok(text) => serializer.serialize_str(text),
            Err(_) => serializer.serialize_bytes(bytes),
        }
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BytesVisitor;

        impl<'de> Visitor<'de> for BytesVisitor {
            type Value = Vec<u8>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a UTF-8 string or byte array")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                Ok(value.as_bytes().to_vec())
            }

            fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E> {
                Ok(value.to_vec())
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut bytes = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
                while let Some(byte) = sequence.next_element()? {
                    bytes.push(byte);
                }
                Ok(bytes)
            }
        }

        deserializer.deserialize_any(BytesVisitor)
    }
}

#[cfg(unix)]
fn encode_os(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(windows)]
fn encode_os(value: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    value.encode_wide().flat_map(u16::to_le_bytes).collect()
}

#[cfg(not(any(unix, windows)))]
fn encode_os(value: &OsStr) -> Vec<u8> {
    value.to_string_lossy().into_owned().into_bytes()
}

#[cfg(unix)]
fn decode_os(bytes: &[u8]) -> Result<OsString> {
    use std::os::unix::ffi::OsStringExt;
    Ok(OsString::from_vec(bytes.to_vec()))
}

#[cfg(windows)]
fn decode_os(bytes: &[u8]) -> Result<OsString> {
    use std::os::windows::ffi::OsStringExt;
    if !bytes.len().is_multiple_of(2) {
        bail!("fixture argument has an odd Windows byte length");
    }
    let wide = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    Ok(OsString::from_wide(&wide))
}

#[cfg(not(any(unix, windows)))]
fn decode_os(bytes: &[u8]) -> Result<OsString> {
    Ok(OsString::from(String::from_utf8(bytes.to_vec())?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::process::{
        CommandSpec, EnvironmentBase, ProcessEnvironment, ProcessLabel,
    };

    fn command(executable: &Path, arguments: &[&str], directory: &Path) -> CommandSpec {
        CommandSpec::new(
            executable.as_os_str().to_owned(),
            arguments.iter().map(OsString::from).collect(),
            directory.to_path_buf(),
            ProcessEnvironment::new(EnvironmentBase::Inherit, BTreeMap::new(), BTreeSet::new())
                .unwrap(),
            ProcessLabel::new("command fixture test".into()).unwrap(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn replay_matches_arguments_stdin_streams_and_invocation_history() {
        let mut fixture = CommandFixture::new().unwrap();
        fixture
            .respond(
                ["send", "peer"],
                CommandResponse::success()
                    .expect_stdin("private input")
                    .event(OutputEvent::stderr("warming\n"))
                    .event(OutputEvent::stdout("ready\n").after(Duration::from_millis(1))),
            )
            .unwrap();
        let processes = ProcessSupervisor::for_test(fixture.root().join("processes")).unwrap();
        let spec = ProcessSpec::new(
            command(&fixture.executable(), &["send", "peer"], fixture.root()),
            InputPolicy::Once(super::super::PrivateBytes::new(b"private input".to_vec())),
            OutputPolicy::Capture(CapturePolicy::new(
                NonZeroUsize::new(1024).unwrap(),
                CaptureOverflow::FailAndTerminate,
            )),
            OutputPolicy::Capture(CapturePolicy::new(
                NonZeroUsize::new(1024).unwrap(),
                CaptureOverflow::FailAndTerminate,
            )),
            ContainmentRequirement::ExplicitProcessGroup,
            ProcessDeadline::After(Duration::from_secs(3)),
            TerminationPolicy::new(Duration::from_secs(1)),
        );
        let report = processes.spawn(spec).await.unwrap().session.wait().await.unwrap();
        assert_eq!(captured(&report.stdout, "stdout").unwrap(), b"ready\n");
        assert_eq!(captured(&report.stderr, "stderr").unwrap(), b"warming\n");
        let invocations = fixture.invocations().unwrap();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].stdin, b"private input");
    }

    #[tokio::test]
    async fn recording_keeps_raw_output_private_and_writes_only_sanitized_replay() {
        let mut source = CommandFixture::new().unwrap();
        source
            .respond(
                ["status"],
                CommandResponse::success().stdout(
                    b"device=/home/alice token=tskey-secret-value ip=100.64.0.8\n".to_vec(),
                ),
            )
            .unwrap();
        let processes =
            ProcessSupervisor::for_test(source.root().join("record-processes")).unwrap();
        let destination = source.root().join("saved.json");
        let policy = RecordingPolicy::strict()
            .redact("/home/alice", "<HOME>")
            .unwrap()
            .redact("tskey-secret-value", "<AUTH_KEY>")
            .unwrap()
            .redact("100.64.0.8", "<TAILNET_IP>")
            .unwrap();
        let raw = record_commands(
            &processes,
            vec![command(&source.executable(), &["status"], source.root())],
            &policy,
            &destination,
        )
        .await
        .unwrap();
        assert!(String::from_utf8_lossy(&fs::read(raw.join("00000000/stdout")).unwrap())
            .contains("tskey-secret-value"));
        let saved = fs::read_to_string(&destination).unwrap();
        assert!(!saved.contains("/home/alice"));
        assert!(!saved.contains("tskey-secret-value"));
        assert!(!saved.contains("100.64.0.8"));
        assert!(saved.contains("<TAILNET_IP>"));
        let replay = CommandFixture::load(&destination).unwrap();
        assert!(replay.executable().is_file());
    }

    #[tokio::test]
    async fn recording_refuses_to_publish_when_forbidden_content_survives() {
        let mut source = CommandFixture::new().unwrap();
        source.respond(["status"], CommandResponse::success().stdout("custom-sensitive")).unwrap();
        let processes =
            ProcessSupervisor::for_test(source.root().join("record-processes")).unwrap();
        let destination = source.root().join("must-not-exist.json");
        let result = record_commands(
            &processes,
            vec![command(&source.executable(), &["status"], source.root())],
            &RecordingPolicy::strict().forbid("custom-sensitive").unwrap(),
            &destination,
        )
        .await;
        assert!(result.is_err());
        assert!(!destination.exists());
    }
}
