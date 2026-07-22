use std::{
    ffi::{CStr, OsString},
    fs,
    io::ErrorKind,
    os::unix::{
        ffi::OsStringExt,
        fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use thiserror::Error;

use crate::framework::{process::ProcessSupervisor, AtomicFileWriter};

use super::{
    command,
    model::{ConsoleServicePlatform, NativeServiceState},
};

pub const PLATFORM: ConsoleServicePlatform = ConsoleServicePlatform::MacosLaunchAgent;

const LABEL: &str = "io.xtava.kit.console.agent";
const PLIST_FILE: &str = "io.xtava.kit.console.agent.plist";
const LAUNCHCTL: &str = "/bin/launchctl";
const PLUTIL: &str = "/usr/bin/plutil";

#[derive(Debug)]
struct EffectiveUser {
    uid: u32,
    home: PathBuf,
}

/// Inspect the per-user GUI LaunchAgent without consulting SSH-derived environment variables.
pub async fn inspect(processes: &ProcessSupervisor) -> Result<NativeServiceState> {
    let user = effective_user()?;
    if !existing_launch_agents_directory(&user)? {
        return Ok(NativeServiceState::NotInstalled);
    }
    let path = launch_agent_path(&user);
    match validate_existing_file(&path, user.uid) {
        Ok(()) => {}
        Err(PathSafetyError::Missing { .. }) => return Ok(NativeServiceState::NotInstalled),
        Err(PathSafetyError::WrongOwner { path, actual_uid, .. }) => {
            return Ok(NativeServiceState::WrongOwner { path, expected_uid: user.uid, actual_uid });
        }
        Err(error) => return Ok(NativeServiceState::Failed { detail: error.to_string() }),
    }

    let output = match launchctl(
        processes,
        "inspect Console macOS LaunchAgent",
        [OsString::from("print"), target(&user)],
    )
    .await
    {
        Ok(output) => output,
        Err(error) => return Ok(NativeServiceState::Unavailable { detail: error.to_string() }),
    };

    if output.success {
        return Ok(state_from_print(&output.stdout, &output.stderr));
    }

    let detail = command_detail(&output.stdout, &output.stderr);
    if missing_service(&detail) {
        Ok(NativeServiceState::Stopped)
    } else if unavailable_gui_domain(&detail) {
        Ok(NativeServiceState::Unavailable { detail })
    } else {
        Ok(NativeServiceState::Failed { detail })
    }
}

/// True only when the securely owned plist is exactly the definition for `executable`.
///
/// `ProcessSupervisor` remains in this contract so both adapters have the same call shape. macOS
/// definition equality is a local, pure file comparison; it never needs to start a service.
pub async fn definition_matches(_processes: &ProcessSupervisor, executable: &Path) -> Result<bool> {
    let user = effective_user()?;
    if !existing_launch_agents_directory(&user)? {
        return Ok(false);
    }
    let path = launch_agent_path(&user);
    match validate_existing_file(&path, user.uid) {
        Ok(()) => {}
        Err(PathSafetyError::Missing { .. }) => return Ok(false),
        Err(error) => return Err(error.into()),
    }

    let actual = fs::read(&path)
        .with_context(|| format!("reading Console LaunchAgent definition {}", path.display()))?;
    Ok(actual == render_plist(executable, &user.home)?.as_bytes())
}

/// Atomically publish the private LaunchAgent plist, then bootstrap and start it in `gui/<uid>`.
/// The caller owns the session-safety decision before replacing a different definition.
pub async fn install_and_start(processes: &ProcessSupervisor, executable: &Path) -> Result<()> {
    validate_executable(executable)?;
    let user = effective_user()?;
    let directory = launch_agents_directory(&user)?;
    let path = directory.join(PLIST_FILE);

    if definition_matches(processes, executable).await? {
        return start(processes).await;
    }

    let previous = match validate_existing_file(&path, user.uid) {
        Ok(()) => Some(
            fs::read(&path)
                .with_context(|| format!("reading Console LaunchAgent {}", path.display()))?,
        ),
        Err(PathSafetyError::Missing { .. }) => None,
        Err(error) => return Err(error.into()),
    };
    let was_running = matches!(inspect(processes).await?, NativeServiceState::Running);
    bootout_if_registered(processes, &user).await?;
    let definition = render_plist(executable, &user.home)?;
    let writer =
        AtomicFileWriter::new(&directory, ".kit-console-launch-agent.lock", ".kit-console-agent");
    let _lock = writer.lock().context("locking the Console LaunchAgent for replacement")?;
    if let Err(error) = writer.replace(&path, definition.as_bytes()) {
        if was_running {
            start(processes)
                .await
                .context("restarting unchanged Console LaunchAgent after publication failed")?;
        }
        return Err(error).with_context(|| {
            format!("atomically publishing Console LaunchAgent {}", path.display())
        });
    }
    let activation = async {
        validate_existing_file(&path, user.uid)?;
        validate_plist(processes, &path).await?;
        start(processes).await
    }
    .await;
    if let Err(error) = activation {
        return rollback_after_failed_activation(
            processes,
            &writer,
            &path,
            previous,
            was_running,
            error,
        )
        .await;
    }
    Ok(())
}

/// Ensure the private plist is registered in the native GUI domain, then restart the agent.
pub async fn start(processes: &ProcessSupervisor) -> Result<()> {
    let user = effective_user()?;
    if !existing_launch_agents_directory(&user)? {
        bail!("Console LaunchAgent is not installed");
    }
    let path = launch_agent_path(&user);
    validate_existing_file(&path, user.uid)?;
    validate_plist(processes, &path).await?;

    let enable = launchctl(
        processes,
        "enable Console macOS LaunchAgent",
        [OsString::from("enable"), target(&user)],
    )
    .await?;
    if !enable.success {
        bail!(
            "enable Console macOS LaunchAgent failed: {}",
            command_detail(&enable.stdout, &enable.stderr)
        );
    }

    let bootstrap = launchctl(
        processes,
        "bootstrap Console macOS LaunchAgent",
        [OsString::from("bootstrap"), gui_domain(&user), path.into_os_string()],
    )
    .await?;
    if !bootstrap.success {
        let inspection = launchctl(
            processes,
            "inspect existing Console macOS LaunchAgent",
            [OsString::from("print"), target(&user)],
        )
        .await?;
        if !inspection.success {
            bail!(
                "bootstrap Console macOS LaunchAgent failed: {}",
                command_detail(&bootstrap.stdout, &bootstrap.stderr)
            );
        }
    }

    let kickstart = launchctl(
        processes,
        "start Console macOS LaunchAgent",
        [OsString::from("kickstart"), target(&user)],
    )
    .await?;
    if !kickstart.success {
        bail!(
            "start Console macOS LaunchAgent failed: {}",
            command_detail(&kickstart.stdout, &kickstart.stderr)
        );
    }
    Ok(())
}

/// Unregister the service from `gui/<uid>` while retaining its private definition for a later start.
pub async fn stop(processes: &ProcessSupervisor) -> Result<()> {
    let user = effective_user()?;
    let disable = launchctl(
        processes,
        "disable Console macOS LaunchAgent",
        [OsString::from("disable"), target(&user)],
    )
    .await?;
    if !disable.success && !missing_service(&command_detail(&disable.stdout, &disable.stderr)) {
        bail!(
            "disable Console macOS LaunchAgent failed: {}",
            command_detail(&disable.stdout, &disable.stderr)
        );
    }
    let output = launchctl(
        processes,
        "stop Console macOS LaunchAgent",
        [OsString::from("bootout"), target(&user)],
    )
    .await?;
    if output.success || missing_service(&command_detail(&output.stdout, &output.stderr)) {
        return Ok(());
    }
    bail!(
        "stop Console macOS LaunchAgent failed: {}",
        command_detail(&output.stdout, &output.stderr)
    )
}

async fn launchctl(
    processes: &ProcessSupervisor,
    label: &str,
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<command::CommandOutput> {
    command::run(processes, label, LAUNCHCTL, arguments.into_iter().collect()).await
}

fn effective_user() -> Result<EffectiveUser> {
    let uid = unsafe { libc::geteuid() };
    if uid == 0 {
        bail!("Kit Console must be installed from the logged-in macOS user, not root");
    }
    let capacity = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let capacity = if capacity <= 0 { 16_384 } else { usize::try_from(capacity)? };
    let mut buffer = vec![0_u8; capacity];
    let mut passwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            passwd.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() {
        bail!("resolve passwd entry for effective uid {uid}: getpwuid_r returned {status}");
    }
    let passwd = unsafe { passwd.assume_init() };
    if passwd.pw_dir.is_null() {
        bail!("resolve passwd home for effective uid {uid}: passwd entry has no home directory");
    }
    let home = unsafe { CStr::from_ptr(passwd.pw_dir) };
    if home.to_bytes().is_empty() {
        bail!(
            "resolve passwd home for effective uid {uid}: passwd entry has an empty home directory"
        );
    }
    if passwd.pw_shell.is_null() {
        bail!("resolve passwd shell for effective uid {uid}: passwd entry has no shell");
    }
    let shell = unsafe { CStr::from_ptr(passwd.pw_shell) };
    if shell.to_bytes().is_empty() || shell.to_bytes().first() != Some(&b'/') {
        bail!("resolve passwd shell for effective uid {uid}: shell must be an absolute path");
    }
    let shell = PathBuf::from(OsString::from_vec(shell.to_bytes().to_vec()));
    let shell_metadata = fs::symlink_metadata(&shell)
        .with_context(|| format!("inspect passwd shell {}", shell.display()))?;
    if !shell_metadata.file_type().is_file() || shell_metadata.mode() & 0o111 == 0 {
        bail!("passwd shell {} is not an executable file", shell.display());
    }

    let home = PathBuf::from(OsString::from_vec(home.to_bytes().to_vec()));
    if !home.is_absolute() {
        bail!("passwd home for effective uid {uid} must be an absolute path");
    }
    Ok(EffectiveUser { uid, home })
}

fn launch_agent_path(user: &EffectiveUser) -> PathBuf {
    user.home.join("Library").join("LaunchAgents").join(PLIST_FILE)
}

fn launch_agents_directory(user: &EffectiveUser) -> Result<PathBuf> {
    validate_existing_directory(&user.home, user.uid, "passwd home directory")?;
    let library = user.home.join("Library");
    validate_existing_directory(&library, user.uid, "Library directory")?;
    let directory = library.join("LaunchAgents");
    match fs::symlink_metadata(&directory) {
        Ok(_) => validate_existing_directory(&directory, user.uid, "LaunchAgents directory")?,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::create_dir(&directory).with_context(|| {
                format!("creating Console LaunchAgents directory {}", directory.display())
            })?;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).with_context(
                || format!("restricting Console LaunchAgents directory {}", directory.display()),
            )?;
            validate_existing_directory(&directory, user.uid, "LaunchAgents directory")?;
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("inspecting Console LaunchAgents directory {}", directory.display())
            })
        }
    }
    Ok(directory)
}

fn existing_launch_agents_directory(user: &EffectiveUser) -> Result<bool> {
    validate_existing_directory(&user.home, user.uid, "passwd home directory")?;
    let library = user.home.join("Library");
    match fs::symlink_metadata(&library) {
        Ok(_) => validate_existing_directory(&library, user.uid, "Library directory")?,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("inspecting Console Library directory {}", library.display())
            })
        }
    }
    let directory = library.join("LaunchAgents");
    match fs::symlink_metadata(&directory) {
        Ok(_) => validate_existing_directory(&directory, user.uid, "LaunchAgents directory")?,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("inspecting Console LaunchAgents directory {}", directory.display())
            })
        }
    }
    Ok(true)
}

fn gui_domain(user: &EffectiveUser) -> OsString {
    OsString::from(format!("gui/{}", user.uid))
}

fn target(user: &EffectiveUser) -> OsString {
    OsString::from(format!("gui/{}/{}", user.uid, LABEL))
}

fn render_plist(executable: &Path, home: &Path) -> Result<String> {
    let executable = executable
        .to_str()
        .context("render Console LaunchAgent: Kit executable path is not valid UTF-8")?;
    let home =
        home.to_str().context("render Console LaunchAgent: passwd home path is not valid UTF-8")?;
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>{LABEL}</string>\n  <key>ProgramArguments</key>\n  <array>\n    <string>{}</string>\n    <string>console</string>\n    <string>__agent</string>\n  </array>\n  <key>WorkingDirectory</key>\n  <string>{}</string>\n  <key>EnvironmentVariables</key>\n  <dict>\n    <key>HOME</key>\n    <string>{}</string>\n  </dict>\n  <key>LimitLoadToSessionType</key>\n  <array>\n    <string>Aqua</string>\n  </array>\n  <key>KeepAlive</key>\n  <true/>\n  <key>ProcessType</key>\n  <string>Interactive</string>\n</dict>\n</plist>\n",
        xml_escape(executable),
        xml_escape(home),
        xml_escape(home)
    ))
}

fn xml_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '\"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

fn validate_existing_directory(path: &Path, expected_uid: u32, role: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting Console {role} {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("Console {role} {} must not be a symlink", path.display());
    }
    if !metadata.file_type().is_dir() {
        bail!("Console {role} {} is not a directory", path.display());
    }
    validate_owner_and_mode(path, &metadata, expected_uid, 0o022, role)
}

fn validate_existing_file(
    path: &Path,
    expected_uid: u32,
) -> std::result::Result<(), PathSafetyError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err(PathSafetyError::Missing { path: path.to_path_buf() });
        }
        Err(source) => return Err(PathSafetyError::Inspect { path: path.to_path_buf(), source }),
    };
    if metadata.file_type().is_symlink() {
        return Err(PathSafetyError::Symlink { path: path.to_path_buf() });
    }
    if !metadata.file_type().is_file() || metadata.nlink() != 1 {
        return Err(PathSafetyError::NotFile { path: path.to_path_buf() });
    }
    if metadata.uid() != expected_uid {
        return Err(PathSafetyError::WrongOwner {
            path: path.to_path_buf(),
            expected_uid,
            actual_uid: metadata.uid(),
        });
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(PathSafetyError::InsecureMode {
            path: path.to_path_buf(),
            mode: metadata.mode() & 0o7777,
        });
    }
    Ok(())
}

fn validate_executable(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("Console LaunchAgent executable {} must be absolute", path.display());
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting Console LaunchAgent executable {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        bail!("Console LaunchAgent executable {} must be a regular file", path.display());
    }
    if metadata.mode() & 0o111 == 0 {
        bail!("Console LaunchAgent executable {} is not executable", path.display());
    }
    Ok(())
}

fn validate_owner_and_mode(
    path: &Path,
    metadata: &fs::Metadata,
    expected_uid: u32,
    forbidden_mode: u32,
    role: &str,
) -> Result<()> {
    if metadata.uid() != expected_uid {
        bail!(
            "Console {role} {} is owned by uid {}, expected uid {expected_uid}",
            path.display(),
            metadata.uid()
        );
    }
    if metadata.mode() & forbidden_mode != 0 {
        bail!(
            "Console {role} {} has insecure permissions {:o}",
            path.display(),
            metadata.mode() & 0o7777
        );
    }
    Ok(())
}

async fn bootout_if_registered(processes: &ProcessSupervisor, user: &EffectiveUser) -> Result<()> {
    let output = launchctl(
        processes,
        "unregister existing Console macOS LaunchAgent",
        [OsString::from("bootout"), target(user)],
    )
    .await?;
    if output.success || missing_service(&command_detail(&output.stdout, &output.stderr)) {
        return Ok(());
    }
    bail!(
        "unregister existing Console macOS LaunchAgent failed: {}",
        command_detail(&output.stdout, &output.stderr)
    )
}

async fn validate_plist(processes: &ProcessSupervisor, path: &Path) -> Result<()> {
    let output = command::run(
        processes,
        "validate Console macOS LaunchAgent plist",
        PLUTIL,
        vec![OsString::from("-lint"), OsString::from("--"), path.as_os_str().to_owned()],
    )
    .await?;
    if output.success {
        return Ok(());
    }
    bail!(
        "validate Console macOS LaunchAgent plist failed: {}",
        command_detail(&output.stdout, &output.stderr)
    )
}

async fn rollback_after_failed_activation(
    processes: &ProcessSupervisor,
    writer: &AtomicFileWriter,
    path: &Path,
    previous: Option<Vec<u8>>,
    was_running: bool,
    activation_error: anyhow::Error,
) -> Result<()> {
    stop(processes).await.context("unregistering failed Console LaunchAgent during rollback")?;
    match previous {
        Some(previous) => writer
            .replace(path, &previous)
            .with_context(|| format!("restoring Console LaunchAgent {}", path.display()))?,
        None => fs::remove_file(path)
            .with_context(|| format!("removing failed Console LaunchAgent {}", path.display()))?,
    }
    if was_running {
        start(processes).await.context("restarting restored Console LaunchAgent")?;
    }
    Err(activation_error).context("activating Console LaunchAgent; restored previous definition")
}

fn state_from_print(stdout: &str, stderr: &str) -> NativeServiceState {
    let detail = command_detail(stdout, stderr);
    let lower = detail.to_ascii_lowercase();
    if lower.contains("state = running") || lower.contains("\npid =") {
        NativeServiceState::Running
    } else if lower.contains("last exit code =") && !lower.contains("last exit code = 0") {
        NativeServiceState::Failed { detail }
    } else {
        NativeServiceState::Stopped
    }
}

fn command_detail(stdout: &str, stderr: &str) -> String {
    match (stdout.trim(), stderr.trim()) {
        ("", "") => String::from("launchctl returned no diagnostic output"),
        (stdout, "") => stdout.to_owned(),
        ("", stderr) => stderr.to_owned(),
        (stdout, stderr) => format!("{stdout}\n{stderr}"),
    }
}

fn missing_service(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    detail.contains("could not find service")
        || detail.contains("no such process")
        || detail.contains("service not found")
}

fn unavailable_gui_domain(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    detail.contains("could not find domain")
        || detail.contains("domain does not support")
        || detail.contains("operation not permitted")
}

#[derive(Debug, Error)]
enum PathSafetyError {
    #[error("Console LaunchAgent {} is not installed", path.display())]
    Missing { path: PathBuf },
    #[error("Console LaunchAgent {} must not be a symlink", path.display())]
    Symlink { path: PathBuf },
    #[error("Console LaunchAgent {} is not a regular file", path.display())]
    NotFile { path: PathBuf },
    #[error(
        "Console LaunchAgent {} is owned by uid {actual_uid}, expected uid {expected_uid}",
        path.display()
    )]
    WrongOwner { path: PathBuf, expected_uid: u32, actual_uid: u32 },
    #[error("Console LaunchAgent {} has insecure permissions {mode:o}", path.display())]
    InsecureMode { path: PathBuf, mode: u32 },
    #[error("inspect Console LaunchAgent {path}: {source}", path = path.display())]
    Inspect { path: PathBuf, source: std::io::Error },
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{render_plist, xml_escape, LABEL};

    #[test]
    fn plist_uses_direct_agent_arguments_without_environment_forwarding() {
        let plist =
            render_plist(Path::new("/Applications/Kit & Co/kit"), Path::new("/Users/Kit & Co"))
                .unwrap();

        assert!(plist.contains("<string>/Applications/Kit &amp; Co/kit</string>"));
        assert!(plist.contains("<string>console</string>"));
        assert!(plist.contains("<string>__agent</string>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(plist.contains("<key>EnvironmentVariables</key>"));
        assert!(plist.contains("<key>HOME</key>"));
        assert!(plist.contains("<string>/Users/Kit &amp; Co</string>"));
        assert!(!plist.contains("SSH_AUTH_SOCK"));
        assert!(!plist.contains("GH_TOKEN"));
        assert!(!plist.contains("CLAUDE"));
        assert!(plist.contains(&format!("<string>{LABEL}</string>")));
    }

    #[test]
    fn plist_escaping_cannot_create_markup() {
        assert_eq!(xml_escape("<&>\"'"), "&lt;&amp;&gt;&quot;&apos;");
    }
}
