#![cfg(any(target_os = "linux", target_os = "macos"))]

mod support;

use std::path::Path;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};
use std::{fs, os::unix::fs::PermissionsExt};

use anyhow::{bail, ensure, Context, Result};

use support::console::{
    HeadlessConsoleClient, LocalConsoleHarness, PublicConsole, PublicConsoleOptions,
};

const READY_TIMEOUT: Duration = Duration::from_secs(8);
const RESIZED_COLS: u16 = 150;
const RESIZED_ROWS: u16 = 48;

async fn live_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    static GUARD: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    GUARD.get_or_init(|| tokio::sync::Mutex::new(())).lock().await
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

fn output_tail(console: &PublicConsole) -> Result<String> {
    let output = console.output_snapshot()?;
    let start = output.char_indices().rev().nth(2_000).map_or(0, |(index, _)| index);
    Ok(output[start..].to_owned())
}

fn sgr_mouse(button: u8, column: u16, row: u16, release: bool) -> Vec<u8> {
    format!("\x1b[<{button};{};{}{}", column + 1, row + 1, if release { 'm' } else { 'M' })
        .into_bytes()
}

fn invoke_command(console: &mut PublicConsole, query: &str) -> Result<()> {
    console.clear_output()?;
    console.send(b"\x10")?;
    console.wait_for_output(b"Commands")?;
    console.type_text(query)?;
    console.send(b"\r")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_console_discovers_this_machine_and_opens_command_palette() -> Result<()> {
    let _live_test_guard = live_test_guard().await;
    wezterm_config::designate_this_as_the_main_thread();
    wezterm_config::common_init(None, &[], true)
        .context("initialize headless Console verifier config")?;

    let fixture_root =
        std::env::temp_dir().join(format!("kit-console-tailnet-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&fixture_root).context("create tailnet fixture directory")?;
    let tailscale = fixture_root.join("tailscale");
    let operating_system = if cfg!(target_os = "macos") { "macOS" } else { "linux" };
    let tailnet_status = format!(
        r#"{{"BackendState":"Running","TailscaleIPs":["100.64.0.1"],"Self":{{"ID":"local-verifier","DNSName":"local-verifier.test.ts.net.","HostName":"local-verifier","OS":"{operating_system}","Online":true,"TailscaleIPs":["100.64.0.1"]}},"Peer":{{"peer-verifier":{{"ID":"peer-verifier","DNSName":"slow-peer.test.ts.net.","HostName":"slow-peer","OS":"linux","Online":true,"TailscaleIPs":["100.64.0.2"]}}}}}}"#
    );
    fs::write(&tailscale, format!("#!/bin/sh\nprintf '%s\\n' '{tailnet_status}'\n"))
        .context("write tailnet fixture")?;
    fs::set_permissions(&tailscale, fs::Permissions::from_mode(0o755))
        .context("make tailnet fixture executable")?;
    let ssh = fixture_root.join("ssh");
    fs::write(&ssh, "#!/bin/sh\nsleep 30\n").context("write slow OpenSSH fixture")?;
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755))
        .context("make slow OpenSSH fixture executable")?;

    let mut harness = LocalConsoleHarness::start().await?;
    let mut console = PublicConsole::start(
        &harness,
        PublicConsoleOptions {
            config_toml: Some("[users]\npeer-verifier = \"tvx\"\n".to_owned()),
            direct_machine: None,
            path_prefix: Some(fixture_root.clone()),
            ..PublicConsoleOptions::default()
        },
    )?;
    console.wait_for_output(b"local-verifier")?;
    console.wait_for_output(b"slow-peer")?;
    console.clear_output()?;
    console.send(&sgr_mouse(2, 10, 3, false))?;
    console.wait_for_output(b"Set Unix user")?;
    console.clear_output()?;
    console.send(b"\x1b")?;
    console.wait_for_output(b"\x1b[?25l")?;
    console.clear_output()?;
    console.send(b"\x10")?;
    console.wait_for_output(b"Commands")?;
    console.clear_output()?;
    console.type_text("open console settings")?;
    console.send(b"\r")?;
    console.wait_for_output(b"operator preferences")?;
    console.clear_output()?;
    console.send(b"q")?;
    console.wait_for_output(b"\x1b[?25l")?;
    let output = console.finish_with(b"q")?;
    ensure!(output.windows(b"\x1b[?1049l".len()).any(|window| window == b"\x1b[?1049l"));

    harness.shutdown()?;
    fs::remove_dir_all(&fixture_root).context("remove tailnet fixture")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_console_honors_custom_prefix_sequences() -> Result<()> {
    let _live_test_guard = live_test_guard().await;
    wezterm_config::designate_this_as_the_main_thread();
    wezterm_config::common_init(None, &[], true)
        .context("initialize headless Console verifier config")?;

    let mut harness = LocalConsoleHarness::start().await?;
    let mut console = PublicConsole::start(
        &harness,
        PublicConsoleOptions {
            config_toml: Some(
                "[keybindings]\nprefix = \"Ctrl+A\"\nnew_session = \"c\"\nquit = \"q\"\n"
                    .to_owned(),
            ),
            ..PublicConsoleOptions::default()
        },
    )?;

    console.send(b"\x01c")?;
    let observer = HeadlessConsoleClient::connect(&harness).await?;
    if let Err(error) = observer.wait_for_session_count(1).await {
        bail!("{error:#}; output={:?}", console.output_snapshot()?);
    }
    drop(observer);
    console.finish_with(b"\x01q")?;
    harness.shutdown()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_console_command_palette_opens_mouse_editable_settings() -> Result<()> {
    let _live_test_guard = live_test_guard().await;
    wezterm_config::designate_this_as_the_main_thread();
    wezterm_config::common_init(None, &[], true)
        .context("initialize headless Console verifier config")?;

    let mut harness = LocalConsoleHarness::start().await?;
    let mut console = PublicConsole::start(&harness, PublicConsoleOptions::default())?;
    invoke_command(&mut console, "open console settings")?;
    console.wait_for_output(b"operator preferences")?;

    console.clear_output()?;
    console.send(&sgr_mouse(0, 27, 2, false))?;
    console.send(&sgr_mouse(0, 27, 2, true))?;
    console.wait_for_output(b"Saved")?;
    console.send(b"q")?;

    let config = std::fs::read_to_string(console.config_path())
        .context("read Console config after mouse Settings edit")?;
    ensure!(
        config.contains("sidebar_split_ratio = 360"),
        "mouse Settings edit did not persist the wide sidebar ratio: {config:?}"
    );

    console.finish_with(b"\x02q")?;
    harness.shutdown()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_console_drives_keyboard_mouse_history_resize_paste_and_clipboard() -> Result<()> {
    let _live_test_guard = live_test_guard().await;
    wezterm_config::designate_this_as_the_main_thread();
    wezterm_config::common_init(None, &[], true)
        .context("initialize headless Console verifier config")?;

    let mut harness = LocalConsoleHarness::start().await?;
    let mut console = PublicConsole::start(&harness, PublicConsoleOptions::default())?;
    let nonce_source = uuid::Uuid::new_v4().simple().to_string();
    let nonce = &nonce_source[..12];

    console.clear_output()?;
    console.send(b"\x02n")?;
    let first_receipt = std::env::temp_dir().join(format!("kitc-{nonce}"));
    let first_marker = format!("KIT_TUI_FIRST_{nonce}");
    let first_command = format!("printf '{first_marker}\\n'; : > '{}'", first_receipt.display());
    console.send(format!("\x1b[200~{first_command}\x1b[201~").as_bytes())?;
    console.send(b"\n")?;
    wait_for_file(&first_receipt)?;
    std::fs::remove_file(&first_receipt).context("remove Console input receipt")?;

    console.clear_output()?;
    console.send(b"\x02n")?;

    console.clear_output()?;
    console.send(b"\x02st")?;
    console.wait_for_any_output(&[b"you", b"Already controlling"])?;
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
    // Rename is a sidebar-owned interaction and deliberately leaves focus in the sessions panel,
    // so history navigation is immediately available without toggling/collapsing that panel.
    console.send(b"\x1b[D")?;
    console.wait_for_output(first_marker.as_bytes())?;
    console.clear_output()?;
    console.send(b"t")?;
    console.wait_for_any_output(&[b"you", b"Already controlling"])?;
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
    console.send(b"\x02s\x1b[C")?;
    console.wait_for_output(title_suffix.as_bytes())?;
    console.clear_output()?;
    console.send(b"t")?;
    console.wait_for_any_output(&[b"you", b"Already controlling"])?;
    console.send(b"\x1b[1;5C")?;
    let history_forward = format!("KIT_HISTORY_FORWARD_{nonce}");
    let history_forward_receipt = std::env::temp_dir().join(format!("kitc-forward-{nonce}"));
    let history_forward_command =
        format!("printf '{history_forward}\\n'; : > '{}'", history_forward_receipt.display());
    console.send(format!("\x1b[200~{history_forward_command}\x1b[201~").as_bytes())?;
    console.send(b"\n")?;
    wait_for_file(&history_forward_receipt)?;
    std::fs::remove_file(&history_forward_receipt).context("remove history-forward receipt")?;

    console.resize(RESIZED_COLS, RESIZED_ROWS)?;
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

    console.send(&sgr_mouse(2, 2, 1, false))?;
    thread::sleep(Duration::from_millis(100));
    console.send(&sgr_mouse(0, 3, 8, false))?;
    console.send(&sgr_mouse(0, 3, 8, true))?;
    console.wait_for_output(b"\x1b]52;c;")?;

    // A second real TUI must remain useful while observing and must transfer control through the
    // same keyboard/mouse action surface without entering the old dead-notice state.
    let mut peer = PublicConsole::start(&harness, PublicConsoleOptions::default())?;
    peer.wait_for_output(b"read-only")?;
    peer.clear_output()?;
    peer.send(&sgr_mouse(0, 50, 8, false))?;
    peer.send(&sgr_mouse(0, 50, 8, true))?;
    thread::sleep(Duration::from_millis(100));
    let peer_projection = peer.output_snapshot()?;
    ensure!(
        !peer_projection.contains("take control to use the mouse"),
        "read-only terminal click regressed to the dead observer notice: {peer_projection:?}"
    );

    console.clear_output()?;
    peer.send(b"t")?;
    peer.wait_for_output(b"you")?;
    console.wait_for_output(b"read-only")?;

    let peer_output = peer.finish()?;
    ensure!(peer_output.windows(b"\x1b[?1049l".len()).any(|w| w == b"\x1b[?1049l"));

    console.clear_output()?;
    console.send(b"\x02s\x02s")?;
    console.wait_for_output(b"Hide sessions")?;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_console_closes_the_focused_session_without_freezing() -> Result<()> {
    let _live_test_guard = live_test_guard().await;
    wezterm_config::designate_this_as_the_main_thread();
    wezterm_config::common_init(None, &[], true)
        .context("initialize headless Console verifier config")?;

    let mut harness = LocalConsoleHarness::start().await?;
    let mut console = PublicConsole::start(&harness, PublicConsoleOptions::default())?;
    invoke_command(&mut console, "new session")?;
    let observer = HeadlessConsoleClient::connect(&harness).await?;
    observer.wait_for_session_count(1).await?;
    drop(observer);
    console.send(b"\x02n")?;
    let observer = HeadlessConsoleClient::connect(&harness).await?;
    observer.wait_for_session_count(2).await?;
    drop(observer);

    invoke_command(&mut console, "close session")?;
    let observer = HeadlessConsoleClient::connect(&harness).await?;
    observer.wait_for_session_count(1).await?;
    drop(observer);
    let first_receipt =
        std::env::temp_dir().join(format!("kit-console-close-{}", uuid::Uuid::new_v4()));
    console.send(format!("\x1b[200~: > '{}'\x1b[201~\n", first_receipt.display()).as_bytes())?;
    if let Err(error) = wait_for_file(&first_receipt) {
        bail!(
            "{error:#}; agent={:?}; recent output={:?}",
            harness.diagnostics(),
            output_tail(&console)?
        );
    }
    std::fs::remove_file(&first_receipt).context("remove post-close input receipt")?;

    invoke_command(&mut console, "close session")?;
    let observer = HeadlessConsoleClient::connect(&harness).await?;
    if let Err(error) = observer.wait_for_session_count(0).await {
        bail!("{error:#}; recent output={:?}", output_tail(&console)?);
    }
    drop(observer);
    console.wait_for_output(b"No sessions yet")?;

    invoke_command(&mut console, "new session")?;
    let observer = HeadlessConsoleClient::connect(&harness).await?;
    let replacement = observer.wait_for_session_count(1).await?[0];
    drop(observer);
    let replacement_receipt =
        std::env::temp_dir().join(format!("kit-console-replacement-{}", uuid::Uuid::new_v4()));
    console
        .send(format!("\x1b[200~: > '{}'\x1b[201~\n", replacement_receipt.display()).as_bytes())?;
    wait_for_file(&replacement_receipt)
        .context("Console could not create and drive a replacement session")?;
    std::fs::remove_file(&replacement_receipt).context("remove replacement input receipt")?;

    let output = console.finish_with(b"\x02q")?;
    ensure!(output.windows(b"\x1b[?1049l".len()).any(|window| window == b"\x1b[?1049l"));
    let observer = HeadlessConsoleClient::connect(&harness).await?;
    observer.close_pane(replacement.pane_id).await?;
    observer.wait_for_session_count(0).await?;
    drop(observer);
    harness.shutdown()
}
