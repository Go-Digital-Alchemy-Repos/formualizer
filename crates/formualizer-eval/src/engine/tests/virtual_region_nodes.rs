//! PROTOTYPE (r8a) — per-region range nodes in the virtual-dependency schedule.
//!
//! The region-node path replaces "one virtual edge per dirty producer per
//! reader" with "producer -> region node" (once per region) plus "region node
//! -> reader" (once per reader). These tests pin the two properties the
//! substitution must preserve:
//!
//!  * the ordering constraint — every reader is emitted in a strictly later
//!    layer than every dirty producer inside a region it reads — holds with
//!    the flag on exactly as it does with the flag off;
//!  * cycles are still cycles. A reader that lies inside the region it reads
//!    keeps the old per-cell edges minus itself, so a self-referencing `SUM`
//!    still resolves to `#CIRC!` rather than becoming a spurious 2-cycle
//!    through the region node (or, worse, silently losing the cycle).

use crate::engine::scheduler::Scheduler;
use crate::engine::virtual_deps::{REGION_NODE_BASE, VirtualDepBuilder};
use crate::engine::{Engine, EvalConfig, VertexId};
use crate::test_workbook::TestWorkbook;
use formualizer_common::LiteralValue;
use formualizer_parse::parser::parse;
use rustc_hash::FxHashMap;

fn config(region_nodes: bool) -> EvalConfig {
    EvalConfig {
        virtual_region_nodes: region_nodes,
        ..Default::default()
    }
}

/// A sheet with several producers, two overlapping range readers and a reader
/// of the readers. Left dirty (never evaluated) so every formula vertex is a
/// schedule candidate.
fn overlapping_ranges_engine(region_nodes: bool) -> Engine<TestWorkbook> {
    let mut engine = Engine::new(TestWorkbook::new(), config(region_nodes));
    engine
        .set_cell_value("Sheet1", 1, 3, LiteralValue::Number(2.0))
        .unwrap(); // C1 = 2
    for row in 1..=6u32 {
        let f = parse(&format!("=$C$1*{row}")).unwrap();
        engine.set_cell_formula("Sheet1", row, 1, f).unwrap(); // A1:A6
    }
    // Two readers over overlapping slices of the same column.
    engine
        .set_cell_formula("Sheet1", 1, 2, parse("=SUM(A1:A4)").unwrap())
        .unwrap(); // B1
    engine
        .set_cell_formula("Sheet1", 2, 2, parse("=SUM(A3:A6)").unwrap())
        .unwrap(); // B2
    // A third reader over the exact same region as B1 (region node reuse).
    engine
        .set_cell_formula("Sheet1", 3, 2, parse("=COUNT(A1:A4)").unwrap())
        .unwrap(); // B3
    // Reader of the readers.
    engine
        .set_cell_formula("Sheet1", 5, 2, parse("=SUM(B1:B3)").unwrap())
        .unwrap(); // B5
    engine
}

/// vertex -> index of the layer it is emitted in.
fn layer_positions(layers: &[crate::engine::scheduler::Layer]) -> FxHashMap<VertexId, usize> {
    let mut out = FxHashMap::default();
    for (i, layer) in layers.iter().enumerate() {
        for &v in &layer.vertices {
            out.insert(v, i);
        }
    }
    out
}

#[test]
fn region_nodes_preserve_reader_after_producer_ordering() {
    // Ground truth for "who must precede whom": the per-cell virtual deps.
    let plain_engine = overlapping_ranges_engine(false);
    let candidates = plain_engine.graph.formula_vertices();
    assert!(candidates.len() >= 10, "expected a non-trivial candidate set");
    let (expected_vdeps, _) = VirtualDepBuilder::new(&plain_engine).build(&candidates);
    assert!(
        !expected_vdeps.is_empty(),
        "the fixture must produce virtual range dependencies"
    );

    let plain_schedule = Scheduler::new(&plain_engine.graph)
        .create_schedule_with_virtual(&candidates, &expected_vdeps)
        .expect("plain schedule");

    // Region-node schedule over the same candidate set.
    let region_engine = overlapping_ranges_engine(true);
    let region_candidates = region_engine.graph.formula_vertices();
    assert_eq!(
        candidates, region_candidates,
        "both engines must produce the same vertex ids for the comparison to mean anything"
    );
    let (vdeps, region_edges, _augmented, plan) =
        VirtualDepBuilder::new(&region_engine).build_regionized(&region_candidates);
    assert!(
        !plan.is_empty(),
        "the fixture must allocate at least one region node"
    );
    // Four distinct regions are read (A1:A4 by B1 and B3, A3:A6 by B2, B1:B3
    // by B5). Only A1:A4 has more than one reader, so exactly one relay node
    // survives; single-reader regions are inlined back to per-cell edges.
    assert_eq!(
        plan.node_ids().len(),
        1,
        "region nodes are per region and only kept when a region has >1 reader"
    );
    assert_eq!(
        region_edges.values().map(|r| r.len()).sum::<usize>(),
        2,
        "the surviving relay node is shared by its two readers"
    );

    let mut sched_vdeps = vdeps.clone();
    for (reader, regions) in region_edges.iter() {
        let slot = sched_vdeps.entry(*reader).or_default();
        slot.extend(regions.iter().copied());
        slot.sort_unstable();
        slot.dedup();
    }
    for (node, producers) in plan.producers.iter() {
        if !producers.is_empty() {
            sched_vdeps.insert(*node, producers.clone());
        }
    }
    let mut sched_vertices = region_candidates.clone();
    sched_vertices.extend(plan.node_ids());

    let region_schedule = Scheduler::new(&region_engine.graph)
        .create_schedule_with_virtual_synthetic(
            &sched_vertices,
            &sched_vdeps,
            VertexId::new(REGION_NODE_BASE),
        )
        .expect("region-node schedule");

    // No synthetic node survives into the emitted schedule.
    for layer in &region_schedule.layers {
        for &v in &layer.vertices {
            assert!(
                v.0 < REGION_NODE_BASE,
                "a synthetic region node leaked into an emitted layer"
            );
        }
    }

    // Both schedules must place every real candidate exactly once.
    let plain_pos = layer_positions(&plain_schedule.layers);
    let region_pos = layer_positions(&region_schedule.layers);
    for &v in &candidates {
        assert!(plain_pos.contains_key(&v), "plain schedule dropped {v:?}");
        assert!(region_pos.contains_key(&v), "region schedule dropped {v:?}");
    }

    // The ordering constraint itself, checked against the per-cell truth.
    for (&reader, producers) in expected_vdeps.iter() {
        for &producer in producers {
            if producer == reader {
                continue;
            }
            assert!(
                plain_pos[&producer] < plain_pos[&reader],
                "per-cell schedule violated reader-after-producer"
            );
            assert!(
                region_pos[&producer] < region_pos[&reader],
                "region-node schedule placed producer {producer:?} at layer {} \
                 but reader {reader:?} at layer {}",
                region_pos[&producer],
                region_pos[&reader],
            );
        }
    }
}

#[test]
fn region_nodes_preserve_computed_values_on_overlapping_ranges() {
    let cells = [
        (1u32, 2u32),
        (2, 2),
        (3, 2),
        (5, 2),
        (1, 1),
        (3, 1),
        (6, 1),
    ];
    let mut plain = overlapping_ranges_engine(false);
    plain.evaluate_all().expect("plain evaluate_all");
    let mut region = overlapping_ranges_engine(true);
    region.evaluate_all().expect("region evaluate_all");

    for (row, col) in cells {
        assert_eq!(
            plain.get_cell_value("Sheet1", row, col),
            region.get_cell_value("Sheet1", row, col),
            "value mismatch at r{row}c{col}"
        );
    }
    // Sanity: the fixture really does compute something.
    assert_eq!(
        plain.get_cell_value("Sheet1", 1, 2),
        Some(LiteralValue::Number(20.0)),
        "B1 = SUM(A1:A4) = 2*(1+2+3+4)"
    );
}

/// A reader that lies inside the region it reads must stay a cycle. With the
/// region-node path this is the self-overlap case: routing D1 through the node
/// for D1:D3 would make `D1 -> R -> D1`, so the prototype falls back to the
/// per-cell edges minus self — which is exactly what the per-cell path did, and
/// the real self-reference is still found.
#[test]
fn self_referencing_sum_is_still_circ_with_region_nodes() {
    for region_nodes in [false, true] {
        let mut engine = Engine::new(TestWorkbook::new(), config(region_nodes));
        engine
            .set_cell_value("Sheet1", 2, 4, LiteralValue::Number(1.0))
            .unwrap();
        engine
            .set_cell_formula("Sheet1", 3, 4, parse("=D2+1").unwrap())
            .unwrap(); // D3
        engine
            .set_cell_formula("Sheet1", 1, 4, parse("=SUM(D1:D3)").unwrap())
            .unwrap(); // D1 reads itself
        // Another reader of the same region that is NOT inside it.
        engine
            .set_cell_formula("Sheet1", 1, 5, parse("=SUM(D1:D3)").unwrap())
            .unwrap(); // E1

        engine.evaluate_all().expect("evaluate_all");
        match engine.get_cell_value("Sheet1", 1, 4) {
            Some(LiteralValue::Error(err)) => assert_eq!(
                err.kind,
                formualizer_common::ExcelErrorKind::Circ,
                "region_nodes={region_nodes}: expected #CIRC! at D1"
            ),
            other => panic!("region_nodes={region_nodes}: expected #CIRC! at D1, got {other:?}"),
        }
        // D3 is outside the cycle and must still compute.
        assert_eq!(
            engine.get_cell_value("Sheet1", 3, 4),
            Some(LiteralValue::Number(2.0)),
            "region_nodes={region_nodes}: D3 = D2+1"
        );
    }
}

/// The coarser relay node must not turn an indirect cycle into a wrong order.
///
/// `F1 = SUM(G1:G3)` reads a region whose dirty producer `G2` is transitively
/// downstream of `F1` itself (`G2 = H1`, `H1 = F1`). The per-cell path sees
/// `G2 -> F1` and `F1 -> ... -> G2` and reports a cycle; with region nodes the
/// same loop runs through the relay (`G2 -> R -> F1 -> H1 -> G2`), so Tarjan —
/// which walks the region nodes — must still find the SCC. Two readers keep the
/// relay node alive (a single-reader region is inlined back to per-cell edges).
#[test]
fn indirect_cycle_through_a_region_node_is_still_detected() {
    for region_nodes in [false, true] {
        let mut engine = Engine::new(TestWorkbook::new(), config(region_nodes));
        // G1 is a plain producer, G2 closes the loop back through H1.
        engine
            .set_cell_value("Sheet1", 1, 9, LiteralValue::Number(1.0))
            .unwrap(); // I1
        engine
            .set_cell_formula("Sheet1", 1, 7, parse("=I1").unwrap())
            .unwrap(); // G1
        engine
            .set_cell_formula("Sheet1", 2, 7, parse("=H1").unwrap())
            .unwrap(); // G2
        engine
            .set_cell_formula("Sheet1", 3, 7, parse("=I1").unwrap())
            .unwrap(); // G3
        engine
            .set_cell_formula("Sheet1", 1, 6, parse("=SUM(G1:G3)").unwrap())
            .unwrap(); // F1
        engine
            .set_cell_formula("Sheet1", 1, 8, parse("=F1").unwrap())
            .unwrap(); // H1 -> closes F1 -> H1 -> G2 -> (region) -> F1
        // A second reader of the same region so the relay node is not inlined.
        engine
            .set_cell_formula("Sheet1", 2, 6, parse("=COUNT(G1:G3)").unwrap())
            .unwrap(); // F2

        engine.evaluate_all().expect("evaluate_all");
        for (row, col, name) in [(1u32, 6u32, "F1"), (2, 7, "G2"), (1, 8, "H1")] {
            match engine.get_cell_value("Sheet1", row, col) {
                Some(LiteralValue::Error(err)) => assert_eq!(
                    err.kind,
                    formualizer_common::ExcelErrorKind::Circ,
                    "region_nodes={region_nodes}: expected #CIRC! at {name}"
                ),
                other => panic!(
                    "region_nodes={region_nodes}: expected #CIRC! at {name}, got {other:?}"
                ),
            }
        }
        // G1 and G3 are outside the loop and must still compute.
        assert_eq!(
            engine.get_cell_value("Sheet1", 3, 7),
            Some(LiteralValue::Number(1.0)),
            "region_nodes={region_nodes}: G3 = I1"
        );
    }
}
