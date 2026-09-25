//! GOD-383 Trial A T2a: `FZ_SPILL_SAME_EXTENT_UPDATE` (toggle A, a re-evaluated
//! anchor whose rectangle is unchanged updates only the differing cells) and
//! `FZ_SPILL_PENDING_BY_POSITION` (toggle B, closure vertices the pass
//! schedules after the anchor are not pended).
//!
//! Every scenario runs with the toggles off (the 40c86895 behaviour) and on,
//! and compares the whole used grid after every step. The first test also
//! pins the replan the toggles remove: an identical re-commit whose closure
//! reaches a static (phantom) SCC replans with the toggles off.

use crate::engine::graph::editor::undo_engine::UndoEngine;
use crate::engine::{
    CancelToken, ChangeEvent, ChangeLog, CycleConfig, CycleDetection, CyclePolicy, Engine,
    EvalConfig, EvalStats,
};
use crate::function::{FnCaps, Function};
use crate::function_registry;
use crate::test_workbook::TestWorkbook;
use crate::traits::{ArgumentHandle, CalcValue, FunctionContext};
use formualizer_common::{ExcelError, ExcelErrorKind, LiteralValue};
use formualizer_parse::parser::parse;
use std::sync::{Arc, LazyLock};

type E = Engine<TestWorkbook>;

static FIVE_ANY: LazyLock<Vec<crate::args::ArgSchema>> =
    LazyLock::new(|| vec![crate::args::ArgSchema::any(); 5]);

fn arg_num(arg: &ArgumentHandle<'_, '_>) -> Result<f64, ExcelError> {
    Ok(match arg.value()?.into_literal() {
        LiteralValue::Number(n) => n,
        LiteralValue::Int(i) => i as f64,
        LiteralValue::Boolean(b) => f64::from(u8::from(b)),
        _ => 0.0,
    })
}

/// `T2A_RECT(rows, cols, a, b, x)`: `rows` x `cols` array whose cell 0 is `b`,
/// cell 1 is `a`, and cell `i` >= 2 is `i` (row-major). `rows` = 0 returns the
/// scalar `a`; `rows` < 0 returns `#VALUE!`. `x` is read and ignored, so an
/// edit of `x` re-evaluates the anchor without changing its result.
#[derive(Debug)]
struct RectFn;

impl Function for RectFn {
    fn caps(&self) -> FnCaps {
        FnCaps::PURE
    }
    fn name(&self) -> &'static str {
        "T2A_RECT"
    }
    fn min_args(&self) -> usize {
        5
    }
    fn arg_schema(&self) -> &'static [crate::args::ArgSchema] {
        &FIVE_ANY[..]
    }
    fn eval<'a, 'b, 'c>(
        &self,
        args: &'c [ArgumentHandle<'a, 'b>],
        _ctx: &dyn FunctionContext<'b>,
    ) -> Result<CalcValue<'b>, ExcelError> {
        let rows = arg_num(&args[0])? as i64;
        let cols = arg_num(&args[1])?.max(1.0) as usize;
        let a = arg_num(&args[2])?;
        let b = arg_num(&args[3])?;
        let _x = arg_num(&args[4])?;
        if rows < 0 {
            return Ok(CalcValue::Scalar(LiteralValue::Error(ExcelError::new(
                ExcelErrorKind::Value,
            ))));
        }
        if rows == 0 {
            return Ok(CalcValue::Scalar(LiteralValue::Number(a)));
        }
        let rows = rows as usize;
        let out = (0..rows)
            .map(|r| {
                (0..cols)
                    .map(|c| {
                        let i = r * cols + c;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Toggles {
    same_extent: bool,
    by_position: bool,
}

const OFF: Toggles = Toggles {
    same_extent: false,
    by_position: false,
};
const A: Toggles = Toggles {
    same_extent: true,
    by_position: false,
};
const B: Toggles = Toggles {
    same_extent: false,
    by_position: true,
};
const AB: Toggles = Toggles {
    same_extent: true,
    by_position: true,
};
const ALL: [Toggles; 4] = [OFF, A, B, AB];

fn base_config() -> EvalConfig {
    let mut cfg = EvalConfig::default().with_cycle(CycleConfig {
        detection: CycleDetection::Runtime,
        policy: CyclePolicy::Error,
    });
    cfg.speculative_chain = false;
    cfg
}

fn engine_with(cfg: EvalConfig, t: Toggles) -> E {
    function_registry::register_function(Arc::new(RectFn));
    let mut engine = Engine::new(TestWorkbook::new(), cfg);
    engine.spill_same_extent_update = t.same_extent;
    engine.spill_pending_by_position = t.by_position;
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

fn clear(engine: &mut E, row: u32, col: u32) {
    engine
        .set_cell_value("Sheet1", row, col, LiteralValue::Empty)
        .expect("clear value");
}

/// Inputs A1..A5 = rows, cols, a, b, x; anchor C1 (3x2 spill over C1:D3,
/// values C1=b, D1=a, C2=2, D2=3, C3=4, D3=5); readers in F..H, including
/// the phantom SCC G1/G2 (static cycle, live path reads the spill) and its
/// downstream reader H1.
fn workbook(engine: &mut E) {
    value(engine, 1, 1, 3.0);
    value(engine, 2, 1, 2.0);
    value(engine, 3, 1, 7.0);
    value(engine, 4, 1, 9.0);
    value(engine, 5, 1, 0.0);
    formula(engine, 1, 3, "=T2A_RECT(A1,A2,A3,A4,A5)");
    formula(engine, 1, 6, "=D1*10");
    formula(engine, 2, 6, "=C3+1");
    formula(engine, 3, 6, "=SUM(C1:D8)");
    formula(engine, 4, 6, "=COUNT(C1:D8)");
    formula(engine, 1, 7, "=IF(FALSE,G2,SUM(C1:D3))");
    formula(engine, 2, 7, "=IF(FALSE,G1,D2+C3)");
    formula(engine, 1, 8, "=G1+G2+F2");
    formula(engine, 2, 8, "=H1*2");
}

const GRID_ROWS: u32 = 10;
const GRID_COLS: u32 = 14;

fn grid(engine: &E) -> Vec<Option<LiteralValue>> {
    let mut out = Vec::new();
    for r in 1..=GRID_ROWS {
        for c in 1..=GRID_COLS {
            out.push(engine.get_cell_value("Sheet1", r, c));
        }
    }
    out
}

fn num(engine: &E, row: u32, col: u32) -> f64 {
    match engine.get_cell_value("Sheet1", row, col) {
        Some(LiteralValue::Number(n)) => n,
        Some(LiteralValue::Int(i)) => i as f64,
        other => panic!("expected number at r{row}c{col}, got {other:?}"),
    }
}

type Step = fn(&mut E);

/// Build, settle (two calls), then apply each step and evaluate; returns the
/// grid and the stats after every step.
fn run(
    cfg: &EvalConfig,
    t: Toggles,
    setup: Step,
    steps: &[Step],
    cancellable: bool,
) -> Vec<(Vec<Option<LiteralValue>>, EvalStats)> {
    let mut engine = engine_with(cfg.clone(), t);
    setup(&mut engine);
    let eval = |engine: &mut E| {
        if cancellable {
            engine.evaluate_all_cancellable(CancelToken::new()).unwrap();
        } else {
            engine.evaluate_all().unwrap();
        }
    };
    eval(&mut engine);
    eval(&mut engine);
    let mut out = vec![(grid(&engine), engine.eval_stats().clone())];
    for step in steps {
        step(&mut engine);
        eval(&mut engine);
        out.push((grid(&engine), engine.eval_stats().clone()));
    }
    out
}

/// Runs the scenario under every toggle combination and both entry points,
/// asserts every grid equals the toggles-off grid, and returns the stats of
/// the non-cancellable runs keyed by toggles.
fn assert_same_grids(
    cfg: &EvalConfig,
    setup: Step,
    steps: &[Step],
) -> Vec<(Toggles, Vec<EvalStats>)> {
    assert_same_grids_within(cfg, setup, steps, &[], 0.0)
}

/// As `assert_same_grids`, except that the cells in `tolerant` (row, col) are
/// compared as numbers within `tol` (iteratively calculated cells).
fn assert_same_grids_within(
    cfg: &EvalConfig,
    setup: Step,
    steps: &[Step],
    tolerant: &[(u32, u32)],
    tol: f64,
) -> Vec<(Toggles, Vec<EvalStats>)> {
    let tolerant_idx: Vec<usize> = tolerant
        .iter()
        .map(|&(r, c)| ((r - 1) * GRID_COLS + (c - 1)) as usize)
        .collect();
    let mut stats = Vec::new();
    for cancellable in [false, true] {
        let baseline = run(cfg, OFF, setup, steps, cancellable);
        for t in ALL {
            let got = run(cfg, t, setup, steps, cancellable);
            for (step, ((g_off, _), (g, _))) in baseline.iter().zip(got.iter()).enumerate() {
                for (i, (o, n)) in g_off.iter().zip(g.iter()).enumerate() {
                    if tolerant_idx.contains(&i)
                        && let (Some(LiteralValue::Number(o)), Some(LiteralValue::Number(n))) = (o, n)
                    {
                        assert!(
                            (o - n).abs() <= tol,
                            "cell {i} differs beyond {tol} at step {step} with {t:?}: {o} vs {n}"
                        );
                        continue;
                    }
                    assert_eq!(
                        o, n,
                        "cell {i} differs from toggles off at step {step} with {t:?} (cancellable {cancellable})"
                    );
                }
            }
            if !cancellable {
                stats.push((t, got.into_iter().map(|(_, s)| s).collect()));
            }
        }
    }
    stats
}

fn stats_for(all: &[(Toggles, Vec<EvalStats>)], t: Toggles) -> &[EvalStats] {
    &all.iter().find(|(k, _)| *k == t).unwrap().1
}

// (1) Identical re-commit whose closure reaches a phantom SCC.
#[test]
fn identical_recommit_into_phantom_scc_replans_only_with_toggles_off() {
    let cfg = base_config();
    let steps: &[Step] = &[|e| value(e, 5, 1, 1.0), |e| value(e, 5, 1, 2.0)];
    let all = assert_same_grids(&cfg, workbook, steps);

    let off = &stats_for(&all, OFF)[1];
    assert!(off.replan_iterations >= 1, "toggles off must replan: {off:?}");
    assert!(off.token_scc_blocked() > 0, "{off:?}");
    assert!(off.pass_pending_at_drain[0] > 0, "{off:?}");
    assert_eq!(off.spill_commit_same_footprint, 1, "{off:?}");
    assert_eq!(off.spill_commit_identical, 1, "{off:?}");
    assert_eq!(off.spill_commit_differing_cells, 0, "{off:?}");
    assert_eq!(off.spill_same_extent_updates, 0, "{off:?}");

    for t in [A, AB] {
        for s in &stats_for(&all, t)[1..] {
            assert_eq!(s.replan_iterations, 0, "{t:?}: {s:?}");
            assert_eq!(s.token_scc_blocked(), 0, "{t:?}: {s:?}");
            assert!(s.pass_pending_at_drain.iter().all(|&n| n == 0), "{t:?}: {s:?}");
            assert_eq!(s.spill_same_extent_updates, 1, "{t:?}: {s:?}");
            assert_eq!(s.spill_same_extent_empty_diffs, 1, "{t:?}: {s:?}");
            assert_eq!(s.spill_commit_identical, 1, "{t:?}: {s:?}");
            assert_eq!(s.spill_clear_count, 0, "{t:?}: {s:?}");
            assert_eq!(s.inv_commit_multi_calls, 0, "{t:?}: {s:?}");
            assert_eq!(s.inv_spill_clear_calls, 0, "{t:?}: {s:?}");
            assert_eq!(s.inv_empty_calls, 0, "{t:?}: {s:?}");
            assert_eq!(s.inv_pending_added, 0, "{t:?}: {s:?}");
            assert_eq!(s.output_footprint_epoch_delta, 0, "{t:?}: {s:?}");
        }
    }
}

// (2) Same extent, some values change.
#[test]
fn same_extent_values_change_updates_only_changed_readers() {
    let cfg = base_config();
    let steps: &[Step] = &[
        |e| value(e, 3, 1, 8.0),               // a: D1 changes
        |e| value(e, 4, 1, 1.0),               // b: C1 (anchor cell) changes
        |e| {
            value(e, 3, 1, 11.0);              // both change together
            value(e, 4, 1, 12.0);
        },
        |e| value(e, 5, 1, 3.0),               // identical again
    ];
    let all = assert_same_grids(&cfg, workbook, steps);

    // Readers of changed cells show new values; readers of unchanged cells
    // keep theirs (checked on the A+B engine; grids equal toggles off).
    let mut engine = engine_with(cfg.clone(), AB);
    workbook(&mut engine);
    engine.evaluate_all().unwrap();
    assert_eq!(num(&engine, 1, 6), 70.0);
    assert_eq!(num(&engine, 2, 6), 5.0);
    value(&mut engine, 3, 1, 8.0);
    engine.evaluate_all().unwrap();
    assert_eq!(num(&engine, 1, 4), 8.0);
    assert_eq!(num(&engine, 1, 6), 80.0);
    assert_eq!(num(&engine, 2, 6), 5.0);
    assert_eq!(num(&engine, 3, 6), 9.0 + 8.0 + 2.0 + 3.0 + 4.0 + 5.0);
    assert_eq!(num(&engine, 1, 7), 9.0 + 8.0 + 2.0 + 3.0 + 4.0 + 5.0);
    assert_eq!(num(&engine, 2, 7), 7.0);

    let a = stats_for(&all, A);
    assert_eq!(a[1].spill_same_extent_updates, 1, "{:?}", a[1]);
    assert_eq!(a[1].spill_commit_differing_cells, 1, "{:?}", a[1]);
    assert_eq!(a[1].spill_same_extent_empty_diffs, 0, "{:?}", a[1]);
    assert_eq!(a[1].output_footprint_epoch_delta, 0, "{:?}", a[1]);
    assert_eq!(a[3].spill_commit_differing_cells, 2, "{:?}", a[3]);
    assert_eq!(a[4].spill_same_extent_empty_diffs, 1, "{:?}", a[4]);
    // Toggles off: the counter fix reports the real differing count.
    let off = stats_for(&all, OFF);
    assert_eq!(off[1].spill_commit_differing_cells, 1, "{:?}", off[1]);
    assert_eq!(off[1].spill_commit_identical, 0, "{:?}", off[1]);
    // With position-based pending the SCC readers after the anchor are not
    // pended, so a real value change no longer replans either.
    for t in [B, AB] {
        for s in &stats_for(&all, t)[1..] {
            assert_eq!(s.replan_iterations, 0, "{t:?}: {s:?}");
            assert_eq!(s.token_scc_blocked(), 0, "{t:?}: {s:?}");
        }
    }
    let ab = stats_for(&all, AB);
    assert!(ab[1].pending_by_position_skipped > 0, "{:?}", ab[1]);
    assert_eq!(ab[1].pending_by_position_pended, 0, "{:?}", ab[1]);
}

// (3) Extent and kind transitions keep today's path and today's results.
#[test]
fn extent_and_kind_transitions_match_toggles_off() {
    let cfg = base_config();
    let steps: &[Step] = &[
        |e| value(e, 1, 1, 5.0),  // grow 3x2 -> 5x2
        |e| value(e, 1, 1, 2.0),  // shrink -> 2x2
        |e| value(e, 2, 1, 1.0),  // narrow -> 2x1
        |e| value(e, 1, 1, 0.0),  // array -> scalar
        |e| value(e, 1, 1, 3.0),  // scalar -> array
        |e| value(e, 1, 1, -1.0), // array -> error
        |e| value(e, 1, 1, 3.0),  // error -> array
        |e| value(e, 7, 3, 99.0), // value below the spill
        |e| value(e, 1, 1, 8.0),  // grow into it: blocked #SPILL!
        |e| value(e, 3, 1, 4.0),  // blocked anchor re-evaluates
        |e| clear(e, 7, 3),       // unblocked
        |e| value(e, 3, 1, 6.0),  // first commit after the unblock (today's path)
        |e| value(e, 3, 1, 7.0),  // same extent again
    ];
    let all = assert_same_grids(&cfg, workbook, steps);
    let a = stats_for(&all, A);
    for (i, s) in a.iter().enumerate().skip(1).take(10) {
        assert_eq!(s.spill_same_extent_updates, 0, "step {i}: {s:?}");
    }
    assert_eq!(a[13].spill_same_extent_updates, 1, "{:?}", a[13]);
}

// (4) Iterative live cycle reached by the spill closure.
#[test]
fn iterative_cycle_reached_by_spill_matches_toggles_off() {
    let cfg = base_config().with_cycle(CycleConfig::iterate(100, 0.001));
    let setup: Step = |e| {
        workbook(e);
        formula(e, 1, 10, "=SUM(C1:D3)/10+K1/2");
        formula(e, 1, 11, "=J1/2");
        formula(e, 1, 12, "=J1+K1");
    };
    let steps: &[Step] = &[
        |e| value(e, 5, 1, 1.0),
        |e| value(e, 3, 1, 20.0),
        |e| value(e, 1, 1, 4.0),
        |e| value(e, 4, 1, -3.0),
    ];
    // J1:L1 are the live cycle. With the toggles off, the spill commit
    // re-pends the cycle and the replan runs its iteration a second time;
    // with them on it iterates once. Both results satisfy the convergence
    // test (max_change 0.001), so they are compared within that tolerance;
    // every other cell must be identical.
    let all = assert_same_grids_within(&cfg, setup, steps, &[(1, 10), (1, 11), (1, 12)], 0.001);
    assert!(stats_for(&all, A)[1].spill_same_extent_updates >= 1);
}

// (5) OFFSET / INDIRECT readers of a changing spill.
#[test]
fn dynamic_reference_readers_match_toggles_off() {
    let cfg = base_config();
    let setup: Step = |e| {
        workbook(e);
        value(e, 6, 1, 0.0);
        formula(e, 5, 6, "=SUM(OFFSET(C1,0,0,3,2))");
        formula(e, 6, 6, "=INDIRECT(\"D1\")*2");
        formula(e, 7, 6, "=SUM(OFFSET(C1,A6,0,1,2))");
        formula(e, 8, 6, "=F5+F6+F7");
    };
    let steps: &[Step] = &[
        |e| value(e, 3, 1, 30.0),
        |e| value(e, 5, 1, 1.0),
        |e| value(e, 6, 1, 2.0),
        |e| value(e, 1, 1, 4.0),
        |e| value(e, 3, 1, 31.0),
    ];
    let all = assert_same_grids(&cfg, setup, steps);
    assert!(stats_for(&all, A)[1].spill_same_extent_updates >= 1);
}

// (6) A follower edited externally between calls: same results as today.
#[test]
fn externally_edited_follower_matches_toggles_off() {
    let cfg = base_config();
    let steps: &[Step] = &[
        |e| value(e, 2, 4, 100.0), // user value over follower D2
        |e| value(e, 5, 1, 1.0),   // anchor re-evaluates with the edit present
        |e| clear(e, 2, 4),        // edit removed
        |e| value(e, 5, 1, 2.0),
        |e| {
            value(e, 3, 3, 50.0); // user value over C3 together with an input edit
            value(e, 3, 1, 9.0);
        },
        |e| clear(e, 3, 3),
        |e| value(e, 3, 1, 10.0),
    ];
    let all = assert_same_grids(&cfg, workbook, steps);
    // `Engine::set_cell_value` over a follower keeps the spill registration.
    // On this small sheet the delta overlay compacts into the base lanes at
    // once, so today's clear masks the user value and the re-commit
    // overwrites it; the same-extent path (whose guard routes a follower with
    // a live delta-overlay entry to today's path) reads the user value as the
    // stored value, so the follower differs and is rewritten and invalidated
    // like today (grids above are identical).
    let a = stats_for(&all, A);
    assert_eq!(a[1].spill_same_extent_updates, 0, "{:?}", a[1]);
    assert_eq!(a[2].spill_same_extent_updates, 1, "{:?}", a[2]);
    assert_eq!(a[2].spill_commit_differing_cells, 1, "{:?}", a[2]);
    assert_eq!(a[5].spill_commit_differing_cells, 2, "{:?}", a[5]);
}

// (7) Undo / rollback after a no-op commit and after a values-only commit.
#[test]
fn undo_after_noop_and_values_only_commit_matches_toggles_off() {
    let run_logged = |t: Toggles| {
        let mut engine = engine_with(base_config(), t);
        workbook(&mut engine);
        engine.evaluate_all().unwrap();
        let mut grids = Vec::new();
        let mut kinds = Vec::new();
        for edit in [(5u32, 1u32, 1.0), (3, 1, 8.0)] {
            let before = grid(&engine);
            value(&mut engine, edit.0, edit.1, edit.2);
            let mut log = ChangeLog::new();
            let mut undo = UndoEngine::new();
            engine.evaluate_all_logged(&mut log).unwrap();
            let after = grid(&engine);
            kinds.push(
                log.events()
                    .iter()
                    .filter_map(|e| match e {
                        ChangeEvent::SpillCommitted { old, .. } => {
                            Some(if old.is_some() { "commit_old" } else { "commit_new" })
                        }
                        ChangeEvent::SpillCleared { .. } => Some("cleared"),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
            );
            engine.undo_logged(&mut undo, &mut log).unwrap();
            let undone = grid(&engine);
            engine.redo_logged(&mut undo, &mut log).unwrap();
            let redone = grid(&engine);
            engine.evaluate_all().unwrap();
            let settled = grid(&engine);
            grids.push((before, after, undone, redone, settled));
        }
        (grids, kinds)
    };
    let (off, off_kinds) = run_logged(OFF);
    for t in [A, AB] {
        let (on, kinds) = run_logged(t);
        for (i, (o, n)) in off.iter().zip(on.iter()).enumerate() {
            assert_eq!(o.1, n.1, "after eval differs, edit {i}, {t:?}");
            assert_eq!(o.2, n.2, "after undo differs, edit {i}, {t:?}");
            assert_eq!(o.3, n.3, "after redo differs, edit {i}, {t:?}");
            assert_eq!(o.4, n.4, "after settle differs, edit {i}, {t:?}");
        }
        // The same-extent path logs one SpillCommitted carrying the old spill.
        for k in &kinds {
            assert_eq!(k, &vec!["commit_old"], "{t:?}");
        }
    }
    for k in &off_kinds {
        assert_eq!(k, &vec!["cleared", "commit_new"]);
    }
    // Undo restores the spill values from before each call.
    for (i, g) in off.iter().enumerate() {
        let spill_idx = |r: u32, c: u32| ((r - 1) * GRID_COLS + (c - 1)) as usize;
        for (r, c) in [(1u32, 3u32), (1, 4), (2, 3), (2, 4), (3, 3), (3, 4)] {
            assert_eq!(g.0[spill_idx(r, c)], g.2[spill_idx(r, c)], "edit {i} r{r}c{c}");
        }
    }
}

