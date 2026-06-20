//! Real CDP integration test for the `errors` view: launch a headless Chrome, drive genuine
//! `console.error` calls and a thrown exception in a live page, capture them through the *production*
//! engine path (auto-attach → `Runtime.enable` → `Track::from_event`), and assert `group_errors`
//! collapses the duplicates into counted groups. No mocks — this exercises the real protocol wire.
//!
//! Skips itself (passes) when no Chrome binary is present, so the suite stays green on a box without
//! one; on CI/dev with Chrome installed it is a true end-to-end check.

mod common;

use std::time::Duration;

use kit::cdp::{
    browser_endpoint, group_errors, CdpConnection, CdpEvent, Source, TimelineEvent, Track,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Attach to the browser exactly as the daemon does — discover + flatten auto-attach, **one** session
/// per target — enable `Runtime` on the page, inject `script` to drive real errors, then fold the
/// resulting `Runtime` events into the same `TimelineEvent` shape the daemon builds. Collection ends
/// the moment `done_marker` is seen in the console, so the test never races the async event stream.
///
/// A single attachment is the whole point: open a *second* one to the same page and every console
/// event arrives twice — which is exactly the double-counting this view must never do.
async fn capture_timeline(
    conn: &CdpConnection,
    mut events: mpsc::UnboundedReceiver<CdpEvent>,
    script: &str,
    done_marker: &str,
) -> Vec<TimelineEvent> {
    conn.call(None, "Target.setDiscoverTargets", json!({ "discover": true })).await.unwrap();
    conn.call(
        None,
        "Target.setAutoAttach",
        json!({ "autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true }),
    )
    .await
    .unwrap();

    let mut timeline: Vec<TimelineEvent> = Vec::new();
    let collect = async {
        while let Some(event) = events.recv().await {
            match event.method.as_str() {
                "Target.attachedToTarget" => {
                    let Some(session) = event.params.get("sessionId").and_then(Value::as_str)
                    else {
                        continue;
                    };
                    let is_page = event.params.pointer("/targetInfo/type")
                        == Some(&Value::String("page".to_owned()));
                    conn.call(Some(session), "Runtime.enable", json!({})).await.unwrap();
                    if is_page {
                        conn.call(
                            Some(session),
                            "Runtime.evaluate",
                            json!({ "expression": script }),
                        )
                        .await
                        .expect("inject error script");
                    }
                }
                _ => {
                    if let Some(track) = Track::from_event(&event) {
                        let done = matches!(&track, Track::Console(line) if line.text.contains(done_marker));
                        timeline.push(TimelineEvent {
                            at_ms: timeline.len() as u64,
                            source: Source::Renderer,
                            target: "page".to_owned(),
                            track,
                        });
                        if done {
                            break;
                        }
                    }
                }
            }
        }
    };

    timeout(Duration::from_secs(10), collect).await.expect("timed out waiting for error events");
    timeline
}

#[tokio::test(flavor = "multi_thread")]
async fn errors_view_collapses_real_duplicate_errors() {
    let Some(chrome) = common::launch_chrome("kit-cdp-errors", 0).await else {
        eprintln!("no chrome binary — skipping live CDP integration test");
        return;
    };

    let endpoint = browser_endpoint(chrome.port).await.expect("browser endpoint");
    let (conn, events) = CdpConnection::connect(&endpoint.ws_url).await.expect("connect");

    // Drive real errors in the page across two tracks: the same console.error three times, a real
    // uncaught exception (via setTimeout so it surfaces as Runtime.exceptionThrown, not the eval's
    // return), a distinct console.error, then a unique marker that tells the collector everything is in.
    let script = r#"
        for (let i = 0; i < 3; i++) console.error('boom: cannot read x');
        setTimeout(() => { throw new TypeError('kaboom: undefined is not a function'); }, 0);
        console.error('different failure');
        setTimeout(() => console.error('DONE-MARKER-7f3a'), 10);
    "#;

    let timeline = capture_timeline(&conn, events, script, "DONE-MARKER-7f3a").await;
    let groups = group_errors(&timeline);

    let boom = groups
        .iter()
        .find(|group| matches!(&group.event.track, Track::Console(line) if line.text.contains("boom")))
        .expect("the repeated error survived as a group");
    assert_eq!(boom.count, 3, "three identical console.errors must collapse to one group of 3");

    let distinct = groups
        .iter()
        .find(|group| matches!(&group.event.track, Track::Console(line) if line.text.contains("different")))
        .expect("the distinct error is its own group");
    assert_eq!(distinct.count, 1);

    // The thrown exception is captured on its own track and grouped alongside the console errors.
    let exception = groups
        .iter()
        .find(|group| matches!(&group.event.track, Track::Exception(info) if info.text.contains("kaboom")))
        .expect("the uncaught exception became a group");
    assert_eq!(exception.count, 1);

    // The view is genuinely smaller than the raw stream: 6 error events became 4 groups
    // (boom×3, the exception, "different failure", and the done marker).
    let raw_errors = timeline.iter().filter(|event| event.track.is_error()).count();
    assert_eq!(raw_errors, 6, "six raw error events were captured");
    assert_eq!(groups.len(), 4, "collapsed to four distinct groups");
}

/// The "never hide an issue" guarantee, proven on live CDP data. Two `console.error`s share a prefix
/// but carry *different* objects — exactly the case that, before object previews, collapsed to one
/// `Object` line and made one of two distinct errors invisible. The contract now is: they either stay
/// distinct (the preview disambiguated them) or land in one group flagged with multiple variants
/// (the audit caught the collision). What must NEVER happen is one clean group of count 2 with a
/// single variant — that would be a silent merge of two different errors.
#[tokio::test(flavor = "multi_thread")]
async fn distinct_objects_never_collapse_silently() {
    let Some(chrome) = common::launch_chrome("kit-cdp-errors", 1).await else {
        eprintln!("no chrome binary — skipping live CDP integration test");
        return;
    };

    let endpoint = browser_endpoint(chrome.port).await.expect("browser endpoint");
    let (conn, events) = CdpConnection::connect(&endpoint.ws_url).await.expect("connect");

    let script = r#"
        console.error('request failed', { code: 500, op: 'getWorkspaceInfo' });
        console.error('request failed', { code: 404, op: 'listMembers' });
        console.error('SENTINEL-done');
    "#;

    let timeline = capture_timeline(&conn, events, script, "SENTINEL-done").await;
    let groups = group_errors(&timeline);

    let failures: Vec<_> = groups
        .iter()
        .filter(|group| matches!(&group.event.track, Track::Console(line) if line.text.contains("request failed")))
        .collect();

    let distinct_kept_apart = failures.len() == 2;
    let collision_was_flagged =
        failures.len() == 1 && failures[0].count == 2 && failures[0].has_variants();
    assert!(
        distinct_kept_apart || collision_was_flagged,
        "two different objects must never collapse to one clean group — got {failures:#?}",
    );

    // The forbidden state, stated as its own assertion so a regression names itself.
    let silent_merge = failures.len() == 1 && failures[0].count == 2 && !failures[0].has_variants();
    assert!(!silent_merge, "SILENT MERGE: two distinct errors hidden behind one unflagged count");
}
