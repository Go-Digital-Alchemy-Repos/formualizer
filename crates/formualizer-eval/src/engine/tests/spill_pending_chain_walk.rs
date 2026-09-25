//! GOD-383 Trial A T4: position-based pending (`FZ_SPILL_PENDING_BY_POSITION`,
//! toggle B) on the speculative-chain walk.
//!
//! A same-extent spill re-commit with changed values, during a chain walk,
//! feeding a phantom SCC and a downstream reader. The readers also read the
//! edited input, so they are in the walk's own set and scheduled after the
//! anchor. With B off the whole closure is pended and the request falls back
//! to the exact path and replans; with B on nothing is pended and the chain
//! finishes the request. Values are compared with B off after every step,
//! and a read-guard fallback (corrupted banked order) must give the same
//! values too.

use crate::engine::{CycleConfig, CycleDetection, CyclePolicy, Engine, EvalConfig, EvalStats};
use crate::function::{FnCaps, Function};
use crate::test_workbook::TestWorkbook;
use crate::traits::{ArgumentHandle, CalcValue, FunctionContext};
use formualizer_common::{ExcelError, LiteralValue};
use formualizer_parse::parser::parse;
use std::sync::{Arc, LazyLock};

type E = Engine<TestWorkbook>;

static THREE_ANY: LazyLock<Vec<crate::args::ArgSchema>> =
    LazyLock::new(|| vec![crate::args::ArgSchema::any(); 3]);

fn arg_num(arg: &ArgumentHandle<'_, '_>) -> Result<f64, ExcelError> {
    Ok(match arg.value()?.into_literal() {
        LiteralValue::Number(n) => n,
        LiteralValue::Int(i) => i as f64,
        _ => 0.0,
    })
}

/// `T4_RECT(a, b, x)`: fixed 3x2 array, cell 0 = `b`, cell 1 = `a`, cell `i`
/// = `i`; `x` is read and ignored.
#[derive(Debug)]
struct Rect3x2;

impl Function for Rect3x2 {
    fn caps(&self) -> FnCaps {
        FnCaps::PURE
    }
    fn name(&self) -> &'static str {
        "T4_RECT"
    }
    fn min_args(&self) -> usize {
        3
    }
    fn arg_schema(&self) -> &'static [crate::args::ArgSchema] {
        &THREE_ANY[..]
    }
    fn eval<'a, 'b, 'c>(
        &self,
        args: &'c [ArgumentHandle<'a, 'b>],
        _ctx: &dyn FunctionContext<'b>,
    ) -> Result<CalcValue<'b>, ExcelError> {
        let a = arg_num(&args[0])?;
        let b = arg_num(&args[1])?;
        let _x = arg_num(&args[2])?;
        let out = (0..3)
            .map(|r| {
                (0..2)
                    .map(|c| {
                        let i = r * 2 + c;
                        LiteralValue::Number(match i {
                            0 => b,
                            1 => a,
                            _ => i as f64,
                        })
                    })
                    .collect()
            })
            .collect();
        Ok(CalcValue::Scalar(LiteralValue::Array(out)))
    }
}

fn engine(by_position: bool) -> E {
    let mut cfg = EvalConfig::default().with_cycle(CycleConfig {
        detection: CycleDetection::Runtime,
        policy: CyclePolicy::Error,
    });
    cfg.speculative_chain = true;
    cfg.enable_parallel = false;
    // Workbook-scoped, not `register_function`: a global registration moves
    // the registry's semantic epoch, which retires the banked chain of every
    // engine in the test binary (`observe_function_semantic_epoch`).
    let workbook = TestWorkbook::new().with_function(Arc::new(Rect3x2));
    let mut engine = Engine::new(workbook, cfg);
    engine.spill_same_extent_update = true;
    engine.spill_pending_by_position = by_position;
    engine
}

fn formula(engine: &mut E, row: u32, col: u32, f: &str) {
    engine
        .set_cell_formula("Sheet1", row, col, parse(f).expect("parse"))
        .expect("set formula");
}

fn value(engine: &mut E, row: u32, col: u32, v: f64) {
    engine
        .set_cell_value("Sheet1", row, col, LiteralValue::Number(v))
        .expect("set value");
}

/// A1 = a, A2 = b, A3 = x; anchor C1 = T4_RECT(A1, A2, A3) spills C1:D3
/// (C1 = b, D1 = a). Readers F1..F3, phantom SCC G1/G2 (static cycle, live
/// path reads the spill) and downstream H1/H2; the readers also read A1 and
/// A3, so an edit of either puts them in the request's dirty set.
fn workbook(engine: &mut E) {
    value(engine, 1, 1, 7.0);
    value(engine, 2, 1, 9.0);
    value(engine, 3, 1, 0.0);
    formula(engine, 1, 3, "=T4_RECT(A1,A2,A3)");
    formula(engine, 1, 6, "=D1*10+0*A1+0*A3");
    formula(engine, 2, 6, "=C3+1+0*A1+0*A3");
    formula(engine, 3, 6, "=SUM(C1:D8)+0*A1+0*A3");
    formula(engine, 1, 7, "=IF(FALSE,G2,SUM(C1:D3))+0*A1+0*A3");
    formula(engine, 2, 7, "=IF(FALSE,G1,D1+C3)+0*A1+0*A3");
    formula(engine, 1, 8, "=G1+G2+F2+0*A1+0*A3");
    formula(engine, 2, 8, "=H1*2");
}

fn grid(engine: &E) -> Vec<Option<LiteralValue>> {
    let mut out = Vec::new();
    for r in 1..=8 {
        for c in 1..=10 {
            out.push(engine.get_cell_value("Sheet1", r, c));
        }
    }
    out
}

type Steps = Vec<(Vec<Option<LiteralValue>>, EvalStats, &'static str)>;

/// `run_once`, repeated when the process-global function registry moved
/// during the attempt. Another test's `register_function` moves the semantic
/// epoch, and `observe_function_semantic_epoch` then drops this engine's
/// banked chain, so a step takes the exact path ("no_chain") for a reason
/// that has nothing to do with the toggles under test (see `spec_chain.rs`,
/// `chain_sequence`, and `write_noops_volatile.rs`). An attempt whose epoch
/// did not move is returned as is, so no assertion is weakened.
fn run(by_position: bool, reverse: bool) -> Steps {
    for _ in 0..5 {
        let epoch = crate::function_registry::semantic_epoch();
        let steps = run_once(by_position, reverse);
        if crate::function_registry::semantic_epoch() == epoch {
            return steps;
        }
    }
    panic!("the global function registry moved during all 5 attempts");
}

/// Build, settle, bank the chain, then edit A1 once per step and
/// evaluate. `reverse` corrupts the banked order before each walk.
fn run_once(by_position: bool, reverse: bool) -> Steps {
    let mut e = engine(by_position);
    workbook(&mut e);
    e.evaluate_all().unwrap();
    e.evaluate_all().unwrap();
    // The first pass replans (first-time spill) and banks nothing; an
    // edit of the ignored input A3 dirties the anchor and every reader, re-commits
    // an identical spill (no invalidation, no replan) and banks them all.
    value(&mut e, 3, 1, 1.0);
    e.evaluate_all().unwrap();
    let mut out = Vec::new();
    for a in [8.0, 11.0, 11.0, 3.0].into_iter() {
        // A fallback that replans banks nothing, so later steps of a toggle
        // off run may take the exact path; the tests check the path taken.
        if reverse {
            e.spec_chain_reverse_banked_units_for_test();
        }
        value(&mut e, 1, 1, a);
        e.evaluate_all().unwrap();
        let reason = e.spec_chain_telemetry().last_reason.unwrap_or("");
        out.push((grid(&e), e.eval_stats().clone(), reason));
    }
    out
}

#[test]
fn chain_walk_changed_spill_into_phantom_scc_does_not_replan_with_position_pending() {
    let off = run(false, false);
    let on = run(true, false);
    for (step, ((g_off, _, _), (g_on, _, _))) in off.iter().zip(on.iter()).enumerate() {
        assert_eq!(g_off, g_on, "grid differs from B off at step {step}");
    }
    // Step 0 changes D1 (a 7 -> 8): a same-extent update with a differing cell.
    let (_, s_off, reason_off) = &off[0];
    assert!(
        s_off.inv_pending_added > 0,
        "B off pends the closure: {s_off:?}"
    );
    // The Rev shape: the chain walk settles the SCC, the pended closure
    // forces the exact path, and the exact path replans.
    assert_eq!(
        *reason_off, "spill_fallback_after_cycle_settle",
        "{s_off:?}"
    );
    assert!(s_off.replan_iterations >= 1, "{s_off:?}");
    let (_, s_on, reason_on) = &on[0];
    assert_eq!(s_on.spill_same_extent_updates, 1, "{s_on:?}");
    assert!(s_on.spill_commit_differing_cells > 0, "{s_on:?}");
    assert_eq!(s_on.path, "chain", "reason {reason_on}: {s_on:?}");
    assert_eq!(s_on.spec_chain_last_path, "chain", "{s_on:?}");
    assert_eq!(s_on.replan_iterations, 0, "{s_on:?}");
    assert_eq!(s_on.pending_by_position_pended, 0, "{s_on:?}");
    assert!(s_on.pending_by_position_skipped > 0, "{s_on:?}");
    assert_eq!(s_on.inv_pending_added, 0, "{s_on:?}");
    // Every changing step stays on the chain without replanning.
    for (step, (_, s, reason)) in on.iter().enumerate() {
        assert_eq!(s.replan_iterations, 0, "step {step} ({reason}): {s:?}");
        assert_eq!(s.path, "chain", "step {step} ({reason}): {s:?}");
    }
}

#[test]
fn chain_read_guard_fallback_with_position_pending_matches_toggle_off() {
    let off = run(false, true);
    let on = run(true, true);
    let reference = run(false, false);
    for (step, (((g_off, _, _), (g_on, _, r_on)), (g_ref, _, _))) in
        off.iter().zip(on.iter()).zip(reference.iter()).enumerate()
    {
        assert_eq!(
            g_off, g_on,
            "grid differs from B off at step {step} ({r_on})"
        );
        assert_eq!(
            g_ref, g_on,
            "grid differs from the in-order walk at step {step}"
        );
    }
    assert!(
        on.iter()
            .any(|(_, _, r)| r.starts_with("read_guard_out_of_order")),
        "the corrupted order must trip the read guard: {:?}",
        on.iter().map(|(_, _, r)| *r).collect::<Vec<_>>()
    );
}
