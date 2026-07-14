use std::collections::VecDeque;
use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use ratatui::backend::TestBackend;
use ratatui::Terminal;

use super::*;
use crate::tools::diff::model::{DiffInput, SourceSnapshot};
use crate::tui::theme::NORD;

#[derive(Clone, Copy)]
struct Case {
    name: &'static str,
    source_lines: usize,
    changed_every: usize,
    files: usize,
    width: u16,
    height: u16,
    mode: ViewMode,
    samples: usize,
}

const CASES: [Case; 3] = [
    Case {
        name: "ordinary",
        source_lines: 250,
        changed_every: 25,
        files: 25,
        width: 100,
        height: 32,
        mode: ViewMode::Inline,
        samples: 30,
    },
    Case {
        name: "realistic",
        source_lines: 5_000,
        changed_every: 50,
        files: 250,
        width: 160,
        height: 50,
        mode: ViewMode::Split,
        samples: 12,
    },
    Case {
        name: "extreme",
        source_lines: 25_000,
        changed_every: 25,
        files: 2_000,
        width: 160,
        height: 50,
        mode: ViewMode::Split,
        samples: 4,
    },
];

#[test]
#[ignore = "deterministic Diff lifecycle performance harness"]
fn benchmark_diff_lifecycle() {
    for case in CASES {
        let (old, new) = benchmark_sources(case.source_lines, case.changed_every);
        measure(case, "model_build", || {
            black_box(text_document(&old, &new));
        });

        let documents = benchmark_documents(case, &old, &new);
        let app = DiffApp::new(documents, NORD, case.mode);
        let effective = match case.mode {
            ViewMode::Split => EffectiveMode::Split,
            ViewMode::Auto | ViewMode::Inline => EffectiveMode::Inline,
        };
        measure(case, "projection_build", || {
            black_box(document_lines(
                &app.documents[0],
                &app,
                effective,
                case.width as usize - TREE_WIDTH as usize - 2,
            ));
        });
        measure(case, "tree_build", || {
            black_box(tree_rows(&app.documents, &app.expanded));
        });

        let backend = TestBackend::new(case.width, case.height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        measure(case, "warm_frame", || {
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
        });
        app.active_region = ActiveRegion::New;
        let mut scroll_index = 0;
        measure(case, "scroll_event_to_frame", || {
            let code = if scroll_index % 12 == 11 { KeyCode::Up } else { KeyCode::Down };
            scroll_index += 1;
            let _ = app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
        });

        let documents = benchmark_text_documents(case, &old, &new);
        let backend = TestBackend::new(case.width, case.height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = DiffApp::new(documents, NORD, case.mode);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        app.active_region = ActiveRegion::Changes;
        measure(case, "tree_selection_event_to_frame", || {
            let _ = app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
        });

        measure_selection_burst(case, &mut terminal, &mut app);
    }
}

fn measure_selection_burst(case: Case, terminal: &mut Terminal<TestBackend>, app: &mut DiffApp) {
    const BURST: usize = 10;

    let samples = case.samples.min(8);
    let mut micros = Vec::with_capacity(samples);
    for _ in 0..samples {
        app.select(0);
        terminal.draw(|frame| render(frame, app)).unwrap();
        let mut events =
            VecDeque::from(vec![
                Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
                BURST
            ]);
        let started = Instant::now();
        let flow = handle_terminal_events(app, events.pop_front(), || events.pop_front());
        assert_eq!(flow, Flow::Continue);
        terminal.draw(|frame| render(frame, app)).unwrap();
        micros.push(started.elapsed().as_micros());
    }
    micros.sort_unstable();
    let p50 = micros[samples / 2];
    let p95 = micros[(samples * 95 / 100).min(samples - 1)];
    let maximum = micros[samples - 1];
    println!(
        "{{\"schema\":2,\"surface\":\"tree_selection_10_event_burst_to_frame\",\"case\":\"{}\",\"source_lines\":{},\"files\":{},\"events\":{BURST},\"samples\":{samples},\"p50_us\":{p50},\"p95_us\":{p95},\"max_us\":{maximum}}}",
        case.name, case.source_lines, case.files
    );
}

fn measure(case: Case, surface: &str, mut operation: impl FnMut()) {
    const WARMUPS: usize = 2;

    for _ in 0..WARMUPS {
        operation();
    }
    let mut micros = Vec::with_capacity(case.samples);
    for _ in 0..case.samples {
        let started = Instant::now();
        operation();
        micros.push(started.elapsed().as_micros());
    }
    micros.sort_unstable();
    let p50 = micros[case.samples / 2];
    let p95 = micros[(case.samples * 95 / 100).min(case.samples - 1)];
    let maximum = micros[case.samples - 1];
    println!(
        "{{\"schema\":2,\"surface\":\"{surface}\",\"case\":\"{}\",\"source_lines\":{},\"files\":{},\"width\":{},\"height\":{},\"warmups\":{WARMUPS},\"samples\":{},\"p50_us\":{p50},\"p95_us\":{p95},\"max_us\":{maximum}}}",
        case.name, case.source_lines, case.files, case.width, case.height, case.samples
    );
}

fn benchmark_sources(source_lines: usize, changed_every: usize) -> (Arc<[u8]>, Arc<[u8]>) {
    let mut old = String::new();
    let mut new = String::new();
    for index in 0..source_lines {
        old.push_str(&format!("pub fn value_{index}() -> usize {{ {index} }}\n"));
        let value = if index % changed_every == 0 { index + 1 } else { index };
        new.push_str(&format!("pub fn value_{index}() -> usize {{ {value} }}\n"));
    }
    (Arc::from(old.into_bytes()), Arc::from(new.into_bytes()))
}

fn text_document(old: &Arc<[u8]>, new: &Arc<[u8]>) -> DiffDocument {
    DiffDocument::build(
        DiffInput {
            group: ChangeGroup::Changes,
            kind: ChangeKind::Modified,
            old_path: Some("src/benchmark.rs".into()),
            new_path: Some("src/benchmark.rs".into()),
            old: SourceSnapshot::Bytes(Arc::clone(old)),
            new: SourceSnapshot::Bytes(Arc::clone(new)),
            special: None,
        },
        DiffContext::default(),
    )
}

fn benchmark_documents(case: Case, old: &Arc<[u8]>, new: &Arc<[u8]>) -> Vec<DiffDocument> {
    std::iter::once(text_document(old, new))
        .chain((1..case.files).map(|index| DiffDocument {
            group: if index % 2 == 0 { ChangeGroup::Changes } else { ChangeGroup::Staged },
            kind: ChangeKind::Modified,
            old_path: Some(format!("fixtures/group-{}/file-{index}.rs", index % 20).into()),
            new_path: Some(format!("fixtures/group-{}/file-{index}.rs", index % 20).into()),
            additions: Some(index % 17 + 1),
            deletions: Some(index % 11 + 1),
            body: DiffBody::Binary,
        }))
        .collect()
}

fn benchmark_text_documents(case: Case, old: &Arc<[u8]>, new: &Arc<[u8]>) -> Vec<DiffDocument> {
    let prototype = text_document(old, new);
    (0..case.samples + 3)
        .map(|index| {
            let mut document = prototype.clone();
            document.old_path = Some(format!("src/file-{index}.rs").into());
            document.new_path = Some(format!("src/file-{index}.rs").into());
            document
        })
        .collect()
}
