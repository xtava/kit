//! The recording engine — the pure half of screencast recording. Frames arrive at whatever rate
//! the page actually painted, so a recording is a list of (file, timestamp) pairs; this module
//! turns that list into the `frames.json` manifest and the ffmpeg concat script that assembles an
//! mp4 with the *real* inter-frame durations. No I/O — the daemon shell owns the disk and ffmpeg.

use serde::{Deserialize, Serialize};

/// The default frame-rate cap — evidence video, not gameplay.
pub const DEFAULT_FPS_CAP: u32 = 4;

/// One captured screencast frame: its file name (relative to the recording dir) and the unix-ms
/// timestamp Chrome painted it at.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameEntry {
    pub file: String,
    pub at_ms: u64,
}

/// The `frames.json` manifest a recording dir carries — enough to reassemble the video later
/// even without the daemon.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub recording: String,
    pub fps_cap: u32,
    pub frames: Vec<FrameEntry>,
}

/// An ffmpeg concat-demuxer script plus the video duration it encodes.
#[derive(Debug, PartialEq)]
pub struct ConcatScript {
    pub content: String,
    pub total_ms: u64,
}

/// The nominal frame interval for a cap — also the hold given to the final frame, which has no
/// successor to measure against.
pub fn frame_interval_ms(fps_cap: u32) -> u64 {
    1_000 / u64::from(fps_cap.max(1))
}

/// Build the concat-demuxer script for a recording: each frame held for the real gap to its
/// successor, the last frame held one nominal interval. `None` when there are no frames.
pub fn concat_script(frames: &[FrameEntry], fps_cap: u32) -> Option<ConcatScript> {
    let last = frames.last()?;
    let last_hold_ms = frame_interval_ms(fps_cap);
    let mut content = String::from("ffconcat version 1.0\n");
    let mut total_ms: u64 = 0;
    for pair in frames.windows(2) {
        // A clock hiccup could order two frames equally; a zero duration confuses the demuxer,
        // so floor every hold at 1ms.
        let hold_ms = pair[1].at_ms.saturating_sub(pair[0].at_ms).max(1);
        content.push_str(&format!("file '{}'\nduration {}\n", pair[0].file, seconds(hold_ms)));
        total_ms += hold_ms;
    }
    content.push_str(&format!("file '{}'\nduration {}\n", last.file, seconds(last_hold_ms)));
    // The concat demuxer ignores the final entry's duration unless the file appears once more —
    // a long-standing ffmpeg slideshow quirk, not a formatting choice.
    content.push_str(&format!("file '{}'\n", last.file));
    total_ms += last_hold_ms;
    Some(ConcatScript { content, total_ms })
}

fn seconds(ms: u64) -> String {
    format!("{}.{:03}", ms / 1_000, ms % 1_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(file: &str, at_ms: u64) -> FrameEntry {
        FrameEntry { file: file.to_owned(), at_ms }
    }

    #[test]
    fn concat_holds_each_frame_for_its_real_gap() {
        let frames = [
            frame("frame-000001.png", 1_000),
            frame("frame-000002.png", 1_250),
            frame("frame-000003.png", 2_400),
        ];
        let script = concat_script(&frames, 4).unwrap();
        assert_eq!(
            script.content,
            "ffconcat version 1.0\n\
             file 'frame-000001.png'\nduration 0.250\n\
             file 'frame-000002.png'\nduration 1.150\n\
             file 'frame-000003.png'\nduration 0.250\n\
             file 'frame-000003.png'\n"
        );
        // 250 + 1150 real gaps + the final frame's nominal 250ms hold.
        assert_eq!(script.total_ms, 1_650);
    }

    #[test]
    fn a_single_frame_is_held_for_one_nominal_interval() {
        let script = concat_script(&[frame("frame-000001.png", 5)], 2).unwrap();
        assert!(script.content.contains("duration 0.500"), "{}", script.content);
        assert!(script.content.ends_with("file 'frame-000001.png'\n"));
        assert_eq!(script.total_ms, 500);
    }

    #[test]
    fn no_frames_means_no_script() {
        assert!(concat_script(&[], 4).is_none());
    }

    #[test]
    fn equal_timestamps_floor_at_one_millisecond() {
        let frames = [frame("a.png", 100), frame("b.png", 100)];
        let script = concat_script(&frames, 4).unwrap();
        assert!(script.content.contains("file 'a.png'\nduration 0.001\n"), "{}", script.content);
    }

    #[test]
    fn manifest_round_trips_with_camel_case_keys() {
        let manifest = Manifest {
            recording: "rec-1".to_owned(),
            fps_cap: 4,
            frames: vec![frame("frame-000001.png", 42)],
        };
        let json = serde_json::to_string(&manifest).unwrap();
        assert!(json.contains("\"atMs\":42"), "{json}");
        assert!(json.contains("\"fpsCap\":4"), "{json}");
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.frames[0].file, "frame-000001.png");
    }
}
