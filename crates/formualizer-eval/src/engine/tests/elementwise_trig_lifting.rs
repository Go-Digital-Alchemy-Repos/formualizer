//! ES-008 element-wise lifting of the trigonometric family, measured through a
//! real `Engine`.
//!
//! GOD-286 / CL-087 review item S2. Before this round only SIN/COS/TAN/ATAN2
//! answered element-wise over a range, and they did so through their own
//! internal loop (`unary_numeric_elementwise`); the other nineteen ELEMENTWISE
//! trig callees read their argument through `unary_numeric_arg` and answered a
//! scalar `#VALUE!`. The round did not touch trig.rs: it made the interpreter
//! consult `FnCaps::ELEMENTWISE`, a flag every one of them already carried, so
//! all of them now lift. That is an OBSERVABLE ANSWER CHANGE for nineteen
//! functions and these tests pin it.
//!
//! PROVENANCE: these assertions pin MEASURED ENGINE BEHAVIOUR against the
//! closed-form mathematical values. There is NO live-Excel oracle for any of
//! them -- GOD-286 could not open Excel, and the OT-198 probe receipt
//! `g4d_excel_cse_probe.json` contains no trigonometric row. The gap between
//! "the engine does this" and "desktop Excel does this" is an explicitly
//! declared residual for a later Excel round, not a settled answer.

use std::f64::consts::PI;

use crate::engine::{Engine, EvalConfig, FormulaPlaneMode};
use crate::test_workbook::TestWorkbook;
use formualizer_common::{ExcelErrorKind, LiteralValue};
use formualizer_parse::parser::parse;

/// Invented inputs, each chosen inside the relevant function's domain:
/// B1:B3 hold 0 / 0.5 / 1 (ASIN needs `[-1, 1]`), C1:C3 hold the degrees
/// 0 / 90 / 180, and D1:D3 hold the radians 0 / PI/2 / PI.
fn trig_engine() -> Engine<TestWorkbook> {
    crate::builtins::load_builtins();
    let mut engine = Engine::new(
        TestWorkbook::default(),
        EvalConfig::default().with_formula_plane_mode(FormulaPlaneMode::Off),
    );
    for (col, values) in [
        (2u32, [0.0f64, 0.5, 1.0]),
        (3, [0.0, 90.0, 180.0]),
        (4, [0.0, PI / 2.0, PI]),
    ] {
        for (index, value) in values.into_iter().enumerate() {
            engine
                .set_cell_value("Sheet1", index as u32 + 1, col, LiteralValue::Number(value))
                .unwrap();
        }
    }
    engine
}

fn spilled_numbers(engine: &Engine<TestWorkbook>, col: u32) -> Vec<f64> {
    (1..=3)
        .map(|row| match engine.get_cell_value("Sheet1", row, col) {
            Some(LiteralValue::Number(value)) => value,
            Some(LiteralValue::Int(value)) => value as f64,
            other => panic!("expected a number at R{row}C{col}, got {other:?}"),
        })
        .collect()
}

fn assert_close(actual: &[f64], expected: [f64; 3], label: &str) {
    assert_eq!(actual.len(), 3, "{label} spills three cells");
    for (index, expected) in expected.into_iter().enumerate() {
        let observed = actual[index];
        assert!(
            (observed - expected).abs() <= 1e-12 * expected.abs().max(1.0),
            "{label}[{index}]: {observed} != {expected}"
        );
    }
}

/// MEASURED ENGINE BEHAVIOUR, no Excel oracle. RADIANS, DEGREES and ASIN
/// answered a scalar `#VALUE!` over a multi-cell range before CL-087 and spill
/// element-wise after it.
#[test]
fn newly_lifting_trig_functions_spill_element_wise_over_a_range() {
    let mut engine = trig_engine();
    engine
        .set_cell_formula("Sheet1", 1, 6, parse("=RADIANS(C1:C3)").unwrap())
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 8, parse("=DEGREES(D1:D3)").unwrap())
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 10, parse("=ASIN(B1:B3)").unwrap())
        .unwrap();
    engine.evaluate_all().unwrap();

    assert_close(
        &spilled_numbers(&engine, 6),
        [0.0, PI / 2.0, PI],
        "RADIANS(C1:C3)",
    );
    assert_close(
        &spilled_numbers(&engine, 8),
        [0.0, 90.0, 180.0],
        "DEGREES(D1:D3)",
    );
    assert_close(
        &spilled_numbers(&engine, 10),
        [0.0, PI / 6.0, PI / 2.0],
        "ASIN(B1:B3)",
    );
}

/// MEASURED ENGINE BEHAVIOUR, no Excel oracle. SIN lifted over a range before
/// CL-087 too, but through its own internal `unary_numeric_elementwise` loop;
/// the interpreter now lifts it per element before dispatch. This pins which of
/// the two possible answers the engine gives when ONE cell of the range holds
/// an error: an array carrying the error in that position only, NOT a single
/// scalar error for the whole call.
#[test]
fn sin_over_a_range_containing_an_error_keeps_the_error_in_one_position() {
    let mut engine = trig_engine();
    engine
        .set_cell_value("Sheet1", 1, 5, LiteralValue::Number(0.0))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 2, 5, parse("=1/0").unwrap())
        .unwrap();
    engine
        .set_cell_value("Sheet1", 3, 5, LiteralValue::Number(0.0))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 12, parse("=SIN(E1:E3)").unwrap())
        .unwrap();
    engine.evaluate_all().unwrap();

    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 12),
        Some(LiteralValue::Number(0.0))
    );
    match engine.get_cell_value("Sheet1", 2, 12) {
        Some(LiteralValue::Error(error)) => assert_eq!(error.kind, ExcelErrorKind::Div),
        other => panic!("expected #DIV/0! in the spilled middle cell, got {other:?}"),
    }
    assert_eq!(
        engine.get_cell_value("Sheet1", 3, 12),
        Some(LiteralValue::Number(0.0))
    );
}
