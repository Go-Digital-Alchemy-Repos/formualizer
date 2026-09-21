//! Declared-output anchor-region lookups are memoised per virtual-dependency
//! build pass, so their cost is O(distinct regions) rather than O(reads).

use crate::engine::{Engine, EvalConfig, FormulaAuthorship, FormulaFence};
use crate::test_workbook::TestWorkbook;
use formualizer_common::LiteralValue;
use formualizer_parse::parser::parse;

/// Multi-cell CSE fences, so they really do register declared-output
/// intervals, plus many readers of only two distinct blocks.
const FENCES: u32 = 200;
const READERS: u32 = 4_000;

fn build_engine() -> Engine<TestWorkbook> {
    let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());
    // Column A: many two-cell CSE fences, one per pair of rows.
    for i in 0..FENCES {
        let row = 1 + i * 2;
        engine
            .set_cell_formula("Sheet1", row, 1, parse("={1;2}").unwrap())
            .unwrap();
        engine.stage_loaded_formula_authorship(
            "Sheet1",
            row,
            1,
            FormulaAuthorship::cse_array(FormulaFence::new(row, 1, row + 1, 1)),
        );
    }
    // Columns D and E: many readers of the same two blocks.
    for row in 1..=READERS {
        engine
            .set_cell_formula("Sheet1", row, 4, parse("=SUM($A$1:$A$200)").unwrap())
            .unwrap();
        engine
            .set_cell_formula("Sheet1", row, 5, parse("=SUM($A$201:$A$400)").unwrap())
            .unwrap();
    }
    engine
}

// NOTE: a perf-shape test asserting the query count is O(distinct regions)
// rather than O(reads) is still owed. A first attempt on this fixture counted
// ~1 query per read with the memo in place, i.e. it did not discriminate, so
// it is not committed rather than being tuned until it passed. The evidence
// for the memo today is the measured builder time on the Rev FIA child
// (builder_ms 23,576 -> 8,185 on the first evaluation) with a byte-identical
// vdep edge count; see the round's measurement archive.

/// The memo lives for exactly one build pass, so a fence registered between
/// two builds must be visible to the second one.
#[test]
fn a_fence_added_between_builds_is_seen_by_the_next_build() {
    let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());
    engine
        .set_cell_formula("Sheet1", 10, 4, parse("=SUM($A$1:$A$4)").unwrap())
        .unwrap();
    engine.evaluate_all().unwrap();
    let reader = engine
        .graph
        .get_vertex_id_for_address(&engine.graph.make_cell_ref("Sheet1", 10, 4))
        .copied()
        .expect("reader vertex");
    assert!(
        crate::engine::virtual_deps::RangeVirtualDepProvider::get_virtual_deps(&engine, reader)
            .is_empty(),
        "no declared outputs exist yet"
    );

    // Register a multi-cell CSE fence covering A1:A4 after the first build.
    engine
        .set_cell_formula("Sheet1", 1, 1, parse("={1;2;3;4}").unwrap())
        .unwrap();
    engine.stage_loaded_formula_authorship(
        "Sheet1",
        1,
        1,
        FormulaAuthorship::cse_array(FormulaFence::new(1, 1, 4, 1)),
    );
    engine.evaluate_all().unwrap();

    let producer = engine
        .graph
        .get_vertex_id_for_address(&engine.graph.make_cell_ref("Sheet1", 1, 1))
        .copied()
        .expect("producer vertex");
    // The producer must be dirty for `get_virtual_deps` to report the edge.
    engine
        .set_cell_formula("Sheet1", 1, 1, parse("={5;6;7;8}").unwrap())
        .unwrap();
    let deps =
        crate::engine::virtual_deps::RangeVirtualDepProvider::get_virtual_deps(&engine, reader);
    assert!(
        deps.contains(&producer),
        "the fence added after the first build must be visible to the next build"
    );
    engine.evaluate_all().unwrap();
    assert_eq!(
        engine.get_cell_value("Sheet1", 10, 4),
        Some(LiteralValue::Number(26.0))
    );
}
