use crate::engine::virtual_deps::RangeVirtualDepProvider;
use crate::engine::{Engine, EvalConfig, FormulaAuthorship, FormulaFence};
use crate::test_workbook::TestWorkbook;
use formualizer_common::LiteralValue;
use formualizer_parse::parser::parse;

fn vertex(
    engine: &Engine<TestWorkbook>,
    sheet: &str,
    row: u32,
    col: u32,
) -> crate::engine::VertexId {
    *engine
        .graph
        .get_vertex_id_for_address(&engine.graph.make_cell_ref(sheet, row, col))
        .unwrap()
}

#[test]
fn declared_cse_follower_reads_have_producer_edges_before_first_spill() {
    for cross_sheet in [false, true] {
        let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());
        let producer_sheet = if cross_sheet { "Child" } else { "Sheet1" };
        engine
            .set_cell_formula(
                producer_sheet,
                15,
                2,
                parse("={\"Term\",\"Rate\";1,10;2,20;3,30;4,40;5,50;6,60;7,70}").unwrap(),
            )
            .unwrap();
        engine.stage_loaded_formula_authorship(
            producer_sheet,
            15,
            2,
            FormulaAuthorship::cse_array(FormulaFence::new(15, 2, 22, 3)),
        );
        let prefix = if cross_sheet { "Child!" } else { "" };
        for (col, formula) in [
            (1, format!("=VLOOKUP(3,{prefix}B16:C22,2,FALSE)")),
            (4, format!("={prefix}C18")),
            (5, format!("=SUM({prefix}C18:C18)")),
        ] {
            engine
                .set_cell_formula("Sheet1", 1, col, parse(&formula).unwrap())
                .unwrap();
        }
        let producer = vertex(&engine, producer_sheet, 15, 2);
        assert!(
            engine
                .graph
                .spill_anchors_in_region(
                    engine.graph.sheet_id(producer_sheet).unwrap(),
                    17,
                    2,
                    17,
                    2
                )
                .is_empty()
        );
        for col in [1, 4, 5] {
            assert!(
                RangeVirtualDepProvider::get_virtual_deps(
                    &engine,
                    vertex(&engine, "Sheet1", 1, col)
                )
                .contains(&producer),
                "missing producer edge for column {col}"
            );
        }
        engine.evaluate_all().unwrap();
        for col in [1, 4, 5] {
            assert_eq!(
                engine.get_cell_value("Sheet1", 1, col),
                Some(LiteralValue::Number(30.0))
            );
        }
    }
}

#[test]
fn declared_output_index_replaces_fence_and_ignores_overwritten_formula() {
    let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());
    engine
        .set_cell_formula("Sheet1", 15, 2, parse("={1,10;2,20}").unwrap())
        .unwrap();
    let producer = vertex(&engine, "Sheet1", 15, 2);
    let sheet = engine.graph.sheet_id("Sheet1").unwrap();
    engine.stage_loaded_formula_authorship(
        "Sheet1",
        15,
        2,
        FormulaAuthorship::cse_array(FormulaFence::new(15, 2, 16, 3)),
    );
    assert_eq!(
        engine.graph.output_anchors_in_region(sheet, 15, 2, 15, 2),
        vec![producer]
    );
    engine.stage_loaded_formula_authorship(
        "Sheet1",
        15,
        2,
        FormulaAuthorship::cse_array(FormulaFence::new(15, 2, 15, 3)),
    );
    assert!(
        engine
            .graph
            .output_anchors_in_region(sheet, 15, 2, 15, 2)
            .is_empty()
    );
    engine
        .set_cell_formula("Sheet1", 15, 2, parse("=1").unwrap())
        .unwrap();
    assert!(
        engine
            .graph
            .output_anchors_in_region(sheet, 14, 2, 14, 2)
            .is_empty()
    );
    engine.stage_loaded_formula_authorship(
        "Sheet1",
        15,
        2,
        FormulaAuthorship::cse_array(FormulaFence::new(15, 2, 16, 3)),
    );
    engine
        .set_cell_value("Sheet1", 15, 2, LiteralValue::Number(9.0))
        .unwrap();
    assert!(
        engine
            .graph
            .output_anchors_in_region(sheet, 14, 2, 14, 2)
            .is_empty()
    );
}

#[test]
fn committed_dynamic_output_has_range_and_cell_producer_edges_on_mutation() {
    let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());
    engine
        .set_cell_value("Sheet1", 1, 8, LiteralValue::Number(30.0))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 15, 2, parse("={1,10;2,20;3,H1}").unwrap())
        .unwrap();
    engine.stage_loaded_formula_authorship("Sheet1", 15, 2, FormulaAuthorship::dynamic_array());
    engine.evaluate_all().unwrap();
    for (col, formula) in [(1, "=VLOOKUP(3,B16:C17,2,FALSE)"), (4, "=C17")] {
        engine
            .set_cell_formula("Sheet1", 1, col, parse(formula).unwrap())
            .unwrap();
    }
    engine
        .set_cell_value("Sheet1", 1, 8, LiteralValue::Number(60.0))
        .unwrap();
    let producer = vertex(&engine, "Sheet1", 15, 2);
    for col in [1, 4] {
        assert!(
            RangeVirtualDepProvider::get_virtual_deps(&engine, vertex(&engine, "Sheet1", 1, col))
                .contains(&producer)
        );
    }
    engine.evaluate_all().unwrap();
    for col in [1, 4] {
        assert_eq!(
            engine.get_cell_value("Sheet1", 1, col),
            Some(LiteralValue::Number(60.0))
        );
    }
}

#[test]
fn declared_follower_dependencies_preserve_two_producer_cycle() {
    let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());
    for (row, col, formula) in [(1, 1, "=SUM(D2:D2)"), (1, 4, "=SUM(A2:A2)")] {
        engine
            .set_cell_formula("Sheet1", row, col, parse(formula).unwrap())
            .unwrap();
        engine.stage_loaded_formula_authorship(
            "Sheet1",
            row,
            col,
            FormulaAuthorship::cse_array(FormulaFence::new(row, col, row + 1, col)),
        );
    }
    let a = vertex(&engine, "Sheet1", 1, 1);
    let d = vertex(&engine, "Sheet1", 1, 4);
    assert!(RangeVirtualDepProvider::get_virtual_deps(&engine, a).contains(&d));
    assert!(RangeVirtualDepProvider::get_virtual_deps(&engine, d).contains(&a));
    let result = engine.evaluate_all();
    assert!(
        matches!(result, Err(ref error) if error.kind == formualizer_common::ExcelErrorKind::Circ)
            || matches!(engine.get_cell_value("Sheet1",1,1),Some(LiteralValue::Error(ref error)) if error.kind == formualizer_common::ExcelErrorKind::Circ),
        "expected circular dependency error, got {result:?}"
    );
}

#[test]
fn declared_self_follower_dependency_is_a_cycle() {
    let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());
    engine
        .set_cell_formula("Sheet1", 1, 1, parse("=SUM(A2:A2)").unwrap())
        .unwrap();
    engine.stage_loaded_formula_authorship(
        "Sheet1",
        1,
        1,
        FormulaAuthorship::cse_array(FormulaFence::new(1, 1, 2, 1)),
    );
    let a = vertex(&engine, "Sheet1", 1, 1);
    assert!(RangeVirtualDepProvider::get_virtual_deps(&engine, a).contains(&a));
    let result = engine.evaluate_all();
    assert!(
        matches!(result, Err(ref error) if error.kind == formualizer_common::ExcelErrorKind::Circ)
            || matches!(engine.get_cell_value("Sheet1",1,1),Some(LiteralValue::Error(ref error)) if error.kind == formualizer_common::ExcelErrorKind::Circ),
        "expected cycle, got {result:?}"
    );
}

#[test]
fn existing_declared_output_consumers_enter_dirty_closure_before_evaluation() {
    for cross_sheet in [false, true] {
        let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());
        let producer_sheet = if cross_sheet { "Child" } else { "Sheet1" };
        engine
            .set_cell_value(producer_sheet, 1, 8, LiteralValue::Number(30.0))
            .unwrap();
        engine
            .set_cell_formula(producer_sheet, 15, 2, parse("={1,10;2,20;3,H1}").unwrap())
            .unwrap();
        engine.stage_loaded_formula_authorship(
            producer_sheet,
            15,
            2,
            FormulaAuthorship::cse_array(FormulaFence::new(15, 2, 17, 3)),
        );
        let prefix = if cross_sheet { "Child!" } else { "" };
        for (col, formula) in [
            (1, format!("=VLOOKUP(3,{prefix}B16:C17,2,FALSE)")),
            (4, format!("={prefix}C17")),
            (5, "=IFERROR(A1,-1)".into()),
            (6, "=E1*2".into()),
        ] {
            engine
                .set_cell_formula("Sheet1", 1, col, parse(&formula).unwrap())
                .unwrap();
        }
        engine.evaluate_all().unwrap();
        for input in [
            LiteralValue::Number(60.0),
            LiteralValue::Error(formualizer_common::ExcelError::new(
                formualizer_common::ExcelErrorKind::Na,
            )),
            LiteralValue::Number(90.0),
        ] {
            engine
                .set_cell_value(producer_sheet, 1, 8, input.clone())
                .unwrap();
            let producer = vertex(&engine, producer_sheet, 15, 2);
            assert!(engine.graph.is_dirty(producer));
            for col in [1, 4, 5, 6] {
                assert!(
                    engine.graph.is_dirty(vertex(&engine, "Sheet1", 1, col)),
                    "consumer {col} absent from pre-evaluation dirty closure"
                );
                if col == 1 || col == 4 {
                    assert!(
                        RangeVirtualDepProvider::get_virtual_deps(
                            &engine,
                            vertex(&engine, "Sheet1", 1, col)
                        )
                        .contains(&producer)
                    );
                }
            }
            engine.evaluate_all().unwrap();
            for col in [1, 4] {
                assert_eq!(engine.get_cell_value("Sheet1", 1, col), Some(input.clone()));
            }
            let expected = match input {
                LiteralValue::Number(n) => n,
                _ => -1.0,
            };
            assert_eq!(
                engine.get_cell_value("Sheet1", 1, 5),
                Some(LiteralValue::Number(expected))
            );
            assert_eq!(
                engine.get_cell_value("Sheet1", 1, 6),
                Some(LiteralValue::Number(expected * 2.0))
            );
        }
    }
}
