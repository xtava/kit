//! The thin client behind every non-daemon `kit cdp` command. It finds the warm Attachment for an
//! Instance selector — lazily spawning the daemon if none is live (`docs/adr/0003`) — sends one
//! [`Query`] over the unix socket, and prints the rendered [`Reply`]. No CDP, no state of its own.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::sleep;

use tokio::sync::mpsc::{self, UnboundedReceiver};

use crate::cdp::{self, TrackKind};

use super::protocol::{Command, Frame, Query, Reply};
use super::registry::{self, Record};

const READY_TRIES: u32 = 60;
const READY_INTERVAL: Duration = Duration::from_millis(100);

/// Run one command against the warm Attachment for `app`, attaching first if needed. Returns whether
/// the result was a success (drives the process exit code).
pub async fn query(app: Option<&str>, json: bool, command: Command) -> Result<bool> {
    let record = ensure(app, &TrackKind::ALL).await?;
    let reply = send(&record, &Query { command, json }).await?;
    println!("{}", reply.output);
    Ok(reply.ok)
}

/// Resolve the warm Attachment for `app`, lazily attaching with all tracks if none is live. The
/// entry point for the interactive session, which then reuses the returned record for every command.
pub async fn ensure_attached(app: Option<&str>) -> Result<Record> {
    ensure(app, &TrackKind::ALL).await
}

/// Run one command against a known Attachment and return its `Reply` verbatim (no printing) — the
/// interactive session renders the result itself.
pub async fn run_one(record: &Record, command: Command, json: bool) -> Result<Reply> {
    send(record, &Query { command, json }).await
}

/// Open a live Timeline subscription to an Attachment. Sends `Subscribe`, then reads `Frame`s off
/// the socket on a spawned task into the returned channel; the channel closes when the daemon
/// disconnects or the socket dies.
pub async fn subscribe(record: &Record, since_ms: u64) -> Result<UnboundedReceiver<Frame>> {
    let stream = UnixStream::connect(registry::socket_path(&record.name))
        .await
        .with_context(|| format!("subscribe to attachment '{}'", record.name))?;
    let mut reader = BufReader::new(stream);
    let mut line = serde_json::to_string(&Query { command: Command::Subscribe { since_ms }, json: false })?;
    line.push('\n');
    reader.get_mut().write_all(line.as_bytes()).await?;

    let (sender, receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut buffer = String::new();
        loop {
            buffer.clear();
            match reader.read_line(&mut buffer).await {
                Ok(0) | Err(_) => break,
                // A frame that fails to decode is skipped, never fatal — one malformed frame must
                // not silently kill the whole live stream (it once did: a wire-type collision).
                Ok(_) => {
                    if let Some(frame) = decode_frame(buffer.trim()) {
                        if sender.send(frame).is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });
    Ok(receiver)
}

/// Decode one subscription line into a [`Frame`], or `None` if it doesn't parse. The seam the
/// reader skips on and the wire-contract tests exercise.
fn decode_frame(line: &str) -> Option<Frame> {
    serde_json::from_str(line).ok()
}

/// `kit cdp attach` — pre-warm an Attachment with a chosen Track set (idempotent).
pub async fn attach(app: Option<&str>, tracks: Vec<TrackKind>, json: bool) -> Result<()> {
    let record = ensure(app, &tracks).await?;
    let reply = send(&record, &Query { command: Command::Status, json }).await?;
    println!("{}", reply.output);
    Ok(())
}

/// `kit cdp detach` — dispose one or all Attachments.
pub async fn detach(app: Option<&str>, all: bool) -> Result<()> {
    let live = registry::reconcile();
    let targets: Vec<Record> = if all {
        live
    } else if let Some(selector) = app {
        live.into_iter().filter(|record| matches(record, selector)).collect()
    } else if live.len() <= 1 {
        live
    } else {
        bail!("multiple attachments — pass --app <selector> or --all");
    };

    if targets.is_empty() {
        println!("no matching attachment");
        return Ok(());
    }
    for record in &targets {
        let _ = send(record, &Query { command: Command::Detach, json: false }).await;
        if registry::is_alive(record.pid) {
            unsafe { libc::kill(record.pid as i32, libc::SIGTERM) };
        }
        registry::remove(&record.name);
        println!("detached {}", record.name);
    }
    Ok(())
}

/// `kit cdp ls` — live Attachments and their health.
pub fn ls(json: bool) -> Result<()> {
    let live = registry::reconcile();
    if json {
        println!("{}", serde_json::to_string_pretty(&live)?);
        return Ok(());
    }
    if live.is_empty() {
        println!("no attachments (a command will attach lazily)");
        return Ok(());
    }
    for record in &live {
        println!(
            "{:<16} {:<12} :{:<6} pid {:<8} up {:<6} tracks {}",
            record.name,
            record.app,
            record.port,
            record.pid,
            human_ms(now_unix_ms().saturating_sub(record.started_at_ms)),
            record.tracks.join(",")
        );
    }
    Ok(())
}

/// `kit cdp gc` — sweep dead Attachments.
pub fn gc(json: bool) -> Result<()> {
    let before: Vec<String> = registry::all().into_iter().map(|record| record.name).collect();
    let after: Vec<String> = registry::reconcile().into_iter().map(|record| record.name).collect();
    let swept: Vec<&String> = before.iter().filter(|name| !after.contains(name)).collect();
    if json {
        println!("{}", serde_json::json!({ "swept": swept, "live": after }));
    } else if swept.is_empty() {
        println!("nothing to sweep ({} live)", after.len());
    } else {
        println!("swept {} dead ({} live)", swept.len(), after.len());
    }
    Ok(())
}

/// `kit cdp` (bare) — one-call orientation: instances available + live attachments.
pub async fn overview(json: bool) -> Result<()> {
    let live = registry::reconcile();
    let instances = cdp::discover().await;

    if json {
        let instances: Vec<_> = instances
            .iter()
            .map(|instance| {
                serde_json::json!({
                    "name": instance.name(),
                    "app": instance.endpoint.app,
                    "port": instance.endpoint.port,
                    "pid": instance.pid,
                    "worktree": instance.worktree,
                })
            })
            .collect();
        println!("{}", serde_json::json!({ "instances": instances, "attachments": live }));
        return Ok(());
    }

    println!("instances:");
    if instances.is_empty() {
        println!("  (none — launch the app with a remote debugging port)");
    }
    for instance in &instances {
        println!("  {:<16} {:<12} :{}", instance.name(), instance.endpoint.app, instance.endpoint.port);
    }
    println!("\nattachments:");
    if live.is_empty() {
        println!("  (none — a command will attach lazily)");
    }
    for record in &live {
        println!("  {:<16} {:<12} :{:<6} pid {}", record.name, record.app, record.port, record.pid);
    }
    Ok(())
}

async fn ensure(app: Option<&str>, tracks: &[TrackKind]) -> Result<Record> {
    if let Some(record) = find(app) {
        return Ok(record);
    }
    attach_new(app, tracks).await
}

fn find(app: Option<&str>) -> Option<Record> {
    let live = registry::reconcile();
    match app {
        Some(selector) => live.into_iter().find(|record| matches(record, selector)),
        None if live.len() <= 1 => live.into_iter().next(),
        None => live
            .iter()
            .find(|record| record.app.contains("dev"))
            .cloned()
            .or_else(|| live.into_iter().next()),
    }
}

async fn attach_new(app: Option<&str>, tracks: &[TrackKind]) -> Result<Record> {
    let instances = cdp::discover().await;
    if instances.is_empty() {
        bail!("no running CDP instance found — is the app launched with a remote debugging port?");
    }
    let instance = pick(instances, app)?;
    let name = instance.name();
    let selector = app.map(str::to_owned).unwrap_or_else(|| name.clone());

    spawn_daemon(&name, &selector, instance.endpoint.port, instance.pid, tracks)?;
    wait_ready(&name).await
}

fn pick(instances: Vec<cdp::Instance>, app: Option<&str>) -> Result<cdp::Instance> {
    match app {
        Some(selector) => instances
            .into_iter()
            .find(|instance| instance.matches(selector))
            .with_context(|| format!("no instance matches '{selector}'")),
        None if instances.len() == 1 => Ok(instances.into_iter().next().unwrap()),
        None => {
            if let Some(dev) = instances.iter().find(|instance| instance.endpoint.app.contains("dev")).cloned() {
                Ok(dev)
            } else {
                instances.into_iter().next().context("no instance")
            }
        }
    }
}

fn spawn_daemon(name: &str, selector: &str, port: u16, root_pid: u32, tracks: &[TrackKind]) -> Result<()> {
    std::fs::create_dir_all(registry::dir()).context("create runtime dir")?;
    let exe = std::env::current_exe().context("resolve own path")?;
    let log = std::fs::File::create(registry::log_path(name)).context("open daemon log")?;

    let mut command = std::process::Command::new(exe);
    command
        .arg("cdp")
        .arg("__serve")
        .arg("--name")
        .arg(name)
        .arg("--selector")
        .arg(selector)
        .arg("--port")
        .arg(port.to_string())
        .arg("--root-pid")
        .arg(root_pid.to_string());
    if !tracks.is_empty() {
        let csv = tracks.iter().map(|track| track.as_str()).collect::<Vec<_>>().join(",");
        command.arg("--track").arg(csv);
    }
    command.stdin(Stdio::null()).stdout(Stdio::from(log.try_clone()?)).stderr(Stdio::from(log));

    // Detach into its own session so the daemon outlives this CLI process and its terminal.
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command.spawn().context("spawn cdp daemon")?;
    Ok(())
}

async fn wait_ready(name: &str) -> Result<Record> {
    for _ in 0..READY_TRIES {
        if let Some(record) = registry::read(name) {
            if send(&record, &Query { command: Command::Ping, json: false }).await.is_ok() {
                return Ok(record);
            }
        }
        sleep(READY_INTERVAL).await;
    }
    bail!("attachment '{name}' did not come up — see {}", registry::log_path(name).display())
}

async fn send(record: &Record, query: &Query) -> Result<Reply> {
    let stream = UnixStream::connect(registry::socket_path(&record.name))
        .await
        .with_context(|| format!("connect attachment '{}'", record.name))?;
    let mut line = serde_json::to_string(query)?;
    line.push('\n');

    let mut reader = BufReader::new(stream);
    reader.get_mut().write_all(line.as_bytes()).await?;
    let mut response = String::new();
    reader.read_line(&mut response).await.context("read reply")?;
    serde_json::from_str(response.trim()).context("decode reply")
}

fn matches(record: &Record, selector: &str) -> bool {
    let needle = selector.to_lowercase();
    record.name.to_lowercase().contains(&needle)
        || record.app.to_lowercase().contains(&needle)
        || record.selector.to_lowercase().contains(&needle)
        || record.port.to_string() == selector
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn human_ms(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h{}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reader's contract: a valid frame decodes, anything malformed becomes `None` so the loop
    /// skips it instead of killing the stream. (Wire-shape coverage lives in `protocol`/`timeline`.)
    #[test]
    fn decode_frame_keeps_valid_drops_garbage() {
        let wire = serde_json::to_string(&Frame::Backfill(vec![])).unwrap();
        assert!(matches!(decode_frame(&wire), Some(Frame::Backfill(_))));

        assert!(decode_frame("not json").is_none());
        assert!(decode_frame("").is_none());
        assert!(decode_frame("{\"Unknown\":1}").is_none());
    }
}
