#![cfg(any(target_os = "linux", target_os = "macos"))]

mod support;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

use support::console::{HeadlessConsoleClient, LocalConsoleHarness};

const READY_TIMEOUT: Duration = Duration::from_secs(8);
const EXIT_TIMEOUT: Duration = Duration::from_secs(3);

struct PublicConsole {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    reader: Option<thread::JoinHandle<()>>,
    output: Arc<Mutex<Vec<u8>>>,
    config_root: PathBuf,
}

impl PublicConsole {
    fn start(harness: &LocalConsoleHarness) -> Result<Self> {
        let config_root = harness.runtime_root().join("config");
        std::fs::create_dir(&config_root).context("create isolated Console config root")?;
        let pty = native_pty_system();
        let pair = pty.openpty(PtySize { rows: 40, cols: 120, pixel_width: 0, pixel_height: 0 })?;
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_kit"));
        command.arg("console");
        command.env("TERM", "xterm-256color");
        command.env("RUST_BACKTRACE", "1");
        command.env("RUST_LOG", "wezterm_client=warn");
        command.env("KIT_CONSOLE_RUNTIME_DIR", harness.runtime_root());
        command.env("XDG_CONFIG_HOME", &config_root);
        let child = pair.slave.spawn_command(command).context("start public kit console")?;
        drop(pair.slave);

        let mut source = pair.master.try_clone_reader().context("clone Console PTY reader")?;
        let writer = pair.master.take_writer().context("take Console PTY writer")?;
        let output = Arc::new(Mutex::new(Vec::new()));
        let reader_output = Arc::clone(&output);
        let reader = thread::Builder::new()
            .name("kit-console-tui-verifier-reader".to_owned())
            .spawn(move || {
                let mut buffer = [0_u8; 4096];
                while let Ok(count) = source.read(&mut buffer) {
                    if count == 0 {
                        break;
                    }
                    if let Ok(mut output) = reader_output.lock() {
                        output.extend_from_slice(&buffer[..count]);
                    }
                }
            })
            .context("start Console PTY reader")?;
        let mut console =
            Self { master: pair.master, writer, child, reader: Some(reader), output, config_root };
        // Crossterm probes the cursor with DSR while constructing the Ratatui terminal. A real
        // terminal answers this; the verifier is the terminal for this PTY.
        console.send(b"\x1b[1;1R")?;
        console.wait_for_output(b"kit console")?;
        console.wait_for_output(b"\x1b[?1049h")?;
        Ok(console)
    }

    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes).context("write Console PTY input")?;
        self.writer.flush().context("flush Console PTY input")
    }

    fn type_text(&mut self, text: &str) -> Result<()> {
        for byte in text.bytes() {
            self.send(&[byte])?;
            thread::sleep(Duration::from_millis(15));
        }
        Ok(())
    }

    fn clear_output(&self) -> Result<()> {
        self.output
            .lock()
            .map_err(|_| anyhow::anyhow!("Console PTY output lock was poisoned"))?
            .clear();
        Ok(())
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
    }

    fn wait_for_output(&mut self, needle: &[u8]) -> Result<()> {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            let found = self
                .output
                .lock()
                .map_err(|_| anyhow::anyhow!("Console PTY output lock was poisoned"))?
                .windows(needle.len())
                .any(|window| window == needle);
            if found {
                return Ok(());
            }
            if let Some(status) = self.child.try_wait().context("observe public Console")? {
                let output = self
                    .output
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Console PTY output lock was poisoned"))?;
                bail!(
                    "public Console exited before producing {:?}: {status}; output={:?}",
                    String::from_utf8_lossy(needle),
                    String::from_utf8_lossy(&output)
                );
            }
            if Instant::now() >= deadline {
                let output = self
                    .output
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Console PTY output lock was poisoned"))?;
                bail!(
                    "public Console did not produce {:?}; output={:?}",
                    String::from_utf8_lossy(needle),
                    String::from_utf8_lossy(&output)
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_any_output(&mut self, needles: &[&[u8]]) -> Result<usize> {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            let output = self
                .output
                .lock()
                .map_err(|_| anyhow::anyhow!("Console PTY output lock was poisoned"))?;
            if let Some(index) = needles
                .iter()
                .position(|needle| output.windows(needle.len()).any(|window| window == *needle))
            {
                return Ok(index);
            }
            drop(output);
            if let Some(status) = self.child.try_wait().context("observe public Console")? {
                let output = self
                    .output
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Console PTY output lock was poisoned"))?;
                bail!(
                    "public Console exited before expected control state: {status}; output={:?}",
                    String::from_utf8_lossy(&output)
                )
            }
            if Instant::now() >= deadline {
                let output = self
                    .output
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Console PTY output lock was poisoned"))?;
                bail!(
                    "public Console did not produce any expected control state; output={:?}",
                    String::from_utf8_lossy(&output)
                )
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn config_path(&self) -> PathBuf {
        self.config_root.join("kit/console.toml")
    }

    fn output_snapshot(&self) -> Result<String> {
        let output = self
            .output
            .lock()
            .map_err(|_| anyhow::anyhow!("Console PTY output lock was poisoned"))?;
        Ok(String::from_utf8_lossy(&output).into_owned())
    }

    fn finish(mut self) -> Result<Vec<u8>> {
        self.send(b"\x02\x11")?;
        let deadline = Instant::now() + EXIT_TIMEOUT;
        let status = loop {
            if let Some(status) = self.child.try_wait().context("observe Console exit")? {
                break status;
            }
            if Instant::now() >= deadline {
                self.child.kill().context("kill unresponsive public Console")?;
                break self.child.wait().context("reap killed public Console")?;
            }
            thread::sleep(Duration::from_millis(10));
        };
        ensure!(status.success(), "public Console exited unsuccessfully: {status}");
        let _ = self.master.cancel_reader();
        if let Some(reader) = self.reader.take() {
            reader.join().map_err(|_| anyhow::anyhow!("Console PTY reader panicked"))?;
        }
        let output = self
            .output
            .lock()
            .map_err(|_| anyhow::anyhow!("Console PTY output lock was poisoned"))?
            .clone();
        Ok(output)
    }
}

impl Drop for PublicConsole {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = self.master.cancel_reader();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn wait_for_file(path: &Path) -> Result<()> {
    let deadline = Instant::now() + READY_TIMEOUT;
    while !path.exists() {
        if Instant::now() >= deadline {
            bail!("Console terminal input did not create {}", path.display());
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

fn sgr_mouse(button: u8, column: u16, row: u16, release: bool) -> Vec<u8> {
    format!("\x1b[<{button};{};{}{}", column + 1, row + 1, if release { 'm' } else { 'M' })
        .into_bytes()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_console_drives_keyboard_mouse_history_resize_paste_and_clipboard() -> Result<()> {
    wezterm_config::designate_this_as_the_main_thread();
    wezterm_config::common_init(None, &[], true)
        .context("initialize headless WezTerm verifier config")?;

    let mut harness = LocalConsoleHarness::start()?;
    let mut console = PublicConsole::start(&harness)?;
    let nonce_source = uuid::Uuid::new_v4().simple().to_string();
    let nonce = &nonce_source[..12];

    console.clear_output()?;
    console.send(b"\x0e")?;
    if console.wait_for_any_output(&[b"attached", b"observing"])? == 1 {
        console.clear_output()?;
        console.send(b"\x02t")?;
        console.wait_for_output(b"attached")?;
    }
    let first_receipt = std::env::temp_dir().join(format!("kitc-{nonce}"));
    let first_marker = format!("KIT_TUI_FIRST_{nonce}");
    let first_command = format!("printf '{first_marker}\\n'; : > '{}'", first_receipt.display());
    console.send(format!("\x1b[200~{first_command}\x1b[201~").as_bytes())?;
    console.send(b"\n")?;
    wait_for_file(&first_receipt)?;
    std::fs::remove_file(&first_receipt).context("remove Console input receipt")?;

    console.clear_output()?;
    console.send(b"\x02\x0e")?;
    if console.wait_for_any_output(&[b"attached", b"observing"])? == 1 {
        console.clear_output()?;
        console.send(b"\x02t")?;
        console.wait_for_output(b"attached")?;
    }

    console.clear_output()?;
    console.send(b"\x02t")?;
    console.wait_for_any_output(&[b"attached", b"already has control"])?;
    console.send(b"\x1bOQ")?;
    let title_suffix = format!("-{}", &nonce[..4]);
    let shell_path = std::env::var_os("SHELL").context("the Console verifier requires SHELL")?;
    let shell_name = Path::new(&shell_path)
        .file_name()
        .and_then(|name| name.to_str())
        .context("the Console verifier requires a UTF-8 SHELL basename")?;
    let title = format!("{shell_name}{title_suffix}");
    console.type_text(&title_suffix)?;
    console.send(b"\r")?;
    console.wait_for_output(title_suffix.as_bytes())?;

    console.clear_output()?;
    console.send(b"\x02\x1b[D")?;
    console.wait_for_output(first_marker.as_bytes())?;
    console.clear_output()?;
    console.send(b"t")?;
    console.wait_for_any_output(&[b"attached", b"already has control"])?;
    console.send(b"\x1b[1;5C")?;
    let history_back = format!("KIT_HISTORY_BACK_{nonce}");
    let history_back_receipt = std::env::temp_dir().join(format!("kitc-back-{nonce}"));
    let history_back_command =
        format!("printf '{history_back}\\n'; : > '{}'", history_back_receipt.display());
    console.send(format!("\x1b[200~{history_back_command}\x1b[201~").as_bytes())?;
    console.send(b"\n")?;
    if let Err(error) = wait_for_file(&history_back_receipt) {
        bail!("{error:#}; output={:?}", console.output_snapshot()?);
    }
    std::fs::remove_file(&history_back_receipt).context("remove history-back receipt")?;

    console.clear_output()?;
    console.send(b"\x02\x1b[C")?;
    console.wait_for_output(title_suffix.as_bytes())?;
    console.clear_output()?;
    console.send(b"t")?;
    console.wait_for_any_output(&[b"attached", b"already has control"])?;
    console.send(b"\x1b[1;5C")?;
    let history_forward = format!("KIT_HISTORY_FORWARD_{nonce}");
    let history_forward_receipt = std::env::temp_dir().join(format!("kitc-forward-{nonce}"));
    let history_forward_command =
        format!("printf '{history_forward}\\n'; : > '{}'", history_forward_receipt.display());
    console.send(format!("\x1b[200~{history_forward_command}\x1b[201~").as_bytes())?;
    console.send(b"\n")?;
    wait_for_file(&history_forward_receipt)?;
    std::fs::remove_file(&history_forward_receipt).context("remove history-forward receipt")?;

    console.resize(150, 48)?;
    thread::sleep(Duration::from_millis(150));

    console.send(&sgr_mouse(0, 45, 8, false))?;
    console.send(&sgr_mouse(32, 52, 8, false))?;
    console.send(&sgr_mouse(0, 52, 8, true))?;
    console.send(&sgr_mouse(0, 39, 15, false))?;
    console.send(&sgr_mouse(32, 50, 15, false))?;
    console.send(&sgr_mouse(0, 50, 15, true))?;
    let config_path = console.config_path();
    wait_for_file(&config_path)?;
    let config = std::fs::read_to_string(&config_path).context("read persisted Console divider")?;
    ensure!(config.contains("sidebar_split_ratio"), "divider drag did not persist its ratio");

    console.send(&sgr_mouse(2, 2, 3, false))?;
    thread::sleep(Duration::from_millis(100));
    console.send(&sgr_mouse(0, 3, 11, false))?;
    console.send(&sgr_mouse(0, 3, 11, true))?;
    console.wait_for_output(b"\x1b]52;c;")?;

    let output = console.finish()?;
    ensure!(output.windows(b"\x1b[?1049l".len()).any(|w| w == b"\x1b[?1049l"));

    let observer = HeadlessConsoleClient::connect(&harness).await?;
    let sessions = observer.wait_for_session_count(2).await?;
    let first = sessions[0];
    let second = sessions[1];
    let first_output = observer.pane_text(first.pane_id).await?;
    let second_output = observer.pane_text(second.pane_id).await?;
    ensure!(first_output.contains(&first_marker), "first session lost its keyboard input");
    ensure!(
        first_output.contains(&history_back),
        "history back did not select the first session; first={first_output:?} second={second_output:?}"
    );
    ensure!(
        second_output.contains(&history_forward),
        "history forward did not restore the second session; first={first_output:?} second={second_output:?}"
    );
    observer.wait_for_title(second.tab_id, &title).await?;
    let after =
        observer.wait_for_dimensions(second.pane_id, |cols, rows| cols > 86 && rows > 34).await?;
    ensure!(after.0 > 86 && after.1 > 34, "PTY resize did not reach the mux");

    drop(observer);
    harness.shutdown()
}
