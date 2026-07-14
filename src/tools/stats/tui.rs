use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use super::actions::ActionController;
use super::app::{Action, StatsApp};
use super::render::{render, UiRegions};
use super::sampler::{Sampler, SamplerWorker};
use crate::tui::{EventReader, Session, SessionOptions};

pub async fn run(interval: Duration, mouse_capture: bool) -> Result<()> {
    let sampler = Sampler::new(interval)?;
    let (worker, mut snapshots, mut details) = SamplerWorker::start(sampler)?;
    let initial = Arc::clone(&snapshots.borrow());
    let mut app = StatsApp::new(initial);
    let mut session = Session::open(SessionOptions { mouse_capture })?;
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

    use super::super::app::{ActiveRegion, DetailIntent, InspectorTab, SortBy};
    use super::super::host::ProcessAction;
    use super::super::model::{
        CpuSample, DetailCompleteness, DetailData, DetailOutcome, DetailRequest, DetailRequestKind,
        DetailSnapshot, Observed, ProcessIdentity, ProcessKey, ProcessSample, ProcessState,
        SampleReadiness, StatsSnapshot, SystemSample, ThreadSample,
    };
    use super::*;
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };

    fn draw_app(
        terminal: &mut ratatui::Terminal<ratatui::backend::TestBackend>,
        app: &StatsApp,
    ) -> UiRegions {
        let mut regions = UiRegions::default();
        terminal.draw(|frame| regions = render(frame, app)).unwrap();
        regions
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
        let first_process_row = regions.rows.first().unwrap().0.y;
        assert!(core_bottom <= 4, "32 cores consumed rows through {core_bottom}");
        assert!(
            first_process_row <= 7,
            "process table did not begin until row {first_process_row}"
        );
        assert!(regions.end_process.is_some(), "inspector was not visible");

        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for label in ["KIT / STATS", "CPU HISTORY", "PROCESS TREE", "OVERVIEW"] {
            assert!(screen.contains(label), "missing {label} surface");
        }
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
        assert_eq!(regions.rows[0].1, 0);
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
    fn explicit_navigation_scrolls_but_refresh_returns_to_the_top() {
        let mut snapshot = Arc::unwrap_or_clone(snapshot());
        snapshot.processes = (0..30)
            .map(|index| process(index + 10, &format!("process-{index:02}"), index as f32))
            .collect();
        let snapshot = Arc::new(snapshot);
        let mut app = StatsApp::new(Arc::clone(&snapshot));
        app.viewport_rows = 5;
        app.move_selection(12);
        assert!(app.row_offset > 0);
        let selected = app.selected;
        app.ingest(snapshot);
        assert_eq!(app.row_offset, 0);
        assert_eq!(app.selected, selected);
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
        app.on_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: memory.x,
                row: memory.y,
                modifiers: KeyModifiers::NONE,
            },
            &regions,
        );

        assert_eq!(app.sort, SortBy::Memory);
        assert!(app.descending);
        assert_eq!(app.row_offset, 0);
        assert_eq!(app.visible[0].pid, 3);

        let first_row = regions.rows[0].0;
        app.on_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: memory.x,
                row: first_row.y,
                modifiers: KeyModifiers::NONE,
            },
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

        app.on_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), &UiRegions::default());

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
        assert!(regions.end_process.is_none());
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
        app.open_confirmation(ProcessAction::GracefulTerminate);
        assert!(app.confirm.is_none());
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

        app.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &regions);
        assert_eq!(app.active_region, ActiveRegion::Processes);
        assert!(!app.collapsed.contains(&selected));

        app.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &regions);
        assert_eq!(app.active_region, ActiveRegion::Inspector);
        app.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &regions);
        assert_eq!(app.active_region, ActiveRegion::Processes);
    }

    #[test]
    fn tab_and_mouse_navigation_work_in_wide_and_compact_layouts() {
        let mut app = StatsApp::new(snapshot());
        let backend = ratatui::backend::TestBackend::new(140, 35);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let regions = draw_app(&mut terminal, &app);

        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &regions);
        assert_eq!(app.active_region, ActiveRegion::Inspector);
        let processes = regions.processes.expect("process region");
        app.on_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: processes.x,
                row: processes.y,
                modifiers: KeyModifiers::NONE,
            },
            &regions,
        );
        assert_eq!(app.active_region, ActiveRegion::Processes);

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let compact = draw_app(&mut terminal, &app);
        assert!(compact.processes.is_some());
        assert!(compact.inspector.is_none());
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &compact);
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

        app.on_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: original.separator.x,
                row: original.separator.y + original.separator.height / 2,
                modifiers: KeyModifiers::NONE,
            },
            &regions,
        );
        assert!(app.split_drag.is_some());

        let target = original.content.x + original.content.width * 3 / 4;
        app.on_mouse(
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: target,
                row: original.separator.y,
                modifiers: KeyModifiers::NONE,
            },
            &regions,
        );
        app.on_mouse(
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: target,
                row: original.separator.y,
                modifiers: KeyModifiers::NONE,
            },
            &regions,
        );
        assert!(app.split_drag.is_none());

        let resized = draw_app(&mut terminal, &app).split.expect("resized split");
        assert!(resized.separator.x > original.separator.x);

        app.on_key(KeyEvent::new(KeyCode::Char('='), KeyModifiers::NONE), &regions);
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
        app.on_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: area.x,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            },
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
            app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &regions);
        }
        assert!(app.family_cursor >= regions.family_rows.len());
        let expected = app.family_row_key(app.family_cursor).unwrap();
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &regions);
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
        app.on_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE), &UiRegions::default());
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
        app.on_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE), &UiRegions::default());

        let mut replacement = Arc::unwrap_or_clone(snapshot());
        replacement.processes[0].command = "replacement command".into();
        app.ingest(Arc::new(replacement));
        assert_eq!(app.command_viewer.as_ref().unwrap().command, original);

        let backend = ratatui::backend::TestBackend::new(130, 35);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let regions = draw_app(&mut terminal, &app);
        app.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &regions);
        assert!(app.command_viewer.as_ref().unwrap().column_offset > 0);
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &regions);
        assert!(app.command_viewer.is_none());
    }

    #[test]
    fn drawn_core_and_confirmation_buttons_are_clickable() {
        let backend = ratatui::backend::TestBackend::new(130, 35);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = StatsApp::new(snapshot_with_cores(16));
        let mut regions = draw_app(&mut terminal, &app);
        assert_eq!(regions.cores.len(), 16);
        for (index, (core, logical_index)) in regions.cores.iter().copied().enumerate() {
            assert_eq!(core.width, 5);
            if index > 0 {
                assert_eq!(core.x, regions.cores[index - 1].0.right());
            }
            app.on_mouse(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: core.x + core.width / 2,
                    row: core.y,
                    modifiers: KeyModifiers::NONE,
                },
                &regions,
            );
            assert_eq!(app.focused_core, Some(logical_index));
        }

        app.focused_core = None;
        app.reproject();
        app.open_confirmation(ProcessAction::GracefulTerminate);
        assert_eq!(
            app.confirm.as_ref().unwrap().choice,
            super::super::app::ConfirmationChoice::Cancel
        );
        assert!(matches!(
            app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &regions),
            Action::None
        ));
        assert!(app.confirm.is_none());

        app.open_confirmation(ProcessAction::GracefulTerminate);
        regions = draw_app(&mut terminal, &app);
        let force = regions.confirm_force.unwrap();
        assert!(matches!(
            app.on_mouse(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: force.x,
                    row: force.y,
                    modifiers: KeyModifiers::NONE,
                },
                &regions
            ),
            Action::Process(_, ProcessAction::ForceTerminate)
        ));
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
