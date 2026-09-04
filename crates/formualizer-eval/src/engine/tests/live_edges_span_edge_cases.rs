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
