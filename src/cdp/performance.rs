use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::time::sleep;

use super::{probe_metrics, CdpConnection, TargetMetrics};

#[derive(Debug, Clone, Serialize)]
pub struct CpuHotspot {
    pub function_name: String,
    pub url: Option<String>,
    pub line: Option<u64>,
    pub column: Option<u64>,
    pub samples: u64,
    pub self_time_us: u64,
    pub self_percent: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CpuProfileSummary {
    pub total_samples: u64,
    pub total_sampled_us: u64,
    pub hotspots: Vec<CpuHotspot>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LayerMetrics {
    pub total: usize,
    pub drawing: usize,
    pub dom_mapped: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct PerformanceReport {
    pub requested_ms: u64,
    pub wall_time_ms: u64,
    pub sampling_interval_us: u64,
    pub performance_metrics_error: Option<String>,
    pub layer_metrics_error: Option<String>,
    pub layers: Option<LayerMetrics>,
    pub metric_deltas: BTreeMap<String, f64>,
    pub before: TargetMetrics,
    pub after: TargetMetrics,
    pub cpu: CpuProfileSummary,
}

#[derive(Debug)]
pub struct PerformanceCapture {
    pub profile: Value,
    pub report: PerformanceReport,
}

/// Capture one target with Chrome's sampling profiler while recording main-thread and memory
/// counters around the same window. The raw profile remains suitable for DevTools import.
pub async fn capture_performance(
    connection: &CdpConnection,
    session: &str,
    duration: Duration,
    sampling_interval_us: u64,
    top: usize,
) -> Result<PerformanceCapture> {
    if duration.is_zero() {
        bail!("performance capture duration must be greater than zero");
    }
    if sampling_interval_us == 0 {
        bail!("performance sampling interval must be greater than zero");
    }

    connection.call(Some(session), "Profiler.enable", json!({})).await?;
    let capture = async {
        connection
            .call(
                Some(session),
                "Profiler.setSamplingInterval",
                json!({ "interval": sampling_interval_us }),
            )
            .await?;
        let mut performance_metrics_error = None;
        let performance_enabled =
            match connection.call(Some(session), "Performance.enable", json!({})).await {
                Ok(_) => true,
                Err(error) => {
                    performance_metrics_error = Some(error.to_string());
                    false
                }
            };
        let metrics_before = if performance_enabled {
            match connection.call(Some(session), "Performance.getMetrics", json!({})).await {
                Ok(metrics) => metrics,
                Err(error) => {
                    performance_metrics_error = Some(error.to_string());
                    Value::Null
                }
            }
        } else {
            Value::Null
        };
        let before = probe_metrics(connection, Some(session)).await;

        connection.call(Some(session), "Profiler.start", json!({})).await?;
        let started = Instant::now();
        sleep(duration).await;
        let stopped = connection.call(Some(session), "Profiler.stop", json!({})).await?;
        let wall_time_ms = started.elapsed().as_millis() as u64;

        let metrics_after = if performance_enabled {
            match connection.call(Some(session), "Performance.getMetrics", json!({})).await {
                Ok(metrics) => metrics,
                Err(error) => {
                    performance_metrics_error = Some(error.to_string());
                    Value::Null
                }
            }
        } else {
            Value::Null
        };
        let after = probe_metrics(connection, Some(session)).await;
        let profile =
            stopped.get("profile").cloned().context("Profiler.stop returned no profile")?;
        let cpu = summarize_cpu_profile(&profile, top)?;

        Ok(PerformanceCapture {
            profile,
            report: PerformanceReport {
                requested_ms: duration.as_millis() as u64,
                wall_time_ms,
                sampling_interval_us,
                performance_metrics_error,
                layer_metrics_error: None,
                layers: None,
                metric_deltas: metric_deltas(&metrics_before, &metrics_after),
                before,
                after,
                cpu,
            },
        })
    }
    .await;

    let _ = connection.call(Some(session), "Performance.disable", json!({})).await;
    let _ = connection.call(Some(session), "Profiler.disable", json!({})).await;
    capture
}

fn metric_deltas(before: &Value, after: &Value) -> BTreeMap<String, f64> {
    let values = |input: &Value| {
        input
            .get("metrics")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|metric| {
                Some((metric.get("name")?.as_str()?.to_owned(), metric.get("value")?.as_f64()?))
            })
            .collect::<BTreeMap<_, _>>()
    };
    let before = values(before);
    values(after)
        .into_iter()
        .filter_map(|(name, value)| before.get(&name).map(|old| (name, value - old)))
        .collect()
}

fn summarize_cpu_profile(profile: &Value, top: usize) -> Result<CpuProfileSummary> {
    let nodes = profile.get("nodes").and_then(Value::as_array).context("profile has no nodes")?;
    let samples =
        profile.get("samples").and_then(Value::as_array).context("profile has no samples")?;
    let deltas =
        profile.get("timeDeltas").and_then(Value::as_array).context("profile has no timeDeltas")?;
    if samples.len() != deltas.len() {
        bail!("profile has {} samples but {} time deltas", samples.len(), deltas.len());
    }

    let frames: HashMap<u64, &Value> = nodes
        .iter()
        .filter_map(|node| Some((node.get("id")?.as_u64()?, node.get("callFrame")?)))
        .collect();
    let mut sampled: HashMap<u64, (u64, u64)> = HashMap::new();
    for (sample, delta) in samples.iter().zip(deltas) {
        let sample = sample.as_u64().context("profile sample is not an id")?;
        let delta = delta.as_u64().context("profile time delta is not a duration")?;
        let entry = sampled.entry(sample).or_default();
        entry.0 += 1;
        entry.1 += delta;
    }
    let total_sampled_us = sampled.values().map(|(_, duration)| duration).sum::<u64>();
    let total_samples = samples.len() as u64;

    let mut hotspots = sampled
        .into_iter()
        .filter_map(|(id, (samples, self_time_us))| {
            let frame = frames.get(&id)?;
            let function_name = frame
                .get("functionName")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .unwrap_or("(anonymous)")
                .to_owned();
            let url = frame
                .get("url")
                .and_then(Value::as_str)
                .filter(|url| !url.is_empty())
                .map(str::to_owned);
            let one_based = |name: &str| {
                frame
                    .get(name)
                    .and_then(Value::as_i64)
                    .filter(|number| *number >= 0)
                    .map(|number| number as u64 + 1)
            };
            Some(CpuHotspot {
                function_name,
                url,
                line: one_based("lineNumber"),
                column: one_based("columnNumber"),
                samples,
                self_time_us,
                self_percent: if total_sampled_us == 0 {
                    0.0
                } else {
                    self_time_us as f64 * 100.0 / total_sampled_us as f64
                },
            })
        })
        .collect::<Vec<_>>();
    hotspots.sort_by(|left, right| {
        right
            .self_time_us
            .cmp(&left.self_time_us)
            .then_with(|| left.function_name.cmp(&right.function_name))
    });
    hotspots.truncate(top);

    Ok(CpuProfileSummary { total_samples, total_sampled_us, hotspots })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{metric_deltas, summarize_cpu_profile};

    #[test]
    fn cpu_profile_summary_ranks_nodes_by_exact_self_time() {
        let profile = json!({
            "nodes": [
                { "id": 1, "callFrame": { "functionName": "(root)", "url": "", "lineNumber": -1, "columnNumber": -1 } },
                { "id": 2, "callFrame": { "functionName": "(program)", "url": "", "lineNumber": -1, "columnNumber": -1 } },
                { "id": 3, "callFrame": { "functionName": "renderRows", "url": "app.js", "lineNumber": 41, "columnNumber": 3 } },
                { "id": 4, "callFrame": { "functionName": "layout", "url": "app.js", "lineNumber": 86, "columnNumber": 7 } }
            ],
            "samples": [2, 3, 3, 4],
            "timeDeltas": [1000, 2000, 3000, 4000]
        });

        let summary = summarize_cpu_profile(&profile, 2).expect("valid profile");

        assert_eq!(summary.total_sampled_us, 10_000);
        assert_eq!(summary.hotspots.len(), 2);
        assert_eq!(summary.hotspots[0].function_name, "renderRows");
        assert_eq!(summary.hotspots[0].self_time_us, 5_000);
        assert_eq!(summary.hotspots[0].self_percent, 50.0);
        assert_eq!(summary.hotspots[1].function_name, "layout");
        assert_eq!(summary.hotspots[1].self_time_us, 4_000);
    }

    #[test]
    fn performance_metric_deltas_keep_only_numeric_pairs() {
        let before = json!({
            "metrics": [
                { "name": "TaskDuration", "value": 12.5 },
                { "name": "ScriptDuration", "value": 3.0 },
                { "name": "Frames", "value": 20.0 }
            ]
        });
        let after = json!({
            "metrics": [
                { "name": "TaskDuration", "value": 14.0 },
                { "name": "ScriptDuration", "value": 3.25 },
                { "name": "LayoutCount", "value": 7.0 }
            ]
        });

        let deltas = metric_deltas(&before, &after);

        assert_eq!(deltas.get("TaskDuration"), Some(&1.5));
        assert_eq!(deltas.get("ScriptDuration"), Some(&0.25));
        assert!(!deltas.contains_key("Frames"));
        assert!(!deltas.contains_key("LayoutCount"));
    }
}
