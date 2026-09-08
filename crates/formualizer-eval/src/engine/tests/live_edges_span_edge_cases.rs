//! Edge cases for 3-D span live-edge recording (GOD-242 / CL-051).
//!
//! Adopted verbatim (test names and assertions) from the review cycle 1 probe.
//!
//! Each case asserts the SAME invariant the fix claims: the live edges the
//! recorder writes for a 3-D span are EXACTLY the member cells the engine's own
//! read consumed -- no more (false staleness / spurious live cycles), no less
//! (the CL-051 defect).  The engine's read is characterised independently by
//! the value the span produces.

use crate::engine::live_edges::{LiveEdgeCollector, RecordingContext};
use crate::engine::{Engine, EvalConfig};
use crate::interpreter::Interpreter;
use crate::reference::{CellRef, Coord};
use crate::test_workbook::TestWorkbook;
use formualizer_common::LiteralValue;
use rustc_hash::FxHashSet;

fn parse(f: &str) -> formualizer_parse::parser::ASTNode {
    formualizer_parse::parser::parse(f).expect("valid formula")
}
fn new_engine() -> Engine<TestWorkbook> {
    Engine::new(TestWorkbook::new(), EvalConfig::default())
}
fn cell(e: &Engine<TestWorkbook>, s: &str, r: u32, c: u32) -> CellRef {
    CellRef::new(
        e.sheet_id(s).expect("sheet exists"),
        Coord::from_excel(r, c, true, true),
    )
}
fn set_num(e: &mut Engine<TestWorkbook>, s: &str, r: u32, c: u32, v: f64) {
    e.set_cell_value(s, r, c, LiteralValue::Number(v)).unwrap();
}
fn eval_as_member(
    e: &Engine<TestWorkbook>,
    col: &LiveEdgeCollector,
    idx: u32,
    sheet: &str,
    member: CellRef,
    formula: &str,
) -> LiteralValue {
    col.set_current(idx);
    let ctx = RecordingContext::new(e, col);
    let interp = Interpreter::new_with_cell(&ctx, sheet, member);
    interp
        .evaluate_ast(&parse(formula))
        .map(|cv| cv.into_literal())
        .unwrap_or_else(LiteralValue::Error)
}

/// Members: 0 = site (Sheet1!A1); 1..=3 = Acct1..Acct3 !B1; 4 = Acct1!B5.
fn fixture() -> (Engine<TestWorkbook>, Vec<CellRef>) {
    let mut engine = new_engine();
    for s in ["Acct1", "Acct2", "Acct3", "Zed"] {
        engine.add_sheet(s).unwrap();
    }
    set_num(&mut engine, "Acct1", 1, 2, 1.0);
    set_num(&mut engine, "Acct2", 1, 2, 2.0);
    set_num(&mut engine, "Acct3", 1, 2, 4.0);
    set_num(&mut engine, "Acct1", 5, 2, 8.0);
    // A non-member cell inside the Range3D rect, to catch over-recording.
    set_num(&mut engine, "Acct2", 2, 2, 16.0);
    set_num(&mut engine, "Zed", 1, 2, 32.0);
    engine.evaluate_all().unwrap();
    let members = vec![
        cell(&engine, "Sheet1", 1, 1),
        cell(&engine, "Acct1", 1, 2),
        cell(&engine, "Acct2", 1, 2),
        cell(&engine, "Acct3", 1, 2),
        cell(&engine, "Acct1", 5, 2),
    ];
    (engine, members)
}

fn case(
    engine: &Engine<TestWorkbook>,
    members: &[CellRef],
    formula: &str,
    expect_value: LiteralValue,
    expect_edges: &[u32],
) {
    let collector = LiveEdgeCollector::new(members);
    let v = eval_as_member(engine, &collector, 0, "Sheet1", members[0], formula);
    let got: FxHashSet<(u32, u32)> = collector.take_edges();
    let want: FxHashSet<(u32, u32)> = expect_edges.iter().map(|t| (0u32, *t)).collect();
    assert_eq!(v, expect_value, "value for {formula}");
    assert_eq!(got, want, "recorded live edges for {formula}");
}

#[test]
fn span_reversed_endpoints_record_the_normalised_window() {
    let (e, m) = fixture();
    case(
        &e,
        &m,
        "=SUM(Acct3:Acct1!B1)",
        LiteralValue::Number(7.0),
        &[1, 2, 3],
    );
}

#[test]
fn span_single_sheet_x_colon_x_records_one_sheet() {
    let (e, m) = fixture();
    case(
        &e,
        &m,
        "=SUM(Acct2:Acct2!B1)",
        LiteralValue::Number(2.0),
        &[2],
    );
}

#[test]
fn span_case_mismatched_endpoints_record_the_same_window() {
    let (e, m) = fixture();
    case(
        &e,
        &m,
        "=SUM(acct1:ACCT3!B1)",
        LiteralValue::Number(7.0),
        &[1, 2, 3],
    );
}

#[test]
fn span_with_a_missing_endpoint_records_nothing_and_does_not_panic() {
    let (e, m) = fixture();
    let collector = LiveEdgeCollector::new(&m);
    let v = eval_as_member(
        &e,
        &collector,
        0,
        "Sheet1",
        m[0],
        "=SUM(Acct1:NoSuchTab!B1)",
    );
    assert!(
        matches!(v, LiteralValue::Error(_)),
        "missing endpoint must be an error, got {v:?}"
    );
    assert!(
        collector.take_edges().is_empty(),
        "a #REF! span must record no partial member window"
    );
}

#[test]
fn range3d_records_only_members_inside_the_rect() {
    let (e, m) = fixture();
    // B1:B2 over the three tabs: Acct2!B2 is inside the rect but is not a
    // member, and must not appear as an edge.
    case(
        &e,
        &m,
        "=SUM(Acct1:Acct3!B1:B2)",
        LiteralValue::Number(23.0),
        &[1, 2, 3],
    );
}

#[test]
fn range3d_unbounded_column_records_the_engine_normalised_extent() {
    let (e, m) = fixture();
    // Whole column B over the three tabs: Acct1!B5 (member 4) is inside the
    // used extent and must be recorded.
    case(
        &e,
        &m,
        "=SUM(Acct1:Acct3!B:B)",
        LiteralValue::Number(31.0),
        &[1, 2, 3, 4],
    );
}

#[test]
fn cell3d_span_stopping_before_a_later_sheet_excludes_it() {
    let (e, m) = fixture();
    // Zed is registered after Acct3 and must be outside Acct1:Acct3.
    case(
        &e,
        &m,
        "=SUM(Acct1:Acct3!B1)",
        LiteralValue::Number(7.0),
        &[1, 2, 3],
    );
}

/* ───── bounded Range3D shortcut edge cases (GOD-246 lever 1, review F3) ───── */

/// Record a hand-built `Range3D` reference through `RecordingContext` with the
/// bounded shortcut forced on (`1`) and forced off (`2`), and return the sorted
/// `(from, to, selected, mechanisms)` tuples for each.
fn shortcut_vs_engine(
    engine: &Engine<TestWorkbook>,
    members: &[CellRef],
    reference: &formualizer_parse::parser::ReferenceType,
) -> (Vec<(u32, u32, bool, u8)>, Vec<(u32, u32, bool, u8)>) {
    use crate::traits::EvaluationContext;

    let run = |mode: u8| {
        let collector = LiveEdgeCollector::new(members);
        collector.set_range3d_shortcut_mode(mode);
        collector.set_current(0);
        {
            let ctx = RecordingContext::new(engine, &collector);
            let _ = ctx.resolve_range_view(reference, "Sheet1");
        }
        let mut out: Vec<(u32, u32, bool, u8)> = collector
            .take_edge_records()
            .into_iter()
            .map(|e| (e.from, e.to, e.selected, e.mechanisms))
            .collect();
        out.sort_unstable();
        out
    };
    (run(1), run(2))
}

fn range3d(
    first: &str,
    last: &str,
    start_row: Option<u32>,
    start_col: Option<u32>,
    end_row: Option<u32>,
    end_col: Option<u32>,
) -> formualizer_parse::parser::ReferenceType {
    formualizer_parse::parser::ReferenceType::Range3D {
        sheet_first: first.to_string(),
        sheet_last: last.to_string(),
        start_row,
        start_col,
        end_row,
        end_col,
        start_row_abs: true,
        start_col_abs: true,
        end_row_abs: true,
        end_col_abs: true,
    }
}

/// F3(a): a literal `0` coordinate. `Engine::resolve_shared_ref` converts
/// 1-based to 0-based with `checked_sub(1)` and returns `#REF!`, so the engine
/// path records nothing; the shortcut must not underflow `u32` (a debug-build
/// panic) and must record nothing too.
#[test]
fn range3d_shortcut_with_a_zero_coordinate_records_nothing_on_either_branch() {
    let (e, m) = fixture();
    for reference in [
        range3d("Acct1", "Acct3", Some(0), Some(2), Some(1), Some(2)),
        range3d("Acct1", "Acct3", Some(1), Some(0), Some(1), Some(2)),
        range3d("Acct1", "Acct3", Some(1), Some(2), Some(0), Some(2)),
        range3d("Acct1", "Acct3", Some(1), Some(2), Some(1), Some(0)),
    ] {
        let (shortcut, engine_path) = shortcut_vs_engine(&e, &m, &reference);
        assert_eq!(shortcut, engine_path, "branch mismatch for {reference:?}");
        assert!(
            shortcut.is_empty(),
            "a zero coordinate records nothing: {reference:?}"
        );
    }
}

/// F3(a), other half: reversed endpoints. `SheetRangeRef::from_parts` rejects
/// them with `RangeOrder`, so the engine path records nothing.
#[test]
fn range3d_shortcut_with_reversed_endpoints_matches_the_engine_path() {
    let (e, m) = fixture();
    for reference in [
        range3d("Acct1", "Acct3", Some(5), Some(2), Some(1), Some(2)),
        range3d("Acct1", "Acct3", Some(1), Some(4), Some(1), Some(2)),
    ] {
        let (shortcut, engine_path) = shortcut_vs_engine(&e, &m, &reference);
        assert_eq!(shortcut, engine_path, "branch mismatch for {reference:?}");
        assert!(shortcut.is_empty(), "reversed rect records nothing");
    }
}

/// F3(b) + the ordinary case: a bounded member rect records the identical edge
/// set, with identical `selected`/`mechanisms` flags, on both branches --
/// including when the span endpoints differ in case from the registered sheet
/// names, which is the path the sheet-id round trip has to survive.
#[test]
fn range3d_shortcut_records_the_same_edges_as_the_engine_path() {
    let (e, m) = fixture();
    for reference in [
        range3d("Acct1", "Acct3", Some(1), Some(2), Some(1), Some(2)),
        range3d("Acct1", "Acct3", Some(1), Some(2), Some(2), Some(2)),
        range3d("Acct1", "Acct3", Some(1), Some(1), Some(5), Some(4)),
        range3d("acct1", "ACCT3", Some(1), Some(2), Some(5), Some(2)),
        // A rect that lands on no member at all.
        range3d("Acct1", "Acct3", Some(20), Some(20), Some(21), Some(21)),
    ] {
        let (shortcut, engine_path) = shortcut_vs_engine(&e, &m, &reference);
        assert_eq!(shortcut, engine_path, "branch mismatch for {reference:?}");
    }
    // And the assertion is not vacuous: the B1:B5 span really does record the
    // three B1 members plus Acct1!B5.
    let (shortcut, _) = shortcut_vs_engine(
        &e,
        &m,
        &range3d("Acct1", "Acct3", Some(1), Some(2), Some(5), Some(2)),
    );
    assert_eq!(
        shortcut.iter().map(|t| t.1).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
}

/// F3(c): a member sheet registered in the graph but absent from the Arrow
/// sheet store. The engine returns an empty owned view and `record_view` skips
/// it, so the shortcut must record nothing for that member either -- even
/// though a collector member sits inside its rect.
#[test]
fn range3d_shortcut_on_a_sheet_with_no_arrow_store_records_nothing() {
    let (mut e, mut m) = fixture();
    // `Engine::add_sheet` would call `ensure_arrow_sheet`; going through the
    // graph directly registers the sheet without an Arrow sheet behind it.
    let ghost = e.graph.add_sheet("Ghost").expect("register sheet");
    assert!(
        e.sheet_store().sheet("Ghost").is_none(),
        "fixture precondition: Ghost has no Arrow sheet"
    );
    // A member cell on the ghost sheet, inside the rect the span would read.
    m.push(CellRef::new(ghost, Coord::from_excel(1, 2, true, true)));

    let reference = range3d("Ghost", "Ghost", Some(1), Some(2), Some(1), Some(2));
    let (shortcut, engine_path) = shortcut_vs_engine(&e, &m, &reference);
    assert_eq!(shortcut, engine_path, "branch mismatch for {reference:?}");
    assert!(
        shortcut.is_empty(),
        "a member sheet with no Arrow store records nothing on either branch"
    );
}

/// The shortcut must not be taken when any axis is unbounded: those still need
/// the engine's used-region normalisation.
#[test]
fn range3d_unbounded_axes_still_agree_between_branches() {
    let (e, m) = fixture();
    for reference in [
        range3d("Acct1", "Acct3", None, Some(2), None, Some(2)),
        range3d("Acct1", "Acct3", Some(1), None, Some(1), None),
        range3d("Acct1", "Acct3", Some(1), Some(2), None, Some(2)),
    ] {
        let (shortcut, engine_path) = shortcut_vs_engine(&e, &m, &reference);
        assert_eq!(shortcut, engine_path, "branch mismatch for {reference:?}");
    }
    // Non-vacuous: the whole-column form records the used extent, which picks
    // up Acct1!B5 (member 4).
    let (shortcut, _) = shortcut_vs_engine(
        &e,
        &m,
        &range3d("Acct1", "Acct3", None, Some(2), None, Some(2)),
    );
    assert_eq!(
        shortcut.iter().map(|t| t.1).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
}

#[test]
fn settle_trace_span_reader_marker_is_opt_in_and_drains_once() {
    let (engine, members) = fixture();
    let enabled = LiveEdgeCollector::new_with_names_and_diagnostics_and_settle_trace(
        &members,
        &[],
        false,
        true,
    );
    let _ = eval_as_member(
        &engine,
        &enabled,
        0,
        "Sheet1",
        members[0],
        "=SUM(Acct1:Acct3!B1)",
    );
    let readers = enabled.take_three_dimensional_readers();
    assert_eq!(readers.len(), 1);
    assert!(readers.contains(&0));
    assert!(
        enabled.take_three_dimensional_readers().is_empty(),
        "the reader generation drains with the pass recordings"
    );

    let disabled = LiveEdgeCollector::new(&members);
    let _ = eval_as_member(
        &engine,
        &disabled,
        0,
        "Sheet1",
        members[0],
        "=SUM(Acct1:Acct3!B1)",
    );
    assert!(
        disabled.take_three_dimensional_readers().is_empty(),
        "the disabled diagnostic must not retain reader state"
    );
}
