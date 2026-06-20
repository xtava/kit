//! Shared live-Chrome harness for the CDP integration tests: launch a real headless Chrome on a
//! unique port + profile, wait for its DevTools endpoint, and clean up on drop. Tests skip
//! themselves (pass) when no Chrome binary is present, so the suite stays green without one.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use kit::cdp::browser_endpoint;
use tokio::time::sleep;

/// A headless Chrome that cleans itself up on drop — a leaked browser would wedge the next run.
pub struct Chrome {
    child: Child,
    pub port: u16,
    profile: String,
}

impl Drop for Chrome {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Chrome's headless zygote releases the profile lock a beat after the launcher dies; retry so
        // the data dir doesn't leak into /tmp across runs.
        for _ in 0..20 {
            if std::fs::remove_dir_all(&self.profile).is_ok()
                || !std::path::Path::new(&self.profile).exists()
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn chrome_binary() -> Option<&'static str> {
    ["google-chrome-stable", "google-chrome", "chromium", "chromium-browser"]
        .into_iter()
        .find(|binary| which(binary))
}

fn which(binary: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {binary}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Launch headless Chrome on a fixed debug port with a data-url page, and wait for its DevTools
/// endpoint to answer. Returns `None` only if no Chrome binary exists. `prefix` keeps each test
/// file's profiles apart; `salt` separates the tests that share this binary's pid so they don't
/// fight over one port.
pub async fn launch_chrome(prefix: &str, salt: u16) -> Option<Chrome> {
    let binary = chrome_binary()?;
    // A unique port + profile per run keeps concurrent invocations (and stale locks) from colliding.
    let port = 9300 + ((std::process::id() as u16).wrapping_add(salt) % 200);
    let profile =
        format!("{}/{prefix}-{}-{port}", std::env::temp_dir().display(), std::process::id());
    let _ = std::fs::remove_dir_all(&profile);
    let child = Command::new(binary)
        .args([
            "--headless=new",
            &format!("--remote-debugging-port={port}"),
            "--no-sandbox",
            "--disable-gpu",
            "--no-first-run",
            &format!("--user-data-dir={profile}"),
            "data:text/html,<title>kit-test</title><body>error fixture</body>",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn chrome");

    let chrome = Chrome { child, port, profile };
    for _ in 0..50 {
        if browser_endpoint(port).await.is_some() {
            return Some(chrome);
        }
        sleep(Duration::from_millis(100)).await;
    }
    panic!("chrome did not expose a DevTools endpoint on :{port}");
}
