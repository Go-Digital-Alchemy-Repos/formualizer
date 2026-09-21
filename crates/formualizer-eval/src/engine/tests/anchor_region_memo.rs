//! Declared-output anchor-region lookups are memoised across the virtual-
//! dependency build passes of one schedule build, so their cost is
//! O(distinct regions) rather than O(reads), and the memo is dropped as soon
//! as anything that could change an answer moves.

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

/// The memo now outlives a single build pass, so its epoch guard — not its
/// scope — is what keeps it honest. Registering a fence must invalidate it.
#[test]
fn registering_a_fence_invalidates_the_anchor_region_memo() {
    use crate::engine::virtual_deps::AnchorRegionMemo;

    let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());
    engine
        .set_cell_formula("Sheet1", 10, 4, parse("=SUM($A$1:$A$4)").unwrap())
        .unwrap();
    engine.evaluate_all().unwrap();

    // Region A1:A4, zero-based, as the builder asks it.
    let sheet = engine.graph.sheet_id("Sheet1").expect("sheet");
    let region = (sheet, 0u32, 0u32, 3u32, 0u32);

    let memo = AnchorRegionMemo::default();
    memo.refresh(&engine);
    assert!(
        memo.output_anchors(&engine, region).is_empty(),
        "no declared output covers A1:A4 yet"
    );

    engine
        .set_cell_formula("Sheet1", 1, 1, parse("={1;2;3;4}").unwrap())
        .unwrap();
    engine.stage_loaded_formula_authorship(
        "Sheet1",
        1,
        1,
        FormulaAuthorship::cse_array(FormulaFence::new(1, 1, 4, 1)),
    );
    let producer = engine
        .graph
        .get_vertex_id_for_address(&engine.graph.make_cell_ref("Sheet1", 1, 1))
        .copied()
        .expect("producer vertex");

    // The same memo object, reused across the mutation: refresh must drop the
    // answer it cached before the fence existed.
    memo.refresh(&engine);
    assert!(
        memo.output_anchors(&engine, region).contains(&producer),
        "a fence registered after the memo cached a region must invalidate it"
    );
}

/// The dirty-formula region scan is memoised alongside the anchor sets, so a
/// warm memo must produce exactly the edges an unmemoised build produces —
/// and, because the cache holds only the raw kind-filtered list, a formula
/// that goes clean while the memo stays warm must drop out of the edges.
#[test]
fn the_region_scan_memo_reproduces_the_unmemoised_edges() {
    use crate::engine::VertexId;
    use crate::engine::virtual_deps::{AnchorRegionMemo, RangeVirtualDepProvider};

    // Above the default `range_expansion_limit` of 64, so the read stays a
    // compressed range dependency and goes through the region scan.
    const PRODUCERS: u32 = 100;
    const READERS: u32 = 8;

    let mut engine = Engine::new(TestWorkbook::new(), EvalConfig::default());
    for row in 1..=PRODUCERS {
        engine
            .set_cell_formula("Sheet1", row, 1, parse("=ROW()").unwrap())
            .unwrap();
    }
    for row in 1..=READERS {
        engine
            .set_cell_formula("Sheet1", row, 4, parse("=SUM($A$1:$A$100)").unwrap())
            .unwrap();
    }

    let vertex_at = |engine: &Engine<TestWorkbook>, row: u32, col: u32| -> VertexId {
        engine
            .graph
            .get_vertex_id_for_address(&engine.graph.make_cell_ref("Sheet1", row, col))
            .copied()
            .expect("vertex")
    };

    let candidates: Vec<VertexId> = (1..=PRODUCERS)
        .map(|row| vertex_at(&engine, row, 1))
        .chain((1..=READERS).map(|row| vertex_at(&engine, row, 4)))
        .collect();

    // An unmemoised build: `get_virtual_deps` hands every call a fresh memo.
    let unmemoised = |engine: &Engine<TestWorkbook>| -> Vec<(VertexId, Vec<VertexId>)> {
        candidates
            .iter()
            .map(|&v| (v, RangeVirtualDepProvider::get_virtual_deps(engine, v)))
            .collect()
    };

    // One memo reused across two builds, as the builder reuses it across the
    // passes of one schedule build.
    let memo = AnchorRegionMemo::default();
    let memoised = |engine: &Engine<TestWorkbook>| -> Vec<(VertexId, Vec<VertexId>)> {
        memo.refresh(engine);
        candidates
            .iter()
            .map(|&v| {
                (
                    v,
                    RangeVirtualDepProvider::get_virtual_deps_memoized(engine, v, &memo),
                )
            })
            .collect()
    };

    let cold = memoised(&engine);
    let warm = memoised(&engine);
    let reference = unmemoised(&engine);
    assert_eq!(cold, warm, "a warm memo must not change the edge map");
    assert_eq!(
        cold, reference,
        "the memoised edge map must equal the unmemoised one"
    );

    let first_producer = vertex_at(&engine, 1, 1);
    let first_reader = vertex_at(&engine, 1, 4);
    let edges_of = |map: &[(VertexId, Vec<VertexId>)], v: VertexId| -> Vec<VertexId> {
        map.iter()
            .find(|(k, _)| *k == v)
            .map(|(_, deps)| deps.clone())
            .expect("candidate present")
    };
    assert!(
        edges_of(&cold, first_reader).contains(&first_producer),
        "a dirty producer in the read range must be an edge"
    );

    // Mark one producer clean without touching the graph's shape, so the memo
    // keeps its cached region lists (the epochs do not move). The dirty filter
    // lives outside the cache, so the edge must disappear anyway.
    engine.graph.set_dirty(first_producer, false);

    let after = memoised(&engine);
    let after_reference = unmemoised(&engine);
    assert_eq!(
        after, after_reference,
        "the warm memo must still track the dirty flags"
    );
    assert!(
        !edges_of(&after, first_reader).contains(&first_producer),
        "a producer marked clean between builds must not remain an edge"
    );
}
