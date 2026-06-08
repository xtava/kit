//! `scout dive` — the bridge from the map to the microscope. Captures a clean heap snapshot of a
//! workbench window (Playwright `connectOverCDP` + chunk-settle — the one reliable way) and runs
//! memlab's shape/detached-DOM analyses on it.
//!
//! The capture trick is load-bearing: `HeapProfiler.takeHeapSnapshot` resolves *before* the
//! `addHeapSnapshotChunk` stream finishes flushing, so we finalize the file only after the stream
//! has been idle ~1.5s. Finalizing on the command result yields a truncated, corrupt snapshot.

use anyhow::{bail, Context, Result};
use tokio::process::Command;

use super::{survey, DiveArgs};

const CAPTURE_JS: &str = r#"
const { chromium } = require('__PLAYWRIGHT__');
const fs = require('fs');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
(async () => {
  const browser = await chromium.connectOverCDP('http://127.0.0.1:__PORT__');
  const ctx = browser.contexts()[0];
  const pages = ctx.pages();
  const page = pages.find((p) => p.url().includes('/workspace/')) || pages[0];
  console.error('capturing:', page.url().slice(0, 60));
  const session = await ctx.newCDPSession(page);
  await session.send('HeapProfiler.enable');
  const out = fs.createWriteStream('__OUT__');
  let bytes = 0;
  let last = Date.now();
  session.on('HeapProfiler.addHeapSnapshotChunk', (e) => {
    out.write(e.chunk);
    bytes += e.chunk.length;
    last = Date.now();
  });
  await session.send('HeapProfiler.takeHeapSnapshot', { reportProgress: false, captureNumericValue: false });
  while (Date.now() - last < 1500) await sleep(200);
  await new Promise((r) => out.end(r));
  console.error('snapshot:', (bytes / 1048576).toFixed(0), 'MB');
  process.exit(0);
})().catch((e) => {
  console.error('ERR', e.message);
  process.exit(1);
});
"#;

pub async fn run(marker: &str, args: &DiveArgs) -> Result<()> {
    let (name, port) = match args.port {
        Some(port) => (format!(":{port}"), port),
        None => pick_target(marker).await?,
    };
    let out = args
        .out
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join(format!("kit-scout-{port}.heapsnapshot")));
    let out_str = out.to_string_lossy().to_string();

    eprintln!("scout dive · {name} :{port} → {out_str}");
    capture(port, &args.playwright, &out_str).await?;
    println!("snapshot captured: {out_str}\n");

    section("memlab analyze shape");
    memlab(&["analyze", "shape", "--snapshot", &out_str]).await;
    section("memlab analyze detached-DOM");
    memlab(&["analyze", "detached-DOM", "--snapshot", &out_str]).await;

    println!(
        "\nsnapshot: {out_str}\n  drill in:  memlab trace --node-id=<id> --snapshot {out_str}"
    );
    Ok(())
}

async fn pick_target(marker: &str) -> Result<(String, u16)> {
    survey::collect(marker)
        .await
        .instances
        .into_iter()
        .find_map(|instance| instance.debug_port.map(|port| (instance.name, port)))
        .context("no instance exposes a debug port to dive")
}

async fn capture(port: u16, playwright: &str, out: &str) -> Result<()> {
    let script = CAPTURE_JS
        .replace("__PLAYWRIGHT__", playwright)
        .replace("__PORT__", &port.to_string())
        .replace("__OUT__", out);
    let script_path = std::env::temp_dir().join("kit-scout-capture.cjs");
    std::fs::write(&script_path, script).context("write capture script")?;

    let status = Command::new("node")
        .arg(&script_path)
        .status()
        .await
        .context("spawn node (is it installed?)")?;
    if !status.success() {
        bail!("heap capture failed — check that Playwright resolves (--playwright <path>)");
    }
    Ok(())
}

async fn memlab(args: &[&str]) {
    let ran = Command::new("memlab").args(args).status().await;
    if let Ok(status) = &ran {
        if status.success() {
            return;
        }
    }
    eprintln!("(memlab unavailable — install with: PUPPETEER_SKIP_DOWNLOAD=1 npm i -g memlab)");
}

fn section(title: &str) {
    println!("\n=== {title} ===");
}
