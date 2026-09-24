//! OT-291: a dynamic-array anchor loaded with a saved multi-cell extent (no
//! CSE fence) orders the readers of cells inside that extent after itself on
//! the first evaluation, before any spill has been committed.
//!
//! Without that ordering a reader of a follower cell can run before the
//! anchor, see a blank, and be re-run by the post-pass replan once the anchor
//! commits its spill; a non-volatile custom function downstream of the reader
//! then fires twice per recalculation.

use crate::engine::{Engine, EvalConfig, FormulaAuthorship, FormulaFence};
use crate::test_workbook::TestWorkbook;
use formualizer_common::{ExcelErrorKind, LiteralValue};
use formualizer_parse::parser::parse;
use std::sync::{Arc, LazyLock, Mutex};

static ANY_ONE: LazyLock<Vec<crate::args::ArgSchema>> =
    LazyLock::new(|| vec![crate::args::ArgSchema::any()]);

/// `SRC(n)`: returns an `n` x 2 array `[i, 10*i]` (rows 1..=n) and counts calls.
#[derive(Debug)]
struct SrcFn(Arc<Mutex<Vec<LiteralValue>>>);
impl crate::function::Function for SrcFn {
    fn caps(&self) -> crate::function::FnCaps {
        crate::function::FnCaps::PURE
    }
    fn name(&self) -> &'static str {
        "SRC"
    }
    fn min_args(&self) -> usize {
        1
    }
    fn arg_schema(&self) -> &'static [crate::args::ArgSchema] {
        &ANY_ONE[..]
    }
    fn eval<'a, 'b, 'c>(
        &self,
        args: &'c [crate::traits::ArgumentHandle<'a, 'b>],
        _ctx: &dyn crate::traits::FunctionContext<'b>,
    ) -> Result<crate::traits::CalcValue<'b>, formualizer_common::ExcelError> {
        let arg = args[0].value()?.into_literal();
        self.0.lock().unwrap().push(arg.clone());
        let n = match arg {
            LiteralValue::Number(n) => n as i64,
            LiteralValue::Int(n) => n,
            _ => 0,
        };
        if n <= 0 {
            return Ok(crate::traits::CalcValue::Scalar(LiteralValue::Number(0.0)));
        }
        let rows = (1..=n)
            .map(|i| {
                vec![
                    LiteralValue::Number(i as f64),
                    LiteralValue::Number((10 * i) as f64),
                ]
            })
            .collect::<Vec<_>>();
        Ok(crate::traits::CalcValue::Scalar(LiteralValue::Array(rows)))
    }
}

/// `SINK(x)`: records its argument and returns it; counts calls.
#[derive(Debug)]
struct SinkFn(Arc<Mutex<Vec<LiteralValue>>>);
impl crate::function::Function for SinkFn {
    fn caps(&self) -> crate::function::FnCaps {
        crate::function::FnCaps::PURE
    }
    fn name(&self) -> &'static str {
        "SINK"
    }
    fn min_args(&self) -> usize {
        1
    }
    fn arg_schema(&self) -> &'static [crate::args::ArgSchema] {
        &ANY_ONE[..]
    }
    fn eval<'a, 'b, 'c>(
        &self,
        args: &'c [crate::traits::ArgumentHandle<'a, 'b>],
        _ctx: &dyn crate::traits::FunctionContext<'b>,
    ) -> Result<crate::traits::CalcValue<'b>, formualizer_common::ExcelError> {
        let arg = args[0].value()?.into_literal();
        self.0.lock().unwrap().push(arg.clone());
        Ok(crate::traits::CalcValue::Scalar(arg))
    }
}

static ANY_TWO: LazyLock<Vec<crate::args::ArgSchema>> =
    LazyLock::new(|| vec![crate::args::ArgSchema::any(), crate::args::ArgSchema::any()]);

fn as_number(v: &LiteralValue) -> f64 {
    match v {
        LiteralValue::Number(n) => *n,
        LiteralValue::Int(n) => *n as f64,
        _ => 0.0,
    }
}

/// `BLK(x, k)`: an XCALL-like custom function that returns the 3 x 1 array
/// `[x*k; k; x+k]` and records `k` (the call-site key) on each call.
#[derive(Debug)]
struct BlkFn(Arc<Mutex<Vec<LiteralValue>>>);
impl crate::function::Function for BlkFn {
    fn caps(&self) -> crate::function::FnCaps {
        crate::function::FnCaps::PURE
    }
    fn name(&self) -> &'static str {
        "BLK"
    }
    fn min_args(&self) -> usize {
        2
    }
    fn arg_schema(&self) -> &'static [crate::args::ArgSchema] {
        &ANY_TWO[..]
    }
    fn eval<'a, 'b, 'c>(
        &self,
        args: &'c [crate::traits::ArgumentHandle<'a, 'b>],
        _ctx: &dyn crate::traits::FunctionContext<'b>,
    ) -> Result<crate::traits::CalcValue<'b>, formualizer_common::ExcelError> {
        let x = as_number(&args[0].value()?.into_literal());
        let k = as_number(&args[1].value()?.into_literal());
        self.0.lock().unwrap().push(LiteralValue::Number(k));
        Ok(crate::traits::CalcValue::Scalar(LiteralValue::Array(vec![
            vec![LiteralValue::Number(x * k)],
            vec![LiteralValue::Number(k)],
            vec![LiteralValue::Number(x + k)],
        ])))
    }
}

type Calls = Arc<Mutex<Vec<LiteralValue>>>;

/// Every `(region_nodes, parallel)` combination: `region_nodes` selects the
/// region-node virtual-dependency build (the default) or the per-cell build
/// (the recheck compares in the same form); `parallel` is `enable_parallel`
/// (default on), which evaluates a multi-vertex layer on the rayon pool and
/// commits its spills only after every member has evaluated.
const MODES: [(bool, bool); 4] = [(true, true), (true, false), (false, true), (false, false)];

fn engine_with_counters_mode(
    region_nodes: bool,
    parallel: bool,
) -> (Engine<TestWorkbook>, Calls, Calls) {
    let (engine, src, sink, _blk) = engine_with_all_counters(region_nodes, parallel);
    (engine, src, sink)
}

fn engine_with_all_counters(
    region_nodes: bool,
    parallel: bool,
) -> (Engine<TestWorkbook>, Calls, Calls, Calls) {
    let src: Calls = Arc::new(Mutex::new(Vec::new()));
    let sink: Calls = Arc::new(Mutex::new(Vec::new()));
    let blk: Calls = Arc::new(Mutex::new(Vec::new()));
    let wb = TestWorkbook::new()
        .with_function(Arc::new(SrcFn(src.clone())))
        .with_function(Arc::new(SinkFn(sink.clone())))
        .with_function(Arc::new(BlkFn(blk.clone())));
    let config = EvalConfig {
        enable_virtual_dep_telemetry: true,
        ..EvalConfig::default()
    }
    .with_virtual_region_nodes(region_nodes)
    .with_parallel(parallel);
    let engine = Engine::new(wb, config);
    if parallel {
        assert!(
            engine.thread_pool().is_some(),
            "parallel mode needs the pool"
        );
    }
    (engine, src, sink, blk)
}

fn set(engine: &mut Engine<TestWorkbook>, row: u32, col: u32, formula: &str) {
    engine
        .set_cell_formula("Sheet1", row, col, parse(formula).unwrap())
        .unwrap();
}

/// Author A1 as a loaded dynamic-array anchor whose saved extent is A1:B<rows>.
fn stage_anchor(engine: &mut Engine<TestWorkbook>, rows: u32) {
    engine.stage_loaded_formula_authorship(
        "Sheet1",
        1,
        1,
        FormulaAuthorship::dynamic_array_with_saved_extent(FormulaFence::new(1, 1, rows, 2)),
    );
}

fn value(engine: &Engine<TestWorkbook>, row: u32, col: u32) -> Option<LiteralValue> {
    engine.get_cell_value("Sheet1", row, col)
}

fn num(n: f64) -> Option<LiteralValue> {
    Some(LiteralValue::Number(n))
}

fn is_blank(v: &Option<LiteralValue>) -> bool {
    matches!(v, None | Some(LiteralValue::Empty))
        || matches!(v, Some(LiteralValue::Number(n)) if *n == 0.0)
}

fn calls(c: &Calls) -> Vec<LiteralValue> {
    c.lock().unwrap().clone()
}

/// Readers first (lower vertex ids), then the anchor behind a formula
/// precedent so that nothing but an ordering edge from the saved extent can
/// put the readers after it.
fn build_t1(engine: &mut Engine<TestWorkbook>, reader_row: u32) {
    set(engine, 1, 4, &format!("=A{reader_row}")); // D1
    set(engine, 1, 5, "=SINK(D1)"); // E1
    set(engine, 1, 8, "=15"); // H1: SRC's size
    set(engine, 1, 1, "=SRC(H1)"); // A1
    stage_anchor(engine, 15);
}

#[test]
fn saved_extent_reader_runs_after_anchor_so_downstream_custom_fn_fires_once() {
    for (region_nodes, parallel) in MODES {
        saved_extent_reader_runs_after_anchor(region_nodes, parallel);
    }
}

fn saved_extent_reader_runs_after_anchor(region_nodes: bool, parallel: bool) {
    let (mut engine, src, sink) = engine_with_counters_mode(region_nodes, parallel);
    build_t1(&mut engine, 15);

    let result = engine.evaluate_all().unwrap();
    let telemetry = engine.last_virtual_dep_telemetry().clone();

    eprintln!(
        "SRC calls: {:?}\nSINK calls: {:?}\nreplan_iterations: {}\ncycle_errors: {}",
        calls(&src),
        calls(&sink),
        telemetry.replan_iterations,
        result.cycle_errors
    );
    assert_eq!(value(&engine, 15, 1), num(15.0));
    assert_eq!(value(&engine, 1, 4), num(15.0));
    assert_eq!(value(&engine, 1, 5), num(15.0));
    assert_eq!(calls(&src).len(), 1, "anchor custom function fires once");
    assert_eq!(
        calls(&sink),
        vec![LiteralValue::Number(15.0)],
        "downstream custom function fires once, with the committed follower value"
    );
    assert_eq!(telemetry.replan_iterations, 0, "no replan pass");
    assert_eq!(result.cycle_errors, 0);
}

/// Same through a range read (the Rev Suite shape: the reader's block covers
/// the follower) and through the demand-driven `evaluate_cells` path.
#[test]
fn saved_extent_range_reader_and_demand_path_fire_downstream_once() {
    for (region_nodes, parallel) in MODES {
        let (mut engine, src, sink) = engine_with_counters_mode(region_nodes, parallel);
        set(&mut engine, 1, 4, "=SUM(A14:B15)"); // D1
        set(&mut engine, 1, 5, "=SINK(D1)"); // E1
        set(&mut engine, 1, 8, "=15");
        set(&mut engine, 1, 1, "=SRC(H1)");
        stage_anchor(&mut engine, 15);
        let result = engine.evaluate_all().unwrap();
        assert_eq!(value(&engine, 1, 4), num(14.0 + 140.0 + 15.0 + 150.0));
        assert_eq!(calls(&src).len(), 1);
        assert_eq!(calls(&sink), vec![LiteralValue::Number(319.0)]);
        assert_eq!(engine.last_virtual_dep_telemetry().replan_iterations, 0);
        assert_eq!(result.cycle_errors, 0);

        let (mut engine, src, sink) = engine_with_counters_mode(region_nodes, parallel);
        build_t1(&mut engine, 15);
        let out = engine.evaluate_cells(&[("Sheet1", 1, 5)]).unwrap();
        assert_eq!(out, vec![num(15.0)]);
        assert_eq!(calls(&src).len(), 1);
        assert_eq!(calls(&sink), vec![LiteralValue::Number(15.0)]);
    }
}

/// (a) The actual spill is smaller than the saved extent: a reader of a cell
/// inside the saved extent but outside the spill reads blank, with no cycle
/// and no extra pass; growing the spill later reaches it.
#[test]
fn saved_extent_larger_than_actual_spill_reads_blank_without_replan() {
    for (region_nodes, parallel) in MODES {
        let (mut engine, src, sink) = engine_with_counters_mode(region_nodes, parallel);
        build_t1(&mut engine, 15);
        set(&mut engine, 1, 8, "=3"); // SRC(3): spill A1:B3 only
        let result = engine.evaluate_all().unwrap();
        assert_eq!(value(&engine, 3, 2), num(30.0));
        assert!(is_blank(&value(&engine, 15, 1)));
        assert!(is_blank(&value(&engine, 1, 4)));
        assert_eq!(calls(&src).len(), 1);
        assert_eq!(calls(&sink).len(), 1);
        assert_eq!(engine.last_virtual_dep_telemetry().replan_iterations, 0);
        assert_eq!(result.cycle_errors, 0);

        set(&mut engine, 1, 8, "=15");
        let result = engine.evaluate_all().unwrap();
        assert_eq!(value(&engine, 1, 4), num(15.0));
        assert_eq!(value(&engine, 1, 5), num(15.0));
        assert_eq!(calls(&sink).last(), Some(&LiteralValue::Number(15.0)));
        assert_eq!(result.cycle_errors, 0);
    }
}

/// (b) The actual spill is larger than the saved extent: a reader outside the
/// saved extent is not ordered by it, and the existing replan path still
/// delivers the committed value.
#[test]
fn actual_spill_larger_than_saved_extent_still_reaches_reader() {
    for (region_nodes, parallel) in MODES {
        let (mut engine, _src, sink) = engine_with_counters_mode(region_nodes, parallel);
        set(&mut engine, 1, 4, "=A15");
        set(&mut engine, 1, 5, "=SINK(D1)");
        set(&mut engine, 1, 8, "=15");
        set(&mut engine, 1, 1, "=SRC(H1)");
        stage_anchor(&mut engine, 5);
        let result = engine.evaluate_all().unwrap();
        assert_eq!(value(&engine, 15, 2), num(150.0));
        assert_eq!(value(&engine, 1, 4), num(15.0));
        assert_eq!(value(&engine, 1, 5), num(15.0));
        assert_eq!(calls(&sink).last(), Some(&LiteralValue::Number(15.0)));
        assert_eq!(result.cycle_errors, 0);
    }
}

/// (c) A blocked spill: the anchor shows #SPILL!, readers inside the saved
/// extent read the blocking value or blank, no cycle.
#[test]
fn blocked_spill_inside_saved_extent_reads_blockers_without_cycle() {
    for (region_nodes, parallel) in MODES {
        let (mut engine, _src, sink) = engine_with_counters_mode(region_nodes, parallel);
        build_t1(&mut engine, 15);
        engine
            .set_cell_value("Sheet1", 10, 1, LiteralValue::Number(99.0))
            .unwrap(); // A10 plain value blocks the spill
        set(&mut engine, 12, 2, "=H1*2"); // B12 formula also inside the extent
        set(&mut engine, 2, 4, "=A10"); // D2
        set(&mut engine, 3, 4, "=B12"); // D3
        let result = engine.evaluate_all().unwrap();
        match value(&engine, 1, 1) {
            Some(LiteralValue::Error(e)) => assert_eq!(e.kind, ExcelErrorKind::Spill),
            other => panic!("expected #SPILL!, got {other:?}"),
        }
        assert!(is_blank(&value(&engine, 1, 4)));
        assert_eq!(value(&engine, 2, 4), num(99.0));
        assert_eq!(value(&engine, 3, 4), num(30.0));
        assert_eq!(calls(&sink).len(), 1);
        assert_eq!(result.cycle_errors, 0);

        // Unblock and re-dirty the anchor: the spill lands and the reader
        // follows. (Clearing the blockers alone does not re-run the anchor on
        // this engine, with or without the OT-291 ordering; out of scope here.)
        engine
            .set_cell_value("Sheet1", 10, 1, LiteralValue::Empty)
            .unwrap();
        engine
            .set_cell_value("Sheet1", 12, 2, LiteralValue::Empty)
            .unwrap();
        set(&mut engine, 1, 8, "=15+0");
        let result = engine.evaluate_all().unwrap();
        assert_eq!(value(&engine, 1, 1), num(1.0));
        assert_eq!(value(&engine, 1, 4), num(15.0));
        assert_eq!(value(&engine, 2, 4), num(10.0));
        assert_eq!(calls(&sink).last(), Some(&LiteralValue::Number(15.0)));
        assert_eq!(result.cycle_errors, 0);
    }
}

/// (d) Re-spill with a different size on later recalculations.
#[test]
fn respill_with_different_sizes_keeps_readers_correct() {
    for (region_nodes, parallel) in MODES {
        let (mut engine, _src, sink) = engine_with_counters_mode(region_nodes, parallel);
        build_t1(&mut engine, 15);
        set(&mut engine, 2, 4, "=A20"); // D2: outside the saved extent
        engine.evaluate_all().unwrap();
        assert_eq!(value(&engine, 1, 4), num(15.0));
        assert!(is_blank(&value(&engine, 2, 4)));

        set(&mut engine, 1, 8, "=5");
        let result = engine.evaluate_all().unwrap();
        assert_eq!(value(&engine, 5, 2), num(50.0));
        assert!(is_blank(&value(&engine, 15, 1)));
        assert!(is_blank(&value(&engine, 1, 4)));
        assert!(is_blank(&value(&engine, 2, 4)));
        assert_eq!(result.cycle_errors, 0);

        set(&mut engine, 1, 8, "=20");
        let result = engine.evaluate_all().unwrap();
        assert_eq!(value(&engine, 1, 4), num(15.0));
        assert_eq!(value(&engine, 2, 4), num(20.0));
        assert_eq!(value(&engine, 1, 5), num(15.0));
        assert_eq!(calls(&sink).last(), Some(&LiteralValue::Number(15.0)));
        assert_eq!(result.cycle_errors, 0);
    }
}

/// (e) A cell inside the saved extent read by the anchor itself or by one of
/// its precedents is not a cycle when the actual spill does not reach it: the
/// ordering hint that would close the loop is dropped.
#[test]
fn saved_extent_read_by_anchor_or_its_precedent_is_not_a_cycle() {
    for (region_nodes, parallel) in MODES {
        // Precedent H1 reads A15 (inside the saved extent A1:B15).
        let (mut engine, src, _sink) = engine_with_counters_mode(region_nodes, parallel);
        set(&mut engine, 1, 8, "=A15+3");
        set(&mut engine, 1, 1, "=SRC(H1)");
        stage_anchor(&mut engine, 15);
        let (a1, h1) = (vid(&engine, 1, 1), vid(&engine, 1, 8));
        order_hint_test_hook::take();
        let result = engine.evaluate_all().unwrap();
        // Review point 2: the hint H1 -> A1 closes a loop with the real edge
        // A1 -> H1; it is offered, dropped, and the returned schedule has no
        // cycle, so cycle-error stamping never sees it.
        let builds = order_hint_test_hook::take();
        assert!(
            builds.iter().any(|b| b.offered.contains(&(h1, a1))),
            "{builds:?}"
        );
        for b in &builds {
            assert!(!b.applied.contains(&(h1, a1)), "{b:?}");
            assert_eq!(b.cycles_in_result, 0, "{b:?}");
        }
        assert!(!is_circ(&value(&engine, 1, 1)));
        assert!(!is_circ(&value(&engine, 1, 8)));
        assert_eq!(value(&engine, 1, 8), num(3.0));
        assert_eq!(value(&engine, 3, 2), num(30.0));
        assert!(is_blank(&value(&engine, 15, 1)));
        assert_eq!(calls(&src).len(), 1);
        assert_eq!(result.cycle_errors, 0);

        // The anchor reads B15 directly.
        let (mut engine, src, _sink) = engine_with_counters_mode(region_nodes, parallel);
        set(&mut engine, 1, 1, "=SRC(B15+4)");
        stage_anchor(&mut engine, 15);
        let a1 = vid(&engine, 1, 1);
        order_hint_test_hook::take();
        let result = engine.evaluate_all().unwrap();
        // Review point 1: the anchor's read of its own saved extent never
        // becomes a hint (no self edge is offered or applied).
        for b in order_hint_test_hook::take() {
            assert!(!b.offered.contains(&(a1, a1)), "{b:?}");
            assert_eq!(b.cycles_in_result, 0, "{b:?}");
        }
        assert!(!is_circ(&value(&engine, 1, 1)));
        assert_eq!(value(&engine, 4, 2), num(40.0));
        assert_eq!(calls(&src).len(), 1);
        assert_eq!(result.cycle_errors, 0);
    }
}

// ---------------------------------------------------------------------------
// Review follow-up (OT-291 r2).
// ---------------------------------------------------------------------------

use crate::engine::eval::order_hint_test_hook;
use crate::engine::{FormulaFence as Fence, VertexId};

/// Stage `(row, col)` as a loaded dynamic-array anchor with saved extent
/// `(row, col)..(end_row, end_col)`.
fn stage_extent(engine: &mut Engine<TestWorkbook>, row: u32, col: u32, end_row: u32, end_col: u32) {
    engine.stage_loaded_formula_authorship(
        "Sheet1",
        row,
        col,
        FormulaAuthorship::dynamic_array_with_saved_extent(Fence::new(row, col, end_row, end_col)),
    );
}

fn vid(engine: &Engine<TestWorkbook>, row: u32, col: u32) -> VertexId {
    *engine
        .graph
        .get_vertex_id_for_address(&engine.graph.make_cell_ref("Sheet1", row, col))
        .unwrap()
}

fn is_circ(v: &Option<LiteralValue>) -> bool {
    matches!(v, Some(LiteralValue::Error(e)) if e.kind == ExcelErrorKind::Circ)
}

fn sorted_numbers(c: &Calls) -> Vec<f64> {
    let mut out: Vec<f64> = calls(c).iter().map(as_number).collect();
    out.sort_by(f64::total_cmp);
    out
}

/// A reader of the hinted follower that is itself a dynamic-array anchor
/// (custom function, multi-cell spill) with its own downstream reader: every
/// custom function fires once and there is no replan.
#[test]
fn dynamic_anchor_reader_with_downstream_reader_fires_each_custom_fn_once() {
    for (region_nodes, parallel) in MODES {
        let mode = format!("region_nodes={region_nodes} parallel={parallel}");
        let (mut engine, src, sink, blk) = engine_with_all_counters(region_nodes, parallel);
        set(&mut engine, 1, 12, "=SINK(J3)"); // L1: reads the reader-anchor's follower
        set(&mut engine, 1, 10, "=BLK(A15,1)"); // J1: reader of A1's saved extent
        stage_extent(&mut engine, 1, 10, 3, 10);
        set(&mut engine, 1, 8, "=15"); // H1
        set(&mut engine, 1, 1, "=SRC(H1)"); // A1
        stage_anchor(&mut engine, 15);

        let result = engine.evaluate_all().unwrap();
        let replans = engine.last_virtual_dep_telemetry().replan_iterations;
        assert_eq!(value(&engine, 1, 10), num(15.0), "{mode}");
        assert_eq!(value(&engine, 3, 10), num(16.0), "{mode}");
        assert_eq!(value(&engine, 1, 12), num(16.0), "{mode}");
        assert_eq!(calls(&src).len(), 1, "{mode}: SRC once");
        assert_eq!(
            calls(&blk),
            vec![LiteralValue::Number(1.0)],
            "{mode}: BLK once"
        );
        assert_eq!(
            calls(&sink),
            vec![LiteralValue::Number(16.0)],
            "{mode}: SINK once"
        );
        assert_eq!(replans, 0, "{mode}: no replan");
        assert_eq!(result.cycle_errors, 0, "{mode}");
    }
}

/// Builds the Rev Suite shape: A1 = SRC(H1) with saved extent A1:B15; D12 =
/// A15 (the `Input!B12 = AN20` reader); three sibling XCALL-like dynamic
/// anchors G3/H3/I3 read the block D3:E20 containing D12; M1..M3 read a
/// follower of each sibling. Readers are created first (lower vertex ids).
fn build_rev_siblings(engine: &mut Engine<TestWorkbook>) {
    set(engine, 1, 13, "=SINK(G5)"); // M1
    set(engine, 2, 13, "=SINK(H5)"); // M2
    set(engine, 3, 13, "=SINK(I5)"); // M3
    for (col, k) in [(7, 1), (8, 2), (9, 3)] {
        set(engine, 3, col, &format!("=BLK(SUM($D$3:$E$20),{k})"));
        stage_extent(engine, 3, col, 5, col);
    }
    engine
        .set_cell_value("Sheet1", 3, 5, LiteralValue::Number(2.0))
        .unwrap(); // E3: a plain input in the block
    set(engine, 12, 4, "=A15"); // D12
    set(engine, 1, 8, "=15"); // H1
    set(engine, 1, 1, "=SRC(H1)"); // A1
    stage_anchor(engine, 15);
}

/// The Rev shape: each sibling custom function fires once, in both parallel
/// modes, with no replan (review point 4: in parallel mode a layer evaluates
/// every member, taking and retiring its invalidation token, before any
/// member's spill is committed, so a sibling commit cannot fail another
/// sibling's retirement).
#[test]
fn sibling_custom_fn_anchors_reading_hinted_follower_fire_once_without_replan() {
    for (region_nodes, parallel) in MODES {
        let mode = format!("region_nodes={region_nodes} parallel={parallel}");
        let (mut engine, src, sink, blk) = engine_with_all_counters(region_nodes, parallel);
        build_rev_siblings(&mut engine);
        let result = engine.evaluate_all().unwrap();
        let replans = engine.last_virtual_dep_telemetry().replan_iterations;
        eprintln!(
            "{mode}: replan_iterations={replans} SRC={} BLK={:?} SINK={:?}",
            calls(&src).len(),
            sorted_numbers(&blk),
            sorted_numbers(&sink)
        );
        // SUM(D3:E20) = 15 + 2 = 17.
        for (col, k) in [(7, 1.0), (8, 2.0), (9, 3.0)] {
            assert_eq!(value(&engine, 3, col), num(17.0 * k), "{mode} col={col}");
            assert_eq!(value(&engine, 5, col), num(17.0 + k), "{mode} col={col}");
        }
        assert_eq!(calls(&src).len(), 1, "{mode}: SRC once");
        assert_eq!(
            sorted_numbers(&blk),
            vec![1.0, 2.0, 3.0],
            "{mode}: each BLK once"
        );
        assert_eq!(
            sorted_numbers(&sink),
            vec![18.0, 19.0, 20.0],
            "{mode}: each SINK once"
        );
        assert_eq!(replans, 0, "{mode}: replan_iterations");
        assert_eq!(result.cycle_errors, 0, "{mode}");
    }
}

/// Review point 3 (not adopted, measured): after the anchor has committed a
/// spill, including one that shrank below the saved extent, the saved extent
/// is still part of the anchor's invalidation footprint: every commit counts
/// the anchor cell as changed, and the dirty closure from the anchor walks
/// `planning_extent` (`DependencyGraph::collect_output_dependents`), so a
/// reader inside the stale saved extent is invalidated by each commit. The
/// hint therefore keeps ordering that reader after the anchor; without it
/// the reader runs first, stays pending, and the replan re-runs it and its
/// downstream custom function (measured with the hint dropped once a spill
/// was committed: replan_iterations 1 on each later recalc, D1 and E1
/// pending; with the hint: 0).
#[test]
fn saved_extent_hint_persists_after_commit_so_persisted_sessions_fire_once() {
    for (region_nodes, parallel) in MODES {
        let mode = format!("region_nodes={region_nodes} parallel={parallel}");
        let (mut engine, _src, sink) = engine_with_counters_mode(region_nodes, parallel);
        set(&mut engine, 1, 4, "=A15+H2"); // D1
        set(&mut engine, 1, 5, "=SINK(D1)"); // E1
        engine
            .set_cell_value("Sheet1", 2, 8, LiteralValue::Number(0.0))
            .unwrap(); // H2
        set(&mut engine, 1, 8, "=15"); // H1
        set(&mut engine, 1, 1, "=SRC(H1)"); // A1
        stage_anchor(&mut engine, 15);
        let (a1, d1) = (vid(&engine, 1, 1), vid(&engine, 1, 4));
        let applied = |builds: &[order_hint_test_hook::HintBuild]| {
            builds.iter().any(|b| b.applied.contains(&(d1, a1)))
        };

        order_hint_test_hook::take();
        engine.evaluate_all().unwrap();
        assert!(applied(&order_hint_test_hook::take()), "{mode}");
        assert_eq!(value(&engine, 1, 4), num(15.0), "{mode}");
        assert_eq!(calls(&sink), vec![LiteralValue::Number(15.0)], "{mode}");
        assert_eq!(engine.last_virtual_dep_telemetry().replan_iterations, 0);

        // Shrink to A1:B3. (The committed footprint A1:B15 gave D1 a hard
        // edge that the pass removes; that recheck replan predates OT-291
        // and also happens without a saved extent, so it is not asserted.)
        set(&mut engine, 1, 8, "=3");
        engine
            .set_cell_value("Sheet1", 2, 8, LiteralValue::Number(1.0))
            .unwrap();
        let result = engine.evaluate_all().unwrap();
        order_hint_test_hook::take();
        assert_eq!(value(&engine, 3, 2), num(30.0), "{mode}");
        assert!(is_blank(&value(&engine, 15, 1)), "{mode}");
        assert_eq!(value(&engine, 1, 4), num(1.0), "{mode}");
        assert_eq!(result.cycle_errors, 0, "{mode}");

        // Committed A1:B3 and A1:B5 no longer cover A15, the stale saved
        // extent does: the hint still orders D1 after A1, no replan, and the
        // downstream custom function fires exactly once per recalculation.
        for (h1, h2, rows) in [("=4", 2.0, 4u32), ("=5", 3.0, 5)] {
            set(&mut engine, 1, 8, h1);
            engine
                .set_cell_value("Sheet1", 2, 8, LiteralValue::Number(h2))
                .unwrap();
            let before = calls(&sink).len();
            let result = engine.evaluate_all().unwrap();
            assert!(applied(&order_hint_test_hook::take()), "{mode} {h1}");
            assert_eq!(value(&engine, rows, 2), num(10.0 * rows as f64), "{mode}");
            assert_eq!(value(&engine, 1, 4), num(h2), "{mode} {h1}");
            assert_eq!(
                calls(&sink)[before..],
                [LiteralValue::Number(h2)],
                "{mode} {h1}: SINK once"
            );
            assert_eq!(
                engine.last_virtual_dep_telemetry().replan_iterations,
                0,
                "{mode} {h1}"
            );
            assert_eq!(result.cycle_errors, 0, "{mode}");
        }
    }
}

/// Two anchors each read a cell inside the other's stale saved extent. The
/// two hints form a loop by themselves; both are dropped before the schedule
/// is returned (review point 2), so no build hands the cycle stamping a hint
/// edge, there is no #CIRC!, and the outcome is 3086cbc2's (receipt
/// `base_probe_3086cbc2.txt`): the same values, and the same pre-existing
/// non-convergence error, which comes from each anchor's commit invalidating
/// the other through its saved extent (`collect_output_dependents`), not from
/// the hints. When the actual spills do reach the cells read, the loop is
/// real and stays circular exactly as on 3086cbc2 (one cycle, #CIRC! on both).
#[test]
fn anchors_reading_each_others_saved_extent_do_not_become_circular() {
    for (region_nodes, parallel) in MODES {
        let mode = format!("region_nodes={region_nodes} parallel={parallel}");
        // Stale: SRC(3) spills A1:B3 and D1:E3; saved extents are 15 rows.
        let (mut engine, _src, _sink) = engine_with_counters_mode(region_nodes, parallel);
        set(&mut engine, 1, 8, "=3"); // H1
        set(&mut engine, 2, 8, "=3"); // H2
        set(&mut engine, 1, 1, "=SRC(H1+0*E14)"); // A1 reads E14 (D1's extent)
        stage_anchor(&mut engine, 15);
        set(&mut engine, 1, 4, "=SRC(H2+0*A14)"); // D1 reads A14 (A1's extent)
        stage_extent(&mut engine, 1, 4, 15, 5);
        let (a1, d1) = (vid(&engine, 1, 1), vid(&engine, 1, 4));
        order_hint_test_hook::take();
        let result = engine.evaluate_all();
        let builds = order_hint_test_hook::take();
        match &result {
            Ok(result) => assert_eq!(result.cycle_errors, 0, "{mode}"),
            // 3086cbc2 returns this same error for this workbook in every
            // mode; it is not introduced by the hints.
            Err(e) => assert!(
                e.message
                    .as_deref()
                    .is_some_and(|m| m.contains("did not converge")),
                "{mode}: {e:?}"
            ),
        }
        assert!(
            builds
                .iter()
                .any(|b| b.offered.contains(&(a1, d1)) && b.offered.contains(&(d1, a1))),
            "{mode}: both hints offered: {builds:?}"
        );
        for b in &builds {
            assert!(
                !b.applied.contains(&(a1, d1)) && !b.applied.contains(&(d1, a1)),
                "{mode}: a hint loop is never applied: {b:?}"
            );
            assert_eq!(b.cycles_in_result, 0, "{mode}: {b:?}");
        }
        assert!(!is_circ(&value(&engine, 1, 1)), "{mode}");
        assert!(!is_circ(&value(&engine, 1, 4)), "{mode}");
        for (row, col, n) in [(1, 1, 1.0), (3, 2, 30.0), (1, 4, 1.0), (3, 5, 30.0)] {
            assert_eq!(value(&engine, row, col), num(n), "{mode} r{row}c{col}");
        }
        assert!(is_blank(&value(&engine, 14, 1)), "{mode}");
        assert!(is_blank(&value(&engine, 14, 5)), "{mode}");

        // Real: SRC(15) reaches A14 and E14, a genuine mutual dependency.
        let (mut engine, _src, _sink) = engine_with_counters_mode(region_nodes, parallel);
        set(&mut engine, 1, 8, "=15");
        set(&mut engine, 2, 8, "=15");
        set(&mut engine, 1, 1, "=SRC(H1+0*E14)");
        stage_anchor(&mut engine, 15);
        set(&mut engine, 1, 4, "=SRC(H2+0*A14)");
        stage_extent(&mut engine, 1, 4, 15, 5);
        let result = engine.evaluate_all().unwrap();
        assert_eq!(result.cycle_errors, 1, "{mode}");
        assert!(
            is_circ(&value(&engine, 1, 1)),
            "{mode}: {:?}",
            value(&engine, 1, 1)
        );
        assert!(
            is_circ(&value(&engine, 1, 4)),
            "{mode}: {:?}",
            value(&engine, 1, 4)
        );
    }
}
