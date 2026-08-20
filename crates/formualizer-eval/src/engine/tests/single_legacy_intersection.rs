use crate::engine::{Engine, EvalConfig};
use crate::test_workbook::TestWorkbook;
use formualizer_common::LiteralValue;
use formualizer_parse::parser::parse;

fn engine() -> Engine<TestWorkbook> {
    crate::builtins::load_builtins();
    Engine::new(
        TestWorkbook::new(),
        EvalConfig {
            enable_parallel: false,
            ..Default::default()
        },
    )
}

#[test]
fn legacy_single_preserves_scalar_and_intersects_ranges() {
    let mut engine = engine();
    engine
        .set_cell_value("Sheet1", 4, 1, LiteralValue::Number(41.0))
        .unwrap();
    engine
        .set_cell_value("Sheet1", 1, 3, LiteralValue::Number(17.0))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 2, 2, parse("=_xlfn.SINGLE(9)").unwrap())
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 4, 2, parse("=_xlfn.SINGLE(A1:A8)").unwrap())
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 2, 3, parse("=_xlfn.SINGLE(A1:E1)").unwrap())
        .unwrap();
    engine.evaluate_all().unwrap();

    assert_eq!(
        engine.get_cell_value("Sheet1", 2, 2),
        Some(LiteralValue::Number(9.0))
    );
    assert_eq!(
        engine.get_cell_value("Sheet1", 4, 2),
        Some(LiteralValue::Number(41.0))
    );
    assert_eq!(
        engine.get_cell_value("Sheet1", 2, 3),
        Some(LiteralValue::Number(17.0))
    );
}

#[test]
fn legacy_single_returns_value_error_without_an_intersection() {
    let mut engine = engine();
    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Number(5.0))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 20, 2, parse("=_xlfn.SINGLE(A1:A8)").unwrap())
        .unwrap();
    engine.evaluate_all().unwrap();

    match engine.get_cell_value("Sheet1", 20, 2) {
        Some(LiteralValue::Error(error)) => assert_eq!(error.to_string(), "#VALUE!"),
        other => panic!("expected #VALUE!, got {other:?}"),
    }
}
