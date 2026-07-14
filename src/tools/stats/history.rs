use std::collections::{HashMap, HashSet, VecDeque};

use super::model::{ProcessKey, StatsSnapshot};

const SAMPLE_INTERVAL_MS: u64 = 2_000;
const WINDOW_SECONDS: u64 = 8 * 60;
const MAX_POINTS: usize = 240;
const MAX_IDENTITIES: usize = 1_024;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct HistoryPoint {
    pub(super) elapsed_seconds: u32,
    pub(super) cpu_percent: f32,
    pub(super) rss_bytes: u64,
}

pub(super) struct HistorySeries {
    base_ms: u64,
    points: VecDeque<HistoryPoint>,
    current_cpu: f32,
    current_rss: u64,
}

#[derive(Default)]
pub(super) struct HistoryStore {
    series: HashMap<ProcessKey, HistorySeries>,
    elapsed_ms: u64,
    last_recorded_ms: Option<u64>,
}

impl HistoryStore {
    pub(super) fn record(
        &mut self,
        snapshot: &StatsSnapshot,
        protected: impl IntoIterator<Item = ProcessKey>,
    ) -> bool {
        self.elapsed_ms = self.elapsed_ms.saturating_add(snapshot.interval_ms);
        if self
            .last_recorded_ms
            .is_some_and(|previous| self.elapsed_ms.saturating_sub(previous) < SAMPLE_INTERVAL_MS)
        {
            return false;
        }
        self.last_recorded_ms = Some(self.elapsed_ms);
        let protected = protected.into_iter().collect::<HashSet<_>>();
        self.prune_expired();
        for process in &snapshot.processes {
            let Some(key) = process.identity.stable_key() else { continue };
            if !self.series.contains_key(&key)
                && !self.admit(key, process.cpu_percent, process.rss_bytes, &protected)
            {
                continue;
            }
            let series = self.series.entry(key).or_insert_with(|| HistorySeries {
                base_ms: self.elapsed_ms,
                points: VecDeque::new(),
                current_cpu: process.cpu_percent,
                current_rss: process.rss_bytes,
            });
            let elapsed_seconds = self
                .elapsed_ms
                .saturating_sub(series.base_ms)
                .saturating_div(1_000)
                .min(u32::MAX as u64) as u32;
            while series.points.front().is_some_and(|point| {
                u64::from(elapsed_seconds.saturating_sub(point.elapsed_seconds)) >= WINDOW_SECONDS
            }) {
                series.points.pop_front();
            }
            if series.points.len() == MAX_POINTS {
                series.points.pop_front();
            }
            series.points.push_back(HistoryPoint {
                elapsed_seconds,
                cpu_percent: process.cpu_percent,
                rss_bytes: process.rss_bytes,
            });
            series.current_cpu = process.cpu_percent;
            series.current_rss = process.rss_bytes;
        }
        true
    }

    pub(super) fn get(&self, key: ProcessKey) -> Option<&HistorySeries> {
        self.series.get(&key)
    }

    fn prune_expired(&mut self) {
        let now = self.elapsed_ms;
        self.series.retain(|_, series| {
            let elapsed_seconds =
                now.saturating_sub(series.base_ms).saturating_div(1_000).min(u32::MAX as u64)
                    as u32;
            while series.points.front().is_some_and(|point| {
                u64::from(elapsed_seconds.saturating_sub(point.elapsed_seconds)) >= WINDOW_SECONDS
            }) {
                series.points.pop_front();
            }
            !series.points.is_empty()
        });
    }

    fn admit(
        &mut self,
        incoming: ProcessKey,
        cpu: f32,
        rss: u64,
        protected: &HashSet<ProcessKey>,
    ) -> bool {
        if self.series.len() < MAX_IDENTITIES {
            return true;
        }
        let victim = self
            .series
            .iter()
            .filter(|(key, _)| !protected.contains(key))
            .min_by(|(left_key, left), (right_key, right)| {
                left.current_cpu
                    .total_cmp(&right.current_cpu)
                    .then_with(|| left.current_rss.cmp(&right.current_rss))
                    .then_with(|| left_key.pid.cmp(&right_key.pid))
                    .then_with(|| left_key.start_token.cmp(&right_key.start_token))
            })
            .map(|(key, series)| (*key, series.current_cpu, series.current_rss));
        let Some((victim, victim_cpu, victim_rss)) = victim else { return false };
        let hotter = cpu
            .total_cmp(&victim_cpu)
            .then_with(|| rss.cmp(&victim_rss))
            .then_with(|| incoming.pid.cmp(&victim.pid))
            .then_with(|| incoming.start_token.cmp(&victim.start_token))
            .is_gt();
        if hotter {
            self.series.remove(&victim);
        }
        hotter
    }
}

impl HistorySeries {
    pub(super) fn points(&self) -> impl DoubleEndedIterator<Item = HistoryPoint> + '_ {
        self.points.iter().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::{
        CpuSample, HostCapabilities, ProcessIdentity, ProcessSample, ProcessState, SampleReadiness,
        SystemSample,
    };
    use super::*;

    fn snapshot(sequence: u64, interval_ms: u64, count: usize) -> StatsSnapshot {
        StatsSnapshot {
            sequence,
            sampled_at_ms: sequence.saturating_mul(1_000),
            interval_ms,
            collection_duration_ms: 1,
            readiness: SampleReadiness::Ready,
            host: HostCapabilities {
                last_observed_core: super::super::model::CapabilityState::Available,
                threads: super::super::model::CapabilityState::Available,
                resources: super::super::model::CapabilityState::Available,
                graceful_terminate: super::super::model::CapabilityState::Available,
                force_terminate: super::super::model::CapabilityState::Available,
                code_profile: super::super::model::CapabilityState::Available,
            },
            system: SystemSample {
                global_cpu_percent: 1.0,
                cpus: vec![CpuSample { logical_index: 0, usage_percent: 1.0 }],
                total_memory_bytes: 1,
                used_memory_bytes: 1,
                total_swap_bytes: 0,
                used_swap_bytes: 0,
                process_count: count,
                thread_count: 0,
                load_average: [0.0; 3],
                uptime_seconds: 1,
            },
            processes: (0..count)
                .map(|index| ProcessSample {
                    identity: ProcessIdentity::stable(ProcessKey {
                        pid: index as u32 + 1,
                        start_token: index as u64 + 1,
                    }),
                    parent_pid: None,
                    name: format!("p{index}"),
                    command: format!("p{index}"),
                    user: None,
                    state: ProcessState::Sleeping,
                    cpu_percent: index as f32,
                    rss_bytes: index as u64,
                    started_at_ms: 0,
                    run_time_seconds: 1,
                    last_cpu: None,
                })
                .collect(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn two_second_cadence_bounds_fast_overview_input() {
        let mut history = HistoryStore::default();
        assert!(history.record(&snapshot(1, 250, 1), []));
        assert!(!history.record(&snapshot(2, 250, 1), []));
        assert!(history.record(&snapshot(3, 1_750, 1), []));
        let key = ProcessKey { pid: 1, start_token: 1 };
        assert_eq!(history.get(key).unwrap().points().count(), 2);
    }

    #[test]
    fn bounded_store_protects_selection_and_admits_hotter_processes() {
        let mut history = HistoryStore::default();
        let first = snapshot(1, 2_000, MAX_IDENTITIES);
        let protected = first.processes[0].identity.stable_key().unwrap();
        history.record(&first, [protected]);
        let mut next = snapshot(2, 2_000, 1);
        next.processes[0].identity =
            ProcessIdentity::stable(ProcessKey { pid: 50_000, start_token: 50_000 });
        next.processes[0].cpu_percent = 10_000.0;
        history.record(&next, [protected]);
        assert_eq!(history.series.len(), MAX_IDENTITIES);
        assert!(history.series.contains_key(&protected));
        assert!(history.series.contains_key(&next.processes[0].identity.stable_key().unwrap()));
    }

    #[test]
    fn exited_series_expire_without_reappearing_in_a_snapshot() {
        let mut history = HistoryStore::default();
        let key = ProcessKey { pid: 1, start_token: 1 };
        history.record(&snapshot(1, 2_000, 1), []);
        assert!(history.get(key).is_some());
        history.record(&snapshot(2, WINDOW_SECONDS * 1_000, 0), []);
        assert!(history.get(key).is_none());
    }

    #[test]
    fn wall_clock_changes_do_not_control_history_cadence() {
        let mut history = HistoryStore::default();
        let mut first = snapshot(1, 2_000, 1);
        first.sampled_at_ms = 100_000;
        let mut second = snapshot(2, 2_000, 1);
        second.sampled_at_ms = 1;
        assert!(history.record(&first, []));
        assert!(history.record(&second, []));
    }

    #[test]
    fn point_and_container_layout_fit_the_five_mebibyte_budget() {
        assert_eq!(std::mem::size_of::<HistoryPoint>(), 16);
        let point_capacity = MAX_POINTS.next_power_of_two();
        let per_series_overhead =
            std::mem::size_of::<HistorySeries>() + std::mem::size_of::<ProcessKey>() + 32;
        let conservative_bytes = MAX_IDENTITIES
            * (point_capacity * std::mem::size_of::<HistoryPoint>() + per_series_overhead)
            + std::mem::size_of::<HistoryStore>();
        assert!(
            conservative_bytes < 5 * 1024 * 1024,
            "history upper estimate is {conservative_bytes} bytes"
        );
    }
}
