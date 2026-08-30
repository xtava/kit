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
    console.send(format!("\x1b[200~{query}\x1b[201~").as_bytes())?;
    console.wait_for_output(query.as_bytes())?;
    console.send(b"\r")
}

fn copied_terminal_text(console: &mut PublicConsole) -> Result<String> {
    invoke_command(console, "copy visible terminal")?;
    console.wait_for_output(b"\x1b]52;c;")?;
    let output = console.output_snapshot()?;
    let encoded = output
        .rsplit_once("\x1b]52;c;")
        .map(|(_, payload)| payload)
        .and_then(|payload| payload.split('\x07').next())
        .context("Console copy did not contain a complete OSC 52 payload")?;
    String::from_utf8(decode_base64(encoded)?).context("Console copied non-UTF-8 terminal text")
}

fn decode_base64(encoded: &str) -> Result<Vec<u8>> {
    fn value(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let mut decoded = Vec::with_capacity(encoded.len() / 4 * 3);
    for chunk in encoded.as_bytes().chunks(4) {
        ensure!(chunk.len() == 4, "invalid OSC 52 base64 length");
        let mut sextets = [0_u8; 4];
        let mut padding = 0;
        for (index, byte) in chunk.iter().copied().enumerate() {
            if byte == b'=' {
                padding += 1;
            } else {
                sextets[index] = value(byte).context("invalid OSC 52 base64 character")?;
            }
        }
        let bits = u32::from(sextets[0]) << 18
            | u32::from(sextets[1]) << 12
            | u32::from(sextets[2]) << 6
            | u32::from(sextets[3]);
        decoded.push((bits >> 16) as u8);
        if padding < 2 {
            decoded.push((bits >> 8) as u8);
        }
        if padding == 0 {
            decoded.push(bits as u8);
        }
    }
    Ok(decoded)
}

fn scroll_marker_rows(text: &str) -> Vec<usize> {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("KIT_SCROLL_"))
        .filter_map(|number| number.parse().ok())
        .collect()
}

fn byte_count(bytes: &[u8], needle: u8) -> usize {
    bytes.iter().filter(|byte| **byte == needle).count()
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
        r#"{{"BackendState":"Running","TailscaleIPs":["100.64.0.1"],"Self":{{"ID":"local-verifier","UserID":42,"DNSName":"local-verifier.test.ts.net.","HostName":"local-verifier","OS":"{operating_system}","Online":true,"TailscaleIPs":["100.64.0.1"]}},"Peer":{{"peer-verifier":{{"ID":"peer-verifier","UserID":42,"DNSName":"slow-peer.test.ts.net.","HostName":"slow-peer","OS":"linux","Online":true,"TailscaleIPs":["100.64.0.2"]}}}}}}"#
    );
    fs::write(&tailscale, format!("#!/bin/sh\nprintf '%s\\n' '{tailnet_status}'\n"))
        .context("write tailnet fixture")?;
    fs::set_permissions(&tailscale, fs::Permissions::from_mode(0o755))
        .context("make tailnet fixture executable")?;
    let mut harness = LocalConsoleHarness::start().await?;
    let mut console = PublicConsole::start(
        &harness,
        PublicConsoleOptions {
            direct_machine: None,
            path_prefix: Some(fixture_root.clone()),
            ..PublicConsoleOptions::default()
        },
    )?;
    console.wait_for_output(b"local-verifier")?;
    console.wait_for_output(b"slow-peer")?;
    console.clear_output()?;
    console.send(&sgr_mouse(2, 10, 3, false))?;
    console.wait_for_output(b"Connect")?;
    console.clear_output()?;
    console.send(b"\x1b")?;
    console.wait_for_output(b"\x1b[?25l")?;
    console.clear_output()?;
    console.send(b"\x10")?;
    console.wait_for_output(b"Commands")?;
    console.clear_output()?;
    console.type_text("open console settings")?;
    console.send(b"\r")?;
    console.wait_for_output(b"Persistent terminal session presentation")?;
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
    // Let the just-exited client connection drain before asking the mux owner to join. Sending
    // SIGTERM in the same scheduler slice can leave the listener waiting on that final disconnect.
    thread::sleep(Duration::from_millis(150));
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
    console.wait_for_output(b"Persistent terminal session presentation")?;

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
async fn public_console_replaces_synchronized_alternate_screen_viewports() -> Result<()> {
    let _live_test_guard = live_test_guard().await;
    wezterm_config::designate_this_as_the_main_thread();
    wezterm_config::common_init(None, &[], true)
        .context("initialize headless Console verifier config")?;

    let mut harness = LocalConsoleHarness::start().await?;
    let enter = harness.runtime_root().join("enter-alt-screen");
    let leave = harness.runtime_root().join("leave-alt-screen");
    let script = format!(
        "printf 'PRIMARY_SCREEN\\n'; \
         while [ ! -e '{}' ]; do sleep 0.02; done; \
         printf '\\033[?1049h\\033[?2026h\\033[2J\\033[HALTERNATE_SCREEN\\033[?2026l'; \
         while [ ! -e '{}' ]; do sleep 0.02; done; \
         printf '\\033[?2026h\\033[?1049l\\033[?2026l'; \
         sleep 30",
        enter.display(),
        leave.display()
    );
    let bootstrap = HeadlessConsoleClient::connect(&harness).await?;
    bootstrap.spawn_script(script).await?;
    drop(bootstrap);

    let mut console = PublicConsole::start(&harness, PublicConsoleOptions::default())?;
    console.wait_for_output(b"PRIMARY_SCREEN")?;

    console.clear_output()?;
    fs::write(&enter, []).context("trigger alternate-screen transition")?;
    console.wait_for_output(b"ALTERNATE_SCREEN")?;

    console.clear_output()?;
    fs::write(&leave, []).context("trigger primary-screen restoration")?;
    console.wait_for_output(b"PRIMARY_SCREEN")?;

    console.finish()?;
    harness.shutdown()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_console_scrolls_wheel_by_lines_and_anchors_history() -> Result<()> {
    let _live_test_guard = live_test_guard().await;
    wezterm_config::designate_this_as_the_main_thread();
    wezterm_config::common_init(None, &[], true)
        .context("initialize headless Console verifier config")?;

    let mut harness = LocalConsoleHarness::start().await?;
    let append = harness.runtime_root().join("append-scroll-output");
    let script = format!(
        "i=0; while [ \"$i\" -lt 100 ]; do printf 'KIT_SCROLL_%04d\\n' \"$i\"; i=$((i + 1)); done; \
         while [ ! -e '{}' ]; do sleep 0.02; done; \
         while [ \"$i\" -lt 105 ]; do printf 'KIT_SCROLL_%04d\\n' \"$i\"; i=$((i + 1)); done; \
         sleep 30",
        append.display()
    );
    let bootstrap = HeadlessConsoleClient::connect(&harness).await?;
    let session = bootstrap.spawn_script(script).await?;
    bootstrap.wait_for_output(session.pane_id, "KIT_SCROLL_0099").await?;
    drop(bootstrap);

    let mut console = PublicConsole::start(&harness, PublicConsoleOptions::default())?;
    console.wait_for_output(b"KIT_SCROLL_0099")?;

    // The client projection fetches historical rows lazily. Prime that bounded cache before using
    // clipboard text as the baseline so placeholder lines cannot masquerade as scroll movement.
    let _ = copied_terminal_text(&mut console)?;
    thread::sleep(Duration::from_millis(350));
    let live = scroll_marker_rows(&copied_terminal_text(&mut console)?);
    ensure!(live.len() >= 10, "live viewport did not contain enough scroll markers: {live:?}");

    console.send(&sgr_mouse(64, 70, 20, false))?;
    let wheel = scroll_marker_rows(&copied_terminal_text(&mut console)?);
    ensure!(
        wheel.first() == live.first().and_then(|row| row.checked_sub(3)).as_ref(),
        "one wheel event did not move exactly three rows: live={live:?} wheel={wheel:?}"
    );

    console.send(b"\x1b[5~")?;
    let page = scroll_marker_rows(&copied_terminal_text(&mut console)?);
    ensure!(
        page.first() == wheel.first().and_then(|row| row.checked_sub(live.len())).as_ref(),
        "Page Up did not retain page-sized movement: live={live:?} wheel={wheel:?} page={page:?}"
    );

    console.send(b"\x1b[F")?;
    let returned_live = scroll_marker_rows(&copied_terminal_text(&mut console)?);
    ensure!(returned_live == live, "End did not restore live output: {returned_live:?}");

    console.send(&sgr_mouse(64, 70, 20, false))?;
    let anchored = copied_terminal_text(&mut console)?;
    fs::write(&append, []).context("trigger appended Console scroll output")?;
    let observer = HeadlessConsoleClient::connect(&harness).await?;
    observer.wait_for_output(session.pane_id, "KIT_SCROLL_0104").await?;
    drop(observer);
    thread::sleep(Duration::from_millis(350));
    let after_append = copied_terminal_text(&mut console)?;
    ensure!(
        scroll_marker_rows(&after_append) == scroll_marker_rows(&anchored),
        "appended output moved the anchored viewport: before={anchored:?} after={after_append:?}"
    );

    console.finish()?;
    harness.shutdown()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_console_rings_once_only_after_an_agent_becomes_ready() -> Result<()> {
    let _live_test_guard = live_test_guard().await;
    wezterm_config::designate_this_as_the_main_thread();
    wezterm_config::common_init(None, &[], true)
        .context("initialize headless Console verifier config")?;

    let mut harness = LocalConsoleHarness::start().await?;
    let ready = harness.runtime_root().join("agent-ready");
    let repeated_idle = harness.runtime_root().join("agent-still-idle");
    let initial_idle_script =
        "printf '\\033]2;✳ initially-idle\\007❯ KIT_NOTIFY_INITIAL_IDLE\\n'; sleep 30";
    let script = format!(
        "printf '\\033]2;⠋ notify-verifier\\007KIT_NOTIFY_WORKING\\n'; \
         while [ ! -e '{}' ]; do sleep 0.02; done; \
         printf '\\033]2;✳ notify-verifier\\007\\n❯ KIT_NOTIFY_READY\\n'; \
         while [ ! -e '{}' ]; do sleep 0.02; done; \
         printf 'KIT_NOTIFY_STILL_IDLE\\n'; \
         sleep 30",
        ready.display(),
        repeated_idle.display()
    );
    let bootstrap = HeadlessConsoleClient::connect(&harness).await?;
    let initial_idle = bootstrap.spawn_script(initial_idle_script.to_owned()).await?;
    bootstrap.wait_for_output(initial_idle.pane_id, "KIT_NOTIFY_INITIAL_IDLE").await?;
    let agent = bootstrap.spawn_script(script).await?;
    bootstrap.wait_for_output(agent.pane_id, "KIT_NOTIFY_WORKING").await?;
    drop(bootstrap);

    let mut console = PublicConsole::start(&harness, PublicConsoleOptions::default())?;
    console.wait_for_output(b"claude")?;
    console.clear_output()?;
    thread::sleep(Duration::from_millis(450));
    ensure!(
        byte_count(console.output_snapshot()?.as_bytes(), b'\x07') == 0,
        "initial idle or working attachment emitted a terminal bell"
    );

    fs::write(&ready, []).context("trigger agent ready transition")?;
    console.wait_for_output(b"\x07")?;
    thread::sleep(Duration::from_millis(450));
    let ready_output = console.output_snapshot()?;
    ensure!(
        byte_count(ready_output.as_bytes(), b'\x07') == 1,
        "confirmed completion did not emit exactly one bell: {ready_output:?}"
    );

    console.clear_output()?;
    fs::write(&repeated_idle, []).context("trigger repeated idle observation")?;
    let observer = HeadlessConsoleClient::connect(&harness).await?;
    observer.wait_for_output(agent.pane_id, "KIT_NOTIFY_STILL_IDLE").await?;
    drop(observer);
    thread::sleep(Duration::from_millis(650));
    let repeated_output = console.output_snapshot()?;
    ensure!(
        byte_count(repeated_output.as_bytes(), b'\x07') == 0,
        "repeated idle observation emitted another bell: {repeated_output:?}"
    );

    console.finish()?;

    let disabled_ready = harness.runtime_root().join("disabled-agent-ready");
    let disabled_script = format!(
        "printf '\\033]2;⠋ disabled-verifier\\007KIT_NOTIFY_DISABLED_WORKING\\n'; \
         while [ ! -e '{}' ]; do sleep 0.02; done; \
         printf '\\033]2;✳ disabled-verifier\\007\\n❯ KIT_NOTIFY_DISABLED_READY\\n'; \
         sleep 30",
        disabled_ready.display()
    );
    let bootstrap = HeadlessConsoleClient::connect(&harness).await?;
    let disabled_agent = bootstrap.spawn_script(disabled_script).await?;
    bootstrap.wait_for_output(disabled_agent.pane_id, "KIT_NOTIFY_DISABLED_WORKING").await?;
    drop(bootstrap);
    let mut disabled_console = PublicConsole::start(
        &harness,
        PublicConsoleOptions {
            config_toml: Some("ready_notification = \"off\"\n".to_owned()),
            ..PublicConsoleOptions::default()
        },
    )?;
    disabled_console.wait_for_output(b"claude")?;
    disabled_console.clear_output()?;
    fs::write(&disabled_ready, []).context("trigger disabled ready transition")?;
    let observer = HeadlessConsoleClient::connect(&harness).await?;
    observer.wait_for_output(disabled_agent.pane_id, "KIT_NOTIFY_DISABLED_READY").await?;
    drop(observer);
    thread::sleep(Duration::from_millis(650));
    let disabled_output = disabled_console.output_snapshot()?;
    ensure!(
        byte_count(disabled_output.as_bytes(), b'\x07') == 0,
        "disabled ready notification emitted a bell: {disabled_output:?}"
    );

    disabled_console.finish()?;
    harness.shutdown()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_console_shows_and_routes_two_independent_terminal_panels() -> Result<()> {
    let _live_test_guard = live_test_guard().await;
    wezterm_config::designate_this_as_the_main_thread();
    wezterm_config::common_init(None, &[], true)
        .context("initialize headless Console verifier config")?;

    let mut harness = LocalConsoleHarness::start().await?;
    let script = |panel: &str| {
        format!(
            "i=0; while [ \"$i\" -lt 80 ]; do printf '{panel}_%04d\\n' \"$i\"; \
             i=$((i + 1)); done; exec /bin/sh"
        )
    };
    let bootstrap = HeadlessConsoleClient::connect(&harness).await?;
    let first = bootstrap.spawn_script(script("KIT_PANEL_A")).await?;
    let second = bootstrap.spawn_script(script("KIT_PANEL_B")).await?;
    bootstrap.wait_for_output(first.pane_id, "KIT_PANEL_A_0079").await?;
    bootstrap.wait_for_output(second.pane_id, "KIT_PANEL_B_0079").await?;
    drop(bootstrap);

    let mut console = PublicConsole::start(&harness, PublicConsoleOptions::default())?;
    invoke_command(&mut console, "split terminal panel")?;
    console.wait_for_output(b"KIT_PANEL_A_0060")?;
    console.wait_for_output(b"KIT_PANEL_B_0060")?;

    // A pointer click focuses and acquires only the right-hand panel. Paste then flows to that
    // panel's independent mux pane rather than whichever session was selected first.
    console.clear_output()?;
    console.send(&sgr_mouse(0, 100, 10, false))?;
    console.send(&sgr_mouse(0, 100, 10, true))?;
    console.wait_for_output(b"you")?;
    let second_receipt = harness.runtime_root().join("second-panel-input");
    let second_command =
        format!("printf 'KIT_PANEL_B_INPUT\\n'; : > '{}'", second_receipt.display());
    console.send(format!("\x1b[200~{second_command}\x1b[201~\n").as_bytes())?;
    wait_for_file(&second_receipt)?;

    // Focus the left panel with the mouse, acquire it independently, and drive its shell.
    console.clear_output()?;
    console.send(&sgr_mouse(0, 55, 10, false))?;
    console.send(&sgr_mouse(0, 55, 10, true))?;
    console.wait_for_output(b"you")?;
    let first_receipt = harness.runtime_root().join("first-panel-input");
    let first_command = format!("printf 'KIT_PANEL_A_INPUT\\n'; : > '{}'", first_receipt.display());
    console.send(format!("\x1b[200~{first_command}\x1b[201~\n").as_bytes())?;
    wait_for_file(&first_receipt)?;

    console.send(&sgr_mouse(64, 100, 10, false))?;
    let right_visible = copied_terminal_text(&mut console)?;
    ensure!(
        right_visible.contains("KIT_PANEL_B_") && !right_visible.contains("KIT_PANEL_A_"),
        "right-panel scroll/copy used the wrong projection: {right_visible:?}"
    );
    console.send(b"\x02o")?;
    let left_visible = copied_terminal_text(&mut console)?;
    ensure!(
        left_visible.contains("KIT_PANEL_A_") && !left_visible.contains("KIT_PANEL_B_"),
        "left panel did not retain its independent projection: {left_visible:?}"
    );

    let observer = HeadlessConsoleClient::connect(&harness).await?;
    let first_text = observer.pane_text(first.pane_id).await?;
    let second_text = observer.pane_text(second.pane_id).await?;
    ensure!(first_text.contains("KIT_PANEL_A_INPUT"), "left panel missed its input");
    ensure!(!first_text.contains("KIT_PANEL_B_INPUT"), "right-panel input reached the left pane");
    ensure!(second_text.contains("KIT_PANEL_B_INPUT"), "right panel missed its input");
    ensure!(!second_text.contains("KIT_PANEL_A_INPUT"), "left-panel input reached the right pane");
    let first_size = observer.wait_for_dimensions(first.pane_id, |cols, _| cols < 70).await?;
    let second_size = observer.wait_for_dimensions(second.pane_id, |cols, _| cols < 70).await?;
    ensure!(first_size.0 > 20 && second_size.0 > 20, "split panels resized too narrowly");
    drop(observer);

    let mut peer = PublicConsole::start(&harness, PublicConsoleOptions::default())?;
    peer.wait_for_output(b"read-only")?;
    invoke_command(&mut peer, "split terminal panel")?;
    peer.wait_for_output(b"KIT_PANEL_B_0060")?;
    let blocked_receipt = harness.runtime_root().join("observer-panel-input");
    peer.clear_output()?;
    peer.send(&sgr_mouse(0, 100, 10, false))?;
    peer.send(&sgr_mouse(0, 100, 10, true))?;
    let blocked_command = format!(": > '{}'", blocked_receipt.display());
    peer.send(format!("\x1b[200~{blocked_command}\x1b[201~\n").as_bytes())?;
    thread::sleep(Duration::from_millis(350));
    ensure!(
        !blocked_receipt.exists(),
        "observer input crossed the secondary panel's control boundary"
    );
    peer.finish()?;

    // Compact terminals keep both assignments but render one focused panel with explicit guidance.
    console.clear_output()?;
    console.resize(55, 25)?;
    console.wait_for_output(b"Narrow view: one panel shown")?;
    console.clear_output()?;
    console.send(b"\x02o")?;
    console.wait_for_output(b"KIT_PANEL_B_0060")?;

    // Closing the visual panel leaves both independent sessions alive in the mux.
    console.send(b"\x02x")?;
    console.finish()?;
    let observer = HeadlessConsoleClient::connect(&harness).await?;
    observer.wait_for_session_count(2).await?;
    drop(observer);
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
    let observer = HeadlessConsoleClient::connect(&harness).await?;
    let renamed = observer.wait_for_session_count(2).await?[1];
    observer.wait_for_title(renamed.tab_id, &title).await?;
    drop(observer);

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
