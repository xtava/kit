#![cfg(target_os = "linux")]

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    path::Path,
    process::{Command, Output},
};

use serde_json::{json, Value};

#[test]
fn public_stream_inspection_is_structured_and_read_only() {
    let instances = run("hyprctl", ["instances", "-j"]);
    if !instances.status.success() {
        eprintln!("skipping Stream inspection: Hyprland is unavailable");
        return;
    }

    let topology_before = normalized_topology();
    let windows_before = normalized_windows();
    let service_before = run(
        "systemctl",
        [
            "--user",
            "show",
            "sunshine.service",
            "-p",
            "ActiveState",
            "-p",
            "SubState",
            "-p",
            "MainPID",
            "--no-pager",
        ],
    )
    .stdout;
    let listeners_before = sunshine_listeners();
    let processes_before = streaming_processes();

    let config_root =
        std::env::temp_dir().join(format!("kit-stream-integration-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&config_root).expect("create isolated Stream config root");
    let inspect = kit_json(&config_root, ["stream", "inspect"]);
    let status = kit_json(&config_root, ["stream", "status"]);
    let doctor = kit_json(&config_root, ["stream", "doctor"]);

    assert_eq!(inspect["schema_version"], 1);
    assert_eq!(inspect["target"]["kind"], "local");
    assert_eq!(inspect["readiness"], "ready");
    assert!(inspect["hyprland"]["outputs"].as_array().is_some_and(|outputs| !outputs.is_empty()));
    assert!(inspect["hyprland"]["workspaces"]
        .as_array()
        .is_some_and(|workspaces| !workspaces.is_empty()));
    assert_eq!(inspect["sunshine"]["available"], true);
    assert_eq!(inspect["moonlight"]["available"], true);

    assert_eq!(status["schema_version"], 1);
    assert_eq!(status["session"], "inactive");
    assert_eq!(status["inspection"]["target"]["kind"], "local");

    assert_eq!(doctor["schema_version"], 1);
    assert!(doctor["checks"].as_array().is_some_and(|checks| checks.len() >= 4));

    let unresolved =
        kit_json(&config_root, ["stream", "inspect", "kit-stream-no-such-host.invalid"]);
    assert_eq!(unresolved["readiness"], "unavailable");
    assert!(unresolved["diagnostics"].as_array().is_some_and(|diagnostics| {
        diagnostics.iter().any(|diagnostic| diagnostic["id"] == "stream.host.notResolved")
    }));
    let tailscale_setup = kit_json(&config_root, ["stream", "setup", "tailscale"]);
    assert_eq!(tailscale_setup["action"], "authenticate_tailscale");
    assert_eq!(tailscale_setup["state"], "ready");

    let tailscale_status: Value =
        serde_json::from_slice(&run("tailscale", ["status", "--json"]).stdout)
            .expect("decode Tailscale status for Stream host setup");
    if let Some(node_id) = tailscale_status["Peer"]
        .as_object()
        .and_then(|peers| {
            peers.values().find(|peer| {
                peer["Online"] == true
                    && peer["OS"].as_str().is_some_and(|operating_system| {
                        operating_system.eq_ignore_ascii_case("linux")
                    })
            })
        })
        .and_then(|peer| peer["ID"].as_str())
    {
        let configured_host_root =
            std::env::temp_dir().join(format!("kit-stream-host-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&configured_host_root).expect("create Stream host config root");
        let host_setup = kit_json(
            &configured_host_root,
            ["stream", "setup", "host", node_id, "--user", "integration-user", "--preferred"],
        );
        assert_eq!(host_setup["action"], "configure_host");
        assert_eq!(host_setup["state"], "ready");
        let stored = std::fs::read_to_string(configured_host_root.join("kit/stream.toml"))
            .expect("read configured Stream host");
        assert!(stored.contains("preferred_host"));
        assert!(stored.contains("integration-user"));
        std::fs::remove_dir_all(configured_host_root).expect("remove Stream host config root");
    }

    let configured_root =
        std::env::temp_dir().join(format!("kit-stream-config-{}", uuid::Uuid::new_v4()));
    let configured_directory = configured_root.join("kit");
    std::fs::create_dir_all(&configured_directory).expect("create configured Stream root");
    std::fs::write(
        configured_directory.join("stream.toml"),
        "version = 1\npreferred_host = \"local\"\n\n[executables]\nhyprctl = \
         \"kit-stream-missing-hyprctl\"\n",
    )
    .expect("write synthetic Stream config");
    let configured = kit_json(&configured_root, ["stream", "inspect"]);
    assert_eq!(configured["target"]["kind"], "local");
    assert!(configured["hyprland"].is_null());
    assert!(configured["diagnostics"].as_array().is_some_and(|diagnostics| {
        diagnostics.iter().any(|diagnostic| diagnostic["id"] == "hyprland.unavailable")
    }));

    assert_eq!(normalized_topology(), topology_before);
    assert_eq!(normalized_windows(), windows_before);
    assert_eq!(
        run(
            "systemctl",
            [
                "--user",
                "show",
                "sunshine.service",
                "-p",
                "ActiveState",
                "-p",
                "SubState",
                "-p",
                "MainPID",
                "--no-pager",
            ],
        )
        .stdout,
        service_before
    );
    assert_eq!(sunshine_listeners(), listeners_before);
    assert_eq!(streaming_processes(), processes_before);
    assert!(!config_root.join("kit/stream.toml").exists());

    std::fs::remove_dir_all(config_root).expect("remove isolated Stream config root");
    std::fs::remove_dir_all(configured_root).expect("remove configured Stream root");
}

fn kit_json<const N: usize>(config_root: &Path, arguments: [&str; N]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_kit"))
        .arg("--json")
        .args(arguments)
        .env("XDG_CONFIG_HOME", config_root)
        .output()
        .expect("run public Kit Stream command");
    assert!(
        output.status.success(),
        "Kit Stream command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("decode public Kit Stream JSON")
}

fn normalized_topology() -> Value {
    let output = run("hyprctl", ["-j", "monitors"]);
    assert!(output.status.success(), "read Hyprland output topology");
    let monitors: Vec<Value> =
        serde_json::from_slice(&output.stdout).expect("decode Hyprland output topology");
    let mut normalized = monitors
        .into_iter()
        .map(|monitor| {
            json!({
                "name": monitor["name"],
                "width": monitor["width"],
                "height": monitor["height"],
                "refreshRate": monitor["refreshRate"],
                "x": monitor["x"],
                "y": monitor["y"],
                "scale": monitor["scale"],
                "transform": monitor["transform"],
                "disabled": monitor["disabled"],
            })
        })
        .collect::<Vec<_>>();
    normalized.sort_by(|left, right| {
        left["name"].as_str().unwrap_or_default().cmp(right["name"].as_str().unwrap_or_default())
    });
    Value::Array(normalized)
}

fn normalized_windows() -> Value {
    let output = run("hyprctl", ["-j", "clients"]);
    assert!(output.status.success(), "read Hyprland window state");
    let windows: Vec<Value> =
        serde_json::from_slice(&output.stdout).expect("decode Hyprland window state");
    let mut normalized = windows
        .into_iter()
        .map(|window| {
            json!({
                "address": window["address"],
                "stableId": window["stableId"],
                "workspace": window["workspace"],
                "mapped": window["mapped"],
                "hidden": window["hidden"],
                "floating": window["floating"],
                "fullscreen": window["fullscreen"],
                "at": window["at"],
                "size": window["size"],
            })
        })
        .collect::<Vec<_>>();
    normalized.sort_by(|left, right| {
        left["address"]
            .as_str()
            .unwrap_or_default()
            .cmp(right["address"].as_str().unwrap_or_default())
    });
    Value::Array(normalized)
}

fn sunshine_listeners() -> Vec<String> {
    let output = run("ss", ["-H", "-lntp"]);
    assert!(output.status.success(), "inspect listener ports");
    let mut listeners = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| {
            [":47984", ":47989", ":47990", ":47998", ":47999", ":48000", ":48010"]
                .iter()
                .any(|port| line.contains(port))
        })
        .map(str::to_owned)
        .collect::<Vec<_>>();
    listeners.sort();
    listeners
}

fn streaming_processes() -> BTreeMap<&'static str, Vec<u8>> {
    BTreeMap::from([
        ("moonlight", exact_processes("moonlight")),
        ("sunshine", exact_processes("sunshine")),
    ])
}

fn exact_processes(name: &str) -> Vec<u8> {
    let output = run("pgrep", ["-a", "-x", name]);
    if output.status.success() {
        output.stdout
    } else {
        Vec::new()
    }
}

fn run<I, S>(program: &str, arguments: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new(program).args(arguments).output().expect("run integration dependency")
}
