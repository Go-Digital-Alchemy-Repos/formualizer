//! PROTOTYPE (r8b): tests for the Excel-style speculative calculation chain
//! (`EvalConfig::speculative_chain`).

use crate::engine::{Engine, EvalConfig};
use crate::test_workbook::TestWorkbook;
use formualizer_common::{ExcelError, LiteralValue};
use formualizer_parse::parser::parse;

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

#[test]
fn spec_chain_second_evaluate_all_reuses_the_chain() -> Result<(), ExcelError> {
    let mut engine = build_range_workbook(chain_config())?;

    engine.evaluate_all()?;
    // First call builds through the ordinary schedule path and banks a chain.
    assert_eq!(engine.spec_chain_telemetry().chain_builds, 1);
    assert_eq!(engine.spec_chain_telemetry().chain_walks, 0);
    assert_eq!(engine.spec_chain_telemetry().last_path, Some("full"));

    engine.set_cell_value("Sheet1", 1, 1, LiteralValue::Int(100))?;
    engine.evaluate_all()?;

    let telemetry = engine.spec_chain_telemetry().clone();
    assert_eq!(telemetry.chain_walks, 1, "second evaluate must walk the chain");
    assert_eq!(telemetry.chain_builds, 1, "no rebuild on the second call");
    assert_eq!(telemetry.fallbacks, 1, "only the first call fell back");
    assert_eq!(telemetry.last_path, Some("chain"));
    assert_eq!(telemetry.demotion_rounds, 0);
    assert!(engine.spec_chain_is_installed());
    Ok(())
}

#[test]
fn spec_chain_matches_the_full_path_on_value_edits() -> Result<(), ExcelError> {
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

    assert_eq!(with_counts, without_counts, "computed counts must match");
    assert_eq!(with.spec_chain_telemetry().chain_walks, 3);
    Ok(())
}

#[test]
fn spec_chain_invalidated_by_a_topology_edit() -> Result<(), ExcelError> {
    let mut engine = build_range_workbook(chain_config())?;
    engine.evaluate_all()?;
    engine.set_cell_value("Sheet1", 1, 1, LiteralValue::Int(5))?;
    engine.evaluate_all()?;
    assert_eq!(engine.spec_chain_telemetry().chain_walks, 1);

    // A formula edit bumps the topology epoch and must retire the chain.
    engine.set_cell_formula("Sheet1", 2, 4, parse("=SUM($A$1:$A$20)").unwrap())?;
    assert!(!engine.spec_chain_is_installed(), "topology edit retires chain");

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
    let mut engine = build_range_workbook(chain_config())?;
    engine.evaluate_all()?;
    engine.set_cell_formula("Sheet1", 3, 5, parse("=$A$1+1").unwrap())?;
    engine.evaluate_all()?;
    assert_eq!(
        engine.get_cell_value("Sheet1", 3, 5),
        Some(LiteralValue::Number(2.0))
    );
    Ok(())
}
