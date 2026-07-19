use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use super::actions::ActionController;
use super::app::{Action, StatsApp};
use super::contributions::{self, StatsActionRegistry};
use super::render::{render, UiRegions};
use super::sampler::{Sampler, SamplerWorker};
use crate::tui::{EventReader, Session, SessionOptions};

pub async fn run(interval: Duration, mouse_capture: bool) -> Result<()> {
    let registry = contributions::registry()?;
    run_validated(interval, mouse_capture, registry).await
}

async fn run_validated(
    interval: Duration,
    mouse_capture: bool,
    registry: StatsActionRegistry,
) -> Result<()> {
    let sampler = Sampler::new(interval)?;
    let (worker, mut snapshots, mut details) = SamplerWorker::start(sampler)?;
    let initial = Arc::clone(&snapshots.borrow());
    let mut app = StatsApp::new_validated(initial, registry, mouse_capture);
    let mut session = Session::open(SessionOptions { mouse_capture, bracketed_paste: false })?;
    let mut events = EventReader::start();
    let mut actions = ActionController::new();
    let mut hit_map = UiRegions::default();
    if let Some(intent) = app.reconcile_detail_intent() {
        worker.set_detail(intent.request());
    }

    loop {
        session.draw(|frame| hit_map = render(frame, &app))?;
        app.viewport_rows = hit_map.rows.len();
        tokio::select! {
            changed = snapshots.changed() => {
                if changed.is_err() {
                    break;
                }
                app.ingest(Arc::clone(&snapshots.borrow()));
            }
            changed = details.changed() => {
                if changed.is_err() {
                    break;
                }
                app.ingest_detail(details.borrow().clone());
            }
            event = events.recv() => {
                let Some(event) = event else { break };
                match app.on_event(event, &hit_map) {
                    Action::Quit => break,
                    Action::Process(key, requested) => {
                        match actions.start(key, requested) {
                            Ok(request) => app.action_started(request),
                            Err(active) => {
                                app.status = Some(format!(
                                    "Action already running for PID {}",
                                    active.key.pid
                                ));
                            }
                        }
                    }
                    Action::None => {}
                }
            }
            result = actions.recv() => {
                let succeeded = result.result.is_ok();
                app.action_finished(result);
                if succeeded {
                    worker.refresh();
                }
            }
        }
        if let Some(intent) = app.reconcile_detail_intent() {
            worker.set_detail(intent.request());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::super::app::{
        ActiveRegion, CommandViewer, Confirmation, ConfirmationChoice, DetailIntent, InspectorTab,
        SortBy, StatsOverlay,
    };
    use super::super::contributions::{
        self, StatsActionContext, StatsActionRegistry, StatsCommand, FORCE_TERMINATE, OPEN_PROFILE,
        PROCESS_COMMAND_INLINE, PROCESS_CONTEXT_MENU, PROCESS_INSPECTOR_INLINE, TERMINATE,
        VIEW_COMMAND,
    };
    use super::super::host::ProcessAction;
    use super::super::model::{
        CapabilityState, CpuSample, DetailCompleteness, DetailData, DetailOutcome, DetailRequest,
        DetailRequestKind, DetailSnapshot, Observed, ProcessIdentity, ProcessKey, ProcessSample,
        ProcessState, SampleReadiness, StatsSnapshot, SystemSample, ThreadSample,
    };
    use super::*;
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::layout::Position;

    use crate::tui::{
        ActionInvocation, ActionRegistryBuilder, ActionSpec, ActionState, ContextMenu, KeyChord,
        KeybindingPlacement, MenuPlacement,
    };

    fn draw_app(
        terminal: &mut ratatui::Terminal<ratatui::backend::TestBackend>,
        app: &StatsApp,
    ) -> UiRegions {
        let mut regions = UiRegions::default();
        terminal.draw(|frame| regions = render(frame, app)).unwrap();
        regions
    }

    fn confirmation(app: &StatsApp) -> Option<&Confirmation> {
        match app.overlay.as_ref() {
            Some(StatsOverlay::Confirmation(confirmation)) => Some(confirmation),
            _ => None,
        }
    }

    fn command_viewer(app: &StatsApp) -> Option<&CommandViewer> {
        match app.overlay.as_ref() {
            Some(StatsOverlay::CommandViewer(viewer)) => Some(viewer),
            _ => None,
        }
    }

    fn context_menu_target(app: &StatsApp) -> Option<ProcessIdentity> {
        match app.overlay.as_ref() {
            Some(StatsOverlay::ContextMenu(menu)) => Some(menu.context().identity),
            _ => None,
        }
    }

    fn open_context_menu(app: &mut StatsApp, identity: ProcessIdentity, anchor: Position) {
        let context = app.action_context(identity);
        let resolved = app.registry.resolve_menu(PROCESS_CONTEXT_MENU, &context);
        app.overlay = ContextMenu::open(anchor, context, resolved).map(StatsOverlay::ContextMenu);
    }

    fn event_key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn event_mouse(kind: MouseEventKind, position: Position) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: position.x,
            row: position.y,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn snapshot() -> Arc<StatsSnapshot> {
        Arc::new(StatsSnapshot {
            sequence: 1,
            sampled_at_ms: 0,
            interval_ms: 1_000,
            collection_duration_ms: 5,
            readiness: SampleReadiness::Ready,
            host: super::super::host::capabilities(),
            system: SystemSample {
                global_cpu_percent: 25.0,
                cpus: vec![CpuSample { logical_index: 0, usage_percent: 25.0 }],
                total_memory_bytes: 1024,
                used_memory_bytes: 512,
                total_swap_bytes: 0,
                used_swap_bytes: 0,
                process_count: 2,
                thread_count: 0,
                load_average: [1.0, 0.5, 0.25],
                uptime_seconds: 1,
            },
            processes: vec![process(2, "cool", 20.0), process(3, "quiet", 1.0)],
            warnings: Vec::new(),
        })
    }

    fn snapshot_with_cores(count: u16) -> Arc<StatsSnapshot> {
        let mut snapshot = Arc::unwrap_or_clone(snapshot());
        snapshot.system.cpus = (0..count)
            .map(|logical_index| CpuSample {
                logical_index,
                usage_percent: logical_index as f32 * 3.0 % 100.0,
            })
            .collect();
        Arc::new(snapshot)
    }

    fn snapshot_with_processes(count: usize) -> Arc<StatsSnapshot> {
        let base_pid = 10_000;
        let root_count = if count >= 1_000 { 50 } else { 10 };
        let mut snapshot = Arc::unwrap_or_clone(snapshot_with_cores(32));
        snapshot.processes = (0..count)
            .map(|index| {
                let pid = base_pid + index as u32;
                let mut process = process(
                    pid,
                    &format!("fixture-{index:04}"),
                    ((index * 37) % 3_200) as f32 / 10.0,
                );
                process.parent_pid = if index < root_count {
                    None
                } else if index % 97 == 0 {
                    Some(900_000 + index as u32)
                } else {
                    Some(base_pid + ((index - root_count) / 2) as u32)
                };
                process.command = format!("/usr/bin/fixture --worker {index:04}");
                process.user = Some(format!("user-{}", index % 8));
                process.rss_bytes = 1_048_576 + index as u64 * 4_096;
                process.last_cpu = Some((index % 32) as u16);
                process
            })
            .collect();
        if count > root_count + 1 {
            snapshot.processes[root_count].parent_pid = Some(base_pid + root_count as u32 + 1);
            snapshot.processes[root_count + 1].parent_pid = Some(base_pid + root_count as u32);
        }
        snapshot.system.process_count = snapshot.processes.len();
        snapshot.system.thread_count = count * 2 + count / 5;
        Arc::new(snapshot)
    }

    fn process(pid: u32, name: &str, cpu: f32) -> ProcessSample {
        ProcessSample {
            identity: ProcessIdentity::stable(ProcessKey { pid, start_token: pid as u64 }),
            parent_pid: Some(1),
            name: name.into(),
            command: format!("/bin/{name}"),
            user: Some("user".into()),
            state: ProcessState::Running,
            cpu_percent: cpu,
            rss_bytes: pid as u64 * 100,
            started_at_ms: 0,
            run_time_seconds: 1,
            last_cpu: Some(0),
        }
    }

    fn reused_target_snapshot(identity: ProcessIdentity) -> Arc<StatsSnapshot> {
        let mut replacement = Arc::unwrap_or_clone(snapshot());
        replacement.sequence += 1;
        let process = replacement
            .processes
            .iter_mut()
            .find(|process| process.identity.pid() == identity.pid())
            .expect("fixture PID must exist");
        process.identity = ProcessIdentity::stable(ProcessKey {
            pid: identity.pid(),
            start_token: identity.stable_key().unwrap().start_token + 10_000,
        });
        process.command = "replacement generation".into();
        Arc::new(replacement)
    }

    fn ready<T>(value: T) -> DetailOutcome<T> {
        DetailOutcome::Available {
            readiness: SampleReadiness::Ready,
            completeness: DetailCompleteness::Complete,
            value,
        }
    }

    fn warming<T>(value: T) -> DetailOutcome<T> {
        DetailOutcome::Available {
            readiness: SampleReadiness::Warming,
            completeness: DetailCompleteness::Complete,
            value,
        }
    }

    #[test]
    fn filtering_and_sorting_preserve_process_identity() {
        let mut app = StatsApp::new(snapshot());
        app.selected = Some(ProcessIdentity::stable(ProcessKey { pid: 3, start_token: 3 }));
        app.filter.set("quiet".into());
        app.reproject();
        assert_eq!(app.selected.unwrap().pid(), 3);
        assert_eq!(app.visible.len(), 1);
    }

    #[test]
    fn compact_and_wide_render_without_panicking() {
        for size in [(1, 1), (20, 5), (30, 8), (60, 18), (71, 20), (72, 20), (100, 32), (160, 50)] {
            let backend = ratatui::backend::TestBackend::new(size.0, size.1);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            let app = StatsApp::new(snapshot());
            let regions = draw_app(&mut terminal, &app);
            let screen = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(!screen.contains("needs at least"), "size {size:?} rendered a refusal");
            if size.0 >= 60 && size.1 >= 18 {
                assert!(!regions.rows.is_empty());
            }
        }
    }

    #[test]
    fn deterministic_performance_fixtures_match_the_control_contract() {
        for (processes, threads, roots) in [(100, 220, 10), (1_000, 2_200, 50)] {
            let snapshot = snapshot_with_processes(processes);
            assert_eq!(snapshot.processes.len(), processes);
            assert_eq!(snapshot.system.thread_count, threads);
            assert_eq!(snapshot.system.cpus.len(), 32);
            assert!(snapshot.processes.iter().any(|process| process.parent_pid == Some(900_097)));
            assert_eq!(
                snapshot.processes.iter().filter(|process| process.parent_pid.is_none()).count(),
                roots
            );
            assert_eq!(snapshot.processes[roots].parent_pid, Some(10_000 + roots as u32 + 1));
            assert_eq!(snapshot.processes[roots + 1].parent_pid, Some(10_000 + roots as u32));
        }
    }

    #[test]
    fn target_process_investigator_landmarks_replace_the_legacy_composition() {
        let backend = ratatui::backend::TestBackend::new(160, 50);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let app = StatsApp::new(snapshot_with_processes(100));
        let _ = draw_app(&mut terminal, &app);
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        for landmark in ["PROCESS TREE", "OVERVIEW", "FAMILY", "THREADS", "RESOURCES", "PROFILE"] {
            assert!(screen.contains(landmark), "missing approved landmark {landmark}");
        }
    }

    #[test]
    fn many_cores_stay_compact_and_leave_the_process_surface_dominant() {
        let backend = ratatui::backend::TestBackend::new(160, 50);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let app = StatsApp::new(snapshot_with_cores(32));
        let regions = draw_app(&mut terminal, &app);

        let core_bottom = regions.cores.iter().map(|(area, _)| area.bottom()).max().unwrap();
        let first_process_row = regions.rows.first().unwrap().area.y;
        assert!(core_bottom <= 4, "32 cores consumed rows through {core_bottom}");
        assert!(
            first_process_row <= 7,
            "process table did not begin until row {first_process_row}"
        );
        assert!(regions.inspector.is_some(), "inspector was not visible");
        assert!(regions.inline_actions.iter().any(|region| region.action == TERMINATE));

        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for label in [
            "KIT / STATS",
            "CPU AVG",
            "PEAK CORE",
            "CORE MAP",
            "TOP CORES NOW/RECENT",
            "PRESSURE SOURCES NOW/RECENT",
            "PROCESS TREE",
            "OVERVIEW",
        ] {
            assert!(screen.contains(label), "missing {label} surface");
        }
    }

    #[test]
    fn consolidated_pressure_exposes_a_hot_core_when_the_map_must_group_cores() {
        let mut snapshot = Arc::unwrap_or_clone(snapshot_with_cores(96));
        for cpu in &mut snapshot.system.cpus {
            cpu.usage_percent = 1.0;
        }
        snapshot.system.cpus[95].usage_percent = 100.0;
        snapshot.system.global_cpu_percent = 4.1;
        let app = StatsApp::new(Arc::new(snapshot));
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let regions = draw_app(&mut terminal, &app);
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(screen.contains("PEAK CORE C95 100.0%"));
        assert!(screen.contains("TOP CORES NOW/RECENT C95"));
        let top_core = app.core_pressure(1)[0];
        assert_eq!(top_core.logical_index, 95);
        assert_eq!(top_core.now_percent, 100.0);
        assert_eq!(top_core.recent_peak_percent, 100.0);
        assert!(regions.cores.len() < 96);
        assert!(regions.cores.iter().any(|(_, logical_index)| *logical_index == 95));
    }

    #[test]
    fn selected_inspector_identity_never_scrolls_the_top_processes_away() {
        let mut snapshot = Arc::unwrap_or_clone(snapshot());
        snapshot.processes = (0..30)
            .map(|index| process(index + 10, &format!("process-{index:02}"), index as f32))
            .collect();
        let quiet = snapshot.processes[0].identity;
        let mut app = StatsApp::new(Arc::new(snapshot));
        app.selected = Some(quiet);

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let regions = draw_app(&mut terminal, &app);

        assert_eq!(app.process(app.visible[0].key).unwrap().name, "process-29");
        assert_eq!(regions.rows[0].identity, app.visible[0].key);
        assert_eq!(app.selected, Some(quiet));
    }

    #[test]
    fn filter_and_focus_never_silently_transfer_the_selected_identity() {
        let mut app = StatsApp::new(snapshot());
        let selected = app.selected.unwrap();
        let other = app
            .snapshot
            .processes
            .iter()
            .find(|process| process.identity != selected)
            .unwrap()
            .name
            .clone();
        app.filter.set(other);
        app.reproject();
        assert_eq!(app.selected, Some(selected));
        assert!(!app.visible.iter().any(|row| row.key == selected));
    }

    #[test]
    fn refresh_preserves_the_first_manually_scrolled_process_as_viewport_anchor() {
        let mut snapshot = Arc::unwrap_or_clone(snapshot());
        snapshot.processes = (0..30)
            .map(|index| process(index + 10, &format!("process-{index:02}"), index as f32))
            .collect();
        let mut app = StatsApp::new(Arc::new(snapshot.clone()));
        app.set_sort(SortBy::Name);
        app.viewport_rows = 5;
        app.move_selection(12);
        assert!(app.row_offset > 0);
        let anchor = app.visible[app.row_offset].key;
        let selected = app.selected;

        for (index, process) in snapshot.processes.iter_mut().enumerate() {
            process.name = format!("process-{:02}", 29 - index);
        }
        snapshot.sequence += 1;
        snapshot.interval_ms = 2_000;
        app.ingest(Arc::new(snapshot));

        assert_eq!(app.visible[app.row_offset].key, anchor);
        assert_eq!(app.selected, selected);
    }

    #[test]
    fn recent_cpu_orders_stably_while_rows_keep_exact_current_cpu() {
        let mut first = Arc::unwrap_or_clone(snapshot());
        first.interval_ms = 2_000;
        first.processes[0].cpu_percent = 100.0;
        first.processes[1].cpu_percent = 0.0;
        let leader = first.processes[0].identity;
        let challenger = first.processes[1].identity;
        let mut app = StatsApp::new(Arc::new(first.clone()));

        first.sequence += 1;
        first.processes[0].cpu_percent = 0.0;
        first.processes[1].cpu_percent = 60.0;
        app.ingest(Arc::new(first));

        assert_eq!(app.visible[0].key, leader, "one spike must not immediately reorder the tree");
        assert_eq!(app.visible.iter().find(|row| row.key == leader).unwrap().cpu, 0.0);
        assert_eq!(app.visible.iter().find(|row| row.key == challenger).unwrap().cpu, 60.0);
        let sources = app.pressure_sources(2);
        assert_eq!(sources[0].identity, leader);
        assert_eq!(sources[0].now_percent, 0.0);
        assert_eq!(sources[0].recent_percent, 50.0);
    }

    #[test]
    fn pressure_sources_rank_hot_descendants_independently_of_tree_position() {
        let mut source = Arc::unwrap_or_clone(snapshot());
        source.interval_ms = 2_000;
        source.processes[0].cpu_percent = 0.0;
        source.processes[1].parent_pid = Some(source.processes[0].identity.pid());
        source.processes[1].cpu_percent = 95.0;
        let hot_child = source.processes[1].identity;
        let app = StatsApp::new(Arc::new(source));

        assert_eq!(app.visible[0].depth, 0);
        assert_eq!(app.visible[1].key, hot_child);
        assert_eq!(app.visible[1].depth, 1);
        assert_eq!(app.pressure_sources(1)[0].identity, hot_child);
    }

    #[test]
    fn recent_core_peak_keeps_a_migrated_hot_core_identifiable() {
        let mut source = Arc::unwrap_or_clone(snapshot_with_cores(2));
        source.interval_ms = 2_000;
        source.system.cpus[0].usage_percent = 100.0;
        source.system.cpus[1].usage_percent = 0.0;
        let mut app = StatsApp::new(Arc::new(source.clone()));

        source.sequence += 1;
        source.system.cpus[0].usage_percent = 0.0;
        source.system.cpus[1].usage_percent = 100.0;
        app.ingest(Arc::new(source));

        let cores = app.core_pressure(2);
        let previous = cores.iter().find(|core| core.logical_index == 0).unwrap();
        let current = cores.iter().find(|core| core.logical_index == 1).unwrap();
        assert_eq!((previous.now_percent, previous.recent_peak_percent), (0.0, 100.0));
        assert_eq!((current.now_percent, current.recent_peak_percent), (100.0, 100.0));
    }

    #[test]
    fn process_headers_sort_their_exact_columns_and_return_to_the_top() {
        let mut app = StatsApp::new(snapshot());
        app.viewport_rows = 1;
        app.move_selection(1);
        assert_eq!(app.row_offset, 1);

        let backend = ratatui::backend::TestBackend::new(140, 35);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let regions = draw_app(&mut terminal, &app);
        assert_eq!(regions.headers.len(), 4);

        let memory = regions
            .headers
            .iter()
            .find(|(_, sort)| *sort == SortBy::Memory)
            .expect("memory header")
            .0;
        app.on_event(
            event_mouse(
                MouseEventKind::Down(MouseButton::Left),
                Position { x: memory.x, y: memory.y },
            ),
            &regions,
        );

        assert_eq!(app.sort, SortBy::Memory);
        assert!(app.descending);
        assert_eq!(app.row_offset, 0);
        assert_eq!(app.visible[0].pid, 3);

        let first_row = regions.rows[0].area;
        app.on_event(
            event_mouse(
                MouseEventKind::Down(MouseButton::Left),
                Position { x: memory.x, y: first_row.y },
            ),
            &regions,
        );
        assert_eq!(app.sort, SortBy::Memory, "row clicks must not activate a header");
        assert!(app.descending, "row clicks must not reverse the active sort");
    }

    #[test]
    fn home_selects_the_first_sorted_process_and_resets_scroll() {
        let mut app = StatsApp::new(snapshot_with_processes(30));
        app.viewport_rows = 5;
        app.move_selection(12);
        assert!(app.row_offset > 0);

        app.on_event(event_key(KeyCode::Home), &UiRegions::default());

        assert_eq!(app.row_offset, 0);
        assert_eq!(app.selected, Some(app.visible[0].key));
    }

    #[test]
    fn pid_reuse_does_not_transfer_selection_to_the_replacement_generation() {
        let mut app = StatsApp::new(snapshot());
        let selected = app.selected.unwrap();
        let pid = selected.pid();
        let mut replacement = Arc::unwrap_or_clone(snapshot());
        replacement.processes.retain(|process| process.identity.pid() != pid);
        replacement.processes.push(process(pid, "replacement", 99.0));
        replacement.processes.last_mut().unwrap().identity =
            ProcessIdentity::stable(ProcessKey { pid, start_token: 999_999 });
        app.ingest(Arc::new(replacement));
        assert_eq!(app.selected, Some(selected));
        assert_eq!(app.detail_kind(), None);
        let backend = ratatui::backend::TestBackend::new(130, 35);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let regions = draw_app(&mut terminal, &app);
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("exited"));
        let profile = regions
            .inline_actions
            .iter()
            .find(|region| region.action == OPEN_PROFILE)
            .expect("Profile remains visible for an exited snapshot")
            .area;
        assert!(regions.inline_actions.iter().any(|region| region.action == TERMINATE));
        assert!(screen.contains("Profile"));
        app.on_event(
            event_mouse(
                MouseEventKind::Down(MouseButton::Left),
                Position { x: profile.x, y: profile.y },
            ),
            &regions,
        );
        assert_eq!(app.inspector_tab, InspectorTab::Profile);
    }

    #[test]
    fn snapshot_only_process_cannot_open_an_action_confirmation() {
        let mut snapshot = Arc::unwrap_or_clone(snapshot());
        let pid = snapshot.processes[0].identity.pid();
        snapshot.processes[0].identity = ProcessIdentity::SnapshotOnly {
            snapshot_sequence: snapshot.sequence,
            pid,
            reason: super::super::model::IdentityUnavailable::PermissionDenied,
        };
        let snapshot_only = snapshot.processes[0].identity;
        let mut app = StatsApp::new(Arc::new(snapshot));
        app.selected = Some(snapshot_only);
        app.request_confirmation(snapshot_only, ProcessAction::GracefulTerminate);
        assert!(confirmation(&app).is_none());
        assert!(app.status.as_deref().unwrap().contains("snapshot-only"));
    }

    #[test]
    fn late_detail_response_cannot_overwrite_the_current_request() {
        let mut app = StatsApp::new(snapshot());
        let process = app.selected.unwrap().stable_key().unwrap();
        let current_request =
            DetailRequest { request_id: 3, kind: DetailRequestKind::Threads { process } };
        app.expected_detail = Some(current_request);
        let late = Arc::new(DetailSnapshot {
            request_id: 1,
            sampled_at_ms: 0,
            collection_duration_ms: 1,
            detail: DetailData::Threads { process, outcome: ready(Vec::new()) },
            warnings: Vec::new(),
        });
        app.ingest_detail(Some(late));
        assert!(app.detail.is_none());

        let other = ProcessKey { pid: process.pid + 1, start_token: process.start_token + 1 };
        let wrong_target = Arc::new(DetailSnapshot {
            request_id: current_request.request_id,
            sampled_at_ms: 0,
            collection_duration_ms: 1,
            detail: DetailData::Threads { process: other, outcome: ready(Vec::new()) },
            warnings: Vec::new(),
        });
        app.ingest_detail(Some(wrong_target));
        assert!(app.detail.is_none());

        let current = Arc::new(DetailSnapshot {
            request_id: current_request.request_id,
            sampled_at_ms: 0,
            collection_duration_ms: 1,
            detail: DetailData::Threads { process, outcome: ready(Vec::new()) },
            warnings: Vec::new(),
        });
        app.ingest_detail(Some(Arc::clone(&current)));
        assert_eq!(app.detail.as_ref().unwrap().request_id, 3);
    }

    #[test]
    fn process_tree_places_a_child_after_its_parent() {
        let mut snapshot = Arc::unwrap_or_clone(snapshot());
        snapshot.processes[1].parent_pid = Some(2);
        let mut app = StatsApp::new(Arc::new(snapshot));
        app.reproject();
        assert_eq!(app.visible[0].pid, 2);
        assert_eq!(app.visible[1].pid, 3);
        assert_eq!(app.visible[1].depth, 1);
    }

    #[test]
    fn process_tree_orders_a_parent_cycle_from_the_active_sort() {
        let mut snapshot = Arc::unwrap_or_clone(snapshot());
        snapshot.processes[0].parent_pid = Some(3);
        snapshot.processes[1].parent_pid = Some(2);
        let mut app = StatsApp::new(Arc::new(snapshot));
        app.reproject();
        assert_eq!(app.visible[0].pid, 2);
        assert_eq!(app.visible[1].pid, 3);
    }

    #[test]
    fn tree_navigation_consumes_arrows_before_moving_to_the_inspector() {
        let mut snapshot = Arc::unwrap_or_clone(snapshot());
        snapshot.processes[1].parent_pid = Some(2);
        let mut app = StatsApp::new(Arc::new(snapshot));
        let selected = app.selected.expect("selected process");
        app.collapsed.insert(selected);

        let backend = ratatui::backend::TestBackend::new(140, 35);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let regions = draw_app(&mut terminal, &app);

        app.on_event(event_key(KeyCode::Right), &regions);
        assert_eq!(app.active_region, ActiveRegion::Processes);
        assert!(!app.collapsed.contains(&selected));

        app.on_event(event_key(KeyCode::Right), &regions);
        assert_eq!(app.active_region, ActiveRegion::Inspector);
        app.on_event(event_key(KeyCode::Left), &regions);
        assert_eq!(app.active_region, ActiveRegion::Processes);
    }

    #[test]
    fn tab_and_mouse_navigation_work_in_wide_and_compact_layouts() {
        let mut app = StatsApp::new(snapshot());
        let backend = ratatui::backend::TestBackend::new(140, 35);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let regions = draw_app(&mut terminal, &app);

        app.on_event(event_key(KeyCode::Tab), &regions);
        assert_eq!(app.active_region, ActiveRegion::Inspector);
        let processes = regions.processes.expect("process region");
        app.on_event(
            event_mouse(
                MouseEventKind::Down(MouseButton::Left),
                Position { x: processes.x, y: processes.y },
            ),
            &regions,
        );
        assert_eq!(app.active_region, ActiveRegion::Processes);

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let compact = draw_app(&mut terminal, &app);
        assert!(compact.processes.is_some());
        assert!(compact.inspector.is_none());
        app.on_event(event_key(KeyCode::Tab), &compact);
        assert_eq!(app.active_region, ActiveRegion::Inspector);
        let compact = draw_app(&mut terminal, &app);
        assert!(compact.processes.is_none());
        assert!(compact.inspector.is_some());
    }

    #[test]
    fn wide_panel_divider_drags_resizes_and_resets() {
        let mut app = StatsApp::new(snapshot());
        let backend = ratatui::backend::TestBackend::new(160, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let regions = draw_app(&mut terminal, &app);
        let original = regions.split.expect("wide split");

        app.on_event(
            event_mouse(
                MouseEventKind::Down(MouseButton::Left),
                Position {
                    x: original.separator.x,
                    y: original.separator.y + original.separator.height / 2,
                },
            ),
            &regions,
        );
        assert!(app.split_drag.is_some());

        let target = original.content.x + original.content.width * 3 / 4;
        app.on_event(
            event_mouse(
                MouseEventKind::Drag(MouseButton::Left),
                Position { x: target, y: original.separator.y },
            ),
            &regions,
        );
        app.on_event(
            event_mouse(
                MouseEventKind::Up(MouseButton::Left),
                Position { x: target, y: original.separator.y },
            ),
            &regions,
        );
        assert!(app.split_drag.is_none());

        let resized = draw_app(&mut terminal, &app).split.expect("resized split");
        assert!(resized.separator.x > original.separator.x);

        app.on_event(event_key(KeyCode::Char('=')), &regions);
        let reset = draw_app(&mut terminal, &app).split.expect("reset split");
        assert_eq!(reset.separator.x, original.separator.x);

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        assert!(draw_app(&mut terminal, &app).split.is_none());
    }

    #[test]
    fn threads_tab_requests_only_the_selected_process_threads() {
        let mut app = StatsApp::new(snapshot());
        let selected = app.selected.unwrap();
        app.inspector_tab = InspectorTab::Threads;
        assert_eq!(
            app.detail_kind(),
            Some(DetailRequestKind::Threads { process: selected.stable_key().unwrap() })
        );
        app.inspector_tab = InspectorTab::Overview;
        assert_eq!(app.detail_kind(), None);
    }

    #[test]
    fn core_focus_aggregates_only_threads_last_seen_on_that_core() {
        let snapshot = Arc::unwrap_or_clone(snapshot());
        let process = snapshot.processes[0].identity.stable_key().unwrap();
        let mut app = StatsApp::new(Arc::new(snapshot));
        app.focused_core = Some(0);
        app.detail = Some(Arc::new(DetailSnapshot {
            request_id: 1,
            sampled_at_ms: 0,
            collection_duration_ms: 1,
            detail: DetailData::Core {
                logical_index: 0,
                outcome: ready(vec![ThreadSample {
                    tid: 20,
                    process,
                    name: Observed::Value("worker".into()),
                    state: Observed::Value(ProcessState::Running),
                    cpu_percent: Observed::Value(12.5),
                    accumulated_cpu_seconds: Observed::Value(1.0),
                    last_cpu: Observed::Value(0),
                }]),
            },
            warnings: Vec::new(),
        }));
        app.reproject();
        assert_eq!(app.visible.len(), 1);
        assert_eq!(app.visible[0].pid, process.pid);
        assert_eq!(app.visible[0].cpu, 12.5);
    }

    #[test]
    fn core_focus_invalidates_process_detail_before_filtering_the_tree() {
        let mut app = StatsApp::new(snapshot());
        let process = app.selected.unwrap().stable_key().unwrap();
        app.inspector_tab = InspectorTab::Threads;
        let DetailIntent::Request(thread_request) = app.reconcile_detail_intent().unwrap() else {
            panic!("threads tab should request process detail")
        };
        app.ingest_detail(Some(Arc::new(DetailSnapshot {
            request_id: thread_request.request_id,
            sampled_at_ms: 0,
            collection_duration_ms: 1,
            detail: DetailData::Threads {
                process,
                outcome: ready(vec![ThreadSample {
                    tid: 20,
                    process,
                    name: Observed::Value("worker".into()),
                    state: Observed::Value(ProcessState::Running),
                    cpu_percent: Observed::Value(12.5),
                    accumulated_cpu_seconds: Observed::Value(1.0),
                    last_cpu: Observed::Value(0),
                }]),
            },
            warnings: Vec::new(),
        })));

        app.focus_core(1);
        let DetailIntent::Request(core_request) = app.reconcile_detail_intent().unwrap() else {
            panic!("core focus should request core detail")
        };
        assert_eq!(core_request.kind, DetailRequestKind::Core { logical_index: 0 });
        assert!(app.detail.is_none());
        assert_eq!(app.visible.len(), app.snapshot.processes.len());
    }

    #[test]
    fn family_rankings_select_exact_processes_without_changing_tabs() {
        let mut source = Arc::unwrap_or_clone(snapshot());
        source.processes[1].parent_pid = Some(source.processes[0].identity.pid());
        let mut app = StatsApp::new(Arc::new(source));
        app.set_inspector_tab(InspectorTab::Family);
        app.active_region = ActiveRegion::Inspector;
        let backend = ratatui::backend::TestBackend::new(130, 35);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let regions = draw_app(&mut terminal, &app);
        let (area, _, expected) = regions.family_rows[0];
        app.on_event(
            event_mouse(MouseEventKind::Down(MouseButton::Left), Position { x: area.x, y: area.y }),
            &regions,
        );
        assert_eq!(app.selected, Some(expected));
        assert_eq!(app.inspector_tab, InspectorTab::Family);
        assert_eq!(app.active_region, ActiveRegion::Inspector);
    }

    #[test]
    fn family_keyboard_navigation_reaches_rows_hidden_by_the_viewport() {
        let mut source = Arc::unwrap_or_clone(snapshot());
        let root = process(1, "root", 1.0);
        source.processes = std::iter::once(root.clone())
            .chain((0..20).map(|index| {
                let mut child = process(100 + index, &format!("child-{index:02}"), index as f32);
                child.parent_pid = Some(root.identity.pid());
                child
            }))
            .collect();
        let mut app = StatsApp::new(Arc::new(source));
        app.selected = Some(root.identity);
        app.set_inspector_tab(InspectorTab::Family);
        app.active_region = ActiveRegion::Inspector;
        let backend = ratatui::backend::TestBackend::new(130, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let regions = draw_app(&mut terminal, &app);
        assert!(app.family_row_count() > regions.family_rows.len());

        for _ in 0..30 {
            app.on_event(event_key(KeyCode::Down), &regions);
        }
        assert!(app.family_cursor >= regions.family_rows.len());
        let expected = app.family_row_key(app.family_cursor).unwrap();
        app.on_event(event_key(KeyCode::Enter), &regions);
        assert_eq!(app.selected, Some(expected));
    }

    #[test]
    fn thread_sorting_preserves_warming_as_distinct_from_zero() {
        let mut app = StatsApp::new(snapshot());
        let process = app.selected.unwrap().stable_key().unwrap();
        app.inspector_tab = InspectorTab::Threads;
        app.active_region = ActiveRegion::Inspector;
        app.detail = Some(Arc::new(DetailSnapshot {
            request_id: 1,
            sampled_at_ms: 0,
            collection_duration_ms: 1,
            detail: DetailData::Threads {
                process,
                outcome: warming(vec![
                    ThreadSample {
                        tid: 40,
                        process,
                        name: Observed::Value("warming".into()),
                        state: Observed::Value(ProcessState::Sleeping),
                        cpu_percent: Observed::Warming,
                        accumulated_cpu_seconds: Observed::Value(2.0),
                        last_cpu: Observed::Value(0),
                    },
                    ThreadSample {
                        tid: 20,
                        process,
                        name: Observed::Value("measured".into()),
                        state: Observed::Value(ProcessState::Running),
                        cpu_percent: Observed::Value(0.0),
                        accumulated_cpu_seconds: Observed::Value(1.0),
                        last_cpu: Observed::Value(0),
                    },
                ]),
            },
            warnings: Vec::new(),
        }));
        assert_eq!(app.sorted_threads()[0].tid, 20);
        app.on_event(event_key(KeyCode::Char('3')), &UiRegions::default());
        assert_eq!(
            app.sorted_threads().iter().map(|thread| thread.tid).collect::<Vec<_>>(),
            vec![20, 40]
        );
    }

    #[test]
    fn full_command_viewer_freezes_observed_text_and_scrolls_without_copying() {
        let mut source = Arc::unwrap_or_clone(snapshot());
        source.processes[0].command = format!("secret-token={}\nsecond line", "x".repeat(180));
        let selected = source.processes[0].identity;
        let original = source.processes[0].command.clone();
        let mut app = StatsApp::new(Arc::new(source));
        app.selected = Some(selected);
        app.inspector_tab = InspectorTab::Overview;
        app.active_region = ActiveRegion::Inspector;
        app.on_event(event_key(KeyCode::Char('v')), &UiRegions::default());

        let mut replacement = Arc::unwrap_or_clone(snapshot());
        replacement.processes[0].command = "replacement command".into();
        app.ingest(Arc::new(replacement));
        assert_eq!(command_viewer(&app).unwrap().command, original);

        let backend = ratatui::backend::TestBackend::new(130, 35);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let regions = draw_app(&mut terminal, &app);
        app.on_event(event_key(KeyCode::Right), &regions);
        assert!(command_viewer(&app).unwrap().column_offset > 0);
        app.on_event(event_key(KeyCode::Esc), &regions);
        assert!(command_viewer(&app).is_none());
    }

    #[test]
    fn only_the_visible_command_control_opens_the_command_viewer() {
        let backend = ratatui::backend::TestBackend::new(130, 35);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = StatsApp::new(snapshot());
        app.inspector_tab = InspectorTab::Overview;
        app.active_region = ActiveRegion::Inspector;
        let regions = draw_app(&mut terminal, &app);
        let command = regions
            .inline_actions
            .iter()
            .find(|region| region.action == VIEW_COMMAND)
            .expect("command control")
            .area;
        assert_eq!(command.height, 1);

        app.on_event(
            event_mouse(
                MouseEventKind::Down(MouseButton::Left),
                Position { x: command.x, y: command.bottom() },
            ),
            &regions,
        );
        assert!(command_viewer(&app).is_none());

        let inspector = regions.inspector.expect("inspector");
        let same_row_outside = if command.right() < inspector.right() {
            Position { x: command.right(), y: command.y }
        } else {
            Position { x: command.x.saturating_sub(1), y: command.y }
        };
        assert!(inspector.contains(same_row_outside));
        assert!(!command.contains(same_row_outside));
        app.on_event(
            event_mouse(MouseEventKind::Down(MouseButton::Left), same_row_outside),
            &regions,
        );
        assert!(app.overlay.is_none(), "ordinary inspector content opened an overlay");

        app.on_event(
            event_mouse(
                MouseEventKind::Down(MouseButton::Left),
                Position { x: command.x, y: command.y },
            ),
            &regions,
        );
        assert!(command_viewer(&app).is_some());
    }

    #[test]
    fn context_menu_targets_right_clicked_identity() {
        let source = snapshot();
        let selected_a = source.processes[0].identity;
        let target_b = source.processes[1].identity;
        let mut app = StatsApp::new(source);
        app.selected = Some(selected_a);
        let backend = ratatui::backend::TestBackend::new(130, 35);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let regions = draw_app(&mut terminal, &app);
        let target_row = regions
            .rows
            .iter()
            .find(|region| region.identity == target_b)
            .expect("target row")
            .area;

        assert!(matches!(
            app.on_event(
                event_mouse(
                    MouseEventKind::Down(MouseButton::Right),
                    Position { x: target_row.x + 1, y: target_row.y },
                ),
                &regions,
            ),
            Action::None
        ));
        assert_eq!(app.selected, Some(target_b));
        assert_eq!(app.active_region, ActiveRegion::Processes);
        assert_eq!(context_menu_target(&app), Some(target_b));

        let menu_regions = draw_app(&mut terminal, &app);
        assert!(menu_regions.context_menu.is_some());
        assert!(matches!(app.on_event(event_key(KeyCode::Enter), &menu_regions), Action::None));
        assert_eq!(command_viewer(&app).unwrap().pid, target_b.pid());
    }

    #[test]
    fn inspector_context_menu_targets_selected_identity() {
        let source = snapshot();
        let target = source.processes[1].identity;
        let mut app = StatsApp::new(source);
        app.selected = Some(target);
        app.active_region = ActiveRegion::Inspector;
        let backend = ratatui::backend::TestBackend::new(130, 35);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let regions = draw_app(&mut terminal, &app);
        let inspector = regions.inspector.expect("inspector");

        app.on_event(
            event_mouse(
                MouseEventKind::Down(MouseButton::Right),
                Position { x: inspector.x + 2, y: inspector.y + 3 },
            ),
            &regions,
        );

        assert_eq!(app.active_region, ActiveRegion::Inspector);
        assert_eq!(context_menu_target(&app), Some(target));
        assert!(command_viewer(&app).is_none());
    }

    #[test]
    fn headless_process_menu_acceptance_covers_render_input_and_resize() {
        let mut source = Arc::unwrap_or_clone(snapshot());
        source.host.graceful_terminate = CapabilityState::Available;
        source.host.force_terminate =
            CapabilityState::Unsupported { reason: "fixture force unavailable" };
        let selected_a = source.processes[0].identity;
        let target_b = source.processes[1].identity;
        let source = Arc::new(source);
        let mut app = StatsApp::new(Arc::clone(&source));
        app.selected = Some(selected_a);

        let backend = ratatui::backend::TestBackend::new(130, 35);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let wide_regions = draw_app(&mut terminal, &app);
        let target_row = wide_regions
            .rows
            .iter()
            .find(|region| region.identity == target_b)
            .expect("target B row")
            .area;

        assert!(matches!(
            app.on_event(
                event_mouse(
                    MouseEventKind::Down(MouseButton::Right),
                    Position { x: target_row.x + 1, y: target_row.y },
                ),
                &wide_regions,
            ),
            Action::None
        ));
        assert_eq!(app.selected, Some(target_b));
        assert_eq!(context_menu_target(&app), Some(target_b));
        app.selected = Some(selected_a);
        assert_eq!(context_menu_target(&app), Some(target_b), "menu followed later selection");

        let wide_menu_regions = draw_app(&mut terminal, &app);
        let wide_layout = wide_menu_regions.context_menu.as_ref().expect("wide menu layout");
        assert_eq!(
            wide_layout.items().iter().map(|item| item.action()).collect::<Vec<_>>(),
            [VIEW_COMMAND, OPEN_PROFILE, TERMINATE, FORCE_TERMINATE]
        );
        assert_eq!(wide_layout.separators().len(), 1);
        assert!(
            wide_layout.items()[1].area().y < wide_layout.separators()[0].y
                && wide_layout.separators()[0].y < wide_layout.items()[2].area().y
        );

        let menu_rows = wide_layout
            .items()
            .iter()
            .map(|item| {
                let area = item.area();
                (area.x..area.right())
                    .map(|x| terminal.backend().buffer()[(x, area.y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        for ((title, hint), row) in [
            ("View full command", 'v'),
            ("Profile", 'p'),
            ("End process…", 'x'),
            ("Force end process…", 'X'),
        ]
        .into_iter()
        .zip(&menu_rows)
        {
            assert!(row.contains(title), "menu row did not render {title:?}: {row:?}");
            assert_eq!(row.trim_end().chars().last(), Some(hint), "wrong hint for {title:?}");
        }
        assert!(menu_rows[3].contains("fixture force unavailable"));

        let selected_menu_index = |app: &StatsApp| match app.overlay.as_ref() {
            Some(StatsOverlay::ContextMenu(menu)) => menu.selected(),
            _ => panic!("expected context menu"),
        };
        assert_eq!(selected_menu_index(&app), 0);
        app.on_event(event_key(KeyCode::Down), &wide_menu_regions);
        assert_eq!(selected_menu_index(&app), 1);
        app.on_event(event_key(KeyCode::Char('j')), &wide_menu_regions);
        assert_eq!(selected_menu_index(&app), 2);
        app.on_event(event_key(KeyCode::Up), &wide_menu_regions);
        assert_eq!(selected_menu_index(&app), 1);
        app.on_event(event_key(KeyCode::Char('k')), &wide_menu_regions);
        assert_eq!(selected_menu_index(&app), 0);

        let view_item = wide_layout.items()[0].area();
        assert!(matches!(
            app.on_event(
                event_mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    Position { x: view_item.x, y: view_item.y },
                ),
                &wide_menu_regions,
            ),
            Action::None
        ));
        assert_eq!(command_viewer(&app).expect("menu click opened viewer").pid, target_b.pid());

        let viewer_regions = draw_app(&mut terminal, &app);
        app.on_event(event_key(KeyCode::Esc), &viewer_regions);
        assert!(app.overlay.is_none());
        let wide_regions = draw_app(&mut terminal, &app);
        let target_row = wide_regions
            .rows
            .iter()
            .find(|region| region.identity == target_b)
            .expect("target B row after viewer")
            .area;
        app.on_event(
            event_mouse(
                MouseEventKind::Down(MouseButton::Right),
                Position { x: target_row.x + 1, y: target_row.y },
            ),
            &wide_regions,
        );
        assert_eq!(context_menu_target(&app), Some(target_b));

        terminal.backend_mut().resize(80, 24);
        terminal.autoresize().unwrap();
        let compact_regions = draw_app(&mut terminal, &app);
        let compact_layout = compact_regions.context_menu.as_ref().expect("compact menu layout");
        assert!(compact_layout.area().right() <= 80);
        assert!(compact_layout.area().bottom() <= 24);
        assert!(compact_layout.items().iter().all(|item| {
            let area = item.area();
            area.right() <= 80
                && area.bottom() <= 24
                && compact_layout.area().contains(Position { x: area.x, y: area.y })
        }));
        assert_eq!(context_menu_target(&app), Some(target_b));

        app.on_event(event_key(KeyCode::Esc), &compact_regions);
        app.active_region = ActiveRegion::Inspector;
        app.inspector_tab = InspectorTab::Overview;
        let compact_inspector_regions = draw_app(&mut terminal, &app);
        let inspector = compact_inspector_regions.inspector.expect("compact inspector");
        let command_region = compact_inspector_regions
            .inline_actions
            .iter()
            .find(|region| region.action == VIEW_COMMAND)
            .expect("compact command control");
        assert_eq!(command_region.identity, target_b);
        let ordinary_command_text =
            Position { x: command_region.area.x, y: command_region.area.bottom() };
        assert!(inspector.contains(ordinary_command_text));
        assert!(!command_region.area.contains(ordinary_command_text));
        app.on_event(
            event_mouse(MouseEventKind::Down(MouseButton::Left), ordinary_command_text),
            &compact_inspector_regions,
        );
        assert!(app.overlay.is_none(), "ordinary command text opened the command viewer");

        app.on_event(
            event_mouse(
                MouseEventKind::Down(MouseButton::Left),
                Position { x: command_region.area.x, y: command_region.area.y },
            ),
            &compact_inspector_regions,
        );
        assert_eq!(
            command_viewer(&app).expect("visible command control opened the viewer").pid,
            target_b.pid()
        );
        let compact_viewer_regions = draw_app(&mut terminal, &app);
        app.on_event(event_key(KeyCode::Esc), &compact_viewer_regions);
        assert!(app.overlay.is_none());

        let compact_inspector_regions = draw_app(&mut terminal, &app);
        let inspector = compact_inspector_regions.inspector.expect("compact inspector");
        app.on_event(
            event_mouse(
                MouseEventKind::Down(MouseButton::Right),
                Position { x: inspector.x + 1, y: inspector.y + 1 },
            ),
            &compact_inspector_regions,
        );
        assert_eq!(context_menu_target(&app), Some(target_b));

        let compact_menu_regions = draw_app(&mut terminal, &app);
        let compact_menu =
            compact_menu_regions.context_menu.as_ref().expect("compact inspector menu");
        let command_area = compact_menu_regions
            .inline_actions
            .iter()
            .find(|region| region.action == VIEW_COMMAND)
            .expect("underlying command action")
            .area;
        let outside_command_cell = (command_area.y..command_area.bottom())
            .flat_map(|y| (command_area.x..command_area.right()).map(move |x| Position { x, y }))
            .find(|position| !compact_menu.area().contains(*position))
            .expect("command action has a cell outside the menu");
        app.on_event(
            event_mouse(MouseEventKind::Down(MouseButton::Left), outside_command_cell),
            &compact_menu_regions,
        );
        assert!(app.overlay.is_none(), "outside dismissal invoked the inline action");
        assert_eq!(app.inspector_tab, InspectorTab::Overview);

        let mut actionable = Arc::unwrap_or_clone(source);
        actionable.host.graceful_terminate = CapabilityState::Available;
        actionable.host.force_terminate = CapabilityState::Available;
        let actionable = Arc::new(actionable);
        let target_key = target_b.stable_key().expect("stable target B");

        let mut viewer_app = StatsApp::new(Arc::clone(&actionable));
        viewer_app.selected = Some(target_b);
        assert!(matches!(
            viewer_app.on_event(event_key(KeyCode::Char('v')), &UiRegions::default()),
            Action::None
        ));
        assert_eq!(command_viewer(&viewer_app).expect("v viewer").pid, target_b.pid());

        let mut profile_app = StatsApp::new(Arc::clone(&actionable));
        profile_app.selected = Some(target_b);
        assert!(matches!(
            profile_app.on_event(event_key(KeyCode::Char('p')), &UiRegions::default()),
            Action::None
        ));
        assert_eq!(profile_app.selected, Some(target_b));
        assert_eq!(profile_app.active_region, ActiveRegion::Inspector);
        assert_eq!(profile_app.inspector_tab, InspectorTab::Profile);

        for (key, action) in [
            (KeyCode::Char('x'), ProcessAction::GracefulTerminate),
            (KeyCode::Delete, ProcessAction::GracefulTerminate),
            (KeyCode::Char('X'), ProcessAction::ForceTerminate),
        ] {
            let mut confirmation_app = StatsApp::new(Arc::clone(&actionable));
            confirmation_app.selected = Some(target_b);
            assert!(matches!(
                confirmation_app.on_event(event_key(key), &UiRegions::default()),
                Action::None
            ));
            let confirmation = confirmation(&confirmation_app).expect("termination confirmation");
            assert_eq!(confirmation.key, target_key);
            assert_eq!(confirmation.requested, action);
        }
    }

    #[test]
    fn process_action_projections_share_catalog() {
        let mut app = StatsApp::new(snapshot());
        app.inspector_tab = InspectorTab::Overview;
        app.active_region = ActiveRegion::Inspector;
        let identity = app.selected.unwrap();
        let context = app.action_context(identity);
        let backend = ratatui::backend::TestBackend::new(130, 35);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let regions = draw_app(&mut terminal, &app);

        let mut rendered =
            regions.inline_actions.iter().map(|region| region.action).collect::<Vec<_>>();
        assert!(regions.inline_actions.iter().all(|region| region.identity == identity));
        rendered.sort_unstable();
        let mut expected_inline = vec![VIEW_COMMAND, OPEN_PROFILE, TERMINATE];
        expected_inline.sort_unstable();
        assert_eq!(rendered, expected_inline);
        assert_eq!(
            app.registry
                .resolve_menu(PROCESS_COMMAND_INLINE, &context)
                .items()
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            [VIEW_COMMAND]
        );
        assert_eq!(
            app.registry
                .resolve_menu(PROCESS_INSPECTOR_INLINE, &context)
                .items()
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            [OPEN_PROFILE, TERMINATE]
        );
        assert_eq!(
            app.registry
                .resolve_menu(PROCESS_CONTEXT_MENU, &context)
                .items()
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            [VIEW_COMMAND, OPEN_PROFILE, TERMINATE, FORCE_TERMINATE]
        );

        for (code, action, command) in [
            (KeyCode::Char('v'), VIEW_COMMAND, StatsCommand::ViewCommand),
            (KeyCode::Char('p'), OPEN_PROFILE, StatsCommand::OpenProfile),
            (
                KeyCode::Char('x'),
                TERMINATE,
                StatsCommand::RequestTerminate(ProcessAction::GracefulTerminate),
            ),
            (
                KeyCode::Delete,
                TERMINATE,
                StatsCommand::RequestTerminate(ProcessAction::GracefulTerminate),
            ),
            (
                KeyCode::Char('X'),
                FORCE_TERMINATE,
                StatsCommand::RequestTerminate(ProcessAction::ForceTerminate),
            ),
        ] {
            let invocation = app
                .registry
                .resolve_keybinding(KeyChord::new(code, KeyModifiers::NONE), context)
                .expect("registered keybinding");
            assert_eq!(invocation.action, action);
            assert_eq!(app.registry.command_for(&invocation), Ok(command));
        }
    }

    #[test]
    fn inline_rendering_uses_injected_contribution_metadata() {
        fn always(_: &StatsActionContext) -> bool {
            true
        }

        fn overview(context: &StatsActionContext) -> bool {
            context.inspector_tab == InspectorTab::Overview
        }

        fn enabled(_: &StatsActionContext) -> ActionState {
            ActionState::Enabled
        }

        fn sentinel_registry() -> StatsActionRegistry {
            let mut builder = ActionRegistryBuilder::new();
            builder
                .register_action(ActionSpec {
                    id: VIEW_COMMAND,
                    title: "SNTL-CMD",
                    command: StatsCommand::ViewCommand,
                    enablement: enabled,
                })
                .register_action(ActionSpec {
                    id: OPEN_PROFILE,
                    title: "SNTL-PROFILE",
                    command: StatsCommand::OpenProfile,
                    enablement: enabled,
                })
                .register_action(ActionSpec {
                    id: TERMINATE,
                    title: "SNTL-END",
                    command: StatsCommand::RequestTerminate(ProcessAction::GracefulTerminate),
                    enablement: enabled,
                })
                .register_action(ActionSpec {
                    id: FORCE_TERMINATE,
                    title: "SNTL-FORCE",
                    command: StatsCommand::RequestTerminate(ProcessAction::ForceTerminate),
                    enablement: enabled,
                });

            for (action, group, group_order, order) in [
                (VIEW_COMMAND, "navigation", 10, 10),
                (OPEN_PROFILE, "navigation", 10, 20),
                (TERMINATE, "destructive", 20, 10),
                (FORCE_TERMINATE, "destructive", 20, 20),
            ] {
                builder.place_menu(MenuPlacement {
                    menu: PROCESS_CONTEXT_MENU,
                    action,
                    group,
                    group_order,
                    order,
                    when: always,
                });
            }

            builder
                .place_menu(MenuPlacement {
                    menu: PROCESS_COMMAND_INLINE,
                    action: VIEW_COMMAND,
                    group: "navigation",
                    group_order: 10,
                    order: 10,
                    when: overview,
                })
                .place_menu(MenuPlacement {
                    menu: PROCESS_COMMAND_INLINE,
                    action: FORCE_TERMINATE,
                    group: "navigation",
                    group_order: 10,
                    order: 20,
                    when: overview,
                })
                .place_menu(MenuPlacement {
                    menu: PROCESS_INSPECTOR_INLINE,
                    action: OPEN_PROFILE,
                    group: "navigation",
                    group_order: 10,
                    order: 10,
                    when: always,
                })
                .place_menu(MenuPlacement {
                    menu: PROCESS_INSPECTOR_INLINE,
                    action: TERMINATE,
                    group: "destructive",
                    group_order: 20,
                    order: 10,
                    when: always,
                });

            builder.bind_key(KeybindingPlacement {
                chord: KeyChord::new(KeyCode::F(7), KeyModifiers::CONTROL | KeyModifiers::ALT),
                action: VIEW_COMMAND,
                when: overview,
            });
            for (action, key) in [
                (OPEN_PROFILE, KeyCode::F(8)),
                (TERMINATE, KeyCode::F(9)),
                (FORCE_TERMINATE, KeyCode::F(10)),
            ] {
                builder.bind_key(KeybindingPlacement {
                    chord: KeyChord::new(key, KeyModifiers::CONTROL | KeyModifiers::ALT),
                    action,
                    when: always,
                });
            }
            builder.build().expect("sentinel contribution graph must be valid")
        }

        let mut app = StatsApp::new_validated(snapshot(), sentinel_registry(), true);
        app.inspector_tab = InspectorTab::Overview;
        app.active_region = ActiveRegion::Inspector;
        let backend = ratatui::backend::TestBackend::new(160, 50);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let regions = draw_app(&mut terminal, &app);
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        for metadata in [
            "COMMAND",
            "/bin/cool",
            "SNTL-CMD",
            "Ctrl+Alt+F7",
            "SNTL-FORCE",
            "Ctrl+Alt+F10",
            "SNTL-PROFILE",
            "Ctrl+Alt+F8",
            "SNTL-END",
            "Ctrl+Alt+F9",
        ] {
            assert!(screen.contains(metadata), "renderer ignored sentinel metadata {metadata:?}");
        }
        for production_title in ["View full command", "Profile", "End process…"] {
            assert!(
                !screen.contains(production_title),
                "renderer leaked hard-coded production title {production_title:?}"
            );
        }

        let mut rendered_ids =
            regions.inline_actions.iter().map(|region| region.action).collect::<Vec<_>>();
        assert!(regions
            .inline_actions
            .iter()
            .all(|region| region.identity == app.selected.unwrap()));
        rendered_ids.sort_unstable();
        let mut expected_ids = vec![VIEW_COMMAND, FORCE_TERMINATE, OPEN_PROFILE, TERMINATE];
        expected_ids.sort_unstable();
        assert_eq!(rendered_ids, expected_ids);

        let command_regions = [VIEW_COMMAND, FORCE_TERMINATE]
            .into_iter()
            .map(|action| {
                regions
                    .inline_actions
                    .iter()
                    .find(|region| region.action == action)
                    .expect("command-inline action region")
            })
            .collect::<Vec<_>>();
        assert_eq!(command_regions[0].area.right(), command_regions[1].area.x);
        assert_eq!(usize::from(command_regions[0].area.width), " Ctrl+Alt+F7 SNTL-CMD".len());
        assert_eq!(usize::from(command_regions[1].area.width), " Ctrl+Alt+F10 SNTL-FORCE".len());

        let mut tiny_app = StatsApp::new_validated(snapshot(), sentinel_registry(), true);
        tiny_app.inspector_tab = InspectorTab::Overview;
        tiny_app.active_region = ActiveRegion::Inspector;
        let mut tiny_terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(35, 20)).unwrap();
        let tiny_regions = draw_app(&mut tiny_terminal, &tiny_app);
        let tiny_inspector = tiny_regions.inspector.expect("tiny inspector");
        let mut tiny_command_regions = tiny_regions
            .inline_actions
            .iter()
            .filter(|region| matches!(region.action, VIEW_COMMAND | FORCE_TERMINATE))
            .map(|region| region.area)
            .collect::<Vec<_>>();
        tiny_command_regions.sort_unstable_by_key(|area| area.x);
        assert_eq!(tiny_command_regions.len(), 2);
        assert!(tiny_command_regions.iter().all(|area| area.width > 0));
        assert!(tiny_command_regions.windows(2).all(|pair| pair[0].right() <= pair[1].x));
        assert!(tiny_command_regions.iter().all(|area| area.right() <= tiny_inspector.right()));
    }

    #[test]
    fn ctrl_c_quits_from_every_overlay() {
        let control_c = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        let identity = snapshot().processes[0].identity;

        let mut menu_app = StatsApp::new(snapshot());
        open_context_menu(&mut menu_app, identity, Position { x: 10, y: 8 });
        assert!(matches!(
            menu_app.on_event(control_c.clone(), &UiRegions::default()),
            Action::Quit
        ));

        let mut confirmation_app = StatsApp::new(snapshot());
        confirmation_app.request_confirmation(identity, ProcessAction::GracefulTerminate);
        assert!(matches!(
            confirmation_app.on_event(control_c.clone(), &UiRegions::default()),
            Action::Quit
        ));

        let mut viewer_app = StatsApp::new(snapshot());
        let context = viewer_app.action_context(identity);
        viewer_app.invoke_action(ActionInvocation::new(VIEW_COMMAND, context));
        assert!(matches!(viewer_app.on_event(control_c, &UiRegions::default()), Action::Quit));
    }

    #[test]
    fn context_menu_waits_for_its_published_layout_before_mouse_input() {
        let source = snapshot();
        let target = source.processes[0].identity;
        let mut app = StatsApp::new(source);
        app.selected = Some(target);
        open_context_menu(&mut app, target, Position { x: 90, y: 20 });

        assert!(matches!(
            app.on_event(
                event_mouse(MouseEventKind::Down(MouseButton::Left), Position { x: 0, y: 0 },),
                &UiRegions::default(),
            ),
            Action::None
        ));
        assert_eq!(context_menu_target(&app), Some(target));
        assert_eq!(app.selected, Some(target));
    }

    #[test]
    fn context_menu_dismissal_is_consumed_without_base_fallthrough() {
        let source = snapshot();
        let selected = source.processes[0].identity;
        let other = source.processes[1].identity;
        let mut app = StatsApp::new(source);
        app.selected = Some(selected);
        open_context_menu(&mut app, selected, Position { x: 90, y: 20 });
        let backend = ratatui::backend::TestBackend::new(130, 35);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let regions = draw_app(&mut terminal, &app);
        let layout = regions.context_menu.as_ref().expect("menu layout");
        let other_row = regions.rows.iter().find(|region| region.identity == other).unwrap().area;
        let outside = Position { x: other_row.x + 1, y: other_row.y };
        assert!(!layout.area().contains(outside));

        assert!(matches!(
            app.on_event(event_mouse(MouseEventKind::Down(MouseButton::Left), outside), &regions,),
            Action::None
        ));
        assert!(app.overlay.is_none());
        assert_eq!(app.selected, Some(selected), "dismissal click fell through to row selection");

        open_context_menu(&mut app, selected, Position { x: 90, y: 20 });
        assert!(matches!(app.on_event(event_key(KeyCode::Char('q')), &regions), Action::None));
        assert!(app.overlay.is_none(), "menu q should dismiss rather than quit Stats");
    }

    #[test]
    fn disabled_context_action_stays_open_and_reports_reason() {
        let mut source = Arc::unwrap_or_clone(snapshot());
        source.host.force_terminate =
            CapabilityState::Unsupported { reason: "fixture force unavailable" };
        let identity = source.processes[0].identity;
        let mut app = StatsApp::new(Arc::new(source));
        open_context_menu(&mut app, identity, Position { x: 20, y: 10 });
        let backend = ratatui::backend::TestBackend::new(130, 35);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let regions = draw_app(&mut terminal, &app);

        assert!(matches!(app.on_event(event_key(KeyCode::End), &regions), Action::None));
        assert!(matches!(app.on_event(event_key(KeyCode::Enter), &regions), Action::None));
        assert_eq!(context_menu_target(&app), Some(identity));
        assert!(app.status.as_deref().unwrap().contains("fixture force unavailable"));
    }

    #[test]
    fn no_mouse_keeps_hints_and_keyboard_without_pointer_menu() {
        let mut app = StatsApp::new_validated(
            snapshot(),
            contributions::registry().expect("Stats contributions"),
            false,
        );
        app.active_region = ActiveRegion::Inspector;
        app.inspector_tab = InspectorTab::Overview;
        let backend = ratatui::backend::TestBackend::new(130, 35);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let regions = draw_app(&mut terminal, &app);
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        for metadata in ["View full command", "Profile", "End process…"] {
            assert!(screen.contains(metadata), "missing no-mouse action metadata {metadata}");
        }
        assert!(regions.inline_actions.is_empty());
        assert!(regions.context_menu.is_none());

        let mut pointer_terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(130, 35)).unwrap();
        let mut pointer_app = StatsApp::new(snapshot());
        pointer_app.active_region = ActiveRegion::Inspector;
        let pointer_regions = draw_app(&mut pointer_terminal, &pointer_app);
        let command = pointer_regions
            .inline_actions
            .iter()
            .find(|region| region.action == VIEW_COMMAND)
            .expect("pointer-enabled command action")
            .area;
        app.on_event(
            event_mouse(
                MouseEventKind::Down(MouseButton::Left),
                Position { x: command.x, y: command.y },
            ),
            &regions,
        );
        assert!(app.overlay.is_none(), "pointer-disabled inline action remained clickable");

        let row = regions.rows[0].area;
        app.on_event(
            event_mouse(
                MouseEventKind::Down(MouseButton::Right),
                Position { x: row.x + 1, y: row.y },
            ),
            &regions,
        );
        assert!(app.overlay.is_none());

        assert!(matches!(app.on_event(event_key(KeyCode::Char('v')), &regions), Action::None));
        assert!(command_viewer(&app).is_some(), "registered keyboard action stopped working");
    }

    #[test]
    fn drawn_core_and_confirmation_buttons_are_clickable() {
        let backend = ratatui::backend::TestBackend::new(130, 35);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = StatsApp::new(snapshot_with_cores(16));
        let mut regions = draw_app(&mut terminal, &app);
        assert_eq!(regions.cores.len(), 16);
        for (index, (core, logical_index)) in regions.cores.iter().copied().enumerate() {
            assert_eq!(core.width, 1);
            if index > 0 {
                assert_eq!(core.x, regions.cores[index - 1].0.right());
            }
            app.on_event(
                event_mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    Position { x: core.x + core.width / 2, y: core.y },
                ),
                &regions,
            );
            assert_eq!(app.focused_core, Some(logical_index));
        }

        app.focused_core = None;
        app.reproject();
        app.request_confirmation(app.selected.unwrap(), ProcessAction::GracefulTerminate);
        assert_eq!(confirmation(&app).unwrap().choice, ConfirmationChoice::Cancel);
        assert!(matches!(app.on_event(event_key(KeyCode::Enter), &regions), Action::None));
        assert!(confirmation(&app).is_none());

        app.request_confirmation(app.selected.unwrap(), ProcessAction::GracefulTerminate);
        regions = draw_app(&mut terminal, &app);
        let force = regions
            .confirmation_choices
            .iter()
            .find(|(_, choice)| {
                *choice == ConfirmationChoice::Action(ProcessAction::ForceTerminate)
            })
            .expect("force confirmation choice")
            .0;
        assert!(matches!(
            app.on_event(
                event_mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    Position { x: force.x, y: force.y },
                ),
                &regions
            ),
            Action::Process(_, ProcessAction::ForceTerminate)
        ));
    }

    #[test]
    fn confirmation_choices_match_requested_effect_and_host_capabilities() {
        let mut both = Arc::unwrap_or_clone(snapshot());
        both.host.graceful_terminate = CapabilityState::Available;
        both.host.force_terminate = CapabilityState::Available;
        let both = Arc::new(both);

        let mut graceful_app = StatsApp::new(Arc::clone(&both));
        graceful_app
            .request_confirmation(graceful_app.selected.unwrap(), ProcessAction::GracefulTerminate);
        assert_eq!(
            confirmation(&graceful_app).unwrap().choices,
            [
                ConfirmationChoice::Action(ProcessAction::GracefulTerminate),
                ConfirmationChoice::Action(ProcessAction::ForceTerminate),
                ConfirmationChoice::Cancel,
            ]
        );
        assert_eq!(
            confirmation(&graceful_app)
                .unwrap()
                .choices
                .iter()
                .map(|choice| choice.label())
                .collect::<Vec<_>>(),
            ["End process", "Force terminate", "Cancel"]
        );

        let mut force_app = StatsApp::new(Arc::clone(&both));
        force_app.request_confirmation(force_app.selected.unwrap(), ProcessAction::ForceTerminate);
        assert_eq!(
            confirmation(&force_app).unwrap().choices,
            [ConfirmationChoice::Action(ProcessAction::ForceTerminate), ConfirmationChoice::Cancel,],
            "a force request must not expose a second button with the same force effect"
        );
        let backend = ratatui::backend::TestBackend::new(130, 35);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let regions = draw_app(&mut terminal, &force_app);
        let force_process = force_app.selected_process().expect("selected force target");
        let force_prompt = format!(
            "Force terminate {} (PID {})?",
            force_process.name,
            force_process.identity.pid()
        );
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains(&force_prompt));
        assert_eq!(
            regions.confirmation_choices.iter().map(|(_, choice)| *choice).collect::<Vec<_>>(),
            [ConfirmationChoice::Action(ProcessAction::ForceTerminate), ConfirmationChoice::Cancel]
        );
        let force_button = regions
            .confirmation_choices
            .iter()
            .find(|(_, choice)| {
                *choice == ConfirmationChoice::Action(ProcessAction::ForceTerminate)
            })
            .expect("force button")
            .0;
        let force_button_text = (force_button.x..force_button.right())
            .map(|x| terminal.backend().buffer()[(x, force_button.y)].symbol())
            .collect::<String>();
        assert_eq!(force_button_text.trim(), "Force terminate");
        assert!(matches!(force_app.on_event(event_key(KeyCode::Left), &regions), Action::None));
        assert!(matches!(
            force_app.on_event(event_key(KeyCode::Enter), &regions),
            Action::Process(_, ProcessAction::ForceTerminate)
        ));

        let mut graceful_only = Arc::unwrap_or_clone(snapshot());
        graceful_only.host.graceful_terminate = CapabilityState::Available;
        graceful_only.host.force_terminate = CapabilityState::Unsupported { reason: "test" };
        let mut graceful_only_app = StatsApp::new(Arc::new(graceful_only));
        graceful_only_app.request_confirmation(
            graceful_only_app.selected.unwrap(),
            ProcessAction::GracefulTerminate,
        );
        assert_eq!(
            confirmation(&graceful_only_app).unwrap().choices,
            [
                ConfirmationChoice::Action(ProcessAction::GracefulTerminate),
                ConfirmationChoice::Cancel,
            ]
        );

        let mut force_only = Arc::unwrap_or_clone(snapshot());
        force_only.host.graceful_terminate = CapabilityState::Unsupported { reason: "test" };
        force_only.host.force_terminate = CapabilityState::Available;
        let mut force_only_app = StatsApp::new(Arc::new(force_only));
        force_only_app
            .request_confirmation(force_only_app.selected.unwrap(), ProcessAction::ForceTerminate);
        assert_eq!(
            confirmation(&force_only_app).unwrap().choices,
            [ConfirmationChoice::Action(ProcessAction::ForceTerminate), ConfirmationChoice::Cancel,]
        );

        let mut revalidation_app = StatsApp::new(Arc::clone(&both));
        revalidation_app.request_confirmation(
            revalidation_app.selected.unwrap(),
            ProcessAction::ForceTerminate,
        );
        Arc::make_mut(&mut revalidation_app.snapshot).host.force_terminate =
            CapabilityState::Unsupported { reason: "capability changed" };
        assert!(matches!(
            revalidation_app.on_event(event_key(KeyCode::Left), &UiRegions::default()),
            Action::None
        ));
        assert!(matches!(
            revalidation_app.on_event(event_key(KeyCode::Enter), &UiRegions::default()),
            Action::None
        ));
        assert!(revalidation_app
            .status
            .as_deref()
            .is_some_and(|status| status.contains("capability changed")));
    }

    #[test]
    fn overlay_gateway_captures_base_keys_and_mouse_before_stats_navigation() {
        let mut app = StatsApp::new(snapshot());
        app.request_confirmation(app.selected.unwrap(), ProcessAction::GracefulTerminate);
        let backend = ratatui::backend::TestBackend::new(130, 35);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let regions = draw_app(&mut terminal, &app);
        let original_sort = app.sort;

        assert!(matches!(app.on_event(event_key(KeyCode::Char('q')), &regions), Action::None));
        assert!(confirmation(&app).is_some(), "base quit key escaped the modal gateway");

        let memory = regions
            .headers
            .iter()
            .find(|(_, sort)| *sort == SortBy::Memory)
            .expect("memory header behind confirmation")
            .0;
        assert!(matches!(
            app.on_event(
                event_mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    Position { x: memory.x, y: memory.y },
                ),
                &regions,
            ),
            Action::None
        ));
        assert_eq!(app.sort, original_sort, "mouse input bypassed the modal gateway");
        assert!(confirmation(&app).is_some());

        assert!(matches!(
            app.on_event(
                Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
                &regions,
            ),
            Action::Quit
        ));
    }

    #[test]
    fn captured_target_is_revalidated_before_process_effect() {
        let source = snapshot();
        let selected_a = source.processes[0].identity;
        let target_b = source.processes[1].identity;
        let mut app = StatsApp::new(source);
        app.selected = Some(selected_a);

        let view_context = app.action_context(target_b);
        assert!(matches!(
            app.invoke_action(ActionInvocation::new(VIEW_COMMAND, view_context)),
            Action::None
        ));
        assert_eq!(command_viewer(&app).unwrap().pid, target_b.pid());
        assert_eq!(
            app.selected,
            Some(selected_a),
            "viewing B must not silently retarget selection"
        );

        app.overlay = None;
        let profile_context = app.action_context(target_b);
        assert!(matches!(
            app.invoke_action(ActionInvocation::new(OPEN_PROFILE, profile_context)),
            Action::None
        ));
        assert_eq!(app.selected, Some(target_b));
        assert_eq!(app.inspector_tab, InspectorTab::Profile);
        assert_eq!(app.active_region, ActiveRegion::Inspector);

        app.selected = Some(selected_a);
        app.set_inspector_tab(InspectorTab::Overview);
        let terminate_context = app.action_context(target_b);
        assert!(matches!(
            app.invoke_action(ActionInvocation::new(TERMINATE, terminate_context)),
            Action::None
        ));
        assert_eq!(confirmation(&app).unwrap().key, target_b.stable_key().unwrap());
        app.selected = Some(selected_a);
        assert!(matches!(
            app.on_confirmation_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
            Action::Process(key, ProcessAction::GracefulTerminate)
                if key == target_b.stable_key().unwrap()
        ));

        let stale_context = app.action_context(target_b);
        app.ingest(reused_target_snapshot(target_b));
        assert!(matches!(
            app.invoke_action(ActionInvocation::new(FORCE_TERMINATE, stale_context)),
            Action::None
        ));
        assert!(confirmation(&app).is_none());
        assert!(app.status.as_deref().unwrap().contains("no longer the same process"));
    }

    #[test]
    fn target_loss_has_overlay_specific_behavior() {
        let target = snapshot().processes[0].identity;
        let replacement = reused_target_snapshot(target);

        let mut menu_app = StatsApp::new(snapshot());
        let context = menu_app.action_context(target);
        let resolved = menu_app.registry.resolve_menu(PROCESS_CONTEXT_MENU, &context);
        menu_app.overlay = ContextMenu::open(Position { x: 4, y: 3 }, context, resolved)
            .map(StatsOverlay::ContextMenu);
        assert!(matches!(menu_app.overlay.as_ref(), Some(StatsOverlay::ContextMenu(_))));
        menu_app.ingest(Arc::clone(&replacement));
        assert!(menu_app.overlay.is_none());
        assert!(menu_app.status.as_deref().unwrap().contains("no longer the same process"));

        let mut confirmation_app = StatsApp::new(snapshot());
        confirmation_app.request_confirmation(target, ProcessAction::GracefulTerminate);
        assert!(confirmation(&confirmation_app).is_some());
        confirmation_app.ingest(Arc::clone(&replacement));
        assert!(confirmation(&confirmation_app).is_none());
        assert!(confirmation_app.status.as_deref().unwrap().contains("no longer the same process"));

        let mut viewer_app = StatsApp::new(snapshot());
        let original = viewer_app.process(target).unwrap().command.clone();
        let context = viewer_app.action_context(target);
        viewer_app.invoke_action(ActionInvocation::new(VIEW_COMMAND, context));
        viewer_app.ingest(replacement);
        assert_eq!(command_viewer(&viewer_app).unwrap().command, original);
    }

    #[test]
    fn stats_overlay_transitions_are_exclusive() {
        let target = snapshot().processes[0].identity;
        let mut app = StatsApp::new(snapshot());
        let context = app.action_context(target);
        let resolved = app.registry.resolve_menu(PROCESS_CONTEXT_MENU, &context);
        app.overlay = ContextMenu::open(Position { x: 1, y: 1 }, context, resolved)
            .map(StatsOverlay::ContextMenu);

        app.invoke_action(ActionInvocation::new(VIEW_COMMAND, context));
        assert!(matches!(app.overlay.as_ref(), Some(StatsOverlay::CommandViewer(_))));

        let context = app.action_context(target);
        app.invoke_action(ActionInvocation::new(TERMINATE, context));
        assert!(matches!(app.overlay.as_ref(), Some(StatsOverlay::Confirmation(_))));
    }

    #[test]
    #[ignore = "200-cycle deterministic performance control"]
    fn benchmark_event_to_frame_current_shape() {
        const WARMUPS: usize = 10;
        const SAMPLES: usize = 200;

        for (rows, width, height) in
            [(100, 160, 50), (100, 100, 32), (1_000, 160, 50), (1_000, 100, 32)]
        {
            let backend = ratatui::backend::TestBackend::new(width, height);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            let mut app = StatsApp::new(snapshot_with_processes(rows));
            let mut regions = draw_app(&mut terminal, &app);

            for index in 0..WARMUPS {
                let _ = app.on_event(benchmark_event(index), &regions);
                regions = draw_app(&mut terminal, &app);
            }

            let mut micros = Vec::with_capacity(SAMPLES);
            for index in 0..SAMPLES {
                let started = Instant::now();
                let _ = app.on_event(benchmark_event(index), &regions);
                regions = draw_app(&mut terminal, &app);
                micros.push(started.elapsed().as_micros());
            }
            micros.sort_unstable();
            let p50 = micros[SAMPLES / 2];
            let p95 = micros[(SAMPLES * 95 / 100).min(SAMPLES - 1)];
            let maximum = micros[SAMPLES - 1];
            println!(
                "{{\"schema\":1,\"surface\":\"legacy_control\",\"rows\":{rows},\"threads\":{},\"width\":{width},\"height\":{height},\"warmups\":{WARMUPS},\"samples\":{SAMPLES},\"p50_us\":{p50},\"p95_us\":{p95},\"max_us\":{maximum}}}",
                app.snapshot.system.thread_count
            );
        }
    }

    fn benchmark_event(index: usize) -> Event {
        let code = match index % 8 {
            0..=2 => KeyCode::Down,
            3 => KeyCode::Up,
            4 => KeyCode::PageDown,
            5 => KeyCode::PageUp,
            6 => KeyCode::Char('1'),
            _ => KeyCode::Char('4'),
        };
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }
}
