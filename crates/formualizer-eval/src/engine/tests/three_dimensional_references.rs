use crate::engine::{Engine, EvalConfig};
use crate::function::Function;
use crate::test_workbook::TestWorkbook;
use crate::traits::{ArgumentHandle, CalcValue, FunctionContext};
use formualizer_common::{ExcelError, ExcelErrorKind, LiteralValue};
use formualizer_parse::parser::parse;
use std::sync::Arc;

fn engine_with_nonlexical_span() -> Engine<TestWorkbook> {
    let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());
    engine.graph.add_sheet("Zed").unwrap();
    engine.graph.add_sheet("Mid").unwrap();
    engine.graph.add_sheet("Alpha").unwrap();
    engine.graph.add_sheet("Result").unwrap();
    engine
        .set_cell_value("Zed", 1, 1, LiteralValue::Number(1.0))
        .unwrap();
    engine
        .set_cell_value("Mid", 1, 1, LiteralValue::Number(2.0))
        .unwrap();
    engine
        .set_cell_value("Alpha", 1, 1, LiteralValue::Number(3.0))
        .unwrap();
    engine
}

fn assert_number(engine: &Engine<TestWorkbook>, sheet: &str, row: u32, col: u32, expected: f64) {
    match engine.get_cell_value(sheet, row, col) {
        Some(LiteralValue::Number(actual)) => assert_eq!(actual, expected),
        Some(LiteralValue::Int(actual)) => assert_eq!(actual as f64, expected),
        other => panic!("expected {expected}, got {other:?}"),
    }
}

#[test]
fn cell_3d_aggregates_follow_tab_order_and_include_endpoints() {
    let mut engine = engine_with_nonlexical_span();
    for (row, formula) in [
        (1, "=SUM(Zed:Alpha!A1)"),
        (2, "=COUNT(Zed:Alpha!A1)"),
        (3, "=AVERAGE(Zed:Alpha!A1)"),
        (4, "=MIN(Zed:Alpha!A1)"),
        (5, "=MAX(Zed:Alpha!A1)"),
        (6, "=SUM(Mid:Mid!A1)"),
    ] {
        engine
            .set_cell_formula("Result", row, 1, parse(formula).unwrap())
            .unwrap();
    }

    engine.evaluate_all().unwrap();
    for (row, expected) in [(1, 6.0), (2, 3.0), (3, 2.0), (4, 1.0), (5, 3.0), (6, 2.0)] {
        assert_number(&engine, "Result", row, 1, expected);
    }
}

#[test]
fn range_3d_flattens_each_sheet_area_for_aggregation() {
    let mut engine = engine_with_nonlexical_span();
    for (sheet, base) in [("Zed", 0.0), ("Mid", 10.0), ("Alpha", 20.0)] {
        for row in 1..=3 {
            for col in 1..=2 {
                engine
                    .set_cell_value(
                        sheet,
                        row,
                        col,
                        LiteralValue::Number(base + f64::from((row - 1) * 2 + col)),
                    )
                    .unwrap();
            }
        }
    }
    engine
        .set_cell_formula("Result", 1, 1, parse("=SUM(Zed:Alpha!A1:B3)").unwrap())
        .unwrap();
    engine.evaluate_all().unwrap();
    assert_number(&engine, "Result", 1, 1, 243.0);
}

#[test]
fn missing_3d_endpoint_is_ref_error() {
    let mut engine = engine_with_nonlexical_span();
    let error = engine
        .set_cell_formula("Result", 1, 1, parse("=SUM(Zed:Missing!A1)").unwrap())
        .unwrap_err();
    assert_eq!(error.kind, ExcelErrorKind::Ref);
}

#[test]
fn editing_mid_span_cell_recalculates_dependent() {
    let mut engine = engine_with_nonlexical_span();
    engine
        .set_cell_formula("Result", 1, 1, parse("=SUM(Zed:Alpha!A1)").unwrap())
        .unwrap();
    engine.evaluate_all().unwrap();
    assert_number(&engine, "Result", 1, 1, 6.0);

    engine
        .set_cell_value("Mid", 1, 1, LiteralValue::Number(20.0))
        .unwrap();
    engine.evaluate_all().unwrap();
    assert_number(&engine, "Result", 1, 1, 24.0);
}

struct NimplFn;

impl Function for NimplFn {
    fn name(&self) -> &'static str {
        "NIMPL_FOR_TEST"
    }

    fn eval<'a, 'b, 'c>(
        &self,
        _args: &'c [ArgumentHandle<'a, 'b>],
        _ctx: &dyn FunctionContext<'b>,
    ) -> Result<CalcValue<'b>, ExcelError> {
        Err(ExcelError::new(ExcelErrorKind::NImpl))
    }
}

#[test]
fn binary_operators_propagate_first_error_kind() {
    let workbook = TestWorkbook::new().with_function(Arc::new(NimplFn));
    let mut engine = Engine::new(workbook, EvalConfig::default());
    engine
        .set_cell_formula("Sheet1", 1, 1, parse("=NIMPL_FOR_TEST()").unwrap())
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 2, parse("=1/0").unwrap())
        .unwrap();
    for (col, formula) in [(3, "=A1=0"), (4, "=0<A1"), (5, "=A1+B1"), (6, "=B1=A1")] {
        engine
            .set_cell_formula("Sheet1", 1, col, parse(formula).unwrap())
            .unwrap();
    }
    engine.evaluate_all().unwrap();

    for (col, expected) in [
        (3, ExcelErrorKind::NImpl),
        (4, ExcelErrorKind::NImpl),
        (5, ExcelErrorKind::NImpl),
        (6, ExcelErrorKind::Div),
    ] {
        match engine.get_cell_value("Sheet1", 1, col) {
            Some(LiteralValue::Error(error)) => assert_eq!(error.kind, expected),
            other => panic!("expected {expected:?}, got {other:?}"),
        }
    }
}
