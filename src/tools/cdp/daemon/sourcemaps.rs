//! The script registry and its source maps: every parsed script the Debugger reports, the maps
//! they carry, and the two lookups built on them — arming a repo path as a generated-site
//! breakpoint, and resolving minified exception frames back to original sources. Decoding is
//! pure (`crate::cdp::sourcemap`); this is the I/O shell that records, fetches, and caches.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::cdp::{self, CdpEvent, Source, SourceMatch, Track};

use super::super::protocol::TimelineQuery;
use super::super::trace::PointLocation;
use super::{collect_timeline, evaluate, push_marker, Shared, State};

/// Registry size guard — scripts re-parse on every navigation; the prune hooks keep this from
/// mattering, the cap keeps a pathological page from mattering more.
pub(super) const SCRIPT_CAP: usize = 1024;

/// How long a freshly-enabled Debugger gets to replay `scriptParsed` for already-parsed scripts
/// before the registry is consulted.
pub(super) const SCRIPT_BACKLOG_BEAT: Duration = Duration::from_millis(400);

/// One parsed script the registry knows: whose session it runs in, its URL, and where its source
/// map lives (possibly relative, possibly inline) when it has one.
pub(super) struct ScriptRecord {
    pub(super) session: String,
    pub(super) url: String,
    map_url: Option<String>,
}

impl State {
    pub(super) fn prune_scripts(&mut self, gone: impl Fn(&ScriptRecord) -> bool) {
        let dead: Vec<String> = self
            .scripts
            .iter()
            .filter(|(_, record)| gone(record))
            .map(|(id, _)| id.clone())
            .collect();
        for id in dead {
            self.scripts.remove(&id);
            self.source_maps.remove(&id);
        }
    }

    /// Fill `resolved` on an exception whose frame's map is already decoded. Runs at emit time
    /// (so live subscribers and the ring agree) and again at query time (for events captured
    /// before their map loaded — `errors --resolve` pre-loads, then every view resolves free).
    /// The frame's URL must match the registry record: script ids are per-isolate and can
    /// collide across renderer processes.
    pub(super) fn resolve_exception_event(&self, event: &mut cdp::TimelineEvent) {
        let Track::Exception(info) = &mut event.track else {
            return;
        };
        if info.resolved.is_some() {
            return;
        }
        info.resolved = info.frames.iter().find_map(|frame| {
            let map = self.source_maps.get(&frame.script_id)?.as_ref()?;
            let record = self.scripts.get(&frame.script_id)?;
            if record.url != frame.url {
                return None;
            }
            let (source, line, _) = map.original_for(frame.line as u32, frame.column as u32)?;
            Some(format!("{source}:{}", line + 1))
        });
    }

    pub(super) fn resolve_exception_sites(&self, events: &mut [cdp::TimelineEvent]) {
        for event in events {
            self.resolve_exception_event(event);
        }
    }
}

/// Record a parsed script — the registry's only ingestion point. Every script with a URL is
/// recorded (the `trace find` search space and the bound-site readback need them all, not just
/// the map-bearing ones), and armed logpoints are told so drift can be healed.
pub(super) fn record_script(state: &Shared, event: &CdpEvent) {
    let Some(session) = event.session.as_deref() else {
        return;
    };
    let params = &event.params;
    let (Some(script_id), Some(url)) =
        (params.get("scriptId").and_then(Value::as_str), params.get("url").and_then(Value::as_str))
    else {
        return;
    };
    if url.is_empty() {
        return;
    }
    let map_url = params
        .get("sourceMapURL")
        .and_then(Value::as_str)
        .filter(|map_url| !map_url.is_empty())
        .map(str::to_owned);
    let mut guard = state.lock().unwrap();
    if guard.scripts.len() >= SCRIPT_CAP && !guard.scripts.contains_key(script_id) {
        return;
    }
    guard.scripts.insert(
        script_id.to_owned(),
        ScriptRecord { session: session.to_owned(), url: url.to_owned(), map_url },
    );
    super::trace::note_script_parsed(&mut guard, url);
}

/// Load (and cache) one script's source map. Transport order: inline `data:` decodes here;
/// `file:` reads from disk (the daemon shares the machine); anything else fetches *through the
/// page* — the page can resolve its own scheme (`modular://`, asar) where the daemon cannot.
/// A failed load caches `None` and leaves one marker on the Timeline saying why, so an
/// unresolvable stack is a diagnosable fact instead of a silent gap.
pub(super) async fn load_source_map(
    state: &Shared,
    script_id: &str,
) -> Option<Arc<cdp::SourceMap>> {
    let (record_url, map_url, session) = {
        let guard = state.lock().unwrap();
        if let Some(cached) = guard.source_maps.get(script_id) {
            return cached.clone();
        }
        let record = guard.scripts.get(script_id)?;
        (record.url.clone(), record.map_url.clone()?, record.session.clone())
    };
    let resolved = cdp::resolve_map_url(&record_url, &map_url);
    let text: Result<String, String> = if let Some(inline) = cdp::inline_map(&resolved) {
        inline
    } else if let Some(path) = resolved.strip_prefix("file://") {
        std::fs::read_to_string(path).map_err(|error| error.to_string())
    } else {
        let conn = state.lock().unwrap().conn.clone();
        let url = serde_json::to_string(&resolved).expect("string encodes");
        let script = format!(
            "fetch({url}).then(r => {{ if (!r.ok) throw new Error('HTTP ' + r.status); return r.text(); }})"
        );
        evaluate(conn, session, script).await.and_then(|value| match value {
            Value::String(text) => Ok(text),
            _ => Err("fetch returned a non-string".to_owned()),
        })
    };
    let map = match text.and_then(|text| cdp::SourceMap::parse(&text)) {
        Ok(map) => Some(Arc::new(map)),
        Err(error) => {
            push_marker(
                state,
                Source::Renderer,
                &format!("source map for {} failed: {error}", short(&record_url)),
            );
            None
        }
    };
    state.lock().unwrap().source_maps.insert(script_id.to_owned(), map.clone());
    map
}

/// Where a source-mapped location landed: which script, the generated site to arm, and the
/// original line's text when the map carried `sourcesContent` — the proof of *what code* the
/// breakpoint is on.
pub(super) struct MappedSite {
    pub(super) script_url: String,
    pub(super) line: u32,
    pub(super) column: u32,
    pub(super) snippet: Option<String>,
}

/// Resolve a repo path through the registry's maps, scoped to one session. Exact-beats-suffix
/// inside each map; across scripts, one match arms and several is ambiguity to report — never a
/// guess.
pub(super) async fn resolve_via_maps(
    state: &Shared,
    session: &str,
    location: &PointLocation,
) -> Result<Option<MappedSite>, String> {
    let candidates: Vec<String> = {
        let guard = state.lock().unwrap();
        guard
            .scripts
            .iter()
            .filter(|(_, record)| record.session == session && record.map_url.is_some())
            .map(|(id, _)| id.clone())
            .collect()
    };
    let mut sites: Vec<MappedSite> = Vec::new();
    let mut ambiguous: Vec<String> = Vec::new();
    for script_id in candidates {
        let Some(map) = load_source_map(state, &script_id).await else {
            continue;
        };
        match map.match_source(&location.url) {
            SourceMatch::None => {}
            SourceMatch::Many(paths) => ambiguous.extend(paths),
            SourceMatch::One(source) => {
                let Some(script_url) =
                    state.lock().unwrap().scripts.get(&script_id).map(|r| r.url.clone())
                else {
                    continue;
                };
                match map.generated_for(source, location.line - 1) {
                    Some((line, column)) => {
                        let snippet = map
                            .source_line(source, location.line - 1)
                            .map(|text| text.trim().to_owned())
                            .filter(|text| !text.is_empty());
                        sites.push(MappedSite { script_url, line, column, snippet });
                    }
                    None => {
                        return Err(format!(
                            "{} maps '{}' but line {} has no executable mapping — pick a line with code",
                            short(&script_url),
                            location.url,
                            location.line
                        ))
                    }
                }
            }
        }
    }
    if !ambiguous.is_empty() {
        return Err(format!(
            "ambiguous source '{}' — candidates: {}",
            location.url,
            ambiguous.join(", ")
        ));
    }
    match sites.len() {
        0 => Ok(None),
        1 => Ok(sites.pop()),
        n => Err(format!(
            "'{}' is built into {n} scripts ({}) — trace the script url:line directly",
            location.url,
            sites.iter().map(|site| short(&site.script_url)).collect::<Vec<_>>().join(", ")
        )),
    }
}

pub(super) fn short(url: &str) -> &str {
    url.rsplit('/').next().filter(|tail| !tail.is_empty()).unwrap_or(url)
}

/// Warm the registry for `errors --resolve`: enable Debugger on every session (the registry only
/// hears `scriptParsed` on Debugger-enabled sessions — this is the command's disclosed side
/// effect), give a cold backlog a beat, then decode the maps the error frames actually reference.
pub(super) async fn prime_error_maps(state: &Shared, query: &TimelineQuery) {
    let (conn, sessions) = {
        let guard = state.lock().unwrap();
        (guard.conn.clone(), guard.sessions.keys().cloned().collect::<Vec<_>>())
    };
    let mut any_cold = false;
    for session in &sessions {
        if let Ok(true) = super::trace::ensure_debugger(state, &conn, session).await {
            any_cold = true;
        }
    }
    if any_cold {
        tokio::time::sleep(SCRIPT_BACKLOG_BEAT).await;
    }
    let Ok(events) = collect_timeline(state, query) else {
        return;
    };
    let wanted: Vec<String> = {
        let guard = state.lock().unwrap();
        events
            .iter()
            .filter_map(|event| match &event.track {
                Track::Exception(info) => Some(&info.frames),
                _ => None,
            })
            .flatten()
            .filter(|frame| {
                guard.scripts.contains_key(&frame.script_id)
                    && !guard.source_maps.contains_key(&frame.script_id)
            })
            .map(|frame| frame.script_id.clone())
            .collect()
    };
    for script_id in wanted {
        let _ = load_source_map(state, &script_id).await;
    }
}
