//! GOD-383 Trial A T1: `Engine::eval_stats` counters are reset per
//! `evaluate_all` call and name the replan trigger (R1 C1: a re-evaluated
//! registered spill anchor invalidates a reader that sits in a static SCC,
//! whose members cannot retire from `pending_output_invalidations`).

use crate::engine::{CancelToken, CycleConfig, CycleDetection, CyclePolicy, Engine, EvalConfig};
use crate::test_workbook::TestWorkbook;
use formualizer_common::LiteralValue;
use formualizer_parse::parser::parse;

fn engine() -> Engine<TestWorkbook> {
    let mut cfg = EvalConfig::default().with_cycle(CycleConfig {
        detection: CycleDetection::Runtime,
        policy: CyclePolicy::Error,
    });
    // Keep every call on the exact schedule-and-walk path.
    cfg.speculative_chain = false;
    Engine::new(TestWorkbook::new(), cfg)
}

fn formula(engine: &mut Engine<TestWorkbook>, row: u32, col: u32, f: &str) {
    engine
        .set_cell_formula("Sheet1", row, col, parse(f).expect("parse"))
        .expect("set formula");
}

fn value(engine: &mut Engine<TestWorkbook>, row: u32, col: u32, v: f64) {
    engine
        .set_cell_value("Sheet1", row, col, LiteralValue::Number(v))
        .expect("set value");
}

fn num(engine: &Engine<TestWorkbook>, row: u32, col: u32) -> f64 {
    match engine.get_cell_value("Sheet1", row, col) {
        Some(LiteralValue::Number(n)) => n,
        Some(LiteralValue::Int(i)) => i as f64,
        other => panic!("expected number at r{row}c{col}, got {other:?}"),
    }
}

/// A1 spills SEQUENCE(3,1,B1) over A1:A3; C1/D1 are a static (phantom)
/// SCC whose live path reads the spill follower A2 and the input B1.
fn spill_feeding_scc(engine: &mut Engine<TestWorkbook>) {
    value(engine, 1, 2, 1.0);
    formula(engine, 1, 1, "=SEQUENCE(3,1,B1)");
    formula(engine, 1, 3, "=IF(FALSE,D1,A2+B1)");
    formula(engine, 1, 4, "=C1");
}

fn eval(engine: &mut Engine<TestWorkbook>, cancellable: bool) {
    if cancellable {
        engine.evaluate_all_cancellable(CancelToken::new()).unwrap();
    } else {
        engine.evaluate_all().unwrap();
    }
}

#[test]
fn eval_stats_reset_per_call_and_zero_when_nothing_changed() {
    for cancellable in [false, true] {
        let mut engine = engine();
        spill_feeding_scc(&mut engine);
        eval(&mut engine, cancellable);
        let first = engine.eval_stats().clone();
        assert_eq!(first.outcome, "ok");
        assert_eq!(first.path, "full");
        assert!(first.passes >= 1);
        assert!(first.computed_vertices > 0);
        assert_eq!(first.pass_to_evaluate.len() as u64, first.passes);
        assert_eq!(first.pass_outcome.len() as u64, first.passes);
        assert_eq!(
            first.entry,
            if cancellable {
                "evaluate_all_cancellable"
            } else {
                "evaluate_all"
            }
        );

        // Settle, then a call with no edit: nothing pending, no replan, and
        // the counters describe only this call (not first + this).
        eval(&mut engine, cancellable);
        eval(&mut engine, cancellable);
        let idle = engine.eval_stats().clone();
        assert_eq!(idle.replan_iterations, 0, "{idle:?}");
        assert_eq!(idle.pending_at_drain_total, 0, "{idle:?}");
        assert_eq!(idle.spill_clear_count, 0, "{idle:?}");
        assert_eq!(idle.spill_commit_count, 0, "{idle:?}");
        assert_eq!(idle.inv_pending_added, 0, "{idle:?}");
        assert!(idle.passes <= 1, "{idle:?}");
        assert!(idle.schedule_builds <= 1, "{idle:?}");
        assert_eq!(idle.pass_to_evaluate.len() as u64, idle.passes);
        assert_eq!(idle.output_footprint_epoch_delta, 0, "{idle:?}");
        let names: Vec<&str> = idle.to_pairs().iter().map(|(k, _)| *k).collect();
        assert!(names.contains(&"replan_iterations"));
        assert!(names.contains(&"ns_layer_eval"));
        assert!(names.contains(&"pass_pending_at_drain"));
    }
}

#[test]
fn eval_stats_report_spill_invalidation_replan_into_scc() {
    for cancellable in [false, true] {
        let mut engine = engine();
        spill_feeding_scc(&mut engine);
        eval(&mut engine, cancellable);
        eval(&mut engine, cancellable);
        assert_eq!(num(&engine, 1, 3), 3.0);

        // Spill values change: anchor re-evaluates, clear + commit invalidate
        // the SCC reader, whose members cannot retire in this pass.
        value(&mut engine, 1, 2, 5.0);
        eval(&mut engine, cancellable);
        let s = engine.eval_stats().clone();
        assert_eq!(num(&engine, 1, 3), 11.0);
        assert_eq!(num(&engine, 1, 4), 11.0);
        assert!(s.replan_iterations > 0, "{s:?}");
        assert!(s.pending_at_drain_total > 0, "{s:?}");
        assert!(s.pass_pending_at_drain[0] > 0, "{s:?}");
        assert!(s.spill_clear_count >= 1, "{s:?}");
        assert!(s.spill_commit_count >= 1, "{s:?}");
        assert!(
            s.inv_spill_clear_calls + s.inv_commit_multi_calls > 0,
            "{s:?}"
        );
        assert!(s.inv_pending_added > 0, "{s:?}");
        assert!(s.token_scc_blocked() > 0, "{s:?}");
        assert!(s.output_footprint_epoch_delta > 0, "{s:?}");
        assert!(s.scc_static_first_pass >= 1, "{s:?}");
        assert!(s.scc_tasks >= 2, "{s:?}");
        assert_ne!(s.pass_outcome[0], "converged", "{s:?}");
        assert_eq!(*s.pass_outcome.last().unwrap(), "converged", "{s:?}");
        assert_eq!(s.spill_commit_identical, 0, "{s:?}");
    }
}

#[test]
fn eval_stats_count_identical_spill_recommit() {
    let mut engine = engine();
    value(&mut engine, 1, 2, 1.0);
    // INT(B1/100) is 0 for B1 in 1..99: the anchor re-evaluates on a B1
    // edit but re-commits the same footprint with the same values.
    formula(&mut engine, 1, 1, "=SEQUENCE(3,1,INT(B1/100))");
    formula(&mut engine, 1, 3, "=A2+B1");
    engine.evaluate_all().unwrap();
    engine.evaluate_all().unwrap();
    value(&mut engine, 1, 2, 2.0);
    engine.evaluate_all().unwrap();
    let s = engine.eval_stats().clone();
    assert_eq!(num(&engine, 1, 3), 3.0);
    assert_eq!(s.spill_clear_count, 1, "{s:?}");
    assert_eq!(s.spill_commit_count, 1, "{s:?}");
    assert_eq!(s.spill_commit_same_footprint, 1, "{s:?}");
    assert_eq!(s.spill_commit_identical, 1, "{s:?}");
}
