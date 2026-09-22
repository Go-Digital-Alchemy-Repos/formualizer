//! Tests for the Excel-style speculative calculation chain
//! (`EvalConfig::speculative_chain`).

use crate::engine::{CycleConfig, Engine, EvalConfig, SpecChainTelemetry, TemporalEgress};
use crate::function::{FnCaps, Function};
use crate::function_registry;
use crate::test_workbook::TestWorkbook;
use crate::traits::{ArgumentHandle, FunctionContext};
use formualizer_common::{ExcelError, LiteralValue};
use formualizer_parse::parser::parse;
use std::sync::{Arc, Mutex, MutexGuard};

/// Serializes this module's tests against each other.
///
/// `spec_chain_retired_by_a_function_registration` calls `register_function`,
/// which moves the process-global registry's semantic epoch and retires any
/// banked chain in the same binary. Without this lock it can land in the
/// middle of a sibling test's bank-then-walk sequence and force that test onto
/// the retry path (or, in the counting test, change which path a pass took).
/// The lock removes the interference this module creates for itself;
/// `chain_sequence` still covers registrations from the other eight test
/// modules, which take no lock.
static SPEC_CHAIN_TESTS: Mutex<()> = Mutex::new(());

/// Poisoning-tolerant: a panicking test has already failed, and its poison
/// must not cascade into every other test in the module.
fn spec_chain_test_guard() -> MutexGuard<'static, ()> {
    SPEC_CHAIN_TESTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn chain_config() -> EvalConfig {
    EvalConfig {
        speculative_chain: true,
        enable_virtual_dep_telemetry: true,
        ..EvalConfig::default()
    }
}

fn plain_config() -> EvalConfig {
    EvalConfig {
        speculative_chain: false,
        enable_virtual_dep_telemetry: true,
        ..EvalConfig::default()
    }
}

fn iterative_config(speculative_chain: bool) -> EvalConfig {
    EvalConfig {
        speculative_chain,
        enable_virtual_dep_telemetry: true,
        temporal_egress: TemporalEgress::Serial,
        ..EvalConfig::default().with_cycle(CycleConfig::iterate(100, 0.001))
    }
}

fn build_range_workbook(config: EvalConfig) -> Result<Engine<TestWorkbook>, ExcelError> {
    let mut engine = Engine::new(TestWorkbook::new(), config);
    for row in 1..=40u32 {
        engine.set_cell_value("Sheet1", row, 1, LiteralValue::Int(row as i64))?;
    }
    // Range-dependent formulas: exactly the shape the existing static schedule
    // cache refuses (`can_use_static_schedule_cache` rejects range deps).
    for row in 1..=40u32 {
        engine.set_cell_formula("Sheet1", row, 2, parse("=SUM($A$1:$A$40)").unwrap())?;
    }
    engine.set_cell_formula("Sheet1", 1, 3, parse("=SUM($B$1:$B$40)").unwrap())?;
    Ok(engine)
}

/// A workbook whose schedule contains a genuine SCC: B1 and C1 are mutually
/// dependent, settled by `CyclePolicy::Iterate`. A1 is an ordinary value, so
/// the book can also be value-edited.
fn build_cycle_workbook(config: EvalConfig) -> Result<Engine<TestWorkbook>, ExcelError> {
    let mut engine = Engine::new(TestWorkbook::new(), config);
    engine.set_cell_value("Sheet1", 1, 1, LiteralValue::Int(10))?;
    engine.set_cell_formula("Sheet1", 1, 2, parse("=0.5*$A$1+0.5*C1").unwrap())?;
    engine.set_cell_formula("Sheet1", 1, 3, parse("=0.5*B1+0.5*20").unwrap())?;
    Ok(engine)
}

/// The `virtual_dep_recheck_guard::a_spill_that_grows_during_the_pass_still_rechecks`
/// fixture: D1 spills A1 rows down column D, and F5 reads only the tail of the
/// possible footprint, so with a one-row spill it is not ordered behind D1 and
/// is not even dirty when A1 changes.
fn build_growing_spill_workbook(config: EvalConfig) -> Result<Engine<TestWorkbook>, ExcelError> {
    let mut engine = Engine::new(TestWorkbook::new(), config);
    engine.set_cell_value("Sheet1", 1, 1, LiteralValue::Number(1.0))?;
    engine.set_cell_formula("Sheet1", 1, 4, parse("=SEQUENCE($A$1)").unwrap())?;
    engine.set_cell_formula("Sheet1", 5, 6, parse("=SUM($D$5:$D$8)").unwrap())?;
    Ok(engine)
}

/// Run a bank-then-walk sequence, retrying it if the process-global function
/// registry moved underneath it.
///
/// The registry is process-global and every `evaluate_all` observes it:
/// `evaluate_all_unobserved` calls `observe_function_semantic_epoch`, which on
/// a moved semantic epoch with a non-empty change set clears both
/// `cached_static_schedule` and `spec_chain`. The eval test binary has
/// `register_function` call sites in eight other test modules, running
/// concurrently with these tests, so a registration landing between a
/// spec-chain test's banking pass and its walking pass retires the banked
/// chain and that pass runs the exact path instead.
///
/// The signature is `SpecChainTelemetry::chain_drops > 0`: an installed chain
/// was taken away from this engine during the attempt. `last_reason` cannot
/// serve. It is a single slot written several times per `evaluate_all` —
/// `try_spec_chain_evaluate` writes "no_chain" first and `install_spec_chain`
/// overwrites it later in the same request — so a drop that lands in the
/// middle of a multi-edit sequence leaves no trace in it at all: the pass that
/// observed the drop re-banks (in these fixtures a value edit dirties every
/// formula vertex, so the bank succeeds and writes nothing), the next pass
/// walks normally and sets `last_reason` to `None`, and the sequence ends
/// looking clean while one of its passes ran the exact path. That is the r9b2
/// flake in `spec_chain_matches_the_full_path_on_value_edits`.
///
/// WARNING for closures passed here: `chain_drops` counts every out-of-walk
/// retirement, and `clear_cached_static_schedule` is one of them — any
/// topology edit (`set_cell_formula`, a new sheet, a structural edit) made
/// *after* the pass that banked the chain counts a drop and sends this helper
/// round the retry loop until it panics. A closure must therefore finish its
/// topology edits before its banking pass; only value edits belong after it.
/// A test that wants to assert the drop itself must not run under
/// `chain_sequence`.
///
/// There is no shared lock to join: `scc_reuse.rs` has an `EPOCH_LOCK`, but it
/// is module-private and none of the registering modules take it, so holding
/// it would not exclude them. The engine-side alternative — not dropping the
/// chain at `observe_function_semantic_epoch` when no walked formula calls a
/// changed function — is a behaviour change and a separate decision.
///
/// `attempt` must build a *fresh* engine each call and return the telemetry
/// the walking pass produced. Every outcome other than the signature above is
/// handed straight back to the caller, so nothing a test asserts is weakened.
/// No sleep: a retry is needed only when a registration actually landed inside
/// the window, and a fresh attempt opens a fresh window.
fn chain_sequence<T>(
    mut attempt: impl FnMut() -> Result<(T, SpecChainTelemetry), ExcelError>,
) -> Result<T, ExcelError> {
    let mut observed: Vec<SpecChainTelemetry> = Vec::new();
    for _ in 0..3 {
        let (value, telemetry) = attempt()?;
        if telemetry.chain_drops > 0 {
            observed.push(telemetry);
            continue;
        }
        return Ok(value);
    }
    panic!(
        "the banked chain was retired during the sequence on all 3 attempts \
         (a concurrent `register_function` moves the semantic epoch and \
         `observe_function_semantic_epoch` drops the chain); telemetry per \
         attempt: {observed:#?}"
    );
}

#[test]
fn spec_chain_second_evaluate_all_reuses_the_chain() -> Result<(), ExcelError> {
    let _guard = spec_chain_test_guard();
    let (engine, telemetry) = chain_sequence(|| {
        let mut engine = build_range_workbook(chain_config())?;

        engine.evaluate_all()?;
        // First call builds through the ordinary schedule path and banks a chain.
        assert_eq!(engine.spec_chain_telemetry().chain_builds, 1);
        assert_eq!(engine.spec_chain_telemetry().chain_walks, 0);
        assert_eq!(engine.spec_chain_telemetry().last_path, Some("full"));

        engine.set_cell_value("Sheet1", 1, 1, LiteralValue::Int(100))?;
        engine.evaluate_all()?;

        let telemetry = engine.spec_chain_telemetry().clone();
        Ok(((engine, telemetry.clone()), telemetry))
    })?;

    assert_eq!(
        telemetry.chain_walks, 1,
        "second evaluate must walk the chain"
    );
    assert_eq!(telemetry.chain_builds, 1, "no rebuild on the second call");
    assert_eq!(telemetry.fallbacks, 1, "only the first call fell back");
    assert_eq!(telemetry.last_path, Some("chain"));
    assert_eq!(telemetry.demotion_rounds, 0);
    assert!(engine.spec_chain_is_installed());
    Ok(())
}

#[test]
fn spec_chain_matches_the_full_path_on_value_edits() -> Result<(), ExcelError> {
    let _guard = spec_chain_test_guard();
    let (with_counts, without_counts, telemetry) = chain_sequence(|| {
        let mut with = build_range_workbook(chain_config())?;
        let mut without = build_range_workbook(plain_config())?;

        let mut with_counts = vec![with.evaluate_all()?.computed_vertices];
        let mut without_counts = vec![without.evaluate_all()?.computed_vertices];

        for value in [7i64, 11, 13] {
            with.set_cell_value("Sheet1", 1, 1, LiteralValue::Int(value))?;
            with_counts.push(with.evaluate_all()?.computed_vertices);
            without.set_cell_value("Sheet1", 1, 1, LiteralValue::Int(value))?;
            without_counts.push(without.evaluate_all()?.computed_vertices);

            for row in 1..=40u32 {
                assert_eq!(
                    with.get_cell_value("Sheet1", row, 2),
                    without.get_cell_value("Sheet1", row, 2),
                    "row {row} diverged at value {value}"
                );
            }
            assert_eq!(
                with.get_cell_value("Sheet1", 1, 3),
                without.get_cell_value("Sheet1", 1, 3)
            );
        }

        let telemetry = with.spec_chain_telemetry().clone();
        Ok(((with_counts, without_counts, telemetry.clone()), telemetry))
    })?;

    // Count equality is guaranteed for THIS fixture, which is why it is still
    // asserted here even though counts are not a chain-vs-exact gate in
    // general. The chain walk evaluates only dirty vertices while the exact
    // path also evaluates the pass-through vertices the demand subgraph admits
    // unconditionally — `NamedScalar`, `NamedArray`, `Range`, `InfiniteRange`
    // (that is the 68,983-vs-68,980 gap the Avocet flip measured). This book
    // defines no names, and no `Range`/`InfiniteRange` vertex is ever
    // constructed in this crate: `VertexKind::Range` appears only in the
    // discriminant decode and in three kind matches, never in a vertex
    // creation. So both paths cover exactly the 41 dirty formula vertices.
    assert_eq!(with_counts, without_counts, "computed counts must match");
    assert_eq!(telemetry.chain_walks, 3);
    Ok(())
}

#[test]
fn spec_chain_invalidated_by_a_topology_edit() -> Result<(), ExcelError> {
    let _guard = spec_chain_test_guard();
    let (mut engine, telemetry) = chain_sequence(|| {
        let mut engine = build_range_workbook(chain_config())?;
        engine.evaluate_all()?;
        engine.set_cell_value("Sheet1", 1, 1, LiteralValue::Int(5))?;
        engine.evaluate_all()?;
        let telemetry = engine.spec_chain_telemetry().clone();
        Ok(((engine, telemetry.clone()), telemetry))
    })?;
    assert_eq!(telemetry.chain_walks, 1);

    // A formula edit bumps the topology epoch and must retire the chain.
    engine.set_cell_formula("Sheet1", 2, 4, parse("=SUM($A$1:$A$20)").unwrap())?;
    assert!(
        !engine.spec_chain_is_installed(),
        "topology edit retires chain"
    );

    engine.evaluate_all()?;
    let telemetry = engine.spec_chain_telemetry().clone();
    assert_eq!(telemetry.chain_walks, 1, "the post-edit call did not walk");
    assert_eq!(telemetry.last_path, Some("full"));
    // The post-edit pass dirties only the sub-graph the edit touched, so it is
    // not a chain: the chain is banked only from a pass in which every formula
    // vertex was dirty.
    assert_eq!(telemetry.chain_builds, 1);
    assert_eq!(
        telemetry.last_reason,
        Some("producers_not_all_dirty_at_bank_time")
    );
    Ok(())
}

#[test]
fn spec_chain_refuses_to_bank_from_a_partial_pass() -> Result<(), ExcelError> {
    let _guard = spec_chain_test_guard();
    let mut engine = build_range_workbook(chain_config())?;
    engine.evaluate_all()?;
    assert_eq!(engine.spec_chain_telemetry().chain_builds, 1);

    // Retire the chain, then run a pass that is NOT a full recalc: only the
    // newly added formula and its dependents are dirty. Its order was only
    // ever proven for the producers that were dirty in it, so it must not be
    // banked even though every formula vertex may well appear in the schedule.
    engine.set_cell_formula("Sheet1", 2, 4, parse("=$A$1+1").unwrap())?;
    assert!(!engine.spec_chain_is_installed());

    engine.evaluate_all()?;
    let telemetry = engine.spec_chain_telemetry().clone();
    assert_eq!(
        telemetry.chain_builds, 1,
        "the partial pass must not bank a chain"
    );
    assert!(!engine.spec_chain_is_installed());
    assert_eq!(
        telemetry.last_reason,
        Some("producers_not_all_dirty_at_bank_time")
    );

    // A pass in which every formula vertex is dirty — here a fresh engine over
    // the same book, which is the shape the warmed-session runtime starts
    // from — does bank. (The accepted limitation: an editing session that
    // never does a full recalc never gets a chain.)
    let mut fresh = build_range_workbook(chain_config())?;
    fresh.evaluate_all()?;
    assert!(fresh.spec_chain_is_installed());
    assert_eq!(fresh.spec_chain_telemetry().chain_builds, 1);
    Ok(())
}

#[test]
fn spec_chain_handles_a_new_formula_vertex_by_falling_back() -> Result<(), ExcelError> {
    let _guard = spec_chain_test_guard();
    let mut engine = build_range_workbook(chain_config())?;
    engine.evaluate_all()?;
    assert!(engine.spec_chain_is_installed(), "the full pass banks");

    // Adding a formula bumps the topology epoch, which retires the chain at the
    // edit through `clear_cached_static_schedule` — so the next request does
    // not even reach a staleness check, it finds no chain at all.
    engine.set_cell_formula("Sheet1", 3, 5, parse("=$A$1+1").unwrap())?;
    assert!(!engine.spec_chain_is_installed());

    engine.evaluate_all()?;
    assert_eq!(
        engine.get_cell_value("Sheet1", 3, 5),
        Some(LiteralValue::Number(2.0))
    );

    let telemetry = engine.spec_chain_telemetry().clone();
    assert_eq!(telemetry.last_path, Some("full"), "the request fell back");
    assert_eq!(telemetry.chain_walks, 0, "nothing was walked");
    assert_eq!(
        telemetry.fallbacks, 2,
        "both requests reached the exact path"
    );
    // `last_reason` is the LAST thing written in the request, and the fallback's
    // own "no_chain" is overwritten later in the same `evaluate_all` by the bank
    // refusal: the post-edit pass dirties only E3, not every formula vertex.
    assert_eq!(
        telemetry.last_reason,
        Some("producers_not_all_dirty_at_bank_time")
    );
    Ok(())
}

/// Registered by `spec_chain_retired_by_a_function_registration` only, so that
/// the registration it performs is this test's own and is guaranteed to move
/// the registry's semantic epoch.
struct SpecChainRegistryProbe;

impl Function for SpecChainRegistryProbe {
    fn caps(&self) -> FnCaps {
        FnCaps::PURE
    }

    fn name(&self) -> &'static str {
        "SPEC_CHAIN_REGISTRY_PROBE"
    }

    fn eval<'a, 'b, 'c>(
        &self,
        _args: &'c [ArgumentHandle<'a, 'b>],
        _ctx: &dyn FunctionContext<'b>,
    ) -> Result<crate::traits::CalcValue<'b>, ExcelError> {
        Ok(crate::traits::CalcValue::Scalar(LiteralValue::Number(1.0)))
    }
}

/// Pins the signature `chain_sequence` retries, so the retry is anchored to an
/// asserted mechanism rather than to a guess about a flake.
///
/// A `register_function` call moves the registry's semantic epoch with a
/// non-empty change set; the next `evaluate_all` observes it in
/// `observe_function_semantic_epoch` and drops the banked chain, so the
/// request falls back to the exact path and reports
/// `last_reason == Some("no_chain")` with `last_path == Some("full")`. The
/// values it produces still have to be right, which is checked against an
/// engine that never had the chain enabled at all.
#[test]
fn spec_chain_retired_by_a_function_registration() -> Result<(), ExcelError> {
    let _guard = spec_chain_test_guard();
    let mut engine = build_range_workbook(chain_config())?;
    let mut reference = build_range_workbook(plain_config())?;
    engine.evaluate_all()?;
    reference.evaluate_all()?;
    assert!(
        engine.spec_chain_is_installed(),
        "the full pass banks a chain"
    );

    function_registry::register_function(Arc::new(SpecChainRegistryProbe));

    engine.set_cell_value("Sheet1", 1, 1, LiteralValue::Int(100))?;
    engine.evaluate_all()?;
    reference.set_cell_value("Sheet1", 1, 1, LiteralValue::Int(100))?;
    reference.evaluate_all()?;

    let telemetry = engine.spec_chain_telemetry().clone();
    assert_eq!(
        telemetry.last_reason,
        Some("no_chain"),
        "a registration between the two passes retires the banked chain"
    );
    assert_eq!(telemetry.last_path, Some("full"));
    assert_eq!(telemetry.chain_walks, 0, "the retired chain was not walked");
    assert_eq!(
        telemetry.chain_drops, 1,
        "the drop is what `chain_sequence` retries on"
    );

    for row in 1..=40u32 {
        assert_eq!(
            engine.get_cell_value("Sheet1", row, 2),
            reference.get_cell_value("Sheet1", row, 2),
            "row {row} diverged after the registration"
        );
    }
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 3),
        reference.get_cell_value("Sheet1", 1, 3)
    );
    Ok(())
}

/// A schedule containing an SCC IS banked, and a request that dirties the
/// whole SCC walks it (clause (f) of the safety argument).
///
/// The earlier revision refused any schedule carrying a cycle unit. Measured
/// on both r9 gate workbooks that removed the chain entirely: they report
/// `cycles = 0` — no `#CIRC!` stamps at all — and yet their schedules contain
/// cycle units, because a `ScheduleUnit::Cycle` is any statically-cyclic SCC
/// (`Scheduler::separate_cycles_with_virtual`: size >= 2, or a self-loop, over
/// an edge set that includes the pass's virtual relay edges), and under
/// `CycleDetection::Runtime` such a unit settles through `evaluate_scc_unit`
/// without stamping anything.
///
/// The rule that replaced the refusal is per unit, not per schedule: settle
/// the banked members only when every settleable member is pending, so the
/// banked component is exactly the SCC this request's exact path would have
/// built. This fixture is the all-pending case — B1 and C1 are mutually
/// dependent, so any edit reaching one reaches both (`mark_dirty_many` BFS
/// over dependents; dirtiness is mutually reachable inside an SCC).
#[test]
fn spec_chain_banks_and_walks_a_schedule_containing_a_cycle() -> Result<(), ExcelError> {
    let _guard = spec_chain_test_guard();
    let (banked, installed, values, telemetry) = chain_sequence(|| {
        let mut engine = build_cycle_workbook(iterative_config(true))?;
        let mut reference = build_cycle_workbook(iterative_config(false))?;

        engine.evaluate_all()?;
        reference.evaluate_all()?;
        let banked = engine.spec_chain_telemetry().chain_builds;
        let installed = engine.spec_chain_is_installed();

        // A value edit on A1 dirties B1, and the SCC closes dirtiness over C1:
        // every settleable member of the cycle unit is pending.
        engine.set_cell_value("Sheet1", 1, 1, LiteralValue::Int(30))?;
        reference.set_cell_value("Sheet1", 1, 1, LiteralValue::Int(30))?;
        engine.evaluate_all()?;
        reference.evaluate_all()?;

        let values: Vec<(Option<LiteralValue>, Option<LiteralValue>)> = (2..=3u32)
            .map(|col| {
                (
                    engine.get_cell_value("Sheet1", 1, col),
                    reference.get_cell_value("Sheet1", 1, col),
                )
            })
            .collect();
        let telemetry = engine.spec_chain_telemetry().clone();
        Ok((
            (banked, installed, values, telemetry.clone()),
            telemetry,
        ))
    })?;

    assert_eq!(banked, 1, "a schedule with a cycle unit is banked");
    assert!(installed, "and the chain stays installed");
    assert_eq!(
        telemetry.chain_walks, 1,
        "the value edit was served by the chain walk"
    );
    assert_eq!(telemetry.last_path, Some("chain"));
    assert_eq!(telemetry.last_reason, None, "the walk completed");
    for (col, (chained, plain)) in (2..=3u32).zip(values) {
        assert_eq!(chained, plain, "column {col} diverged");
    }
    Ok(())
}

// The partially-pending case (`partially_pending_cycle_unit`) has no test.
// It is the divergent one clause (f) exists for, but it is not constructible
// from the public API by source reading alone. Inside an SCC every member is
// a dependent of every other, so `mark_dirty_many`'s BFS over dependents
// makes dirtiness all-or-nothing: an edit reaching one member reaches them
// all, retained/iterative membership included (`retained_scc_members` holds
// members clean *between* requests, but "any edit that reaches a member
// dirties it like any other formula"). A partially pending unit therefore
// needs a component that is cyclic only through the per-pass virtual/relay
// edges `build_regionized` synthesises — a range reader whose region node
// closes a loop — which the dirty BFS does not traverse in the same shape.
// That is the r9 gate workbooks' situation, not something this crate's
// `TestWorkbook` fixtures reach. The branch is reachable and cheap; it is
// left to a fixture built from a captured book.

/// The mid-walk spill check: a dynamic array that grows during a chain walk
/// hands the rest of the request to the exact path, and the request's values
/// are the ones a never-chained engine produces.
///
/// D1 spills `SEQUENCE($A$1)` down column D. F5 reads only D5:D8, so with a
/// one-row spill it is neither a dependent of D1 nor dirty when A1 changes:
/// the walk's dirty set is `{D1}` alone. Growing the spill to eight rows
/// commits a footprint that invalidates F5 *during* the walk, which is exactly
/// what the mid-walk check exists to catch.
#[test]
fn spec_chain_falls_back_when_a_spill_grows_during_the_walk() -> Result<(), ExcelError> {
    let _guard = spec_chain_test_guard();
    let mut engine = build_growing_spill_workbook(chain_config())?;
    let mut reference = build_growing_spill_workbook(plain_config())?;

    engine.evaluate_all()?;
    reference.evaluate_all()?;
    assert!(
        engine.spec_chain_is_installed(),
        "the full pass banks a chain over {{D1, F5}}"
    );
    assert_eq!(engine.spec_chain_telemetry().chain_builds, 1);
    let fallbacks_before = engine.spec_chain_telemetry().fallbacks;

    engine.set_cell_value("Sheet1", 1, 1, LiteralValue::Number(8.0))?;
    reference.set_cell_value("Sheet1", 1, 1, LiteralValue::Number(8.0))?;
    engine.evaluate_all()?;
    reference.evaluate_all()?;

    assert_eq!(
        engine.get_cell_value("Sheet1", 5, 6),
        Some(LiteralValue::Number(26.0)),
        "5+6+7+8, i.e. the reader saw the grown spill"
    );
    assert_eq!(
        engine.get_cell_value("Sheet1", 5, 6),
        reference.get_cell_value("Sheet1", 5, 6)
    );
    for row in 1..=8u32 {
        assert_eq!(
            engine.get_cell_value("Sheet1", row, 4),
            reference.get_cell_value("Sheet1", row, 4),
            "spilled row {row} diverged"
        );
    }

    let telemetry = engine.spec_chain_telemetry().clone();
    assert_eq!(telemetry.last_path, Some("full"), "the walk did not finish");
    assert_eq!(telemetry.chain_walks, 0, "no walk completed");
    assert_eq!(
        telemetry.fallbacks,
        fallbacks_before + 1,
        "exactly one fallback, and the only one this request could take"
    );
    assert_eq!(
        telemetry.demotion_rounds, 0,
        "the fallback was not the demotion cap"
    );
    assert_eq!(
        telemetry.chain_drops, 0,
        "and not an out-of-walk drop either: this test does not run under \
         `chain_sequence`, so the elimination has to rule that out itself"
    );
    // The fallback's own reason, "spill_footprint_moved_mid_walk", is not
    // readable at the end of the request: the exact path runs next and its
    // converged branch overwrites `last_reason` with the bank refusal. The
    // walk had already evaluated and cleared D1 before re-dirtying F5, so that
    // pass is partial and refuses to bank. The fallback itself is pinned by
    // `fallbacks` above: the chain was installed, the topology and footprint
    // epochs were unmoved at admission, the dirty set `{D1}` is inside the
    // chain, and no demotion round ran — the mid-walk spill check is the only
    // remaining source of a fallback on this request.
    assert_eq!(
        telemetry.last_reason,
        Some("producers_not_all_dirty_at_bank_time")
    );
    Ok(())
}
