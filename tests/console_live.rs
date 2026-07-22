#![cfg(any(target_os = "linux", target_os = "macos"))]

mod support;

use std::fs;

use anyhow::{ensure, Context, Result};

use support::console::{HeadlessConsoleClient, LocalConsoleHarness};

/// Production-path Phase 2 lifecycle proof.
///
/// This test intentionally drives the real agent, socket, embedded mux, native PTYs, and production
/// wire client. It does not substitute for the remaining PTY-driven public `kit console` verifier:
/// keyboard, pointer, context-menu, split drag, selection, paste, and OSC-52 clipboard behavior
/// must still be exercised through the public Ratatui process on the real terminal surface.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_console_reconnects_to_authoritative_sessions_and_cleans_up() -> Result<()> {
    wezterm_config::designate_this_as_the_main_thread();
    wezterm_config::common_init(None, &[], true)
        .context("initialize headless WezTerm verifier config")?;

    let mut harness = LocalConsoleHarness::start()?;
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let mut expected = Vec::new();
    let mut offline_markers = Vec::new();
    let mut process_markers = Vec::new();
    let mut gates = Vec::new();
    let mut emission_receipts = Vec::new();

    let mut client = HeadlessConsoleClient::connect(&harness).await?;
    for index in 0..2 {
        let process_marker = format!("KIT_CONSOLE_PROCESS_{nonce}_{index}");
        let ready_marker = format!("KIT_CONSOLE_READY_{nonce}_{index}");
        let offline_marker = format!("KIT_CONSOLE_OFFLINE_{nonce}_{index}");
        let gate = harness.runtime_root().join(format!("release-{nonce}-{index}"));
        let emission_receipt = harness.runtime_root().join(format!("emitted-{nonce}-{index}"));
        let script = format!(
            "# {process_marker}\n\
             printf '{ready_marker}\\n'; \
             while [ ! -f '{}' ]; do /bin/sleep 0.02; done; \
             printf '{offline_marker}\\n'; \
             : > '{}'; \
             while :; do /bin/sleep 1; done",
            gate.display(),
            emission_receipt.display()
        );
        let identity = client.spawn_script(script).await?;
        client.wait_for_output(identity.pane_id, &ready_marker).await?;
        expected.push(identity);
        offline_markers.push((identity.pane_id, offline_marker));
        process_markers.push(process_marker);
        gates.push(gate);
        emission_receipts.push(emission_receipt);
    }
    expected.sort_unstable();
    ensure!(expected.len() == 2, "verifier did not create exactly two Console sessions");
    ensure!(
        client.wait_for_topology(&expected).await? == expected,
        "initial Console topology differs from spawn identities"
    );

    client.begin_service_drain().await?;
    let rejected = client.spawn_script("printf 'must-not-run\\n'".to_owned()).await;
    ensure!(rejected.is_err(), "service drain admitted a new Console session");
    ensure!(
        client.wait_for_topology(&expected).await? == expected,
        "service drain changed authoritative topology"
    );
    client.cancel_service_drain().await?;

    let leader_pids = process_markers
        .iter()
        .map(|marker| harness.observe_session_leader(marker))
        .collect::<Result<Vec<_>>>()?;
    ensure!(leader_pids[0] != leader_pids[1], "two Console sessions share one shell leader");

    for reconnect in 0..3 {
        drop(client);
        if reconnect == 0 {
            for gate in &gates {
                fs::write(gate, b"release\n").with_context(|| {
                    format!("release offline Console output {}", gate.display())
                })?;
            }
            harness.wait_for_files(&emission_receipts)?;
        }

        client = HeadlessConsoleClient::connect(&harness).await?;
        ensure!(
            client.wait_for_topology(&expected).await? == expected,
            "Console identities changed after reconnect {}",
            reconnect + 1
        );
        if reconnect == 0 {
            for (pane_id, marker) in &offline_markers {
                client.wait_for_output(*pane_id, marker).await?;
            }
        }
        for (marker, expected_pid) in process_markers.iter().zip(&leader_pids) {
            ensure!(
                harness.observe_session_leader(marker)? == *expected_pid,
                "Console shell leader changed after reconnect {}",
                reconnect + 1
            );
        }
    }

    let exited_marker = format!("KIT_CONSOLE_EXITED_{nonce}");
    let exited = client.spawn_script(format!("printf '{exited_marker}\\n'; exit 0")).await?;
    client.wait_for_output(exited.pane_id, &exited_marker).await?;
    let mut retained = expected.clone();
    retained.push(exited);
    retained.sort_unstable();
    ensure!(
        client.wait_for_topology(&retained).await? == retained,
        "exited Console pane did not retain its final terminal state"
    );
    client.close_pane(exited.pane_id).await?;
    ensure!(
        client.wait_for_topology(&expected).await? == expected,
        "explicit close did not remove the retained Console pane"
    );

    harness.assert_socket_policy()?;
    drop(client);
    harness.shutdown()
}
