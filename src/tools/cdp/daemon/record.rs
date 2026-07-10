//! The recording shell: drives `Page.startScreencast` on the resolved Target, acks and buffers
//! every frame to disk as it arrives, survives the Attachment re-binding the target (HMR reload,
//! restart) by re-arming on the new session, and assembles the mp4 with ffmpeg on stop. The pure
//! half — the frame manifest and the concat script — is the sibling `record` engine module.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};

use crate::cdp::{CdpConnection, CdpEvent, Source};

use super::super::protocol::{RecordOp, Reply};
use super::super::record::{concat_script, frame_interval_ms, FrameEntry, Manifest};
use super::super::registry;
use super::{add_mark, no_target, now_unix_ms, push_marker, Shared};

const FPS_CAP_MIN: u32 = 1;
const FPS_CAP_MAX: u32 = 30;
/// How long an assembly may run before it is killed — a stuck ffmpeg must not wedge the reply.
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(120);

/// The one active recording of an Attachment: where frames land, which session streams them, and
/// the throttle that approximates the fps cap.
pub(super) struct RecordingState {
    id: String,
    dir: PathBuf,
    /// The Target selector the recording was started with — what a re-arm resolves fresh after
    /// the Attachment re-binds the target.
    target: Option<String>,
    session: String,
    fps_cap: u32,
    frames: Vec<FrameEntry>,
    next_seq: u64,
    /// Unix ms of the last frame kept — the throttle clock. Chrome's `everyNthFrame` counts
    /// *painted* compositor frames, so on a quiet evidence page it would skip the few frames
    /// that matter; throttling by wall clock caps busy pages without starving quiet ones.
    last_kept_ms: Option<u64>,
}

impl RecordingState {
    pub(super) fn id(&self) -> &str {
        &self.id
    }
}

/// What `record stop` reports — the pinned reply contract a downstream harness parses.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct StopSummary {
    /// The assembled mp4, or `None` when assembly was not possible (`note` says why).
    pub(super) video: Option<PathBuf>,
    pub(super) frames: usize,
    pub(super) duration_ms: u64,
    pub(super) frames_dir: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) note: Option<String>,
}

pub(super) struct Started {
    id: String,
    dir: PathBuf,
    fps_cap: u32,
    target_label: String,
}

pub(super) async fn record_reply(state: &Shared, op: RecordOp, json: bool) -> Reply {
    match op {
        RecordOp::Start { target, fps_cap } => match start(state, target, fps_cap).await {
            Ok(started) => {
                if json {
                    return Reply::ok(
                        json!({
                            "recording": started.id,
                            "framesDir": started.dir,
                            "fpsCap": started.fps_cap,
                            "target": started.target_label,
                        })
                        .to_string(),
                    );
                }
                Reply::ok(format!(
                    "recording {} · target {} · cap {} fps\nframes    {}\nstop      kit cdp record stop",
                    started.id,
                    started.target_label,
                    started.fps_cap,
                    started.dir.display()
                ))
            }
            Err(error) => Reply::fail(error),
        },
        RecordOp::Stop { out } => match stop(state, out).await {
            Ok(summary) => {
                if json {
                    return Reply::ok(serde_json::to_string_pretty(&summary).unwrap_or_default());
                }
                Reply::ok(render_stop(&summary))
            }
            Err(error) => Reply::fail(error),
        },
    }
}

pub(super) fn render_stop(summary: &StopSummary) -> String {
    let mut out = vec![match &summary.video {
        Some(video) => format!("video     {}", video.display()),
        None => "video     (not assembled)".to_owned(),
    }];
    out.push(format!(
        "frames    {} · {}ms · {}",
        summary.frames,
        summary.duration_ms,
        summary.frames_dir.display()
    ));
    if let Some(note) = &summary.note {
        out.push(format!("note      {note}"));
    }
    out.join("\n")
}

/// Begin a recording: claim the single slot, create the frames dir, and arm the screencast.
/// The slot is claimed *before* the arming await (mirroring how traces reserve their name), so a
/// racing second `record start` fails naming this one instead of double-arming.
pub(super) async fn start(
    state: &Shared,
    target: Option<String>,
    fps_cap: u32,
) -> Result<Started, String> {
    let fps_cap = fps_cap.clamp(FPS_CAP_MIN, FPS_CAP_MAX);
    let (conn, session, target_label, id, dir) = {
        let mut guard = state.lock().unwrap();
        if let Some(active) = &guard.recording {
            return Err(format!("recording '{}' already active — `record stop` first", active.id));
        }
        let Some((session, resolved)) = guard.resolve(target.as_deref()) else {
            return Err(no_target());
        };
        let id = format!("rec-{}", now_unix_ms());
        let dir = registry::artifact_dir(&guard.name).join("recordings").join(&id);
        guard.recording = Some(RecordingState {
            id: id.clone(),
            dir: dir.clone(),
            target,
            session: session.clone(),
            fps_cap,
            frames: Vec::new(),
            next_seq: 1,
            last_kept_ms: None,
        });
        (guard.conn.clone(), session, resolved.label(), id, dir)
    };

    if let Err(error) = std::fs::create_dir_all(&dir) {
        clear_claim(state, &id);
        return Err(format!("create {}: {error}", dir.display()));
    }
    if let Err(error) = start_screencast(&conn, &session).await {
        clear_claim(state, &id);
        return Err(format!("start screencast: {error}"));
    }
    add_mark(state, "record", &format!("record start {id} — cap {fps_cap} fps"));
    Ok(Started { id, dir, fps_cap, target_label })
}

/// Release a claimed recording slot — only if it is still ours (a stop may have raced us).
fn clear_claim(state: &Shared, id: &str) {
    let mut guard = state.lock().unwrap();
    if guard.recording.as_ref().is_some_and(|recording| recording.id == id) {
        guard.recording = None;
    }
}

async fn start_screencast(conn: &CdpConnection, session: &str) -> Result<(), String> {
    // Screencast frames only flow on Page-enabled sessions; enabling is idempotent and cheap.
    conn.call(Some(session), "Page.enable", json!({})).await.map_err(|error| error.to_string())?;
    conn.call(
        Some(session),
        "Page.startScreencast",
        json!({ "format": "png", "everyNthFrame": 1 }),
    )
    .await
    .map_err(|error| error.to_string())?;
    Ok(())
}

/// Stop the screencast and assemble. Assembly problems (no ffmpeg, an encode failure) are notes
/// on an `Ok` summary, never errors — the frames on disk are already the evidence, and a missing
/// encoder must not fail the stop. The only error is "nothing was recording".
pub(super) async fn stop(state: &Shared, out: Option<PathBuf>) -> Result<StopSummary, String> {
    let (conn, recording) = {
        let mut guard = state.lock().unwrap();
        let Some(recording) = guard.recording.take() else {
            return Err("no active recording — start one with `record start`".to_owned());
        };
        (guard.conn.clone(), recording)
    };
    // Best-effort: the session may already be gone (a reload mid-recording); the frames on disk
    // are the evidence either way.
    let _ = conn.call(Some(&recording.session), "Page.stopScreencast", json!({})).await;
    push_marker(
        state,
        Source::Renderer,
        &format!("record stop {} — {} frame(s)", recording.id, recording.frames.len()),
    );

    let frames_dir = recording.dir.clone();
    let Some(script) = concat_script(&recording.frames, recording.fps_cap) else {
        return Ok(StopSummary {
            video: None,
            frames: 0,
            duration_ms: 0,
            frames_dir,
            note: Some("no frames captured — the page never painted while recording".to_owned()),
        });
    };

    let concat = frames_dir.join("frames.concat");
    if let Err(error) = std::fs::write(&concat, &script.content) {
        return Ok(StopSummary {
            video: None,
            frames: recording.frames.len(),
            duration_ms: script.total_ms,
            frames_dir: frames_dir.clone(),
            note: Some(format!("could not write {}: {error} — frames kept", concat.display())),
        });
    }
    let video = out.unwrap_or_else(|| frames_dir.join("recording.mp4"));
    let (video, note) = assemble(&frames_dir, &concat, &video).await;
    Ok(StopSummary {
        video,
        frames: recording.frames.len(),
        duration_ms: script.total_ms,
        frames_dir,
        note,
    })
}

/// Run ffmpeg over the concat script. ffmpeg being absent is a supported outcome, not an error:
/// the frames dir stays, and the note carries the exact command to assemble later.
async fn assemble(dir: &Path, concat: &Path, video: &Path) -> (Option<PathBuf>, Option<String>) {
    let mut command = tokio::process::Command::new("ffmpeg");
    command
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "concat", "-safe", "0", "-i"])
        .arg(concat)
        // yuv420p h264 requires even dimensions and viewports aren't always even.
        .args(["-vf", "scale=trunc(iw/2)*2:trunc(ih/2)*2"])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-movflags", "+faststart"])
        .arg(video)
        // Frame paths in the concat script are relative to the recording dir.
        .current_dir(dir)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(FFMPEG_TIMEOUT, command.output()).await {
        Err(_) => (
            None,
            Some(format!(
                "ffmpeg timed out after {}s — frames kept in {}",
                FFMPEG_TIMEOUT.as_secs(),
                dir.display()
            )),
        ),
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => (
            None,
            Some(format!(
                "ffmpeg not on PATH — frames kept; assemble later: \
                 ffmpeg -f concat -safe 0 -i {} -c:v libx264 -pix_fmt yuv420p -movflags +faststart {}",
                concat.display(),
                video.display()
            )),
        ),
        Ok(Err(error)) => {
            (None, Some(format!("ffmpeg failed to run: {error} — frames kept in {}", dir.display())))
        }
        Ok(Ok(output)) if output.status.success() => (Some(video.to_owned()), None),
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let tail: Vec<&str> = stderr.lines().rev().take(3).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            (
                None,
                Some(format!(
                    "ffmpeg failed ({}): {} — frames kept in {}",
                    output.status,
                    tail.join(" · "),
                    dir.display()
                )),
            )
        }
    }
}

/// One `Page.screencastFrame`: ack it (unconditionally — an unacked frame stops Chrome's stream
/// cold), then keep or drop it by the throttle and persist what's kept.
pub(super) async fn handle_screencast_frame(state: &Shared, event: &CdpEvent) {
    let Some(session) = event.session.as_deref() else {
        return;
    };
    let conn = state.lock().unwrap().conn.clone();
    if let Some(ack) = event.params.get("sessionId").and_then(Value::as_i64) {
        let _ =
            conn.call(Some(session), "Page.screencastFrameAck", json!({ "sessionId": ack })).await;
    }

    let at_ms = frame_timestamp_ms(&event.params);
    let claimed = {
        let mut guard = state.lock().unwrap();
        let Some(recording) = guard.recording.as_mut() else {
            return;
        };
        if recording.session != session {
            return;
        }
        // Compositor timing jitters a few ms around the cap; a hard threshold would drop frames
        // arriving a shade early and halve the effective rate, so accept at 80% of the interval.
        let interval = frame_interval_ms(recording.fps_cap);
        let due = recording
            .last_kept_ms
            .is_none_or(|last| at_ms.saturating_sub(last) >= interval - interval / 5);
        if !due {
            return;
        }
        recording.last_kept_ms = Some(at_ms);
        let seq = recording.next_seq;
        recording.next_seq += 1;
        (recording.id.clone(), recording.dir.clone(), seq)
    };
    let (id, dir, seq) = claimed;

    let Some(data) = event.params.get("data").and_then(Value::as_str) else {
        return;
    };
    let Ok(bytes) = super::base64_decode(data) else {
        return;
    };
    let file = format!("frame-{seq:06}.png");
    if std::fs::write(dir.join(&file), bytes).is_err() {
        return;
    }

    // Append and re-render the manifest under the lock; write it outside. Persisting per frame
    // keeps the dir assemblable even if the daemon dies mid-recording.
    let manifest = {
        let mut guard = state.lock().unwrap();
        match guard.recording.as_mut().filter(|recording| recording.id == id) {
            Some(recording) => {
                recording.frames.push(FrameEntry { file, at_ms });
                Some(manifest_json(recording))
            }
            // A stop raced the write — the manifest is final; don't leave an untracked frame.
            None => {
                let _ = std::fs::remove_file(dir.join(&file));
                None
            }
        }
    };
    if let Some(manifest) = manifest {
        let _ = std::fs::write(dir.join("frames.json"), manifest);
    }
}

fn manifest_json(recording: &RecordingState) -> String {
    let manifest = Manifest {
        recording: recording.id.clone(),
        fps_cap: recording.fps_cap,
        frames: recording.frames.clone(),
    };
    serde_json::to_string_pretty(&manifest).unwrap_or_default()
}

/// Chrome stamps each frame with an epoch-seconds timestamp; fall back to the daemon clock for
/// the rare frame without metadata.
fn frame_timestamp_ms(params: &Value) -> u64 {
    params
        .pointer("/metadata/timestamp")
        .and_then(Value::as_f64)
        .map(|seconds| (seconds * 1_000.0) as u64)
        .unwrap_or_else(now_unix_ms)
}

/// Re-arm the screencast after the Attachment re-binds the recording's Target (an HMR reload or
/// restart replaced the session): resolve the selector fresh and start again on the winner —
/// the same survive-the-reload contract watches and traces keep. Called on every new attach; a
/// no-op while the recording session is alive.
pub(super) async fn rearm_after_rebind(state: &Shared) {
    let plan = {
        let guard = state.lock().unwrap();
        let Some(recording) = guard.recording.as_ref() else {
            return;
        };
        if guard.sessions.contains_key(&recording.session) {
            return;
        }
        let Some((session, _)) = guard.resolve(recording.target.as_deref()) else {
            return;
        };
        (guard.conn.clone(), session, recording.id.clone())
    };
    let (conn, session, id) = plan;
    if start_screencast(&conn, &session).await.is_err() {
        // The next attach retries — targets often re-bind in bursts during a reload.
        return;
    }
    let kept = {
        let mut guard = state.lock().unwrap();
        match guard.recording.as_mut().filter(|recording| recording.id == id) {
            Some(recording) => {
                recording.session = session.clone();
                true
            }
            None => false,
        }
    };
    if !kept {
        // A stop raced the re-arm; retire the fresh screencast instead of leaking it.
        let _ = conn.call(Some(&session), "Page.stopScreencast", json!({})).await;
        return;
    }
    push_marker(state, Source::Renderer, &format!("recording {id} re-armed after reload"));
}
