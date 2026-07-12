//! Real CDP integration test for screenshot capture: launch a headless Chrome, attach exactly as
//! the daemon does (flatten auto-attach, one session per target), and capture the live page through
//! the *production* engine path (`Page.captureScreenshot` → base64 decode). Asserts the decoded
//! bytes are genuine images in every supported format — magic numbers and real pixel dimensions,
//! not just "something came back".
//!
//! Skips itself (passes) when no Chrome binary is present, so the suite stays green on a box without
//! one; on CI/dev with Chrome installed it is a true end-to-end check.

mod common;

use std::time::Duration;

use kit::cdp::{
    browser_endpoint, capture_screenshot, CdpConnection, CdpEventStream, ImageFormat, NoFrame,
};
use serde_json::{json, Value};
use tokio::time::{timeout, Instant};

/// A budget generous enough that a live headless page always paints inside it.
const BUDGET: Duration = Duration::from_secs(5);

/// Flatten auto-attach and return the first page target's CDP session id.
async fn page_session(conn: &CdpConnection, events: &mut CdpEventStream) -> String {
    conn.call(
        None,
        "Target.setAutoAttach",
        json!({ "autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true }),
    )
    .await
    .expect("auto-attach");

    let found = async {
        while let Some(event) = events.recv().await {
            if event.method == "Target.attachedToTarget"
                && event.params.pointer("/targetInfo/type")
                    == Some(&Value::String("page".to_owned()))
            {
                return event
                    .params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .expect("attached page carries a sessionId")
                    .to_owned();
            }
        }
        panic!("event stream closed before a page attached");
    };
    timeout(Duration::from_secs(10), found).await.expect("no page target attached")
}

/// PNG pixel dimensions from the IHDR chunk — width and height are big-endian u32 at bytes 16..24.
fn png_dimensions(bytes: &[u8]) -> (u32, u32) {
    let field = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
    (field(16), field(20))
}

#[tokio::test(flavor = "multi_thread")]
async fn captures_real_pixels_in_every_format() {
    let Some(chrome) = common::launch_chrome("kit-cdp-screenshot", 2).await else {
        eprintln!("no chrome binary — skipping live CDP integration test");
        return;
    };

    let endpoint = browser_endpoint(chrome.port).await.expect("browser endpoint");
    let (conn, mut events) = CdpConnection::connect(&endpoint.ws_url).await.expect("connect");
    let session = page_session(&conn, &mut events).await;

    let png = capture_screenshot(&conn, Some(&session), ImageFormat::Png, None, false, BUDGET)
        .await
        .expect("png capture");
    assert!(png.starts_with(&[0x89, b'P', b'N', b'G']), "png magic bytes");
    let (width, height) = png_dimensions(&png);
    assert!(width >= 100 && height >= 100, "a real viewport, not a stub — got {width}x{height}");

    let jpeg =
        capture_screenshot(&conn, Some(&session), ImageFormat::Jpeg, Some(80), false, BUDGET)
            .await
            .expect("jpeg capture");
    assert!(jpeg.starts_with(&[0xFF, 0xD8, 0xFF]), "jpeg magic bytes");

    let webp =
        capture_screenshot(&conn, Some(&session), ImageFormat::Webp, Some(80), false, BUDGET)
            .await
            .expect("webp capture");
    assert!(webp.starts_with(b"RIFF") && &webp[8..12] == b"WEBP", "webp RIFF container");

    let full = capture_screenshot(&conn, Some(&session), ImageFormat::Png, None, true, BUDGET)
        .await
        .expect("full-page capture");
    assert!(full.starts_with(&[0x89, b'P', b'N', b'G']), "full-page png magic bytes");
}

/// A budget too small to ever satisfy must fail *fast* with a typed [`NoFrame`] — never block on the
/// generic call timeout. This is the regression guard for the ~25s mid-reload hang.
#[tokio::test(flavor = "multi_thread")]
async fn overrun_budget_fails_fast_with_no_frame() {
    let Some(chrome) = common::launch_chrome("kit-cdp-noframe", 3).await else {
        eprintln!("no chrome binary — skipping live CDP integration test");
        return;
    };

    let endpoint = browser_endpoint(chrome.port).await.expect("browser endpoint");
    let (conn, mut events) = CdpConnection::connect(&endpoint.ws_url).await.expect("connect");
    let session = page_session(&conn, &mut events).await;

    let started = Instant::now();
    let outcome = capture_screenshot(
        &conn,
        Some(&session),
        ImageFormat::Png,
        None,
        false,
        Duration::from_millis(1),
    )
    .await;
    let elapsed = started.elapsed();

    let error = outcome.expect_err("a 1ms budget cannot satisfy a real capture round-trip");
    assert!(error.downcast_ref::<NoFrame>().is_some(), "a budget overrun is a NoFrame: {error:#}");
    assert!(elapsed < Duration::from_secs(1), "must fail fast, not block — took {elapsed:?}");
}
