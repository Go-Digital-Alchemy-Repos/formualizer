use crate::engine::{
    ChangeLog, CycleConfig, CycleDetection, CyclePolicy, Engine, EvalConfig, FormulaIngestBatch,
    FormulaIngestRecord, FormulaPlaneMode,
};
use crate::test_workbook::TestWorkbook;
use formualizer_common::LiteralValue;
use formualizer_parse::ASTNode;
use formualizer_parse::parser::parse as parse_formula;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

fn parse(formula: &str) -> ASTNode {
    parse_formula(formula).expect("valid formula")
}

#[test]
fn offset_dynamic_ordering_with_dirty_formula_target() {
    let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());

    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Number(0.0))
        .unwrap();
    engine
        .set_cell_value("Sheet1", 1, 3, LiteralValue::Number(1.0))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 2, 1, parse("=C1+1"))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 2, parse("=OFFSET(A1,1,0)"))
        .unwrap();

    engine.evaluate_all().unwrap();
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 2),
        Some(LiteralValue::Number(2.0))
    );

    engine
        .set_cell_value("Sheet1", 1, 3, LiteralValue::Number(5.0))
        .unwrap();
    engine.evaluate_all().unwrap();

    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 2),
        Some(LiteralValue::Number(6.0))
    );
}

#[test]
fn offset_retarget_via_argument_edit() {
    let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());

    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Number(0.0))
        .unwrap();
    engine
        .set_cell_value("Sheet1", 2, 1, LiteralValue::Number(10.0))
        .unwrap();
    engine
        .set_cell_value("Sheet1", 3, 1, LiteralValue::Number(20.0))
        .unwrap();
    engine
        .set_cell_value("Sheet1", 1, 4, LiteralValue::Number(1.0))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 2, parse("=OFFSET(A1,D1,0)"))
        .unwrap();

    engine.evaluate_all().unwrap();
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 2),
        Some(LiteralValue::Number(10.0))
    );

    engine
        .set_cell_value("Sheet1", 1, 4, LiteralValue::Number(2.0))
        .unwrap();
    engine.evaluate_all().unwrap();
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 2),
        Some(LiteralValue::Number(20.0))
    );
}

#[test]
fn index_retarget_recomputes_plain_reference_dependent() {
    let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());
    for row in 1..=10 {
        engine
            .set_cell_value("Sheet1", row, 1, LiteralValue::Number(row as f64))
            .unwrap();
    }
    engine
        .set_cell_value("Sheet1", 1, 4, LiteralValue::Number(1.0))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 2, parse("=INDEX(A1:A10,D1)"))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 3, parse("=B1"))
        .unwrap();
    engine.evaluate_all().unwrap();
    engine
        .set_cell_value("Sheet1", 2, 1, LiteralValue::Number(200.0))
        .unwrap();
    engine
        .set_cell_value("Sheet1", 1, 4, LiteralValue::Number(2.0))
        .unwrap();
    engine.evaluate_all().unwrap();
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 2),
        Some(LiteralValue::Number(200.0))
    );
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 3),
        Some(LiteralValue::Number(200.0))
    );
}

#[test]
fn index_empty_target_recomputes_plain_reference_dependent() {
    let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());
    engine
        .set_cell_value("Sheet1", 1, 4, LiteralValue::Number(1.0))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 2, parse("=INDEX(A1:A10,D1)"))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 3, parse("=B1"))
        .unwrap();
    engine.evaluate_all().unwrap();
    for row in 1..=10 {
        engine
            .set_cell_value("Sheet1", row, 1, LiteralValue::Number((row * 100) as f64))
            .unwrap();
    }
    engine
        .set_cell_value("Sheet1", 1, 4, LiteralValue::Number(2.0))
        .unwrap();
    engine.evaluate_all().unwrap();
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 2),
        Some(LiteralValue::Number(200.0))
    );
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 3),
        Some(LiteralValue::Number(200.0))
    );
}

#[test]
fn dynamic_range_recomputes_plain_reference_dependent() {
    let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());
    engine
        .set_cell_value("Sheet1", 1, 4, LiteralValue::Number(1.0))
        .unwrap();
    engine
        .set_cell_formula(
            "Sheet1",
            1,
            2,
            parse("=SUM(INDEX(A1:A10,D1):INDEX(A1:A10,10))"),
        )
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 3, parse("=B1"))
        .unwrap();
    engine.evaluate_all().unwrap();
    for row in 1..=10 {
        engine
            .set_cell_value("Sheet1", row, 1, LiteralValue::Number(row as f64))
            .unwrap();
    }
    engine
        .set_cell_value("Sheet1", 1, 4, LiteralValue::Number(2.0))
        .unwrap();
    engine.evaluate_all().unwrap();
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 2),
        Some(LiteralValue::Number(54.0))
    );
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 3),
        Some(LiteralValue::Number(54.0))
    );
}

#[test]
fn dynamic_empty_range_per_cell_writes_recompute_plain_reference_dependent() {
    let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());
    for row in 1..=10 {
        engine
            .set_cell_value("Sheet1", row, 1, LiteralValue::Number(row as f64))
            .unwrap();
    }
    engine
        .set_cell_value("Sheet1", 1, 4, LiteralValue::Number(1.0))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 2, parse("=SUM(INDEX(A1:A10,D1):A10)"))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 3, parse("=B1"))
        .unwrap();
    engine.evaluate_all().unwrap();
    for row in 1..=10 {
        engine
            .set_cell_value("Sheet1", row, 1, LiteralValue::Number((row * 10) as f64))
            .unwrap();
    }
    engine
        .set_cell_value("Sheet1", 1, 4, LiteralValue::Number(2.0))
        .unwrap();
    engine.evaluate_all().unwrap();
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 2),
        Some(LiteralValue::Number(540.0))
    );
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 3),
        Some(LiteralValue::Number(540.0))
    );
}

#[test]
fn subset_schedule_orders_reloaded_reference_sink() {
    let config = EvalConfig::default().with_formula_plane_mode(FormulaPlaneMode::Off);
    let mut engine = Engine::new(TestWorkbook::new(), config);
    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Number(1.0))
        .unwrap();
    engine
        .set_cell_value("Sheet1", 2, 1, LiteralValue::Number(2.0))
        .unwrap();

    let mut chain = Vec::new();
    for (col, text) in [(2, "=Z1"), (26, "=A1+5")] {
        let ast_id = engine.intern_formula_ast(&parse(text));
        chain.push(FormulaIngestRecord::new(
            1,
            col,
            ast_id,
            Some(Arc::from(text)),
        ));
    }
    engine
        .ingest_formula_batches(vec![FormulaIngestBatch::new("Sheet1", chain)])
        .unwrap();

    let text = "=A2*2";
    let ast_id = engine.intern_formula_ast(&parse(text));
    engine
        .ingest_formula_batches(vec![FormulaIngestBatch::new(
            "Sheet1",
            vec![FormulaIngestRecord::new(
                1,
                3,
                ast_id,
                Some(Arc::from(text)),
            )],
        )])
        .unwrap();
    engine.evaluate_all().unwrap();
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 2),
        Some(LiteralValue::Number(6.0))
    );

    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Number(10.0))
        .unwrap();
    engine
        .set_cell_value("Sheet1", 1, 3, LiteralValue::Number(777.0))
        .unwrap();
    engine.evaluate_all().unwrap();

    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 26),
        Some(LiteralValue::Number(15.0))
    );
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 2),
        Some(LiteralValue::Number(15.0))
    );

    let text = "=A2*3";
    let ast_id = engine.intern_formula_ast(&parse(text));
    engine
        .ingest_formula_batches(vec![FormulaIngestBatch::new(
            "Sheet1",
            vec![FormulaIngestRecord::new(
                1,
                4,
                ast_id,
                Some(Arc::from(text)),
            )],
        )])
        .unwrap();
    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Number(20.0))
        .unwrap();
    engine.evaluate_all().unwrap();
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 26),
        Some(LiteralValue::Number(25.0))
    );
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 2),
        Some(LiteralValue::Number(25.0))
    );
}

#[test]
fn csr_registry_rebuild_preserves_cycle_unit() {
    let config = EvalConfig::default()
        .with_formula_plane_mode(FormulaPlaneMode::Off)
        .with_cycle(CycleConfig {
            detection: CycleDetection::Runtime,
            policy: CyclePolicy::Error,
        });
    let mut engine = Engine::new(TestWorkbook::new(), config);
    engine
        .set_cell_value("Sheet1", 2, 1, LiteralValue::Number(2.0))
        .unwrap();

    let mut cycle = Vec::new();
    for (col, text) in [(2, "=Z1+1"), (26, "=B1+1")] {
        let ast_id = engine.intern_formula_ast(&parse(text));
        cycle.push(FormulaIngestRecord::new(
            1,
            col,
            ast_id,
            Some(Arc::from(text)),
        ));
    }
    engine
        .ingest_formula_batches(vec![FormulaIngestBatch::new("Sheet1", cycle)])
        .unwrap();

    for (col, text) in [(3, "=A2*2"), (4, "=A2*3")] {
        let ast_id = engine.intern_formula_ast(&parse(text));
        engine
            .ingest_formula_batches(vec![FormulaIngestBatch::new(
                "Sheet1",
                vec![FormulaIngestRecord::new(
                    1,
                    col,
                    ast_id,
                    Some(Arc::from(text)),
                )],
            )])
            .unwrap();
    }

    let first = engine.evaluate_all().unwrap();
    assert_eq!(first.cycle_errors, 1);
    let first_values = [2, 26].map(|col| engine.get_cell_value("Sheet1", 1, col));
    assert!(first_values.iter().all(|value| matches!(
        value,
        Some(LiteralValue::Error(error))
            if error.kind == formualizer_common::ExcelErrorKind::Circ
    )));
    engine
        .set_cell_value("Sheet1", 1, 4, LiteralValue::Number(777.0))
        .unwrap();
    engine.evaluate_all().unwrap();
    for col in [2, 26] {
        match engine.get_cell_value("Sheet1", 1, col) {
            Some(LiteralValue::Error(error)) => {
                assert_eq!(error.kind, formualizer_common::ExcelErrorKind::Circ)
            }
            value => panic!("expected preserved Cycle unit at column {col}, got {value:?}"),
        }
    }
}

#[test]
fn offset_cross_sheet_reference() {
    let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());

    engine
        .set_cell_value("Sheet2", 1, 1, LiteralValue::Number(7.0))
        .unwrap();
    engine
        .set_cell_value("Sheet2", 2, 1, LiteralValue::Number(9.0))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 1, parse("=OFFSET(Sheet2!A1,1,0)"))
        .unwrap();

    engine.evaluate_all().unwrap();
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 1),
        Some(LiteralValue::Number(9.0))
    );
}

#[test]
fn offset_entrypoint_parity() {
    let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());

    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Number(0.0))
        .unwrap();
    engine
        .set_cell_value("Sheet1", 1, 3, LiteralValue::Number(1.0))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 2, 1, parse("=C1+1"))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 2, parse("=OFFSET(A1,1,0)"))
        .unwrap();

    engine.evaluate_all().unwrap();

    engine
        .set_cell_value("Sheet1", 1, 3, LiteralValue::Number(10.0))
        .unwrap();
    let (_res, _delta) = engine.evaluate_all_with_delta().unwrap();
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 2),
        Some(LiteralValue::Number(11.0))
    );

    engine
        .set_cell_value("Sheet1", 1, 3, LiteralValue::Number(20.0))
        .unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    engine
        .evaluate_all_cancellable(crate::engine::CancelToken::from_flag(cancel))
        .unwrap();
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 2),
        Some(LiteralValue::Number(21.0))
    );

    engine
        .set_cell_value("Sheet1", 1, 3, LiteralValue::Number(30.0))
        .unwrap();
    let mut log = ChangeLog::new();
    engine.evaluate_all_logged(&mut log).unwrap();
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 2),
        Some(LiteralValue::Number(31.0))
    );
}

#[test]
fn recalc_plan_with_offset_falls_back_to_dynamic_recalc() {
    let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());

    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Number(0.0))
        .unwrap();
    engine
        .set_cell_value("Sheet1", 1, 3, LiteralValue::Number(1.0))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 2, 1, parse("=C1+1"))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 2, parse("=OFFSET(A1,1,0)"))
        .unwrap();

    let plan = engine.build_recalc_plan().unwrap();
    assert!(plan.has_dynamic_refs());

    engine.evaluate_all().unwrap();
    engine
        .set_cell_value("Sheet1", 1, 3, LiteralValue::Number(9.0))
        .unwrap();

    let before = engine.virtual_dep_fallback_activations();
    engine.evaluate_recalc_plan(&plan).unwrap();
    let after = engine.virtual_dep_fallback_activations();

    assert_eq!(after, before + 1);
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 2),
        Some(LiteralValue::Number(10.0))
    );
}
