//! ES-008 element-wise lifting, measured through a real `Engine` rather than
//! the bare interpreter: the arena/per-placement dispatch
//! (`Interpreter::dispatch_function_at` / `dispatch_function_block_at`), the
//! canonical classification (`engine::arena::canonical`) and the template plane
//! (`formula_plane::template_canonical`).
//!
//! GOD-286 / CL-087 review item S1+S5. The round moved the lifted-argument set
//! from an interpreter-local function-NAME allowlist to a callee declaration
//! (`Function::elementwise_lifted_positions`, derived from `FnCaps::ELEMENTWISE`
//! and the declared argument schema), which made YEAR/MONTH/DAY/DATE/ROUNDUP/
//! ROUNDDOWN/TRUNC/INT lift where they previously answered a scalar `#VALUE!`.
//!
//! None of those callees declares `FnCaps::MAY_SPILL`, so
//! `function_registry::trusted_contract_from_caps` derives
//! `FunctionResultSemantics::ScalarValue` for them and neither
//! `arena/canonical.rs` `REJECT_ARRAY_OR_SPILL_FUNCTION` nor
//! `template_canonical.rs` `CanonicalRejectReason::ArrayOrSpillFunction` fires.
//! These tests measure what that actually produces.

use std::sync::Arc;

use crate::engine::{
    Engine, EvalConfig, FormulaIngestBatch, FormulaIngestRecord, FormulaPlaneMode,
};
use crate::test_workbook::TestWorkbook;
use formualizer_common::LiteralValue;
use formualizer_parse::parser::parse;

/// Invented data: A1:A3 hold the date serials 45000/45001/45002, which are
/// 2023-03-15/16/17 in the 1900 date system.
const SERIALS: [i64; 3] = [45000, 45001, 45002];

fn engine_with_serials(mode: FormulaPlaneMode) -> Engine<TestWorkbook> {
    crate::builtins::load_builtins();
    let mut engine = Engine::new(
        TestWorkbook::default(),
        EvalConfig::default().with_formula_plane_mode(mode),
    );
    for (index, serial) in SERIALS.iter().enumerate() {
        engine
            .set_cell_value("Sheet1", index as u32 + 1, 1, LiteralValue::Int(*serial))
            .unwrap();
    }
    engine
}

fn column_block(engine: &Engine<TestWorkbook>, col: u32) -> Vec<Option<LiteralValue>> {
    (1..=3)
        .map(|row| engine.get_cell_value("Sheet1", row, col))
        .collect()
}

fn numbers(values: [f64; 3]) -> Vec<Option<LiteralValue>> {
    values
        .into_iter()
        .map(|value| Some(LiteralValue::Number(value)))
        .collect()
}

/// The default engine (`FormulaPlaneMode::Off`, the shipped default) spills an
/// element-wise lift from the anchor cell into the two cells below, through the
/// per-placement arena dispatch that a real workbook evaluation uses.
///
/// Excel provenance for the values: OT-198 receipt `g4d_excel_cse_probe.json`,
/// desktop Excel 16.105.3 -- `YEAR(A1:A3)` row `Q27_year_range_dynamic`
/// (dynamic-array entry, 3 cells filled, `{2023;2023;2023}`), and the Knighthead
/// producer shape `LET(r,IF(ISNUMBER(A1:A3),A1:A3,0),YEAR(r))` row
/// `Q28_producer_shape_dynamic` (dynamic-array entry, 3 cells filled). NOTE that
/// the same receipt's ARRAY-entry row for the producer shape (`Q20`) measured
/// `#VALUE!`: the two Excel entry modes disagree on that formula, and the row
/// pinned here is the dynamic-entry one, which is the producer's own mode.
#[test]
fn elementwise_lift_over_a_range_spills_through_the_engine_placement_path() {
    let mut engine = engine_with_serials(FormulaPlaneMode::Off);
    engine
        .set_cell_formula("Sheet1", 1, 3, parse("=YEAR(A1:A3)").unwrap())
        .unwrap();
    engine
        .set_cell_formula(
            "Sheet1",
            1,
            5,
            parse("=LET(r,IF(ISNUMBER(A1:A3),A1:A3,0),YEAR(r))").unwrap(),
        )
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 7, parse("=DAY(A1:A3)").unwrap())
        .unwrap();
    engine.evaluate_all().unwrap();

    assert_eq!(column_block(&engine, 3), numbers([2023.0, 2023.0, 2023.0]));
    assert_eq!(column_block(&engine, 5), numbers([2023.0, 2023.0, 2023.0]));
    assert_eq!(column_block(&engine, 7), numbers([15.0, 16.0, 17.0]));
}

/// The classification fact that S1 turns on, pinned directly: a newly-lifting
/// callee over a multi-cell range is ADMITTED by the template plane, because
/// the plane asks `FnCaps::MAY_SPILL` (which YEAR/MONTH/ABS do not carry) and
/// not `Function::elementwise_lifted_positions`.
///
/// The rejected controls show what an honestly-declared spilling callee does:
/// SEQUENCE, IF and LET all carry `MAY_SPILL` and are rejected.
#[test]
fn elementwise_lift_over_a_range_is_admitted_by_the_template_plane() {
    use crate::formula_plane::template_canonical::canonicalize_template;

    crate::builtins::load_builtins();
    for formula in ["=YEAR(A1:A3)", "=MONTH(A1:A3)", "=ABS(A1:A3)", "=YEAR(B1)"] {
        let ast = parse(formula).unwrap();
        let template = canonicalize_template(&ast, 1, 3);
        assert!(
            template.labels.is_authority_supported(),
            "{formula} is admitted by the template plane: {:?}",
            template.labels.reject_reasons
        );
    }
    for formula in [
        "=SEQUENCE(3)",
        "=IF(ISNUMBER(A1:A3),A1:A3,0)",
        "=LET(r,A1:A3,YEAR(r))",
    ] {
        let ast = parse(formula).unwrap();
        let template = canonicalize_template(&ast, 1, 3);
        assert!(
            !template.labels.is_authority_supported(),
            "{formula} is rejected by the template plane"
        );
    }
}

/// The element-wise lift keeps its spill even when the experimental
/// FormulaPlane owns the span.
///
/// HISTORY. This test was banked by GOD-286/CL-087 as a MEASURED DEFECT: the
/// plane's span evaluator wrote each placement through
/// `formula_plane::span_eval::literal_to_overlay`, whose `LiteralValue::Array`
/// arm kept only the top-left element, so an element-wise lift inside a span
/// silently lost the rest of its spill. The fix was out of that round's scope.
///
/// Upstream fixed it (#388): `literal_to_overlay` now FAILS CLOSED on an array
/// result (`SpanEvalError::ArrayResultRequiresSpill`), the plane demotes the
/// span and the legacy authority re-evaluates it, so the spill lands in full.
/// The expectations below were flipped to the fixed behaviour when upstream
/// v0.9.3 was merged: the plane holds NO active span for these 200 formulas
/// (it demoted), and both authorities produce the same three-cell spill.
/// ABS is kept as the pre-existing control, on the same terms.
///
/// The layout keeps each spill on its own row (a 1x3 horizontal range spilling
/// three columns wide), so 200 copied formulas would form one span if the plane
/// retained them.
#[test]
fn elementwise_lift_inside_a_formula_plane_span_keeps_its_spill() {
    const ROWS: u32 = 200;

    fn build(mode: FormulaPlaneMode, callee: &str) -> Engine<TestWorkbook> {
        crate::builtins::load_builtins();
        let mut engine = Engine::new(
            TestWorkbook::default(),
            EvalConfig::default().with_formula_plane_mode(mode),
        );
        for row in 1..=ROWS {
            for (index, serial) in SERIALS.iter().enumerate() {
                engine
                    .set_cell_value("Sheet1", row, index as u32 + 1, LiteralValue::Int(*serial))
                    .unwrap();
            }
        }
        let mut records = Vec::new();
        for row in 1..=ROWS {
            let text = format!("={callee}(A{row}:C{row})");
            let ast = parse(&text).unwrap();
            let ast_id = engine.intern_formula_ast(&ast);
            records.push(FormulaIngestRecord::new(
                row,
                5,
                ast_id,
                Some(Arc::<str>::from(text.as_str())),
            ));
        }
        engine
            .ingest_formula_batches(vec![FormulaIngestBatch::new("Sheet1", records)])
            .unwrap();
        engine.evaluate_all().unwrap();
        engine
    }

    fn row_block(engine: &Engine<TestWorkbook>, row: u32) -> Vec<Option<LiteralValue>> {
        (5..=7)
            .map(|col| engine.get_cell_value("Sheet1", row, col))
            .collect()
    }

    // YEAR: newly lifting in this round.
    let legacy = build(FormulaPlaneMode::Off, "YEAR");
    let plane = build(FormulaPlaneMode::AuthoritativeExperimental, "YEAR");
    assert_eq!(
        legacy.baseline_stats().formula_plane_active_span_count,
        0,
        "the legacy authority owns no span"
    );
    assert_eq!(
        plane.baseline_stats().formula_plane_active_span_count,
        0,
        "the plane demotes the span rather than collapsing the spill (#388)"
    );
    for row in [1u32, 7, ROWS] {
        assert_eq!(
            row_block(&legacy, row),
            numbers([2023.0, 2023.0, 2023.0]),
            "legacy authority spills at row {row}"
        );
        assert_eq!(
            row_block(&plane, row),
            numbers([2023.0, 2023.0, 2023.0]),
            "the plane authority spills in full at row {row}"
        );
    }

    // ABS: the pre-existing control, lifting since before this round.
    let legacy_abs = build(FormulaPlaneMode::Off, "ABS");
    let plane_abs = build(FormulaPlaneMode::AuthoritativeExperimental, "ABS");
    assert_eq!(
        row_block(&legacy_abs, 1),
        numbers([45000.0, 45001.0, 45002.0])
    );
    assert_eq!(
        row_block(&plane_abs, 1),
        numbers([45000.0, 45001.0, 45002.0]),
        "ABS, the pre-existing control, spills in full too"
    );
}
