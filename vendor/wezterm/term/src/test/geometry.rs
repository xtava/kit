use super::*;
use k9::assert_equal as assert_eq;
use std::mem;
use std::sync::atomic::{AtomicUsize, Ordering};
use wezterm_runtime_admission::{
    RetainedClass, RuntimeAdmission, RuntimeRole, MAX_SERVER_TERMINAL_ACTION_BYTES,
};

fn size(rows: usize, cols: usize, pixel_width: usize, pixel_height: usize) -> TerminalSize {
    TerminalSize {
        rows,
        cols,
        pixel_width,
        pixel_height,
        dpi: 96,
    }
}

fn limits(max_geometry_bytes: usize) -> TerminalGeometryLimits {
    TerminalGeometryLimits {
        max_rows: u16::MAX as usize,
        max_cols: u16::MAX as usize,
        max_pixel_width: u16::MAX as usize,
        max_pixel_height: u16::MAX as usize,
        max_scrollback_rows: usize::MAX,
        max_geometry_bytes,
    }
}

fn permissive_limits() -> TerminalGeometryLimits {
    TerminalGeometryLimits {
        max_rows: usize::MAX,
        max_cols: usize::MAX,
        max_pixel_width: usize::MAX,
        max_pixel_height: usize::MAX,
        max_scrollback_rows: usize::MAX,
        max_geometry_bytes: usize::MAX,
    }
}

fn conservative_capacity(requested: usize) -> usize {
    if requested == 0 {
        0
    } else {
        requested.next_power_of_two() * 2
    }
}

#[test]
fn bounded_construction_rejects_zero_text_dimensions() {
    assert_eq!(
        Terminal::plan_bounded_construction(size(0, 80, 0, 0), 0, limits(usize::MAX)),
        Err(TerminalGeometryError::ZeroDimension { dimension: "rows" })
    );
    assert_eq!(
        Terminal::plan_bounded_construction(size(24, 0, 0, 0), 0, limits(usize::MAX)),
        Err(TerminalGeometryError::ZeroDimension {
            dimension: "columns"
        })
    );
}

#[test]
fn bounded_construction_rejects_declared_dimension_limits() {
    let base = TerminalGeometryLimits {
        max_rows: 24,
        max_cols: 80,
        max_pixel_width: 800,
        max_pixel_height: 600,
        max_scrollback_rows: 100,
        max_geometry_bytes: usize::MAX,
    };

    assert!(matches!(
        Terminal::plan_bounded_construction(size(25, 80, 800, 600), 0, base),
        Err(TerminalGeometryError::DimensionExceedsLimit {
            dimension: "rows",
            actual: 25,
            limit: 24
        })
    ));
    assert!(matches!(
        Terminal::plan_bounded_construction(size(24, 81, 800, 600), 0, base),
        Err(TerminalGeometryError::DimensionExceedsLimit {
            dimension: "columns",
            actual: 81,
            limit: 80
        })
    ));
    assert!(matches!(
        Terminal::plan_bounded_construction(size(24, 80, 801, 600), 0, base),
        Err(TerminalGeometryError::DimensionExceedsLimit {
            dimension: "pixel width",
            actual: 801,
            limit: 800
        })
    ));
    assert!(matches!(
        Terminal::plan_bounded_construction(size(24, 80, 800, 601), 0, base),
        Err(TerminalGeometryError::DimensionExceedsLimit {
            dimension: "pixel height",
            actual: 601,
            limit: 600
        })
    ));
}

#[test]
fn bounded_construction_accepts_pty_boundaries_and_rejects_field_overflow() {
    let boundary = u16::MAX as usize;
    Terminal::plan_bounded_construction(
        size(boundary, 1, boundary, boundary),
        0,
        limits(usize::MAX),
    )
    .unwrap();

    for (field, terminal_size) in [
        ("rows", size(boundary + 1, 1, 0, 0)),
        ("columns", size(1, boundary + 1, 0, 0)),
        ("pixel width", size(1, 1, boundary + 1, 0)),
        ("pixel height", size(1, 1, 0, boundary + 1)),
    ] {
        assert!(matches!(
            Terminal::plan_bounded_construction(terminal_size, 0, permissive_limits()),
            Err(TerminalGeometryError::PtyFieldOverflow {
                field: actual_field,
                actual,
                maximum
            }) if actual_field == field && actual == boundary + 1 && maximum == boundary
        ));
    }
}

#[test]
fn bounded_construction_rejects_rows_plus_scrollback_overflow() {
    assert_eq!(
        Terminal::plan_bounded_construction(size(1, 1, 0, 0), usize::MAX, permissive_limits()),
        Err(TerminalGeometryError::ArithmeticOverflow {
            calculation: "primary rows plus scrollback"
        })
    );
}

#[test]
fn bounded_construction_accounts_primary_alternate_blank_lines_and_tabs() {
    let terminal_size = size(3, 9, 90, 60);
    let scrollback_rows = 5;
    let plan =
        Terminal::plan_bounded_construction(terminal_size, scrollback_rows, limits(usize::MAX))
            .unwrap();

    let line_slots = conservative_capacity(terminal_size.rows + scrollback_rows)
        + conservative_capacity(terminal_size.rows);
    let line_slot_bytes = line_slots * mem::size_of::<Line>();
    let blank_line_heap_bytes = terminal_size.rows * 2 * Line::INITIAL_CLUSTER_TEXT_CAPACITY;
    let tab_storage_bytes = conservative_capacity(terminal_size.cols).max(mem::size_of::<usize>());
    let expected = line_slot_bytes + blank_line_heap_bytes + tab_storage_bytes;

    assert_eq!(plan.size(), terminal_size);
    assert_eq!(plan.configured_scrollback_rows(), scrollback_rows);
    assert_eq!(plan.geometry_bytes(), expected);
    assert!(matches!(
        Terminal::plan_bounded_construction(terminal_size, scrollback_rows, limits(expected - 1)),
        Err(TerminalGeometryError::GeometryBytesExceedLimit {
            required,
            limit
        }) if required == expected && limit == expected - 1
    ));
    assert_eq!(
        Terminal::plan_bounded_construction(terminal_size, scrollback_rows, limits(expected))
            .unwrap()
            .geometry_bytes(),
        expected
    );
}

#[test]
fn bounded_construction_rejects_configured_scrollback_limit() {
    let declared = TerminalGeometryLimits {
        max_rows: 24,
        max_cols: 80,
        max_pixel_width: 800,
        max_pixel_height: 600,
        max_scrollback_rows: 10,
        max_geometry_bytes: usize::MAX,
    };

    assert_eq!(
        Terminal::plan_bounded_construction(size(24, 80, 800, 600), 11, declared),
        Err(TerminalGeometryError::ConfiguredScrollbackRowsExceedLimit {
            actual: 11,
            limit: 10
        })
    );
}

#[derive(Debug)]
struct ReloadableScrollbackConfig {
    scrollback_rows: AtomicUsize,
}

impl TerminalConfiguration for ReloadableScrollbackConfig {
    fn scrollback_size(&self) -> usize {
        self.scrollback_rows.load(Ordering::Relaxed)
    }

    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

#[test]
fn fixed_bounded_scrollback_does_not_expand_after_config_reload() {
    let config = Arc::new(ReloadableScrollbackConfig {
        scrollback_rows: AtomicUsize::new(1),
    });
    let plan =
        Terminal::plan_bounded_construction(size(2, 8, 0, 0), 1, limits(usize::MAX)).unwrap();
    let mut term = Terminal::new_from_geometry_plan(
        plan,
        config.clone(),
        "WezTerm",
        "test",
        Box::new(Vec::new()),
    );

    config.scrollback_rows.store(1_000, Ordering::Relaxed);
    for line in 0..32 {
        term.advance_bytes(format!("line {line}\r\n")).unwrap();
    }

    assert_eq!(term.screen().scrollback_rows(), 3);
}

fn bounded_terminal(rows: usize, cols: usize, scrollback_rows: usize) -> Terminal {
    let plan = Terminal::plan_bounded_construction(
        size(rows, cols, cols * 8, rows * 16),
        scrollback_rows,
        limits(usize::MAX),
    )
    .unwrap();
    Terminal::new_from_geometry_plan(
        plan,
        Arc::new(ReloadableScrollbackConfig {
            scrollback_rows: AtomicUsize::new(scrollback_rows),
        }),
        "WezTerm",
        "test",
        Box::new(Vec::new()),
    )
}

fn admitted_terminal(
    rows: usize,
    cols: usize,
    scrollback_rows: usize,
) -> (Terminal, Arc<RuntimeAdmission>) {
    let geometry_limits = limits(wezterm_runtime_admission::MAX_TERMINAL_STATE_BYTES_TOTAL);
    let plan = Terminal::plan_bounded_construction(
        size(rows, cols, cols * 8, rows * 16),
        scrollback_rows,
        geometry_limits,
    )
    .unwrap();
    let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
    let lease = admission
        .try_retained(
            RetainedClass::ServerTerminal,
            plan.initial_server_retained_bytes().unwrap(),
        )
        .unwrap();
    let terminal = Terminal::new_server_from_geometry_plan(
        plan,
        lease,
        geometry_limits,
        Arc::new(ReloadableScrollbackConfig {
            scrollback_rows: AtomicUsize::new(scrollback_rows),
        }),
        "WezTerm",
        "test",
        Box::new(Vec::new()),
    )
    .unwrap();
    (terminal, admission)
}

#[test]
fn server_terminal_owns_and_releases_its_retained_state_lease() {
    let (terminal, admission) = admitted_terminal(4, 16, 8);
    let retained = terminal.server_retained_bytes().unwrap();

    assert_eq!(
        admission.retained_usage(RetainedClass::ServerTerminal),
        retained
    );
    assert_eq!(admission.retained_aggregate_usage(), retained);

    drop(terminal);
    assert_eq!(admission.retained_usage(RetainedClass::ServerTerminal), 0);
    assert_eq!(admission.retained_aggregate_usage(), 0);
}

#[test]
fn oversized_action_batch_is_rejected_before_terminal_mutation() {
    let (mut terminal, admission) = admitted_terminal(4, 16, 8);
    let retained_before = terminal.server_retained_bytes().unwrap();
    let seqno_before = terminal.current_seqno();
    let size_before = terminal.get_size();
    let oversized = String::with_capacity(MAX_SERVER_TERMINAL_ACTION_BYTES + 1);

    assert!(matches!(
        terminal.perform_actions(vec![wezterm_escape_parser::Action::PrintString(oversized)]),
        Err(TerminalRetainedStateError::ActionBatchTooLarge { .. })
    ));
    assert_eq!(terminal.current_seqno(), seqno_before);
    assert_eq!(terminal.get_size(), size_before);
    assert_eq!(terminal.server_retained_bytes(), Some(retained_before));
    assert_eq!(
        admission.retained_usage(RetainedClass::ServerTerminal),
        retained_before
    );
}

#[test]
fn cancelled_server_resize_releases_peak_charge_without_mutation() {
    let (mut terminal, admission) = admitted_terminal(2, 8, 2);
    let retained_before = terminal.server_retained_bytes().unwrap();
    let size_before = terminal.get_size();

    {
        let prepared = terminal
            .prepare_server_resize(size(32, 160, 1_280, 512))
            .unwrap();
        assert_eq!(prepared.target_size(), size(32, 160, 1_280, 512));
        assert!(admission.retained_usage(RetainedClass::ServerTerminal) > retained_before);
    }

    assert_eq!(terminal.get_size(), size_before);
    assert_eq!(terminal.server_retained_bytes(), Some(retained_before));
    assert_eq!(
        admission.retained_usage(RetainedClass::ServerTerminal),
        retained_before
    );
}

#[test]
fn server_resize_admission_failure_precedes_any_geometry_mutation() {
    let (mut terminal, admission) = admitted_terminal(2, 8, 2);
    let retained_before = terminal.server_retained_bytes().unwrap();
    let size_before = terminal.get_size();
    let remaining = admission.retained_capacity(RetainedClass::ServerTerminal)
        - admission.retained_usage(RetainedClass::ServerTerminal);
    let _capacity_blocker = admission
        .try_retained(RetainedClass::ServerTerminal, remaining)
        .unwrap();

    assert!(matches!(
        terminal.prepare_server_resize(size(512, 1_024, 8_192, 8_192)),
        Err(TerminalRetainedStateError::Admission(_))
    ));
    assert_eq!(terminal.get_size(), size_before);
    assert_eq!(terminal.server_retained_bytes(), Some(retained_before));
}

#[test]
fn committed_server_resize_reconciles_to_settled_state() {
    let (mut terminal, admission) = admitted_terminal(2, 8, 2);
    let target = size(12, 80, 640, 192);
    terminal.prepare_server_resize(target).unwrap().commit();

    assert_eq!(terminal.get_size(), target);
    assert_eq!(
        admission.retained_usage(RetainedClass::ServerTerminal),
        terminal.server_retained_bytes().unwrap()
    );
}

#[test]
fn bounded_resize_rejects_zero_and_pty_overflow_without_mutation() {
    let term = bounded_terminal(2, 8, 2);
    let original_size = term.get_size();
    let original_retained = term.geometry_retained_size_excluding_image_data().unwrap();

    assert!(matches!(
        term.plan_bounded_resize(size(0, 8, 0, 0), limits(usize::MAX)),
        Err(TerminalGeometryError::ZeroDimension { dimension: "rows" })
    ));
    assert!(matches!(
        term.plan_bounded_resize(
            size(2, usize::MAX, 0, 0),
            permissive_limits()
        ),
        Err(TerminalGeometryError::PtyFieldOverflow {
            field: "columns",
            actual: usize::MAX,
            maximum
        }) if maximum == u16::MAX as usize
    ));
    assert_eq!(term.get_size(), original_size);
    assert_eq!(
        term.geometry_retained_size_excluding_image_data().unwrap(),
        original_retained
    );
}

#[test]
fn bounded_resize_accepts_pty_boundary() {
    let term = bounded_terminal(2, 8, 2);
    let boundary = u16::MAX as usize;
    let plan = term
        .plan_bounded_resize(size(2, boundary, boundary, boundary), limits(usize::MAX))
        .unwrap();

    assert_eq!(plan.target_size().cols, boundary);
    assert!(plan.peak_geometry_bytes() >= plan.current_geometry_retained_bytes());
    assert_eq!(
        plan.additional_bytes_required(),
        plan.peak_geometry_bytes() - plan.current_geometry_retained_bytes()
    );
}

#[test]
fn bounded_resize_bounds_narrow_and_wide_clustered_and_vector_lines() {
    let mut term = bounded_terminal(3, 12, 4);
    let seqno = term.current_seqno();
    let attrs = CellAttributes::default();

    let clustered = term.screen_mut().line_mut(0);
    for (idx, ch) in "clustered-data".chars().enumerate() {
        clustered.set_cell_grapheme(idx, &ch.to_string(), 1, attrs.clone(), seqno);
    }
    *term.screen_mut().line_mut(1) = Line::from_text("vector-line-data", &attrs, seqno, None);

    let narrow = term
        .plan_bounded_resize(size(4, 4, 32, 64), limits(usize::MAX))
        .unwrap();
    let narrow_settled = narrow.settled_geometry_retained_upper_bound();
    term.apply_bounded_resize(narrow).unwrap();
    assert_eq!(term.get_size().rows, 4);
    assert_eq!(term.get_size().cols, 4);
    assert!(term.geometry_retained_size_excluding_image_data().unwrap() <= narrow_settled);

    let wide = term
        .plan_bounded_resize(size(5, 20, 160, 80), limits(usize::MAX))
        .unwrap();
    let wide_settled = wide.settled_geometry_retained_upper_bound();
    term.apply_bounded_resize(wide).unwrap();
    assert_eq!(term.get_size().rows, 5);
    assert_eq!(term.get_size().cols, 20);
    assert!(term.geometry_retained_size_excluding_image_data().unwrap() <= wide_settled);
}

#[test]
fn bounded_resize_counts_primary_and_alternate_screens() {
    let mut populated = bounded_terminal(3, 10, 3);
    let empty = bounded_terminal(3, 10, 3);
    let attrs = CellAttributes::default();
    let seqno = populated.current_seqno();
    *populated.screen_mut().line_mut(0) =
        Line::from_text("primary-vector-content", &attrs, seqno, None);
    populated.advance_bytes(b"\x1b[?1049h").unwrap();
    let seqno = populated.current_seqno();
    *populated.screen_mut().line_mut(0) =
        Line::from_text("alternate-vector-content", &attrs, seqno, None);
    populated.advance_bytes(b"\x1b[?1049l").unwrap();

    let populated_plan = populated
        .plan_bounded_resize(size(4, 5, 40, 64), limits(usize::MAX))
        .unwrap();
    let empty_plan = empty
        .plan_bounded_resize(size(4, 5, 40, 64), limits(usize::MAX))
        .unwrap();

    assert_eq!(
        populated_plan.current_geometry_retained_bytes(),
        populated
            .geometry_retained_size_excluding_image_data()
            .unwrap()
    );
    assert!(
        populated_plan.current_geometry_retained_bytes()
            > empty_plan.current_geometry_retained_bytes()
    );
    assert!(populated_plan.peak_geometry_bytes() > empty_plan.peak_geometry_bytes());
}

#[test]
fn bounded_resize_preserves_fixed_scrollback() {
    let mut term = bounded_terminal(2, 8, 2);
    let plan = term
        .plan_bounded_resize(size(4, 6, 48, 64), limits(usize::MAX))
        .unwrap();
    term.apply_bounded_resize(plan).unwrap();

    for line in 0..32 {
        term.advance_bytes(format!("resized {line}\r\n")).unwrap();
    }

    assert_eq!(term.screen().scrollback_rows(), 6);
}

#[test]
fn bounded_resize_accounts_for_conpty_cursor_preservation_padding() {
    let mut term = bounded_terminal(3, 8, 4);
    term.enable_conpty_quirks();
    term.advance_bytes(b"one\r\ntwo\r\nthree\r\n").unwrap();

    let initial_line_count = term.screen().scrollback_rows();
    let initial_cursor = term.cursor_pos();
    let initial_cursor_phys = term.screen().phys_row(initial_cursor.y);
    assert_eq!(initial_cursor_phys + 1, initial_line_count);

    let target = size(6, 8, 64, 96);
    let ordinarily_padded_line_count = initial_line_count.max(target.rows);
    let required_rows_after_cursor = target.rows - initial_cursor.y as usize;
    let ordinary_rows_after_cursor = ordinarily_padded_line_count - initial_cursor_phys;
    assert!(ordinary_rows_after_cursor < required_rows_after_cursor);
    let post_padding_lines = required_rows_after_cursor - ordinary_rows_after_cursor;

    let plan = term
        .plan_bounded_resize(target, limits(usize::MAX))
        .unwrap();
    let planned_line_capacity_request = plan.primary_line_capacity_request();
    let settled_bound = plan.settled_geometry_retained_upper_bound();
    term.apply_bounded_resize(plan).unwrap();

    assert_eq!(
        term.screen().scrollback_rows(),
        ordinarily_padded_line_count + post_padding_lines
    );
    assert!(term.screen().scrollback_rows() <= planned_line_capacity_request);
    assert!(term.screen().line_capacity() <= conservative_capacity(planned_line_capacity_request));
    assert!(term.geometry_retained_size_excluding_image_data().unwrap() <= settled_bound);
}

#[test]
fn bounded_resize_rejects_stale_plan_without_resize_mutation() {
    let mut term = bounded_terminal(2, 8, 2);
    let plan = term
        .plan_bounded_resize(size(4, 4, 16, 16), limits(usize::MAX))
        .unwrap();
    term.advance_bytes(b"output").unwrap();
    let size_after_output = term.get_size();
    let retained_after_output = term.geometry_retained_size_excluding_image_data().unwrap();

    assert!(matches!(
        term.apply_bounded_resize(plan),
        Err(TerminalGeometryError::StaleResizePlan { .. })
    ));
    assert_eq!(term.get_size(), size_after_output);
    assert_eq!(
        term.geometry_retained_size_excluding_image_data().unwrap(),
        retained_after_output
    );
}

#[test]
fn bounded_resize_rejects_equal_footprint_state_change() {
    let mut term = bounded_terminal(2, 8, 2);
    let attrs = CellAttributes::default();
    let seqno = term.current_seqno();
    *term.screen_mut().line_mut(0) = Line::from_text("same", &attrs, seqno, None);
    let plan = term
        .plan_bounded_resize(size(3, 4, 16, 16), limits(usize::MAX))
        .unwrap();
    let planned_retained = plan.current_geometry_retained_bytes();

    *term.screen_mut().line_mut(0) = Line::from_text("size", &attrs, seqno, None);
    assert_eq!(
        term.geometry_retained_size_excluding_image_data().unwrap(),
        planned_retained
    );
    let size_before_apply = term.get_size();

    assert!(matches!(
        term.apply_bounded_resize(plan),
        Err(TerminalGeometryError::ResizePlanGeometryMutated { .. })
    ));
    assert_eq!(term.get_size(), size_before_apply);
    assert_eq!(term.screen().lines_in_phys_range(0..1)[0].as_str(), "size");
}

#[test]
fn rejected_bounded_resize_plan_does_not_mutate_terminal() {
    let term = bounded_terminal(2, 8, 2);
    let original_size = term.get_size();
    let original_retained = term.geometry_retained_size_excluding_image_data().unwrap();

    assert!(matches!(
        term.plan_bounded_resize(size(20, 80, 640, 320), limits(0)),
        Err(TerminalGeometryError::GeometryBytesExceedLimit { .. })
    ));
    assert_eq!(term.get_size(), original_size);
    assert_eq!(
        term.geometry_retained_size_excluding_image_data().unwrap(),
        original_retained
    );
}

#[test]
fn bounded_resize_plan_cannot_target_another_terminal() {
    let first = bounded_terminal(2, 8, 2);
    let mut second = bounded_terminal(2, 8, 2);
    let plan = first
        .plan_bounded_resize(size(3, 4, 16, 16), limits(usize::MAX))
        .unwrap();
    let second_size = second.get_size();

    assert_eq!(
        second.apply_bounded_resize(plan),
        Err(TerminalGeometryError::ResizePlanTerminalMismatch)
    );
    assert_eq!(second.get_size(), second_size);
}

#[test]
fn bounded_resize_planning_rejects_dynamic_scrollback_terminal() {
    let term = Terminal::new(
        size(2, 8, 64, 32),
        Arc::new(ReloadableScrollbackConfig {
            scrollback_rows: AtomicUsize::new(2),
        }),
        "WezTerm",
        "test",
        Box::new(Vec::new()),
    );

    assert!(matches!(
        term.plan_bounded_resize(size(3, 4, 16, 16), limits(usize::MAX)),
        Err(TerminalGeometryError::ResizeRequiresBoundedTerminal)
    ));
}
