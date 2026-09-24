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

type Calls = Arc<Mutex<Vec<LiteralValue>>>;

fn engine_with_counters() -> (Engine<TestWorkbook>, Calls, Calls) {
    engine_with_counters_mode(true)
}

/// `region_nodes` selects the region-node virtual-dependency build (the
/// default) or the per-cell build; the recheck compares in the same form.
fn engine_with_counters_mode(region_nodes: bool) -> (Engine<TestWorkbook>, Calls, Calls) {
    let src: Calls = Arc::new(Mutex::new(Vec::new()));
    let sink: Calls = Arc::new(Mutex::new(Vec::new()));
    let wb = TestWorkbook::new()
        .with_function(Arc::new(SrcFn(src.clone())))
        .with_function(Arc::new(SinkFn(sink.clone())));
    let config = EvalConfig {
        enable_virtual_dep_telemetry: true,
        ..EvalConfig::default()
    }
    .with_virtual_region_nodes(region_nodes);
    (Engine::new(wb, config), src, sink)
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
    for region_nodes in [true, false] {
        saved_extent_reader_runs_after_anchor(region_nodes);
    }
}

fn saved_extent_reader_runs_after_anchor(region_nodes: bool) {
    let (mut engine, src, sink) = engine_with_counters_mode(region_nodes);
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
    for region_nodes in [true, false] {
        let (mut engine, src, sink) = engine_with_counters_mode(region_nodes);
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

        let (mut engine, src, sink) = engine_with_counters_mode(region_nodes);
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
    for region_nodes in [true, false] {
        let (mut engine, src, sink) = engine_with_counters_mode(region_nodes);
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
    for region_nodes in [true, false] {
        let (mut engine, _src, sink) = engine_with_counters_mode(region_nodes);
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
    for region_nodes in [true, false] {
        let (mut engine, _src, sink) = engine_with_counters_mode(region_nodes);
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
    for region_nodes in [true, false] {
        let (mut engine, _src, sink) = engine_with_counters_mode(region_nodes);
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
    for region_nodes in [true, false] {
        // Precedent H1 reads A15 (inside the saved extent A1:B15).
        let (mut engine, src, _sink) = engine_with_counters_mode(region_nodes);
        set(&mut engine, 1, 8, "=A15+3");
        set(&mut engine, 1, 1, "=SRC(H1)");
        stage_anchor(&mut engine, 15);
        let result = engine.evaluate_all().unwrap();
        assert_eq!(value(&engine, 1, 8), num(3.0));
        assert_eq!(value(&engine, 3, 2), num(30.0));
        assert!(is_blank(&value(&engine, 15, 1)));
        assert_eq!(calls(&src).len(), 1);
        assert_eq!(result.cycle_errors, 0);

        // The anchor reads B15 directly.
        let (mut engine, src, _sink) = engine_with_counters_mode(region_nodes);
        set(&mut engine, 1, 1, "=SRC(B15+4)");
        stage_anchor(&mut engine, 15);
        let result = engine.evaluate_all().unwrap();
        assert_eq!(value(&engine, 4, 2), num(40.0));
        assert_eq!(calls(&src).len(), 1);
        assert_eq!(result.cycle_errors, 0);
    }
}
