//! Tests for the Stage-1 live-edge collector (pre-work for RFC #112).
//!
//! These drive a real `Engine` wrapped in `RecordingContext`, evaluating
//! formula ASTs via `Interpreter` directly — exactly how Stage-2 SCC tasks
//! will evaluate statically-cyclic members. Nothing here touches production
//! evaluation paths: `RecordingContext` is constructed only by this test
//! module, and no public `Engine` API exposes or stores a collector.

use crate::engine::live_edges::{LiveEdgeCollector, RecordingContext};
use crate::engine::named_range::{NameScope, NamedDefinition};
use crate::engine::{Engine, EvalConfig};
use crate::interpreter::Interpreter;
use crate::reference::{CellRef, Coord, RangeRef};
use crate::test_workbook::TestWorkbook;
use formualizer_common::LiteralValue;
use rustc_hash::FxHashSet;

fn parse(formula: &str) -> formualizer_parse::parser::ASTNode {
    formualizer_parse::parser::parse(formula).expect("valid formula")
}

fn new_engine() -> Engine<TestWorkbook> {
    Engine::new(TestWorkbook::new(), EvalConfig::default())
}

fn cell(engine: &Engine<TestWorkbook>, sheet: &str, row: u32, col: u32) -> CellRef {
    CellRef::new(
        engine.sheet_id(sheet).expect("sheet exists"),
        Coord::from_excel(row, col, true, true),
    )
}

/// Evaluate `formula` as if it were the body of `member` (an SCC member at
/// `member_idx`), recording live edges into `collector`.
fn eval_as_member(
    engine: &Engine<TestWorkbook>,
    collector: &LiveEdgeCollector,
    member_idx: u32,
    sheet: &str,
    member: CellRef,
    formula: &str,
) -> LiteralValue {
    collector.set_current(member_idx);
    let ctx = RecordingContext::new(engine, collector);
    let interp = Interpreter::new_with_cell(&ctx, sheet, member);
    interp
        .evaluate_ast(&parse(formula))
        .map(|cv| cv.into_literal())
        .unwrap_or_else(LiteralValue::Error)
}

fn edges(collector: &LiveEdgeCollector) -> FxHashSet<(u32, u32)> {
    collector.take_edges()
}

fn set_num(engine: &mut Engine<TestWorkbook>, sheet: &str, row: u32, col: u32, v: f64) {
    engine
        .set_cell_value(sheet, row, col, LiteralValue::Number(v))
        .unwrap();
}

/* ─────────────────────────── 1. scalar reads ─────────────────────────── */

#[test]
fn scalar_read_of_member_records_edge() {
    let mut engine = new_engine();
    set_num(&mut engine, "Sheet1", 1, 1, 5.0); // A1 (member)
    set_num(&mut engine, "Sheet1", 1, 2, 7.0); // B1 (non-member)
    engine.evaluate_all().unwrap();

    // Members: [C1 (the evaluating member), A1].
    let c1 = cell(&engine, "Sheet1", 1, 3);
    let a1 = cell(&engine, "Sheet1", 1, 1);
    let collector = LiveEdgeCollector::new(&[c1, a1]);

    let v = eval_as_member(&engine, &collector, 0, "Sheet1", c1, "=A1");
    assert_eq!(v, LiteralValue::Number(5.0));
    assert_eq!(edges(&collector), FxHashSet::from_iter([(0, 1)]));

    // Reading a non-member records nothing.
    let v = eval_as_member(&engine, &collector, 0, "Sheet1", c1, "=B1");
    assert_eq!(v, LiteralValue::Number(7.0));
    assert!(edges(&collector).is_empty());
}

/* ──────────────────────── 2. short-circuiting ────────────────────────── */

/// For each (formula_no_edge, formula_edge) pair: the member read sits in a
/// branch that is untaken in the first formula and taken in the second.
fn assert_short_circuit_polarity(no_edge: &str, edge_expected: &str) {
    let mut engine = new_engine();
    set_num(&mut engine, "Sheet1", 1, 1, 5.0); // A1 (member)
    engine.evaluate_all().unwrap();

    let c1 = cell(&engine, "Sheet1", 1, 3);
    let a1 = cell(&engine, "Sheet1", 1, 1);
    let collector = LiveEdgeCollector::new(&[c1, a1]);

    eval_as_member(&engine, &collector, 0, "Sheet1", c1, no_edge);
    assert!(
        edges(&collector).is_empty(),
        "{no_edge}: untaken branch must record no live edge"
    );

    eval_as_member(&engine, &collector, 0, "Sheet1", c1, edge_expected);
    assert_eq!(
        edges(&collector),
        FxHashSet::from_iter([(0, 1)]),
        "{edge_expected}: taken branch must record the live edge"
    );
}

#[test]
fn if_short_circuit_polarity() {
    assert_short_circuit_polarity("=IF(TRUE, 1, A1)", "=IF(FALSE, 1, A1)");
}

#[test]
fn ifs_short_circuit_polarity() {
    assert_short_circuit_polarity("=IFS(TRUE, 1, TRUE, A1)", "=IFS(FALSE, 1, TRUE, A1)");
}

#[test]
fn choose_short_circuit_polarity() {
    assert_short_circuit_polarity("=CHOOSE(1, 9, A1)", "=CHOOSE(2, 9, A1)");
}

#[test]
fn switch_short_circuit_polarity() {
    // First: case 1 matches, default (A1) untaken. Second: default taken.
    assert_short_circuit_polarity("=SWITCH(1, 1, 9, A1)", "=SWITCH(2, 1, 9, A1)");
}

/* ─────────────────────────── 3. range reads ──────────────────────────── */

#[test]
fn range_read_intersects_members_exactly() {
    let mut engine = new_engine();
    for r in 1..=10 {
        set_num(&mut engine, "Sheet1", r, 2, r as f64); // B1:B10
        set_num(&mut engine, "Sheet1", r, 3, r as f64); // C1:C10
    }
    engine.evaluate_all().unwrap();

    let d1 = cell(&engine, "Sheet1", 1, 4);
    let b3 = cell(&engine, "Sheet1", 3, 2);
    let b7 = cell(&engine, "Sheet1", 7, 2);
    let collector = LiveEdgeCollector::new(&[d1, b3, b7]);

    let v = eval_as_member(&engine, &collector, 0, "Sheet1", d1, "=SUM(B1:B10)");
    assert_eq!(v, LiteralValue::Number(55.0));
    assert_eq!(edges(&collector), FxHashSet::from_iter([(0, 1), (0, 2)]));

    // A rect not containing any member records nothing.
    eval_as_member(&engine, &collector, 0, "Sheet1", d1, "=SUM(C1:C10)");
    assert!(edges(&collector).is_empty());

    // Rect adjacent to a member (B4:B6 vs members B3/B7) records nothing.
    eval_as_member(&engine, &collector, 0, "Sheet1", d1, "=SUM(B4:B6)");
    assert!(edges(&collector).is_empty());
}

#[test]
fn range_read_boundary_inclusion_at_rect_corners() {
    let mut engine = new_engine();
    for r in 1..=10 {
        set_num(&mut engine, "Sheet1", r, 2, 1.0); // B1:B10
    }
    engine.evaluate_all().unwrap();

    let d1 = cell(&engine, "Sheet1", 1, 4);
    let b1 = cell(&engine, "Sheet1", 1, 2); // top corner of B1:B10
    let b10 = cell(&engine, "Sheet1", 10, 2); // bottom corner of B1:B10
    let collector = LiveEdgeCollector::new(&[d1, b1, b10]);

    eval_as_member(&engine, &collector, 0, "Sheet1", d1, "=SUM(B1:B10)");
    assert_eq!(edges(&collector), FxHashSet::from_iter([(0, 1), (0, 2)]));

    // One row inside: B2:B9 excludes both corner members.
    eval_as_member(&engine, &collector, 0, "Sheet1", d1, "=SUM(B2:B9)");
    assert!(edges(&collector).is_empty());
}

/* ─────────────────── 4. whole-column (unbounded) reads ───────────────── */

#[test]
fn whole_column_read_resolves_used_bounds_and_records_member() {
    let mut engine = new_engine();
    for r in 1..=10 {
        set_num(&mut engine, "Sheet1", r, 2, r as f64); // B1:B10
    }
    engine.evaluate_all().unwrap();

    let d1 = cell(&engine, "Sheet1", 1, 4);
    let b5 = cell(&engine, "Sheet1", 5, 2);
    let a5 = cell(&engine, "Sheet1", 5, 1); // not in column B
    let collector = LiveEdgeCollector::new(&[d1, b5, a5]);

    let v = eval_as_member(&engine, &collector, 0, "Sheet1", d1, "=SUM(B:B)");
    assert_eq!(v, LiteralValue::Number(55.0));
    assert_eq!(edges(&collector), FxHashSet::from_iter([(0, 1)]));
}

/* ───────────────────────── 5. named ranges ───────────────────────────── */

#[test]
fn named_range_region_containing_member_records_edge() {
    let mut engine = new_engine();
    for r in 1..=5 {
        set_num(&mut engine, "Sheet1", r, 2, r as f64); // B1:B5
    }
    engine.evaluate_all().unwrap();

    let sheet_id = engine.sheet_id("Sheet1").unwrap();
    let nr_range = RangeRef::new(
        CellRef::new(sheet_id, Coord::from_excel(2, 2, true, true)), // B2
        CellRef::new(sheet_id, Coord::from_excel(4, 2, true, true)), // B4
    );
    engine
        .define_name("NR", NamedDefinition::Range(nr_range), NameScope::Workbook)
        .unwrap();

    let d1 = cell(&engine, "Sheet1", 1, 4);
    let b3 = cell(&engine, "Sheet1", 3, 2); // inside NR
    let b5 = cell(&engine, "Sheet1", 5, 2); // outside NR
    let collector = LiveEdgeCollector::new(&[d1, b3, b5]);

    let v = eval_as_member(&engine, &collector, 0, "Sheet1", d1, "=SUM(NR)");
    assert_eq!(v, LiteralValue::Number(9.0));
    assert_eq!(edges(&collector), FxHashSet::from_iter([(0, 1)]));
}

#[test]
fn table_column_region_containing_member_records_edge() {
    let mut engine = new_engine();
    // Table T over A1:B3: header row + 2 data rows.
    set_num(&mut engine, "Sheet1", 2, 1, 5.0);
    set_num(&mut engine, "Sheet1", 2, 2, 10.0);
    set_num(&mut engine, "Sheet1", 3, 1, 7.0);
    set_num(&mut engine, "Sheet1", 3, 2, 20.0);
    engine.evaluate_all().unwrap();

    let sheet_id = engine.sheet_id("Sheet1").unwrap();
    let range = RangeRef::new(
        CellRef::new(sheet_id, Coord::from_excel(1, 1, true, true)),
        CellRef::new(sheet_id, Coord::from_excel(3, 2, true, true)),
    );
    engine
        .define_table(
            "Sales",
            range,
            true,
            vec!["Region".into(), "Amount".into()],
            false,
        )
        .unwrap();

    let d1 = cell(&engine, "Sheet1", 1, 4);
    let b2 = cell(&engine, "Sheet1", 2, 2); // inside Sales[Amount]
    let a2 = cell(&engine, "Sheet1", 2, 1); // in Sales[Region], not [Amount]
    let collector = LiveEdgeCollector::new(&[d1, b2, a2]);

    let v = eval_as_member(&engine, &collector, 0, "Sheet1", d1, "=SUM(Sales[Amount])");
    assert_eq!(v, LiteralValue::Number(30.0));
    assert_eq!(edges(&collector), FxHashSet::from_iter([(0, 1)]));
}

/* ──────────────────── 6. dynamic reads (INDIRECT) ─────────────────────── */

#[test]
fn indirect_scalar_read_flows_through_wrapper() {
    let mut engine = new_engine();
    set_num(&mut engine, "Sheet1", 3, 2, 42.0); // B3 (member)
    engine.evaluate_all().unwrap();

    let d1 = cell(&engine, "Sheet1", 1, 4);
    let b3 = cell(&engine, "Sheet1", 3, 2);
    let collector = LiveEdgeCollector::new(&[d1, b3]);

    let v = eval_as_member(&engine, &collector, 0, "Sheet1", d1, "=INDIRECT(\"B3\")");
    assert_eq!(v, LiteralValue::Number(42.0));
    assert_eq!(edges(&collector), FxHashSet::from_iter([(0, 1)]));

    // Dynamic *range* read: the rect is recorded at resolution time.
    let v = eval_as_member(
        &engine,
        &collector,
        0,
        "Sheet1",
        d1,
        "=SUM(INDIRECT(\"B1:B5\"))",
    );
    assert_eq!(v, LiteralValue::Number(42.0));
    assert_eq!(edges(&collector), FxHashSet::from_iter([(0, 1)]));

    // Dynamic read of a non-member records nothing.
    eval_as_member(&engine, &collector, 0, "Sheet1", d1, "=INDIRECT(\"C3\")");
    assert!(edges(&collector).is_empty());
}

/* ─────────────────────────── 8. attribution ──────────────────────────── */

#[test]
fn edges_attribute_to_current_member() {
    let mut engine = new_engine();
    set_num(&mut engine, "Sheet1", 1, 1, 1.0); // A1
    set_num(&mut engine, "Sheet1", 2, 1, 2.0); // A2
    engine.evaluate_all().unwrap();

    let a1 = cell(&engine, "Sheet1", 1, 1);
    let a2 = cell(&engine, "Sheet1", 2, 1);
    let collector = LiveEdgeCollector::new(&[a1, a2]);

    // A1's formula reads A2; A2's formula reads A1 (a 2-cycle).
    eval_as_member(&engine, &collector, 0, "Sheet1", a1, "=A2");
    eval_as_member(&engine, &collector, 1, "Sheet1", a2, "=A1");
    assert_eq!(edges(&collector), FxHashSet::from_iter([(0, 1), (1, 0)]));
}

/* ─────────────────────────── 9. self-edges ───────────────────────────── */

#[test]
fn member_ranging_over_itself_records_self_edge() {
    let mut engine = new_engine();
    for r in 1..=3 {
        set_num(&mut engine, "Sheet1", r, 2, r as f64); // B1:B3
    }
    engine.evaluate_all().unwrap();

    let b2 = cell(&engine, "Sheet1", 2, 2);
    let collector = LiveEdgeCollector::new(&[b2]);

    // B2's formula ranges over B1:B3, which includes B2 itself.
    eval_as_member(&engine, &collector, 0, "Sheet1", b2, "=SUM(B1:B3)");
    assert_eq!(edges(&collector), FxHashSet::from_iter([(0, 0)]));
}

/* ─────────────────────────── 10. multi-sheet ─────────────────────────── */

#[test]
fn cross_sheet_qualified_reads_record_member_on_other_sheet() {
    let mut engine = new_engine();
    engine.add_sheet("Sheet2").unwrap();
    set_num(&mut engine, "Sheet1", 1, 1, 1.0); // Sheet1!A1
    set_num(&mut engine, "Sheet2", 1, 1, 9.0); // Sheet2!A1 (member)
    set_num(&mut engine, "Sheet2", 2, 1, 8.0); // Sheet2!A2
    engine.evaluate_all().unwrap();

    let s1_c1 = cell(&engine, "Sheet1", 1, 3);
    let s2_a1 = cell(&engine, "Sheet2", 1, 1);
    let collector = LiveEdgeCollector::new(&[s1_c1, s2_a1]);

    // Scalar qualified read from Sheet1.
    let v = eval_as_member(&engine, &collector, 0, "Sheet1", s1_c1, "=Sheet2!A1");
    assert_eq!(v, LiteralValue::Number(9.0));
    assert_eq!(edges(&collector), FxHashSet::from_iter([(0, 1)]));

    // Same coordinates on the *current* sheet must NOT match the member.
    eval_as_member(&engine, &collector, 0, "Sheet1", s1_c1, "=A1");
    assert!(edges(&collector).is_empty());

    // Qualified range read.
    let v = eval_as_member(
        &engine,
        &collector,
        0,
        "Sheet1",
        s1_c1,
        "=SUM(Sheet2!A1:A2)",
    );
    assert_eq!(v, LiteralValue::Number(17.0));
    assert_eq!(edges(&collector), FxHashSet::from_iter([(0, 1)]));
}

/* ──────────────────────────── 11. inertness ──────────────────────────── */

/// Structural inertness: the acyclic/hot path never constructs a
/// `RecordingContext` — no `Engine` API creates, stores, or exposes one (the
/// only constructor takes an externally-owned collector), so production
/// evaluation is untouched by this module. This test pins the cheap proxies
/// for that argument: the wrapper is two borrowed pointers (no owned state to
/// allocate), and wrapped evaluation is value-identical to bare evaluation.
#[test]
fn wrapper_is_inert_and_value_transparent() {
    assert_eq!(
        std::mem::size_of::<RecordingContext<'_, TestWorkbook>>(),
        2 * std::mem::size_of::<usize>(),
        "RecordingContext must stay two borrowed pointers"
    );

    let mut engine = new_engine();
    for r in 1..=10 {
        set_num(&mut engine, "Sheet1", r, 2, r as f64); // B1:B10
    }
    set_num(&mut engine, "Sheet1", 1, 1, 100.0); // A1
    engine.evaluate_all().unwrap();

    let d1 = cell(&engine, "Sheet1", 1, 4);
    let formula = "=SUM(B1:B10)+A1*2";

    // Bare engine context (production shape).
    let bare = {
        let interp = Interpreter::new_with_cell(&engine, "Sheet1", d1);
        interp.evaluate_ast(&parse(formula)).unwrap().into_literal()
    };

    // Wrapped with an empty membership: identical value, zero edges.
    let collector = LiveEdgeCollector::new(&[]);
    let ctx = RecordingContext::new(&engine, &collector);
    let wrapped = {
        let interp = Interpreter::new_with_cell(&ctx, "Sheet1", d1);
        interp.evaluate_ast(&parse(formula)).unwrap().into_literal()
    };

    assert_eq!(bare, wrapped);
    assert!(collector.take_edges().is_empty());
}

/// Reads observed before `set_current` is called are not attributable to any
/// member and must be dropped rather than mis-attributed.
#[test]
fn reads_without_current_member_are_dropped() {
    let mut engine = new_engine();
    set_num(&mut engine, "Sheet1", 1, 1, 5.0); // A1 (member)
    engine.evaluate_all().unwrap();

    let a1 = cell(&engine, "Sheet1", 1, 1);
    let collector = LiveEdgeCollector::new(&[a1]);
    let ctx = RecordingContext::new(&engine, &collector);
    let d1 = cell(&engine, "Sheet1", 1, 4);
    let interp = Interpreter::new_with_cell(&ctx, "Sheet1", d1);
    let _ = interp.evaluate_ast(&parse("=A1"));
    assert!(collector.take_edges().is_empty());
}

/// A 3-D span read records a live edge to every member cell it consumes.
///
/// GOD-242 / CL-051: the engine's `Cell3D` / `Range3D` arms resolve each member
/// sheet themselves and return the concatenation as an owned `"__tmp"` view,
/// which carries no registered `SheetId`; the recorder therefore used to record
/// nothing at all for a span site.  Inside an SCC that makes the site's live
/// in-edge set empty, so the settle loop never re-evaluates it when a member's
/// value moves and the site latches a mid-settle transient sum.
#[test]
fn three_dimensional_span_inside_static_scc_records_live_member_edges() {
    let mut engine = new_engine();
    for sheet in ["Acct1", "Acct2", "Acct3"] {
        engine.add_sheet(sheet).unwrap();
    }
    set_num(&mut engine, "Acct1", 1, 2, 1.0);
    set_num(&mut engine, "Acct2", 1, 2, 2.0);
    set_num(&mut engine, "Acct3", 1, 2, 4.0);
    engine.evaluate_all().unwrap();

    let site = cell(&engine, "Sheet1", 1, 1);
    let members = [
        site,
        cell(&engine, "Acct1", 1, 2),
        cell(&engine, "Acct2", 1, 2),
        cell(&engine, "Acct3", 1, 2),
    ];
    let collector = LiveEdgeCollector::new(&members);
    let all_members = FxHashSet::from_iter([(0, 1), (0, 2), (0, 3)]);

    // Cell3D: one cell per member sheet.
    let v = eval_as_member(
        &engine,
        &collector,
        0,
        "Sheet1",
        site,
        "=SUM(Acct1:Acct3!B1)",
    );
    assert_eq!(v, LiteralValue::Number(7.0));
    assert_eq!(edges(&collector), all_members);

    // Range3D: one rect per member sheet.
    let v = eval_as_member(
        &engine,
        &collector,
        0,
        "Sheet1",
        site,
        "=SUM(Acct1:Acct3!B1:B2)",
    );
    assert_eq!(v, LiteralValue::Number(7.0));
    assert_eq!(edges(&collector), all_members);

    // The span window is the registration-order window between the endpoints
    // (ES-010 / ES-047); a narrower span records only the sheets inside it.
    let v = eval_as_member(
        &engine,
        &collector,
        0,
        "Sheet1",
        site,
        "=SUM(Acct1:Acct2!B1)",
    );
    assert_eq!(v, LiteralValue::Number(3.0));
    assert_eq!(edges(&collector), FxHashSet::from_iter([(0, 1), (0, 2)]));
}

/* ─────────── record_rect branch parity (GOD-242 / CL-051) ─────────── */

/// `record_rect` intersects the rect with the SCC membership on whichever
/// side is smaller: it probes `index` per cell for a rect smaller than the
/// membership, and scans `members` otherwise. Both branches must record the
/// identical edge set with identical `selected`/`mechanisms` flags — including
/// the `EDGE_NON_IF_LAZY` classification carried by `pending_rects` — so that
/// the cost fix cannot move an edge.
#[test]
fn record_rect_small_rect_and_member_scan_record_identical_edges() {
    let mut engine = new_engine();
    engine.add_sheet("Other").unwrap();
    for row in 1..=6u32 {
        for col in 1..=4u32 {
            set_num(&mut engine, "Sheet1", row, col, (row * 10 + col) as f64);
        }
    }
    engine.evaluate_all().unwrap();

    // Membership: a block on Sheet1 plus one off-sheet member that no rect on
    // Sheet1 may ever record.
    let mut members: Vec<CellRef> = Vec::new();
    for row in 1..=6u32 {
        for col in 1..=4u32 {
            members.push(cell(&engine, "Sheet1", row, col));
        }
    }
    members.push(cell(&engine, "Other", 2, 2));
    let sheet1 = engine.sheet_id("Sheet1").expect("sheet exists");

    let records = |by_index: bool, lazy_rect: bool, rect: (u32, u32, u32, u32)| {
        let collector = LiveEdgeCollector::new(&members);
        collector.set_current(0);
        if lazy_rect {
            // A pending non-IF-lazy scope covering part of the rect, so the
            // two branches have to agree on the per-cell EDGE_NON_IF_LAZY bit
            // and not merely on the edge set.
            collector.begin_selected_non_if_lazy_arm();
            collector.record_selected_non_if_lazy_rect(sheet1, 0, 0, 2, 1, true);
        }
        collector.record_rect_forced_branch(by_index, sheet1, rect.0, rect.1, rect.2, rect.3);
        let mut out: Vec<(u32, u32, bool, u8)> = collector
            .take_edge_records()
            .into_iter()
            .map(|e| (e.from, e.to, e.selected, e.mechanisms))
            .collect();
        out.sort_unstable();
        out
    };

    // Rects covering: a single cell, a one-row strip (the 3-D span shape), a
    // block straddling the lazy scope, the whole membership, and a rect that
    // runs off the membership on both axes.
    for rect in [
        (0u32, 0u32, 0u32, 0u32),
        (2, 0, 2, 3),
        (0, 0, 3, 2),
        (0, 0, 5, 3),
        (4, 2, 9, 9),
        (7, 7, 8, 8),
    ] {
        for lazy_rect in [false, true] {
            let by_index = records(true, lazy_rect, rect);
            let by_scan = records(false, lazy_rect, rect);
            assert_eq!(
                by_index, by_scan,
                "branch mismatch for rect {rect:?} (lazy_rect={lazy_rect})"
            );
        }
    }

    // And the branches are not vacuously equal: the one-row strip really does
    // record its four members.
    assert_eq!(records(true, false, (2, 0, 2, 3)).len(), 4);
}

/* ────────────── FZ_EDGE_HASH determinism (GOD-246 commit D) ────────────── */

/// The `FZ_EDGE_HASH` receipt is a *receipt*: two evaluations of the same SCC
/// over the same fixture must produce the same member basis and the same
/// per-pass edge chain, and adding one live edge must change the chain.
///
/// Driven through `evaluate_scc_unit` directly (the same door
/// `scc_runtime_cycles.rs` uses) so the hashed vector is the production one,
/// and read back through the thread-local test hook rather than by parsing
/// stderr — the env var's `OnceLock` cannot be re-read once another test in
/// the process has initialised it.
#[test]
fn fz_edge_hash_is_deterministic_and_moves_when_an_edge_is_added() {
    use crate::engine::eval::edge_hash_test_hook;

    fn fixture() -> (Engine<TestWorkbook>, Vec<crate::engine::vertex::VertexId>) {
        let mut engine = new_engine();
        for sheet in ["Acct1", "Acct2", "Acct3"] {
            engine.add_sheet(sheet).unwrap();
        }
        // Member cells must be FORMULA vertices: `evaluate_scc_unit` only
        // makes formula/name members recordable edge targets (plain value
        // cells land in the non-recordable `other` tail).
        engine.set_cell_formula("Acct1", 1, 2, parse("=1")).unwrap();
        engine.set_cell_formula("Acct2", 1, 2, parse("=2")).unwrap();
        engine.set_cell_formula("Acct3", 1, 2, parse("=4")).unwrap();
        engine.set_cell_formula("Acct1", 5, 2, parse("=8")).unwrap();
        engine
            .set_cell_formula("Sheet1", 1, 1, parse("=SUM(Acct1:Acct3!B1)"))
            .unwrap();
        engine.evaluate_all().unwrap();

        let addrs = [
            cell(&engine, "Sheet1", 1, 1),
            cell(&engine, "Acct1", 1, 2),
            cell(&engine, "Acct2", 1, 2),
            cell(&engine, "Acct3", 1, 2),
            cell(&engine, "Acct1", 5, 2),
        ];
        let ids = addrs
            .iter()
            .map(|a| {
                *engine
                    .graph
                    .get_vertex_id_for_address(a)
                    .expect("member vertex exists")
            })
            .collect();
        (engine, ids)
    }

    let summarise = |engine: &mut Engine<TestWorkbook>,
                     members: &[crate::engine::vertex::VertexId]| {
        let _ = edge_hash_test_hook::take();
        edge_hash_test_hook::set_forced(true);
        let result = engine.evaluate_scc_unit(members, None, None);
        edge_hash_test_hook::set_forced(false);
        result.expect("scc unit evaluates");
        let mut got = edge_hash_test_hook::take();
        assert_eq!(got.len(), 1, "exactly one SCC summary per evaluation");
        got.pop().unwrap()
    };

    // Two evaluations of the same fixture: identical receipt.
    let (mut engine, members) = fixture();
    let first = summarise(&mut engine, &members);
    let second = summarise(&mut engine, &members);
    assert_eq!(first.scc_len, 5);
    assert!(first.passes >= 1, "at least one classified pass");
    assert_ne!(first.edge_chain, 0);
    assert_eq!(
        first.member_basis, second.member_basis,
        "same member set must hash to the same basis"
    );
    assert_eq!(
        first.edge_chain, second.edge_chain,
        "same fixture must produce the same per-pass edge chain"
    );
    assert_eq!(first.distinct_edges, second.distinct_edges);
    assert_eq!(first.final_pass_hash, second.final_pass_hash);

    // A second, independently built copy of the same fixture agrees too, so
    // the hash does not depend on engine identity or allocation order.
    let (mut fresh, fresh_members) = fixture();
    let third = summarise(&mut fresh, &fresh_members);
    assert_eq!(third.member_basis, first.member_basis);
    assert_eq!(third.edge_chain, first.edge_chain);

    // Add one live edge (the site also reads member 4, Acct1!B5) — same member
    // set, same basis, different edge chain.
    let (mut widened, widened_members) = fixture();
    widened
        .set_cell_formula("Sheet1", 1, 1, parse("=SUM(Acct1:Acct3!B1)+Acct1!B5"))
        .unwrap();
    let wider = summarise(&mut widened, &widened_members);
    assert_eq!(
        wider.member_basis, first.member_basis,
        "member set is unchanged, so the basis must not move"
    );
    assert_eq!(
        wider.distinct_edges,
        first.distinct_edges + 1,
        "exactly one live edge was added"
    );
    assert_ne!(
        wider.edge_chain, first.edge_chain,
        "an added live edge must change the chain"
    );
}

/* ───── analyze_live_graph memo across settle passes (GOD-246 lever 2A) ───── */

/// Shared harness for the lever-2A memo tests: build a fixture, run one
/// `evaluate_scc_unit` with the memo either enabled or disabled, and return
/// the `FZ_EDGE_HASH` receipt for that run plus the settled member values.
///
/// The receipt is the edge-set witness: `edge_chain` folds every pass's
/// sorted, deduped edge vector in pass order, `final_pass_hash` /
/// `final_pass_edges` / `distinct_edges` pin the last pass and the union, and
/// `stale_chain` pins the stale re-evaluation order the reused `analysis`
/// could corrupt. (`evaluate_scc_unit` owns its collector, so a test cannot
/// call `take_edge_records()` on it directly; the chain is strictly stronger,
/// covering every pass rather than only the drained tail.)
fn run_scc_with_memo(
    memo_enabled: bool,
    build: impl Fn() -> (
        Engine<TestWorkbook>,
        Vec<crate::engine::vertex::VertexId>,
        Vec<(&'static str, u32, u32)>,
    ),
) -> (
    crate::engine::eval::EdgeHashSummary,
    Vec<Option<LiteralValue>>,
) {
    use crate::engine::eval::{edge_hash_test_hook, live_graph_memo_test_hook};

    let (mut engine, members, probes) = build();
    let _ = edge_hash_test_hook::take();
    edge_hash_test_hook::set_forced(true);
    live_graph_memo_test_hook::set_disabled(!memo_enabled);
    let result = engine.evaluate_scc_unit(&members, None, None);
    live_graph_memo_test_hook::set_disabled(false);
    edge_hash_test_hook::set_forced(false);
    result.expect("scc unit evaluates");

    let mut got = edge_hash_test_hook::take();
    assert_eq!(got.len(), 1, "exactly one SCC summary per evaluation");
    let summary = got.pop().unwrap();
    let values = probes
        .iter()
        .map(|&(sheet, row, col)| engine.get_cell_value(sheet, row, col))
        .collect();
    (summary, values)
}

/// Assert that the memo changed nothing observable: same values, same pass
/// count, same per-pass edge chain, same stale order.
fn assert_memo_is_observationally_transparent(
    on: &(
        crate::engine::eval::EdgeHashSummary,
        Vec<Option<LiteralValue>>,
    ),
    off: &(
        crate::engine::eval::EdgeHashSummary,
        Vec<Option<LiteralValue>>,
    ),
) {
    assert_eq!(on.1, off.1, "settled values must not depend on the memo");
    assert_eq!(on.0.passes, off.0.passes, "pass count must not move");
    assert_eq!(on.0.member_basis, off.0.member_basis);
    assert_eq!(
        on.0.edge_chain, off.0.edge_chain,
        "the per-pass recorded edge sets must be identical"
    );
    assert_eq!(on.0.final_pass_edges, off.0.final_pass_edges);
    assert_eq!(on.0.final_pass_hash, off.0.final_pass_hash);
    assert_eq!(on.0.distinct_edges, off.0.distinct_edges);
    assert_eq!(
        on.0.stale_chain, off.0.stale_chain,
        "the stale re-evaluation order must be identical"
    );
}

/// A reader chain whose members are visited before their dependencies: pass 1
/// reads stale values, the settle pass fixes them, and a third classified pass
/// confirms exactness. Every pass records the identical edge vector, so
/// `analyze_live_graph` must run exactly once.
fn static_edge_set_fixture() -> (
    Engine<TestWorkbook>,
    Vec<crate::engine::vertex::VertexId>,
    Vec<(&'static str, u32, u32)>,
) {
    let mut engine = new_engine();
    // Deliberately NOT `evaluate_all()`d: the members must still be stale when
    // `evaluate_scc_unit` runs, or there is no settle pass to memoise across.
    engine
        .set_cell_formula("Sheet1", 1, 1, parse("=Sheet1!A2+1"))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 2, 1, parse("=Sheet1!A3+1"))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 3, 1, parse("=Sheet1!A4+1"))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 4, 1, parse("=1"))
        .unwrap();
    let probes = vec![
        ("Sheet1", 1u32, 1u32),
        ("Sheet1", 2, 1),
        ("Sheet1", 3, 1),
        ("Sheet1", 4, 1),
    ];
    let ids = probes
        .iter()
        .map(|&(sheet, row, col)| {
            *engine
                .graph
                .get_vertex_id_for_address(&cell(&engine, sheet, row, col))
                .expect("member vertex exists")
        })
        .collect();
    (engine, ids, probes)
}

/// The same chain, but the first member's read set is decided by an `IF`
/// condition that is itself stale on pass 1: the taken arm — and therefore the
/// recorded edge vector — moves between pass 1 and the settle pass.
fn changing_edge_set_fixture() -> (
    Engine<TestWorkbook>,
    Vec<crate::engine::vertex::VertexId>,
    Vec<(&'static str, u32, u32)>,
) {
    let mut engine = new_engine();
    engine
        .set_cell_formula(
            "Sheet1",
            1,
            1,
            parse("=IF(Sheet1!A2>0,Sheet1!A3,Sheet1!A4)"),
        )
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 2, 1, parse("=1"))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 3, 1, parse("=7"))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 4, 1, parse("=9"))
        .unwrap();
    let probes = vec![
        ("Sheet1", 1u32, 1u32),
        ("Sheet1", 2, 1),
        ("Sheet1", 3, 1),
        ("Sheet1", 4, 1),
    ];
    let ids = probes
        .iter()
        .map(|&(sheet, row, col)| {
            *engine
                .graph
                .get_vertex_id_for_address(&cell(&engine, sheet, row, col))
                .expect("member vertex exists")
        })
        .collect();
    (engine, ids, probes)
}

/// Law: when the deduped live-edge vector is unchanged from the previous
/// settle pass, `analyze_live_graph` is not re-run, and reusing its result
/// changes nothing observable.
#[test]
fn unchanged_edge_set_across_settle_passes_classifies_once_and_preserves_results() {
    let on = run_scc_with_memo(true, static_edge_set_fixture);
    let off = run_scc_with_memo(false, static_edge_set_fixture);

    // Non-vacuous: the fixture really does settle over more than one pass and
    // really does record edges.
    assert!(
        on.0.passes >= 2,
        "fixture must settle over several passes, got {}",
        on.0.passes
    );
    assert_eq!(on.0.distinct_edges, 3, "the chain records three live edges");

    assert_eq!(
        on.0.classify_calls, 1,
        "an unchanged edge vector must be classified exactly once"
    );
    assert!(
        on.0.classify_skipped >= 1,
        "at least one pass must reuse the memoised analysis, got {}",
        on.0.classify_skipped
    );
    assert_eq!(
        off.0.classify_skipped, 0,
        "the disabled memo must never skip"
    );
    assert_eq!(
        off.0.classify_calls,
        on.0.classify_calls + on.0.classify_skipped,
        "memo-off classifies once per pass"
    );

    assert_memo_is_observationally_transparent(&on, &off);
    assert_eq!(
        on.1,
        vec![
            Some(LiteralValue::Number(4.0)),
            Some(LiteralValue::Number(3.0)),
            Some(LiteralValue::Number(2.0)),
            Some(LiteralValue::Number(1.0)),
        ],
        "the chain settles exactly"
    );
}

/// Law: when a lazy arm flips and the deduped live-edge vector changes,
/// `analyze_live_graph` is re-run for the new vector, and the run is identical
/// to the unmemoised one.
#[test]
fn changed_edge_set_across_settle_passes_reclassifies_and_preserves_results() {
    let on = run_scc_with_memo(true, changing_edge_set_fixture);
    let off = run_scc_with_memo(false, changing_edge_set_fixture);

    assert!(
        on.0.passes >= 2,
        "fixture must settle over several passes, got {}",
        on.0.passes
    );
    assert!(
        on.0.classify_calls >= 2,
        "a changed edge vector must force a reclassification, got {}",
        on.0.classify_calls
    );
    assert_eq!(
        off.0.classify_calls,
        on.0.classify_calls + on.0.classify_skipped,
        "memo-off classifies once per pass"
    );
    assert_eq!(off.0.classify_skipped, 0);

    // Non-vacuous: the arm really did flip — the union over passes holds both
    // arms' edges, while the final pass holds only the taken one.
    assert_eq!(on.0.distinct_edges, 3, "condition plus both arms");
    assert_eq!(on.0.final_pass_edges, 2, "condition plus the taken arm");

    assert_memo_is_observationally_transparent(&on, &off);
    assert_eq!(
        on.1[0],
        Some(LiteralValue::Number(7.0)),
        "the settled IF takes the true arm"
    );
}

/* ─────── bounded Range3D shortcut parity (GOD-246 lever 1) ─────── */

/// `record_rect_small_rect_and_member_scan_record_identical_edges` one level
/// up: a bounded `Range3D` read evaluated through a real formula records the
/// IDENTICAL edge set — including `selected` and `mechanisms` — whether the
/// bounded shortcut (direct `record_rect` from the literal coordinates) or the
/// engine-resolve path produced it.  The lazy variant matters because the
/// shortcut has to reach `record_rect` with the same pending-scope state, so
/// the per-cell `EDGE_NON_IF_LAZY` bit cannot move either.
#[test]
fn bounded_range3d_shortcut_and_engine_resolve_record_identical_edges() {
    let mut engine = new_engine();
    for sheet in ["Acct1", "Acct2", "Acct3"] {
        engine.add_sheet(sheet).unwrap();
    }
    for sheet in ["Acct1", "Acct2", "Acct3"] {
        for row in 1..=3u32 {
            for col in 1..=3u32 {
                set_num(&mut engine, sheet, row, col, (row * 10 + col) as f64);
            }
        }
    }
    engine.evaluate_all().unwrap();

    let site = cell(&engine, "Sheet1", 1, 1);
    let mut members = vec![site];
    for sheet in ["Acct1", "Acct2", "Acct3"] {
        for row in 1..=3u32 {
            for col in 1..=3u32 {
                members.push(cell(&engine, sheet, row, col));
            }
        }
    }

    let records = |mode: u8, formula: &str| {
        let collector = LiveEdgeCollector::new(&members);
        collector.set_range3d_shortcut_mode(mode);
        let value = eval_as_member(&engine, &collector, 0, "Sheet1", site, formula);
        let mut out: Vec<(u32, u32, bool, u8)> = collector
            .take_edge_records()
            .into_iter()
            .map(|e| (e.from, e.to, e.selected, e.mechanisms))
            .collect();
        out.sort_unstable();
        (value, out)
    };

    for formula in [
        "=SUM(Acct1:Acct3!B1:B2)",
        "=SUM(Acct1:Acct3!A1:C3)",
        "=SUM(Acct1:Acct3!B2:B2)",
        // A lazy (non-IF) arm, so the pending-scope mechanism bits are in play.
        "=IFERROR(SUM(Acct1:Acct3!A1:C2), 0)",
        // A rect landing on no member.
        "=SUM(Acct1:Acct3!F9:G9)",
    ] {
        let (shortcut_value, shortcut) = records(1, formula);
        let (engine_value, engine_path) = records(2, formula);
        assert_eq!(shortcut_value, engine_value, "value for {formula}");
        assert_eq!(shortcut, engine_path, "edge records for {formula}");
    }

    // Not vacuous: the B1:B2 span records six member cells.
    assert_eq!(records(1, "=SUM(Acct1:Acct3!B1:B2)").1.len(), 6);
}

/* ───────────── F7: INDEX over a bounded reference records one cell ────── */

/// F7 (session-runtime-f7-diagnosis-2026-09-24): INDEX over a bounded
/// reference must record a live edge to the selected cell only, even when
/// that cell holds an error. At 7b2ee53c the precise path declined on an
/// errored selection and the fallback resolved (and recorded) the whole
/// range, closing false live cycles through unselected members.
#[test]
fn index_selected_error_records_only_selected_member() {
    let mut engine = new_engine();
    engine
        .set_cell_value(
            "Sheet1",
            1,
            2,
            LiteralValue::Error(formualizer_common::ExcelError::new(
                formualizer_common::ExcelErrorKind::Na,
            )),
        )
        .unwrap(); // B1 = #N/A
    set_num(&mut engine, "Sheet1", 2, 2, 0.0); // B2
    set_num(&mut engine, "Sheet1", 3, 2, 5.0); // B3
    engine.evaluate_all().unwrap();

    let d1 = cell(&engine, "Sheet1", 1, 4);
    let b1 = cell(&engine, "Sheet1", 1, 2);
    let b2 = cell(&engine, "Sheet1", 2, 2);
    let b3 = cell(&engine, "Sheet1", 3, 2);
    let collector = LiveEdgeCollector::new(&[d1, b1, b2, b3]);

    let v = eval_as_member(&engine, &collector, 0, "Sheet1", d1, "=INDEX($B$1:$B$3,1)");
    assert!(
        matches!(&v, LiteralValue::Error(e) if e.kind == formualizer_common::ExcelErrorKind::Na),
        "INDEX must return the selected cell's #N/A, got {v:?}"
    );
    assert_eq!(edges(&collector), FxHashSet::from_iter([(0, 1)]));

    // Control: a non-error selection already records one edge.
    let v = eval_as_member(&engine, &collector, 0, "Sheet1", d1, "=INDEX($B$1:$B$3,3)");
    assert_eq!(v, LiteralValue::Number(5.0));
    assert_eq!(edges(&collector), FxHashSet::from_iter([(0, 3)]));
}

/// F7: when the row index is itself an error (MATCH miss), INDEX returns that
/// error and must not resolve or record the base range at all.
#[test]
fn index_error_row_index_records_no_base_edges() {
    let mut engine = new_engine();
    for r in 1..=3 {
        set_num(&mut engine, "Sheet1", r, 1, r as f64); // A1:A3
        set_num(&mut engine, "Sheet1", r, 2, r as f64 * 10.0); // B1:B3
    }
    engine.evaluate_all().unwrap();

    let d1 = cell(&engine, "Sheet1", 1, 4);
    let b1 = cell(&engine, "Sheet1", 1, 2);
    let b2 = cell(&engine, "Sheet1", 2, 2);
    let b3 = cell(&engine, "Sheet1", 3, 2);
    let collector = LiveEdgeCollector::new(&[d1, b1, b2, b3]);

    let v = eval_as_member(
        &engine,
        &collector,
        0,
        "Sheet1",
        d1,
        "=INDEX($B$1:$B$3,MATCH(99,$A$1:$A$3,0))",
    );
    assert!(
        matches!(&v, LiteralValue::Error(e) if e.kind == formualizer_common::ExcelErrorKind::Na),
        "INDEX must return the MATCH #N/A, got {v:?}"
    );
    assert!(edges(&collector).is_empty());

    // Text index: #VALUE!, still no base edges.
    let v = eval_as_member(
        &engine,
        &collector,
        0,
        "Sheet1",
        d1,
        "=INDEX($B$1:$B$3,\"x\")",
    );
    assert!(
        matches!(&v, LiteralValue::Error(e) if e.kind == formualizer_common::ExcelErrorKind::Value),
        "INDEX with a text index must return #VALUE!, got {v:?}"
    );
    assert!(edges(&collector).is_empty());
}

/// Review finding 5 (OT-285 qualification): an index argument whose
/// evaluation returns `Err` (not an error value) decides INDEX's result
/// without the base, so INDEX must not resolve or record the base range.
/// `LAMBDA(x,x)(1)` is `Err(#N/IMPL)` in the AST interpreter. Precedence
/// matches the validated fallback: a later coercion error wins over an
/// earlier evaluation `Err`.
#[test]
fn index_eval_err_index_records_no_base_edges() {
    let mut engine = new_engine();
    for r in 1..=3 {
        set_num(&mut engine, "Sheet1", r, 2, r as f64 * 10.0); // B1:B3
    }
    engine.evaluate_all().unwrap();

    let d1 = cell(&engine, "Sheet1", 1, 4);
    let b1 = cell(&engine, "Sheet1", 1, 2);
    let b2 = cell(&engine, "Sheet1", 2, 2);
    let b3 = cell(&engine, "Sheet1", 3, 2);
    let collector = LiveEdgeCollector::new(&[d1, b1, b2, b3]);

    for (formula, kind) in [
        (
            "=INDEX($B$1:$B$3,LAMBDA(x,x)(1))",
            formualizer_common::ExcelErrorKind::NImpl,
        ),
        (
            "=INDEX($B$1:$B$3,LAMBDA(x,x)(1),NA())",
            formualizer_common::ExcelErrorKind::Na,
        ),
    ] {
        let v = eval_as_member(&engine, &collector, 0, "Sheet1", d1, formula);
        assert!(
            matches!(&v, LiteralValue::Error(e) if e.kind == kind),
            "{formula}: expected {kind:?}, got {v:?}"
        );
        assert!(
            edges(&collector).is_empty(),
            "{formula}: no base edges may be recorded"
        );
    }

    // Control: a selection that reads the base records its selected member.
    let v = eval_as_member(&engine, &collector, 0, "Sheet1", d1, "=INDEX($B$1:$B$3,2)");
    assert_eq!(v, LiteralValue::Number(20.0));
    assert_eq!(edges(&collector), FxHashSet::from_iter([(0, 2)]));
}
