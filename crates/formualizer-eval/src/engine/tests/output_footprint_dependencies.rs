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

#[test]
fn dynamic_saved_hints_do_not_clip_and_new_readers_settle_in_one_public_call() {
    for cross_sheet in [false, true] {
        for hint_rows in [None, Some(1), Some(8)] {
            for targeted in [false, true] {
                if targeted && matches!(hint_rows, None | Some(1)) {
                    continue;
                } // unknown target footprint deliberately unqualified
                let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());
                let sheet = if cross_sheet { "Child" } else { "Sheet1" };
                engine
                    .set_cell_value(sheet, 1, 8, LiteralValue::Number(4.0))
                    .unwrap();
                engine
                    .set_cell_formula(sheet, 15, 2, parse("=SEQUENCE(H1,1,10,10)").unwrap())
                    .unwrap();
                engine.stage_loaded_formula_authorship(
                    sheet,
                    15,
                    2,
                    match hint_rows {
                        Some(n) => FormulaAuthorship::dynamic_array_with_saved_extent(
                            FormulaFence::new(15, 2, 14 + n, 2),
                        ),
                        None => FormulaAuthorship::dynamic_array(),
                    },
                );
                let prefix = if cross_sheet { "Child!" } else { "" };
                for (col, f) in [
                    (1, format!("=SUM({prefix}B17:B18)")),
                    (4, format!("={prefix}B18")),
                    (5, "=A1+D1".into()),
                    (6, "=E1*2".into()),
                    (9, format!("={prefix}B20")),
                    (10, "=I1*2".into()),
                ] {
                    engine
                        .set_cell_formula("Sheet1", 1, col, parse(&f).unwrap())
                        .unwrap();
                }
                for (height, sum, last) in [
                    (4.0, 70.0, 40.0),
                    (2.0, 0.0, 0.0),
                    (6.0, 70.0, 40.0),
                    (1.0, 0.0, 0.0),
                ] {
                    engine
                        .set_cell_value(sheet, 1, 8, LiteralValue::Number(height))
                        .unwrap();
                    if targeted {
                        engine.evaluate_until(&[("Sheet1", 1, 6)]).unwrap();
                    } else {
                        engine.evaluate_all().unwrap();
                    }
                    for (col, n) in [
                        (1, sum),
                        (4, last),
                        (5, sum + last),
                        (6, (sum + last) * 2.0),
                    ] {
                        assert_eq!(
                            engine.get_cell_value("Sheet1", 1, col),
                            Some(LiteralValue::Number(n)),
                            "cross={cross_sheet} hint={hint_rows:?} target={targeted} height={height} col={col}"
                        );
                    }
                    if !targeted {
                        let expanded = if height >= 6.0 { 60.0 } else { 0.0 };
                        for (col, n) in [(9, expanded), (10, expanded * 2.0)] {
                            assert_eq!(
                                engine.read_cell_value("Sheet1", 1, col),
                                Some(LiteralValue::Number(n))
                            );
                        }
                    }
                    assert_eq!(
                        engine.get_cell_value(sheet, 14 + height as u32, 2),
                        Some(LiteralValue::Number(height * 10.0))
                    );
                }
            }
        }
    }
}

#[test]
fn oversized_dynamic_hint_does_not_create_a_phantom_cycle_and_overwrite_clears_hint() {
    let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());
    engine
        .set_cell_formula("Sheet1", 1, 1, parse("={1;2}").unwrap())
        .unwrap();
    engine.stage_loaded_formula_authorship(
        "Sheet1",
        1,
        1,
        FormulaAuthorship::dynamic_array_with_saved_extent(FormulaFence::new(1, 1, 8, 1)),
    );
    engine
        .set_cell_formula("Sheet1", 5, 1, parse("=SUM(A1:A2)").unwrap())
        .unwrap();
    // The stale saved extent intersects this real formula; it is not an actual output/cycle edge.
    engine.evaluate_all().unwrap();
    assert_eq!(
        engine.get_cell_value("Sheet1", 5, 1),
        Some(LiteralValue::Number(3.0))
    );
    let sheet = engine.graph.sheet_id("Sheet1").unwrap();
    assert!(
        !engine
            .graph
            .potential_output_anchors_in_region(sheet, 7, 0, 7, 0)
            .is_empty()
    );
    engine
        .set_cell_formula("Sheet1", 1, 1, parse("=9").unwrap())
        .unwrap();
    assert!(
        engine
            .graph
            .potential_output_anchors_in_region(sheet, 7, 0, 7, 0)
            .is_empty()
    );
    engine.stage_loaded_formula_authorship(
        "Sheet1",
        1,
        1,
        FormulaAuthorship::dynamic_array_with_saved_extent(FormulaFence::new(1, 1, 8, 1)),
    );
    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Number(9.0))
        .unwrap();
    assert!(
        engine
            .graph
            .potential_output_anchors_in_region(sheet, 7, 0, 7, 0)
            .is_empty()
    );
}

#[test]
fn dynamic_error_recovery_and_runtime_reads_preserve_pending_downstream_work() {
    for parallel in [false, true] {
        for cross in [false, true] {
            let mut engine = Engine::new(
                TestWorkbook::new(),
                EvalConfig {
                    enable_parallel: parallel,
                    ..EvalConfig::default()
                },
            );
            let sheet = if cross { "Child" } else { "Sheet1" };
            engine
                .set_cell_value(sheet, 1, 8, LiteralValue::Number(3.0))
                .unwrap();
            engine
                .set_cell_formula(
                    sheet,
                    15,
                    2,
                    parse("=IF(H1<0,NA(),SEQUENCE(H1,1,10,10))").unwrap(),
                )
                .unwrap();
            engine.stage_loaded_formula_authorship(
                sheet,
                15,
                2,
                FormulaAuthorship::dynamic_array_with_saved_extent(FormulaFence::new(15, 2, 18, 2)),
            );
            let prefix = if cross { "Child!" } else { "" };
            for (col, f) in [
                (1, format!("=SUM({prefix}B16:B18)")),
                (4, format!("={prefix}B17")),
                (5, "=IFERROR(A1+D1,-1)".into()),
                (6, "=E1*2".into()),
                (7, "=INDIRECT(\"E1\")*3".into()),
            ] {
                engine
                    .set_cell_formula("Sheet1", 1, col, parse(&f).unwrap())
                    .unwrap();
            }
            for (height, sum, direct, sink) in [
                (3.0, 50.0, 30.0, 160.0),
                (-1.0, 0.0, 0.0, 0.0),
                (4.0, 90.0, 30.0, 240.0),
                (2.0, 20.0, 0.0, 40.0),
            ] {
                engine
                    .set_cell_value(sheet, 1, 8, LiteralValue::Number(height))
                    .unwrap();
                engine.evaluate_all().unwrap();
                for (col, n) in [(1, sum), (4, direct), (6, sink), (7, sink * 1.5)] {
                    assert_eq!(
                        engine.read_cell_value("Sheet1", 1, col),
                        Some(LiteralValue::Number(n)),
                        "parallel={parallel} cross={cross} height={height} col={col}"
                    );
                }
            }
        }
    }
}

#[test]
fn oversized_dynamic_hint_yields_to_real_input_dependencies() {
    let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());
    engine
        .set_cell_formula("Sheet1", 5, 1, parse("=7").unwrap())
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 1, parse("=SEQUENCE(2,1,A5,1)").unwrap())
        .unwrap();
    engine.stage_loaded_formula_authorship(
        "Sheet1",
        1,
        1,
        FormulaAuthorship::dynamic_array_with_saved_extent(FormulaFence::new(1, 1, 8, 1)),
    );
    engine.evaluate_all().unwrap();
    assert_eq!(
        engine.read_cell_value("Sheet1", 1, 1),
        Some(LiteralValue::Number(7.0))
    );
    assert_eq!(
        engine.read_cell_value("Sheet1", 2, 1),
        Some(LiteralValue::Number(8.0))
    );
}

#[test]
fn actual_dynamic_self_and_cross_producer_cycles_remain_circular() {
    for two in [false, true] {
        let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());
        engine
            .set_cell_formula(
                "Sheet1",
                15,
                2,
                parse(if two {
                    "={1,2;3,4}+0*F16"
                } else {
                    "={1,2;3,4}+0*C16"
                })
                .unwrap(),
            )
            .unwrap();
        engine.stage_loaded_formula_authorship(
            "Sheet1",
            15,
            2,
            FormulaAuthorship::dynamic_array_with_saved_extent(FormulaFence::new(15, 2, 16, 3)),
        );
        if two {
            engine
                .set_cell_formula("Sheet1", 15, 5, parse("={1,2;3,4}+0*C16").unwrap())
                .unwrap();
            engine.stage_loaded_formula_authorship(
                "Sheet1",
                15,
                5,
                FormulaAuthorship::dynamic_array_with_saved_extent(FormulaFence::new(15, 5, 16, 6)),
            );
        }
        engine
            .set_cell_formula("Sheet1", 1, 1, parse("=IFERROR(B15,-1)").unwrap())
            .unwrap();
        engine.evaluate_all().unwrap();
        assert_eq!(
            engine.read_cell_value("Sheet1", 1, 1),
            Some(LiteralValue::Number(-1.0))
        );
        assert!(
            matches!(engine.read_cell_value("Sheet1",15,2),Some(LiteralValue::Error(e)) if e.kind == formualizer_common::ExcelErrorKind::Circ)
        );
        engine
            .set_cell_formula("Sheet1", 15, 2, parse("={1,2;3,4}").unwrap())
            .unwrap();
        engine.stage_loaded_formula_authorship(
            "Sheet1",
            15,
            2,
            FormulaAuthorship::dynamic_array_with_saved_extent(FormulaFence::new(15, 2, 16, 3)),
        );
        engine.evaluate_all().unwrap();
        assert_eq!(
            engine.read_cell_value("Sheet1", 1, 1),
            Some(LiteralValue::Number(1.0))
        );
    }
}

/// A single-cell CSE formula publishes only into its own anchor cell —
/// `finalize_cse_result` truncates it to a scalar — so it must not register a
/// declared-output interval. Its reader is still ordered after it by the
/// ordinary cell edge. Multi-cell fences keep their entry.
#[test]
fn single_cell_cse_registers_no_declared_output_but_still_orders_its_reader() {
    let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());

    // A1: single-cell CSE. B1 reads it through a range that covers A1.
    engine
        .set_cell_formula("Sheet1", 1, 1, parse("=SUM({2,3})").unwrap())
        .unwrap();
    engine.stage_loaded_formula_authorship(
        "Sheet1",
        1,
        1,
        FormulaAuthorship::cse_array(FormulaFence::new(1, 1, 1, 1)),
    );
    // A3: multi-cell CSE, which must keep its declared-output entry.
    engine
        .set_cell_formula("Sheet1", 3, 1, parse("={1,2;3,4}").unwrap())
        .unwrap();
    engine.stage_loaded_formula_authorship(
        "Sheet1",
        3,
        1,
        FormulaAuthorship::cse_array(FormulaFence::new(3, 1, 4, 2)),
    );
    engine
        .set_cell_formula("Sheet1", 1, 2, parse("=SUM($A$1:$A$1)").unwrap())
        .unwrap();
    engine.evaluate_all().unwrap();

    let a1 = vertex(&engine, "Sheet1", 1, 1);
    let a3 = vertex(&engine, "Sheet1", 3, 1);

    // The single-cell fence contributes no declared-output anchor...
    assert!(
        !engine
            .graph
            .output_anchors_in_region(0, 0, 0, 0, 0)
            .contains(&a1),
        "a single-cell CSE fence must not register a declared-output interval"
    );
    // ...while the multi-cell fence still does.
    assert!(
        engine
            .graph
            .output_anchors_in_region(0, 2, 0, 3, 1)
            .contains(&a3),
        "a multi-cell CSE fence must keep its declared-output interval"
    );

    // The reader still evaluates after the producer and sees its value.
    assert_eq!(
        engine.read_cell_value("Sheet1", 1, 1),
        Some(LiteralValue::Number(5.0))
    );
    assert_eq!(
        engine.read_cell_value("Sheet1", 1, 2),
        Some(LiteralValue::Number(5.0))
    );
}
