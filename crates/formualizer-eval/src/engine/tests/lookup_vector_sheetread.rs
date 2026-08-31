//! `LOOKUP` vector form (3-arg) retrieves its result by a SHEET READ at
//! (declared start + match offset) along the declared orientation, legally
//! reading PAST the declared end of `result_vector` (GOD-187an / OT-072,
//! Excel semantics row ES-033).
//!
//! Excel does not index into the materialized `result_vector`; it reduces the
//! reference to a (start cell, orientation) pair and reads the sheet at
//! `start + offset`. `=LOOKUP("c",A1:A4,B1:B2)` therefore returns B3, not
//! `#N/A` and not 0 — the declared two-cell extent is discarded for
//! retrieval. The engine used to index positionally into the flattened
//! two-element vector, fall off the end, and materialise the miss as 0.
//!
//! Oracle: real Excel for Mac 16.105.3, six probes (J1-J6) measured on
//! 2026-08-31 and pinned as ES-033. Fixture layout mirrors that oracle
//! workbook exactly:
//!   A1:A5 = a,b,c,d,e   B1:B4 = 10,20,30,40 (B5 empty)   D1:G1 = 100..400
//!
//! MEASURED (oracle-pinned) vs INFERENCE-TIER: only the J1-J6 rows carry a
//! real-Excel measurement. The bare-`Cell` result_vector, the text-target and
//! error-target overruns, and the open-bounded (`B:B`) anchoring control are
//! EXTENSIONS of the measured (start, orientation) reduction, tagged
//! inference-tier at each case. The declared-2-D result_vector is an
//! UNMEASURED degree of freedom and is deliberately PRESERVED on the old
//! row-major positional path.
//!
//! All values here are invented; no client data is used.

use formualizer_common::{ExcelError, ExcelErrorKind, LiteralValue};
use formualizer_parse::parser::parse;

use crate::engine::{Engine, EvalConfig, FormulaPlaneMode};
use crate::test_workbook::TestWorkbook;

/// Formula cells live in column Z, outside every referenced column (A..L), so
/// no synthesized sheet-read target can ever land on a probe cell.
const PROBE_COL: u32 = 26;

#[derive(Clone, Copy)]
enum Expect {
    Value(f64),
    Text(&'static str),
    Error(ExcelErrorKind),
}

struct Case {
    id: &'static str,
    formula: &'static str,
    expect: Expect,
}

const fn c(id: &'static str, formula: &'static str, expect: Expect) -> Case {
    Case {
        id,
        formula,
        expect,
    }
}

fn cases() -> Vec<Case> {
    vec![
        // --- MEASURED: the six real-Excel oracle probes (ES-033) -----------
        // J1: overrun by one past the declared end -> sheet cell B3.
        c("J1", "=LOOKUP(\"c\",A1:A4,B1:B2)", Expect::Value(30.0)),
        // J2: overrun by two -> sheet cell B4.
        c("J2", "=LOOKUP(\"d\",A1:A4,B1:B2)", Expect::Value(40.0)),
        // J3: overrun onto an EMPTY sheet cell (B5) -> Excel displays 0.
        c("J3", "=LOOKUP(\"e\",A1:A5,B1:B2)", Expect::Value(0.0)),
        // J4: in-extent horizontal control -> E1. Unchanged by the fix.
        c("J4", "=LOOKUP(\"b\",A1:A4,D1:G1)", Expect::Value(200.0)),
        // J5: 1x1 declared Range counts as HORIZONTAL (measured) -> D1.
        c("J5", "=LOOKUP(\"c\",A1:A4,B1:B1)", Expect::Value(100.0)),
        // J6: target past the SHEET EDGE (row 1048579) -> Empty -> 0, and
        // above all MUST NOT PANIC (RelativeCoord::new asserts out of bounds).
        c(
            "J6",
            "=LOOKUP(\"e\",A1:A5,B1048575:B1048576)",
            Expect::Value(0.0),
        ),
        // --- HOLD: controls that must not move -----------------------------
        // C1: matched declared length, in extent -> B3 either way.
        c("C1", "=LOOKUP(\"c\",A1:A4,B1:B4)", Expect::Value(30.0)),
        // C2: below-minimum lookup value still returns #N/A before retrieval.
        c(
            "C2",
            "=LOOKUP(0,H1:H4,B1:B2)",
            Expect::Error(ExcelErrorKind::Na),
        ),
        // C3: array-literal result_vector is NOT a Reference node -> keeps the
        // positional path verbatim (offset 2 falls off a 2-element array).
        c("C3", "=LOOKUP(\"c\",A1:A4,{10,20})", Expect::Value(0.0)),
        // C4: declared 2-D reference result_vector — UNMEASURED DOF,
        // PRESERVED. Row-major flatten of D1:E2 = [100,200,1000,2000];
        // positional offset 2 -> 1000. The fix must NOT sheet-read here.
        c("C4", "=LOOKUP(\"c\",A1:A4,D1:E2)", Expect::Value(1000.0)),
        // --- INFERENCE-TIER: extensions of the measured reduction ----------
        // I1: bare `Cell` result_vector. UNMEASURED — the J5 (start,
        // orientation) reduction is extended to it as horizontal: B1 + 2 cols
        // -> D1.
        c("I1", "=LOOKUP(\"c\",A1:A4,B1)", Expect::Value(100.0)),
        // I2: TEXT target on overrun propagates VERBATIM. UNMEASURED.
        c("I2", "=LOOKUP(\"c\",A1:A4,K1:K2)", Expect::Text("hello")),
        // I3: ERROR target on overrun propagates VERBATIM (no swallowing).
        // UNMEASURED.
        c(
            "I3",
            "=LOOKUP(\"c\",A1:A4,L1:L2)",
            Expect::Error(ExcelErrorKind::Div),
        ),
    ]
}

/// The oracle-MEASURED red subset: rows real Excel proves the unfixed engine
/// gets wrong. (Per packet definition, RED_IDS is the J1/J2/J5 analog set.)
const RED_IDS: &[&str] = &["J1", "J2", "J5"];

/// Additional rows that also go red pre-fix because they sit on the same
/// changed branch, but whose expectations are INFERENCE-TIER rather than
/// oracle-measured. Labeled separately so the red receipt stays legible.
const INFERENCE_RED_IDS: &[&str] = &["I1", "I2", "I3"];

fn build(mode: FormulaPlaneMode) -> Engine<TestWorkbook> {
    let mut engine = Engine::new(
        TestWorkbook::default(),
        EvalConfig::default().with_formula_plane_mode(mode),
    );
    // Column A rows 1-5: the ascending lookup vector (oracle layout).
    for (row, text) in [(1u32, "a"), (2, "b"), (3, "c"), (4, "d"), (5, "e")] {
        engine
            .set_cell_value("Sheet1", row, 1, LiteralValue::Text(text.into()))
            .unwrap();
    }
    // Column B rows 1-4: the vertical result vector. B5 stays EMPTY (J3).
    for (row, value) in [(1u32, 10.0), (2, 20.0), (3, 30.0), (4, 40.0)] {
        engine
            .set_cell_value("Sheet1", row, 2, LiteralValue::Number(value))
            .unwrap();
    }
    // D1:G1 = 100,200,300,400: the horizontal result vector (J4), and the
    // sheet cells J5/I1 read into when B1 is treated as a horizontal start.
    for (col, value) in [(4u32, 100.0), (5, 200.0), (6, 300.0), (7, 400.0)] {
        engine
            .set_cell_value("Sheet1", 1, col, LiteralValue::Number(value))
            .unwrap();
    }
    // D2:E2: second row of the declared-2-D result_vector sentinel (C4).
    for (col, value) in [(4u32, 1000.0), (5, 2000.0)] {
        engine
            .set_cell_value("Sheet1", 2, col, LiteralValue::Number(value))
            .unwrap();
    }
    // Column H rows 1-4: a numeric ascending lookup vector for the #N/A
    // below-minimum control (C2).
    for (row, value) in [(1u32, 1.0), (2, 2.0), (3, 3.0), (4, 4.0)] {
        engine
            .set_cell_value("Sheet1", row, 8, LiteralValue::Number(value))
            .unwrap();
    }
    // Column K: numeric K1:K2 with a TEXT cell at the overrun target K3 (I2).
    engine
        .set_cell_value("Sheet1", 1, 11, LiteralValue::Number(1.0))
        .unwrap();
    engine
        .set_cell_value("Sheet1", 2, 11, LiteralValue::Number(2.0))
        .unwrap();
    engine
        .set_cell_value("Sheet1", 3, 11, LiteralValue::Text("hello".into()))
        .unwrap();
    // Column L: numeric L1:L2 with an ERROR cell at the overrun target L3 (I3).
    engine
        .set_cell_value("Sheet1", 1, 12, LiteralValue::Number(1.0))
        .unwrap();
    engine
        .set_cell_value("Sheet1", 2, 12, LiteralValue::Number(2.0))
        .unwrap();
    engine
        .set_cell_value(
            "Sheet1",
            3,
            12,
            LiteralValue::Error(ExcelError::new(ExcelErrorKind::Div)),
        )
        .unwrap();

    for (idx, case) in cases().into_iter().enumerate() {
        engine
            .set_cell_formula(
                "Sheet1",
                idx as u32 + 1,
                PROBE_COL,
                parse(case.formula).unwrap(),
            )
            .unwrap();
    }
    engine.evaluate_all().unwrap();
    engine
}

fn observed(engine: &Engine<TestWorkbook>, idx: usize) -> Option<LiteralValue> {
    engine.get_cell_value("Sheet1", idx as u32 + 1, PROBE_COL)
}

fn matches(got: &Option<LiteralValue>, expect: Expect) -> bool {
    match (got, expect) {
        (Some(LiteralValue::Number(n)), Expect::Value(v)) => *n == v,
        (Some(LiteralValue::Int(n)), Expect::Value(v)) => (*n as f64) == v,
        (Some(LiteralValue::Text(t)), Expect::Text(v)) => t == v,
        (Some(LiteralValue::Error(e)), Expect::Error(k)) => e.kind == k,
        _ => false,
    }
}

#[test]
fn lookup_vector_form_retrieves_by_sheet_read() {
    for mode in [
        FormulaPlaneMode::Off,
        FormulaPlaneMode::AuthoritativeExperimental,
    ] {
        let engine = build(mode);
        let mut deviations: Vec<String> = Vec::new();
        for (idx, case) in cases().into_iter().enumerate() {
            let got = observed(&engine, idx);
            let ok = matches(&got, case.expect);
            let label = if RED_IDS.contains(&case.id) {
                "RED "
            } else if INFERENCE_RED_IDS.contains(&case.id) {
                "RED*"
            } else {
                "HOLD"
            };
            println!(
                "{} {label} {:4} {} -> {got:?}",
                if ok { "PASS" } else { "FAIL" },
                case.id,
                case.formula
            );
            if !ok {
                deviations.push(format!(
                    "{mode:?} sentinel {} `{}`: got {got:?}",
                    case.id, case.formula
                ));
            }
        }
        assert!(
            deviations.is_empty(),
            "{} sentinel deviation(s):\n{}",
            deviations.len(),
            deviations.join("\n")
        );
    }
}

/// OPEN-BOUNDED result_vector anchoring control (INFERENCE-TIER, and a
/// deliberate in-extent behavior delta of this fix).
///
/// `resolve_range_view` fills a `None` start bound from the USED extent, so
/// the old positional path anchored `B:B` at the first USED row (B3). The
/// declared-reference reduction anchors at row 1, so offset 1 reads B2 —
/// empty — and the Empty->0 display law applies. Needs its own fixture (B1:B2
/// empty), hence its own engine.
#[test]
fn lookup_open_bounded_result_vector_anchors_at_row_one() {
    for mode in [
        FormulaPlaneMode::Off,
        FormulaPlaneMode::AuthoritativeExperimental,
    ] {
        let mut engine = Engine::new(
            TestWorkbook::default(),
            EvalConfig::default().with_formula_plane_mode(mode),
        );
        for (row, text) in [(1u32, "a"), (2, "b"), (3, "c"), (4, "d")] {
            engine
                .set_cell_value("Sheet1", row, 1, LiteralValue::Text(text.into()))
                .unwrap();
        }
        // B1:B2 deliberately EMPTY; B3:B5 populated.
        for (row, value) in [(3u32, 30.0), (4, 40.0), (5, 50.0)] {
            engine
                .set_cell_value("Sheet1", row, 2, LiteralValue::Number(value))
                .unwrap();
        }
        engine
            .set_cell_formula(
                "Sheet1",
                1,
                PROBE_COL,
                parse("=LOOKUP(\"b\",A1:A4,B:B)").unwrap(),
            )
            .unwrap();
        engine.evaluate_all().unwrap();
        assert_eq!(
            engine.get_cell_value("Sheet1", 1, PROBE_COL),
            Some(LiteralValue::Number(0.0)),
            "{mode:?} open-bound anchoring: B:B + offset 1 -> B2 (empty) -> 0"
        );
    }
}

/// Names the measured RED subset explicitly so the pre-fix receipt is
/// legible, and pins that the sentinel table actually covers every RED id.
#[test]
fn red_sentinels_are_present_in_the_table() {
    let ids: Vec<&str> = cases().iter().map(|c| c.id).collect();
    for red in RED_IDS.iter().chain(INFERENCE_RED_IDS.iter()) {
        assert!(ids.contains(red), "RED sentinel {red} missing");
    }
}
