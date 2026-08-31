//! XLOOKUP must reject a *declared* length mismatch between `lookup_array` and
//! `return_array` with `#VALUE!`, before any search and regardless of
//! `if_not_found` (GOD-187al / OT-068).
//!
//! Excel grades `XLOOKUP` unconditionally `#VALUE!` when the two arrays have
//! unequal declared lengths along the lookup axis. The engine used to evaluate
//! straight through the mismatch and return a wrongly-permitted lookup value,
//! healing an error the workbook (and the answer key) expect.
//!
//! The guard reads DECLARED dimensions, not used-region-trimmed ones, so it
//! must catch a mismatch whose ranges happen to trim equal (`D3`) and must not
//! manufacture one from ranges that merely trim unequal (`D4`, `T6`). Whole
//! column/row bounds normalize to the sheet limits before comparison, so
//! `A:A` vs `B:B` and `A:A` vs `B1:B1048576` are both legal (`U2`) while
//! `A:A` vs `B1:B3` is not (`U1`).
//!
//! All values here are invented.

use formualizer_common::{ExcelErrorKind, LiteralValue};
use formualizer_parse::parser::parse;

use crate::engine::{Engine, EvalConfig, FormulaPlaneMode};
use crate::test_workbook::TestWorkbook;

/// Formula cells live in column Z so a whole-column argument is never
/// self-inclusive (an in-range probe manufactures a spurious `#CIRC!`).
const PROBE_COL: u32 = 26;

#[derive(Clone, Copy)]
enum Expect {
    Value(f64),
    Text(&'static str),
    ValueError,
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

/// The full sentinel set. RED rows fail against the unguarded engine.
fn cases() -> Vec<Case> {
    vec![
        // --- RED: declared-length mismatches Excel grades #VALUE! -----------
        // T1: vertical declared 3-vs-4.
        c(
            "T1",
            "=XLOOKUP(\"y\",A1:A3,B1:B4,\"\",0)",
            Expect::ValueError,
        ),
        // T2: horizontal declared 3-vs-4.
        c(
            "T2",
            "=XLOOKUP(\"y\",E1:G1,E2:H2,\"\",0)",
            Expect::ValueError,
        ),
        // T3: if_not_found must not rescue a mismatch, on hit or on miss.
        c(
            "T3a",
            "=XLOOKUP(\"y\",A1:A3,B1:B4,\"NF\",0)",
            Expect::ValueError,
        ),
        c(
            "T3b",
            "=XLOOKUP(\"q\",A1:A3,B1:B4,\"NF\",0)",
            Expect::ValueError,
        ),
        // D3: declared 4-vs-5 mismatch whose ranges TRIM to equal used regions
        // (both populated only in rows 1-2). Kills a trimmed-dims guard.
        c(
            "D3",
            "=XLOOKUP(\"y\",A1:A4,B1:B5,\"NF\",0)",
            Expect::ValueError,
        ),
        // U1: unbounded lookup (normalizes to 1048576) vs bounded 3-row return.
        c("U1", "=XLOOKUP(\"y\",A:A,B1:B3,\"\",0)", Expect::ValueError),
        // --- HOLD: everything the guard must leave alone --------------------
        // T4: matched declared lengths, exact hit.
        c(
            "T4",
            "=XLOOKUP(\"y\",A1:A3,B1:B3,\"\",0)",
            Expect::Value(20.0),
        ),
        // T5: matched declared 5-vs-5 with blank tails on both sides.
        c(
            "T5",
            "=XLOOKUP(\"y\",A1:A5,B1:B5,\"NF\",0)",
            Expect::Value(20.0),
        ),
        // T7: matched rows, multi-column return still row-spills (anchor cell).
        c(
            "T7",
            "=XLOOKUP(\"y\",A1:A3,C1:D3,\"\",0)",
            Expect::Value(50.0),
        ),
        // T8: matched-length MISS with if_not_found "" under unary minus stays
        // a Value error (the refuted `-""` gap; semantics unchanged).
        c(
            "T8",
            "=-XLOOKUP(\"q\",A1:A3,B1:B3,\"\",0)",
            Expect::ValueError,
        ),
        // D4: declared 5-vs-5 MATCH whose lookup trims shorter than the return.
        // Kills a trimmed-dims guard that manufactures mismatches.
        c(
            "D4",
            "=XLOOKUP(\"y\",A1:A5,I1:I5,\"NF\",0)",
            Expect::Value(20.0),
        ),
        // U2: `A:A` vs `B1:B1048576` normalize equal under Reading A.
        c(
            "U2",
            "=XLOOKUP(\"y\",A:A,B1:B1048576,\"\",0)",
            Expect::Value(20.0),
        ),
        // Computed (non-Reference) arguments carry their own declared shape and
        // must keep working — these route to view dims, not to a reference.
        c("C1", "=XLOOKUP(20,B1:B3*1,C1:C3)", Expect::Value(50.0)),
        c("C2", "=XLOOKUP(\"y\",A1:A3,B1:B3*1)", Expect::Value(20.0)),
        // A 1x1 return against a multi-row lookup is a declared mismatch too.
        c(
            "S1",
            "=XLOOKUP(\"y\",A1:A3,B1:B1,\"NF\",0)",
            Expect::ValueError,
        ),
        // if_not_found survives on a MATCHED-length miss.
        c(
            "M1",
            "=XLOOKUP(\"q\",A1:A3,B1:B3,\"NF\",0)",
            Expect::Text("NF"),
        ),
    ]
}

/// The RED subset: the rows that must fail against unguarded code.
const RED_IDS: &[&str] = &["T1", "T2", "T3a", "T3b", "D3", "U1", "S1"];

fn build(mode: FormulaPlaneMode) -> Engine<TestWorkbook> {
    let mut engine = Engine::new(
        TestWorkbook::default(),
        EvalConfig::default().with_formula_plane_mode(mode),
    );
    // Column A: the vertical lookup array, populated rows 1-3 (declared 4/5 in
    // the trimming sentinels leaves blank tails).
    for (row, text) in [(1u32, "x"), (2, "y"), (3, "z")] {
        engine
            .set_cell_value("Sheet1", row, 1, LiteralValue::Text(text.into()))
            .unwrap();
    }
    // Column B: the vertical return array, populated rows 1-4.
    for (row, value) in [(1u32, 10.0), (2, 20.0), (3, 30.0), (4, 40.0)] {
        engine
            .set_cell_value("Sheet1", row, 2, LiteralValue::Number(value))
            .unwrap();
    }
    // Columns C/D: a two-column return for the row-spill sentinel.
    for (row, value) in [(1u32, 40.0), (2, 50.0), (3, 60.0)] {
        engine
            .set_cell_value("Sheet1", row, 3, LiteralValue::Number(value))
            .unwrap();
        engine
            .set_cell_value("Sheet1", row, 4, LiteralValue::Number(value + 1.0))
            .unwrap();
    }
    // Row 1 E:G / row 2 E:H: the horizontal mismatch sentinel.
    for (col, text) in [(5u32, "x"), (6, "y"), (7, "z")] {
        engine
            .set_cell_value("Sheet1", 1, col, LiteralValue::Text(text.into()))
            .unwrap();
    }
    for (col, value) in [(5u32, 10.0), (6, 20.0), (7, 30.0), (8, 40.0)] {
        engine
            .set_cell_value("Sheet1", 2, col, LiteralValue::Number(value))
            .unwrap();
    }
    // Column I rows 1-5: a fully populated 5-row return for D4 (lookup A1:A5
    // trims to 3 rows, the return trims to 5 — declared lengths still match).
    // Deliberately NOT column F: F1 belongs to the E1:G1 horizontal sentinel.
    for (row, value) in [(1u32, 10.0), (2, 20.0), (3, 30.0), (4, 40.0), (5, 50.0)] {
        engine
            .set_cell_value("Sheet1", row, 9, LiteralValue::Number(value))
            .unwrap();
    }

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
        (Some(LiteralValue::Error(e)), Expect::ValueError) => e.kind == ExcelErrorKind::Value,
        _ => false,
    }
}

#[test]
fn xlookup_declared_length_mismatch_is_value_error() {
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
            } else {
                "HOLD"
            };
            println!(
                "{} {label} {:5} {} -> {got:?}",
                if ok { "PASS" } else { "FAIL" },
                case.id,
                case.formula
            );
            if !ok {
                deviations.push(format!(
                    "{:?} sentinel {} `{}`: got {got:?}",
                    mode, case.id, case.formula
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

/// The blank-tail row-spill sentinel needs a second cell read, so it lives
/// beside the table rather than in it.
#[test]
fn xlookup_matched_rows_multi_column_return_still_spills() {
    for mode in [
        FormulaPlaneMode::Off,
        FormulaPlaneMode::AuthoritativeExperimental,
    ] {
        let engine = build(mode);
        let idx = cases().iter().position(|c| c.id == "T7").unwrap();
        assert_eq!(
            engine.get_cell_value("Sheet1", idx as u32 + 1, PROBE_COL),
            Some(LiteralValue::Number(50.0)),
            "{mode:?} T7 anchor"
        );
        assert_eq!(
            engine.get_cell_value("Sheet1", idx as u32 + 1, PROBE_COL + 1),
            Some(LiteralValue::Number(51.0)),
            "{mode:?} T7 spill"
        );
    }
}

/// Whole-column lookup against a whole-column return with UNEQUAL used regions
/// must still resolve through the empty-lookup fallback: this is the landed
/// `dynamic_lookup_arrow::xlookup_whole_column_empty_lookup_matches_first_cell`
/// shape, restated here so the guard is pinned against consuming it.
#[test]
fn xlookup_whole_column_pair_survives_the_length_guard() {
    for mode in [
        FormulaPlaneMode::Off,
        FormulaPlaneMode::AuthoritativeExperimental,
    ] {
        let mut engine = Engine::new(
            TestWorkbook::default(),
            EvalConfig::default().with_formula_plane_mode(mode),
        );
        engine
            .set_cell_value("Sheet1", 1, 2, LiteralValue::Int(42))
            .unwrap();
        engine
            .set_cell_formula(
                "Sheet1",
                1,
                PROBE_COL,
                parse("=XLOOKUP(0,A:A,B:B,\"NF\")").unwrap(),
            )
            .unwrap();
        engine.evaluate_all().unwrap();
        assert_eq!(
            engine.get_cell_value("Sheet1", 1, PROBE_COL),
            Some(LiteralValue::Number(42.0)),
            "{mode:?} T6 whole-column pair"
        );
    }
}

/// Names the RED subset explicitly so the pre-fix receipt is legible: this test
/// asserts the sentinel table actually covers every RED id.
#[test]
fn red_sentinels_are_present_in_the_table() {
    let ids: Vec<&str> = cases().iter().map(|c| c.id).collect();
    for red in RED_IDS {
        assert!(ids.contains(red), "RED sentinel {red} missing");
    }
}
