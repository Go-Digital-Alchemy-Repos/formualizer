//! GOD-383 Trial A T2b write-path toggles.
//!
//! * `FZ_CLEAR_VOLATILE_ON_VALUE`: a formula cell overwritten by a value stops
//!   being volatile/dynamic, so it no longer re-dirties its dependents after
//!   every evaluation; a later formula write re-derives both flags.
//! * `FZ_WRITE_NOOPS`: an identical `set_formula` and an Empty-over-empty
//!   write are no-ops.
//!
//! Every scenario runs with both toggles off and on (via
//! `Engine::set_write_toggles`, so the tests do not depend on the process
//! environment) and asserts the observed values agree.

use crate::engine::vertex::VertexId;
use crate::engine::{DeterministicMode, Engine, EvalConfig, TemporalEgress};
use crate::function::Function;
use crate::reference::{CellRef, Coord};
use crate::test_workbook::TestWorkbook;
use crate::timezone::TimeZoneSpec;
use crate::traits::{ArgumentHandle, CalcValue, FunctionContext};
use chrono::TimeZone;
use formualizer_common::{ExcelError, LiteralValue};
use formualizer_parse::parser::parse;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

type Snap = Vec<Option<LiteralValue>>;

#[derive(Debug)]
struct CountingPassthrough {
    calls: Arc<AtomicUsize>,
}

impl Function for CountingPassthrough {
    fn name(&self) -> &'static str {
        "TCOUNT"
    }

    fn eval<'a, 'b, 'c>(
        &self,
        args: &'c [ArgumentHandle<'a, 'b>],
        _ctx: &dyn FunctionContext<'b>,
    ) -> Result<CalcValue<'b>, ExcelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let value = args[0].value()?.into_owned();
        Ok(CalcValue::Scalar(value))
    }
}

fn engine(on: bool, config: EvalConfig) -> (Engine<TestWorkbook>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let workbook = TestWorkbook::new().with_function(Arc::new(CountingPassthrough {
        calls: Arc::clone(&calls),
    }));
    let mut engine = Engine::new(workbook, config);
    engine.set_write_toggles(on, on);
    assert_eq!(engine.write_toggles(), (on, on));
    (engine, calls)
}

fn pinned(y: i32, m: u32, d: u32) -> DeterministicMode {
    DeterministicMode::Enabled {
        timestamp_utc: chrono::Utc
            .with_ymd_and_hms(y, m, d, 10, 0, 0)
            .single()
            .expect("valid timestamp"),
        timezone: TimeZoneSpec::Utc,
    }
}

fn frozen_config(mode: DeterministicMode) -> EvalConfig {
    EvalConfig {
        temporal_egress: TemporalEgress::Serial,
        deterministic_mode: mode,
        enable_parallel: false,
        ..EvalConfig::default()
    }
}

fn formula(engine: &mut Engine<TestWorkbook>, row: u32, col: u32, text: &str) {
    engine
        .set_cell_formula("Sheet1", row, col, parse(text).unwrap())
        .unwrap();
}

fn value(engine: &mut Engine<TestWorkbook>, row: u32, col: u32, v: LiteralValue) {
    engine.set_cell_value("Sheet1", row, col, v).unwrap();
}

fn vertex(engine: &Engine<TestWorkbook>, row: u32, col: u32) -> Option<VertexId> {
    let sheet_id = engine.graph.sheet_id("Sheet1")?;
    engine
        .graph
        .get_vertex_id_for_address(&CellRef::new(
            sheet_id,
            Coord::from_excel(row, col, true, true),
        ))
        .copied()
}

fn snap(engine: &Engine<TestWorkbook>, cells: &[(u32, u32)]) -> Snap {
    cells
        .iter()
        .map(|&(r, c)| engine.get_cell_value("Sheet1", r, c))
        .collect()
}

fn num(engine: &Engine<TestWorkbook>, row: u32, col: u32) -> f64 {
    match engine.get_cell_value("Sheet1", row, col) {
        Some(LiteralValue::Number(n)) => n,
        Some(LiteralValue::Int(i)) => i as f64,
        other => panic!("expected a number at ({row},{col}), got {other:?}"),
    }
}

fn calls_during(calls: &AtomicUsize, f: impl FnOnce()) -> usize {
    let before = calls.load(Ordering::SeqCst);
    f();
    calls.load(Ordering::SeqCst) - before
}

// ---------------------------------------------------------------------------
// (1) TODAY() cell overwritten by a value, then restored.
// ---------------------------------------------------------------------------

fn today_overwrite_scenario(on: bool) -> Vec<Snap> {
    let (mut e, calls) = engine(on, frozen_config(pinned(2025, 1, 15)));
    let cells = [(1, 1), (1, 2), (1, 3)];
    formula(&mut e, 1, 1, "=TODAY()");
    formula(&mut e, 1, 2, "=TCOUNT(A1)+1");
    formula(&mut e, 1, 3, "=B1*2");
    let mut snaps = Vec::new();
    e.evaluate_all().unwrap();
    snaps.push(snap(&e, &cells));

    value(&mut e, 1, 1, LiteralValue::Number(40_000.0));
    e.evaluate_all().unwrap();
    snaps.push(snap(&e, &cells));
    assert_eq!(num(&e, 1, 2), 40_001.0);

    let (total, needing, _) = e.graph.volatile_refresh_census();
    let a1 = vertex(&e, 1, 1).unwrap();
    let recomputed = calls_during(&calls, || {
        e.evaluate_all().unwrap();
    });
    if on {
        assert_eq!((total, needing), (0, 0), "no stale volatile left");
        assert!(!e.graph.is_volatile(a1));
        assert_eq!(recomputed, 0, "a clean graph evaluates nothing");
        assert_eq!(e.write_toggle_counters().0, 1);
    } else {
        assert_eq!(
            (total, needing),
            (1, 1),
            "legacy: stale entry needs refresh"
        );
        assert!(e.graph.is_volatile(a1));
        assert_eq!(recomputed, 1, "legacy: stale volatile re-dirties B1");
        assert_eq!(e.write_toggle_counters().0, 0);
    }
    snaps.push(snap(&e, &cells));

    // Restoring the formula re-derives volatility exactly as today.
    formula(&mut e, 1, 1, "=TODAY()");
    assert!(e.graph.is_volatile(a1));
    assert_eq!(e.graph.volatile_refresh_census(), (1, 0, 1));
    e.evaluate_all().unwrap();
    snaps.push(snap(&e, &cells));
    let before_move = e.get_cell_value("Sheet1", 1, 1);

    // Clock moves: the TODAY cell and its dependents refresh.
    e.set_deterministic_mode(pinned(2025, 3, 1)).unwrap();
    let refreshed = calls_during(&calls, || {
        e.evaluate_all().unwrap();
    });
    assert_eq!(refreshed, 1, "B1 recomputed after the clock moved");
    assert_ne!(e.get_cell_value("Sheet1", 1, 1), before_move);
    snaps.push(snap(&e, &cells));
    snaps
}

#[test]
fn today_formula_overwritten_by_value_stops_redirtying_and_restores() {
    assert_eq!(
        today_overwrite_scenario(false),
        today_overwrite_scenario(true)
    );
}

// ---------------------------------------------------------------------------
// (2) Clock-only / deterministic-mode interplay.
// ---------------------------------------------------------------------------

fn clock_interplay_scenario(on: bool) -> Vec<Snap> {
    let (mut e, _calls) = engine(on, frozen_config(pinned(2025, 1, 15)));
    let cells = [(1, 1), (1, 2), (2, 1), (2, 2), (3, 1), (3, 2)];
    formula(&mut e, 1, 1, "=TODAY()");
    formula(&mut e, 1, 2, "=A1*2");
    formula(&mut e, 2, 1, "=TODAY()");
    formula(&mut e, 2, 2, "=A2+1");
    formula(&mut e, 3, 1, "=NOW()");
    formula(&mut e, 3, 2, "=A3-A2");
    let mut snaps = Vec::new();
    e.evaluate_all().unwrap();
    snaps.push(snap(&e, &cells));

    value(&mut e, 1, 1, LiteralValue::Number(45_000.0));
    e.evaluate_all().unwrap();
    snaps.push(snap(&e, &cells));

    // Re-pinning the same clock is a no-op; a moved clock refreshes the two
    // remaining clock cells and leaves the value-derived row alone.
    e.set_deterministic_mode(pinned(2025, 1, 15)).unwrap();
    e.evaluate_all().unwrap();
    snaps.push(snap(&e, &cells));
    e.set_deterministic_mode(pinned(2026, 6, 30)).unwrap();
    e.evaluate_all().unwrap();
    snaps.push(snap(&e, &cells));
    assert_eq!(num(&e, 1, 2), 90_000.0);
    assert_ne!(snaps[3][2], snaps[2][2], "A2 follows the moved clock");

    let (total, needing, clock_only) = e.graph.volatile_refresh_census();
    if on {
        assert_eq!((total, needing, clock_only), (2, 0, 2));
    } else {
        assert_eq!((total, needing, clock_only), (3, 1, 2));
    }

    // Live clock: only the value-derived row is compared (the clock cells
    // read the wall clock).
    e.set_deterministic_mode(DeterministicMode::Disabled {
        timezone: TimeZoneSpec::Utc,
    })
    .unwrap();
    e.evaluate_all().unwrap();
    snaps.push(snap(&e, &[(1, 1), (1, 2)]));
    let a1 = vertex(&e, 1, 1).unwrap();
    assert_eq!(e.graph.is_volatile(a1), !on);
    snaps
}

#[test]
fn clock_moves_and_pins_agree_with_toggle_on_and_off() {
    assert_eq!(
        clock_interplay_scenario(false),
        clock_interplay_scenario(true)
    );
}

// ---------------------------------------------------------------------------
// (3) Identical set_formula.
// ---------------------------------------------------------------------------

fn chain_config() -> EvalConfig {
    EvalConfig {
        speculative_chain: true,
        enable_virtual_dep_telemetry: true,
        enable_parallel: false,
        ..EvalConfig::default()
    }
}

fn identical_formula_attempt(on: bool) -> Vec<Snap> {
    let (mut e, calls) = engine(on, chain_config());
    for row in 1..=10u32 {
        value(&mut e, row, 1, LiteralValue::Int(row as i64));
    }
    for row in 1..=10u32 {
        formula(&mut e, row, 2, "=SUM($A$1:$A$10)");
    }
    formula(&mut e, 1, 3, "=SUM($B$1:$B$10)");
    formula(&mut e, 1, 4, "=A1*3");
    formula(&mut e, 1, 5, "=TCOUNT(D1)+1");
    let cells = [(1, 2), (1, 3), (1, 4), (1, 5)];
    let mut snaps = Vec::new();
    e.evaluate_all().unwrap();
    value(&mut e, 2, 1, LiteralValue::Int(100));
    e.evaluate_all().unwrap();
    snaps.push(snap(&e, &cells));
    assert!(
        e.spec_chain_is_installed(),
        "the second pass walks and keeps the chain"
    );

    let epoch = e.topology_epoch_for_test();
    let schedule = e.cached_static_schedule_for_test();
    let d1 = vertex(&e, 1, 4).unwrap();
    let e1 = vertex(&e, 1, 5).unwrap();

    formula(&mut e, 1, 4, "=A1*3");
    if on {
        assert_eq!(e.topology_epoch_for_test(), epoch, "no topology edit");
        assert!(e.spec_chain_is_installed(), "chain retained");
        match (&schedule, &e.cached_static_schedule_for_test()) {
            (None, None) => {}
            (Some(a), Some(b)) => assert!(Arc::ptr_eq(a, b), "static schedule retained"),
            _ => panic!("static schedule cache changed"),
        }
        assert!(
            !e.graph.is_dirty(d1) && !e.graph.is_dirty(e1),
            "nothing dirtied"
        );
        assert_eq!(e.write_toggle_counters().1, 1);
    } else {
        assert_ne!(e.topology_epoch_for_test(), epoch);
        assert!(!e.spec_chain_is_installed());
        assert!(e.graph.is_dirty(e1));
        assert_eq!(e.write_toggle_counters().1, 0);
    }
    let recomputed = calls_during(&calls, || {
        e.evaluate_all().unwrap();
    });
    assert_eq!(recomputed, usize::from(!on));
    snaps.push(snap(&e, &cells));

    // A different formula text still re-plans.
    let epoch = e.topology_epoch_for_test();
    formula(&mut e, 1, 4, "=A1*4");
    assert_ne!(e.topology_epoch_for_test(), epoch);
    assert!(e.graph.is_dirty(e1));
    e.evaluate_all().unwrap();
    assert_eq!(num(&e, 1, 4), 4.0);
    snaps.push(snap(&e, &cells));

    // set_formula over a value override restores the formula and re-dirties.
    value(&mut e, 1, 4, LiteralValue::Number(5.0));
    e.evaluate_all().unwrap();
    snaps.push(snap(&e, &cells));
    let epoch = e.topology_epoch_for_test();
    formula(&mut e, 1, 4, "=A1*4");
    assert_ne!(e.topology_epoch_for_test(), epoch);
    assert!(e.graph.is_dirty(e1));
    assert_eq!(e.write_toggle_counters().1, u64::from(on));
    e.evaluate_all().unwrap();
    assert_eq!(num(&e, 1, 5), 5.0);
    snaps.push(snap(&e, &cells));
    snaps
}

/// A concurrent `register_function` in another test module moves the global
/// semantic epoch and retires the chain mid-sequence (see `spec_chain.rs`);
/// retry the sequence when that happened.
fn identical_formula_scenario(on: bool) -> Vec<Snap> {
    for _ in 0..3 {
        let epoch = crate::function_registry::semantic_epoch();
        let result = std::panic::catch_unwind(|| identical_formula_attempt(on));
        if crate::function_registry::semantic_epoch() == epoch {
            match result {
                Ok(snaps) => return snaps,
                Err(panic) => std::panic::resume_unwind(panic),
            }
        }
    }
    panic!("the global function registry moved during all 3 attempts");
}

#[test]
fn identical_set_formula_is_a_noop_and_changes_still_replan() {
    assert_eq!(
        identical_formula_scenario(false),
        identical_formula_scenario(true)
    );
}

// ---------------------------------------------------------------------------
// (4) Empty writes.
// ---------------------------------------------------------------------------

fn empty_write_scenario(on: bool) -> Vec<Snap> {
    let (mut e, calls) = engine(
        on,
        EvalConfig {
            enable_parallel: false,
            ..EvalConfig::default()
        },
    );
    value(&mut e, 10, 1, LiteralValue::Int(1));
    value(&mut e, 2, 1, LiteralValue::Int(5));
    formula(&mut e, 1, 2, "=TCOUNT(COUNTA($A$1:$A$10))");
    formula(&mut e, 2, 2, "=SUM($A$1:$A$10)");
    let cells: Vec<(u32, u32)> = (1..=10).map(|r| (r, 1)).chain([(1, 2), (2, 2)]).collect();
    let mut snaps = Vec::new();
    e.evaluate_all().unwrap();
    snaps.push(snap(&e, &cells));

    // Empty over an absent cell inside the sheet.
    let kind_of =
        |e: &Engine<TestWorkbook>, row: u32| vertex(e, row, 1).map(|v| e.graph.get_vertex_kind(v));
    let a3_kind = kind_of(&e, 3);
    assert_ne!(a3_kind, Some(crate::engine::VertexKind::Cell));
    assert_eq!(e.is_empty_over_empty_noop("Sheet1", 3, 1), on);
    let noops_before = e.write_toggle_counters().2;
    value(&mut e, 3, 1, LiteralValue::Empty);
    let n = calls_during(&calls, || {
        e.evaluate_all().unwrap();
    });
    assert_eq!(n, usize::from(!on));
    if on {
        assert_eq!(
            kind_of(&e, 3),
            a3_kind,
            "a skipped write leaves the cell as it was"
        );
    }
    snaps.push(snap(&e, &cells));

    // Empty over an existing empty value vertex.
    value(&mut e, 4, 1, LiteralValue::Int(9));
    value(&mut e, 4, 1, LiteralValue::Empty);
    e.evaluate_all().unwrap();
    assert!(vertex(&e, 4, 1).is_some());
    value(&mut e, 4, 1, LiteralValue::Empty);
    let n = calls_during(&calls, || {
        e.evaluate_all().unwrap();
    });
    assert_eq!(n, usize::from(!on));
    snaps.push(snap(&e, &cells));

    // Empty over a value dirties in both states.
    assert!(!e.is_empty_over_empty_noop("Sheet1", 2, 1));
    value(&mut e, 2, 1, LiteralValue::Empty);
    let n = calls_during(&calls, || {
        e.evaluate_all().unwrap();
    });
    assert_eq!(n, 1);
    assert_eq!(num(&e, 2, 2), 1.0);
    snaps.push(snap(&e, &cells));

    // Empty over a formatted empty cell keeps today's path and drops the format.
    e.debug_record_derived_format_0based("Sheet1", 4, 0, Some(crate::format::FormatId::DATE));
    assert_eq!(
        e.effective_format_id("Sheet1", 5, 1),
        Some(crate::format::FormatId::DATE)
    );
    assert!(!e.is_empty_over_empty_noop("Sheet1", 5, 1));
    value(&mut e, 5, 1, LiteralValue::Empty);
    assert_eq!(e.effective_format_id("Sheet1", 5, 1), None);
    let n = calls_during(&calls, || {
        e.evaluate_all().unwrap();
    });
    assert_eq!(n, 1);
    snaps.push(snap(&e, &cells));

    // Outside the sheet's rows, and on a formula cell: never a no-op.
    assert!(!e.is_empty_over_empty_noop("Sheet1", 1_000_000, 1));
    assert!(!e.is_empty_over_empty_noop("Sheet1", 1, 2));
    assert_eq!(
        e.write_toggle_counters().2 - noops_before,
        if on { 2 } else { 0 }
    );
    snaps
}

#[test]
fn empty_over_empty_is_a_noop_and_other_empty_writes_are_not() {
    assert_eq!(empty_write_scenario(false), empty_write_scenario(true));
}

// ---------------------------------------------------------------------------
// (5) OFFSET / INDIRECT / RAND cells overwritten by values, then restored.
// ---------------------------------------------------------------------------

fn volatile_kinds_scenario(on: bool) -> Vec<Snap> {
    crate::builtins::random::register_builtins();
    let (mut e, calls) = engine(
        on,
        EvalConfig {
            workbook_seed: 424_242,
            ..frozen_config(pinned(2025, 1, 15))
        },
    );
    value(&mut e, 1, 3, LiteralValue::Int(7));
    let sources = ["=RAND()", "=OFFSET(C1,0,0)", "=INDIRECT(\"C1\")"];
    for (i, text) in sources.iter().enumerate() {
        let row = i as u32 + 1;
        formula(&mut e, row, 1, text);
        formula(&mut e, row, 2, &format!("=TCOUNT(A{row})"));
    }
    let cells = [(1, 1), (1, 2), (2, 1), (2, 2), (3, 1), (3, 2)];
    let flags = |e: &Engine<TestWorkbook>| -> Vec<(bool, bool)> {
        (1..=3)
            .map(|row| {
                let v = vertex(e, row, 1).unwrap();
                (e.graph.is_volatile(v), e.graph.is_dynamic(v))
            })
            .collect()
    };
    let mut snaps = Vec::new();
    e.evaluate_all().unwrap();
    snaps.push(snap(&e, &cells));
    let original = flags(&e);
    assert!(original[0].0, "RAND is volatile");
    let volatile_sources = original.iter().filter(|(v, _)| *v).count();
    let census = e.graph.volatile_refresh_census();

    for row in 1..=3u32 {
        value(&mut e, row, 1, LiteralValue::Number(row as f64 * 10.0));
    }
    e.evaluate_all().unwrap();
    snaps.push(snap(&e, &cells));
    let n = calls_during(&calls, || {
        e.evaluate_all().unwrap();
    });
    if on {
        assert_eq!(flags(&e), vec![(false, false); 3]);
        assert_eq!(
            e.graph.volatile_refresh_census().0,
            census.0 - volatile_sources
        );
        assert_eq!(n, 0);
    } else {
        assert_eq!(flags(&e), original, "legacy: flags stay stale");
        assert_eq!(e.graph.volatile_refresh_census(), census);
        assert_eq!(n, volatile_sources);
    }
    snaps.push(snap(&e, &cells));

    for (i, text) in sources.iter().enumerate() {
        formula(&mut e, i as u32 + 1, 1, text);
    }
    assert_eq!(
        flags(&e),
        original,
        "restored formulas re-derive both flags"
    );
    assert_eq!(e.graph.volatile_refresh_census(), census);
    e.evaluate_all().unwrap();
    assert_eq!(num(&e, 2, 1), 7.0);
    snaps.push(snap(&e, &cells));
    snaps
}

#[test]
fn offset_indirect_rand_overwritten_then_restored() {
    assert_eq!(
        volatile_kinds_scenario(false),
        volatile_kinds_scenario(true)
    );
}
