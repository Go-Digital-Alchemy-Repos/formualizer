use crate::engine::effects::Effect;
use crate::engine::graph::editor::change_log::{ChangeEvent, ChangeLog};
use crate::engine::{EvalConfig, FormulaAuthorship, FormulaFence, eval::Engine};
use crate::test_workbook::TestWorkbook;
use formualizer_common::{ExcelErrorExtra, ExcelErrorKind, LiteralValue};
use formualizer_parse::parser::parse;
use rustc_hash::FxHashSet;

fn assert_spill(value: Option<LiteralValue>, expected_message: &str, expected_shape: (u32, u32)) {
    let Some(LiteralValue::Error(error)) = value else {
        panic!("expected #SPILL!, got {value:?}");
    };
    assert_eq!(error.kind, ExcelErrorKind::Spill);
    assert_eq!(error.message.as_deref(), Some(expected_message));
    assert_eq!(
        error.extra,
        ExcelErrorExtra::Spill {
            expected_rows: expected_shape.0,
            expected_cols: expected_shape.1,
        }
    );
}

fn vertex_at(engine: &Engine<TestWorkbook>, row: u32, col: u32) -> crate::engine::vertex::VertexId {
    *engine
        .graph
        .get_vertex_id_for_address(&engine.graph.make_cell_ref("Sheet1", row, col))
        .expect("vertex at test coordinate")
}

#[test]
fn parallel_formula_blocker_errors_in_place_and_evaluation_continues() {
    let mut engine = Engine::new(
        TestWorkbook::new(),
        EvalConfig {
            enable_parallel: true,
            max_threads: Some(2),
            ..EvalConfig::default()
        },
    );

    engine
        .set_cell_formula("Sheet1", 1, 1, parse("={10,20}").unwrap())
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 2, parse("=99").unwrap())
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 3, parse("=A1").unwrap())
        .unwrap();

    for (row, value) in [(1, 11.0), (2, 22.0), (3, 33.0)] {
        engine
            .set_cell_value("Sheet1", row, 4, LiteralValue::Number(value))
            .unwrap();
    }
    engine
        .set_cell_formula("Sheet1", 2, 5, parse("=OFFSET(D1:D3,0,0)").unwrap())
        .unwrap();
    engine
        .graph
        .stage_formula_authorship("Sheet1", 2, 5, FormulaAuthorship::legacy_scalar());

    engine
        .set_cell_formula("Sheet1", 1, 7, parse("={7,8}").unwrap())
        .unwrap();
    engine.graph.stage_formula_authorship(
        "Sheet1",
        1,
        7,
        FormulaAuthorship::cse_array(FormulaFence::new(1, 7, 1, 7)),
    );

    engine
        .set_cell_formula("Sheet1", 1, 10, parse("={3,4}").unwrap())
        .unwrap();
    for col in 1..=8 {
        engine
            .set_cell_formula("Sheet1", 20, col, parse("=1").unwrap())
            .unwrap();
    }

    engine.evaluate_all().unwrap();

    assert_spill(
        engine.get_cell_value("Sheet1", 1, 1),
        "BlockedByFormula",
        (1, 2),
    );
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 2),
        Some(LiteralValue::Number(99.0))
    );
    assert_spill(
        engine.get_cell_value("Sheet1", 1, 3),
        "BlockedByFormula",
        (1, 2),
    );

    assert_eq!(
        engine.get_cell_value("Sheet1", 2, 5),
        Some(LiteralValue::Number(22.0))
    );
    assert_eq!(engine.get_cell_value("Sheet1", 3, 5), None);
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 7),
        Some(LiteralValue::Number(7.0))
    );
    assert_eq!(engine.get_cell_value("Sheet1", 1, 8), None);
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 10),
        Some(LiteralValue::Number(3.0))
    );
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 11),
        Some(LiteralValue::Number(4.0))
    );
}

#[test]
fn targeted_evaluation_returns_blocked_value_as_spill_error() {
    let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());
    engine
        .set_cell_value("Sheet1", 1, 2, LiteralValue::Number(77.0))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 1, parse("={10,20}").unwrap())
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 3, parse("=A1").unwrap())
        .unwrap();

    let anchor = engine.evaluate_cell("Sheet1", 1, 1).unwrap();
    assert_spill(anchor, "BlockedByValue", (1, 2));
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 2),
        Some(LiteralValue::Number(77.0))
    );

    let values = engine
        .evaluate_cells(&[("Sheet1", 1, 1), ("Sheet1", 1, 3)])
        .unwrap();
    assert_spill(values[0].clone(), "BlockedByValue", (1, 2));
    assert_spill(values[1].clone(), "BlockedByValue", (1, 2));
}

#[test]
fn commit_time_formula_blocker_has_no_phantom_spill_commit_event() {
    let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());
    engine
        .set_cell_formula("Sheet1", 1, 1, parse("={10,20}").unwrap())
        .unwrap();
    engine.evaluate_cell("Sheet1", 1, 1).unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 3, parse("=42").unwrap())
        .unwrap();
    engine.evaluate_cell("Sheet1", 1, 3).unwrap();

    let anchor = vertex_at(&engine, 1, 1);
    let blocker = vertex_at(&engine, 1, 3);
    let mut overwritable = FxHashSet::default();
    overwritable.insert(blocker);
    let effects = engine
        .plan_vertex_effects(
            anchor,
            LiteralValue::Array(vec![vec![
                LiteralValue::Number(10.0),
                LiteralValue::Number(20.0),
                LiteralValue::Number(30.0),
            ]]),
            Some(&overwritable),
        )
        .unwrap();
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::SpillCommit { .. }))
    );

    let mut log = ChangeLog::new();
    engine
        .apply_effects_with_computed_writes(&effects, None, Some(&mut log), None)
        .unwrap();

    assert_spill(
        engine.get_cell_value("Sheet1", 1, 1),
        "BlockedByFormula",
        (1, 3),
    );
    assert_eq!(engine.get_cell_value("Sheet1", 1, 2), None);
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 3),
        Some(LiteralValue::Number(42.0))
    );
    assert!(
        !log.events()
            .iter()
            .any(|event| matches!(event, ChangeEvent::SpillCommitted { .. }))
    );
}

#[test]
fn logged_blocked_spill_completes_without_spill_commit_event() {
    let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());
    engine
        .set_cell_value("Sheet1", 1, 2, LiteralValue::Number(55.0))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 1, parse("={10,20}").unwrap())
        .unwrap();

    let mut log = ChangeLog::new();
    engine.evaluate_all_logged(&mut log).unwrap();

    assert_spill(
        engine.get_cell_value("Sheet1", 1, 1),
        "BlockedByValue",
        (1, 2),
    );
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 2),
        Some(LiteralValue::Number(55.0))
    );
    assert!(
        !log.events()
            .iter()
            .any(|event| matches!(event, ChangeEvent::SpillCommitted { .. }))
    );
}

#[test]
fn commit_time_arrow_value_blocker_is_preserved() {
    let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());
    engine
        .set_cell_formula("Sheet1", 1, 1, parse("={10,20}").unwrap())
        .unwrap();
    let anchor = vertex_at(&engine, 1, 1);
    let effects = engine
        .plan_vertex_effects(
            anchor,
            LiteralValue::Array(vec![vec![
                LiteralValue::Number(10.0),
                LiteralValue::Number(20.0),
            ]]),
            None,
        )
        .unwrap();

    engine
        .set_cell_value("Sheet1", 1, 2, LiteralValue::Number(88.0))
        .unwrap();
    engine
        .apply_effects_with_computed_writes(&effects, None, None, None)
        .unwrap();

    assert_spill(
        engine.get_cell_value("Sheet1", 1, 1),
        "BlockedByValue",
        (1, 2),
    );
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 2),
        Some(LiteralValue::Number(88.0))
    );
}
