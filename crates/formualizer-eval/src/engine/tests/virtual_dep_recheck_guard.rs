//! The post-pass virtual-dependency recheck rebuilds every candidate's
//! virtual dependencies to see whether the pass changed one. It is skipped
//! when nothing in the pass could have changed one: no runtime-reference
//! formula in the domain, and no committed output footprint invalidated.

use crate::engine::{Engine, EvalConfig};
use crate::test_workbook::TestWorkbook;
use formualizer_common::LiteralValue;
use formualizer_parse::parser::parse;

fn engine() -> Engine<TestWorkbook> {
    Engine::new(
        TestWorkbook::new(),
        EvalConfig {
            // These tests pin how the post-pass recheck itself routes, which
            // the speculative chain bypasses; do not take the flag from the
            // ambient `FZ_SPEC_CHAIN`.
            speculative_chain: false,
            ..EvalConfig::default()
        },
    )
}

/// An ordinary workbook — values, scalar formulas and a range read — has
/// nothing that can move mid-pass, so no pass pays for a rebuild.
#[test]
fn a_static_workbook_skips_the_recheck() {
    let mut engine = engine();
    for row in 1..=20 {
        engine
            .set_cell_value("Sheet1", row, 1, LiteralValue::Number(row as f64))
            .unwrap();
    }
    engine
        .set_cell_formula("Sheet1", 1, 3, parse("=SUM(A1:A20)").unwrap())
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 2, 3, parse("=C1*2").unwrap())
        .unwrap();
    engine.evaluate_all().unwrap();

    assert_eq!(
        engine.get_cell_value("Sheet1", 2, 3),
        Some(LiteralValue::Number(420.0))
    );
    let (rebuilds, skips) = engine.virtual_dep_recheck_counts();
    assert_eq!(rebuilds, 0, "a static workbook must not rebuild vdeps");
    assert!(skips > 0, "the recheck must actually have been reached");
}

/// A runtime-reference formula can read somewhere else once its precedents
/// have values, so its passes must still rebuild — and must still replan when
/// the rebuild finds a new dependency.
#[test]
fn an_offset_workbook_still_rechecks_and_replans() {
    let mut engine = engine();
    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Number(3.0))
        .unwrap();
    for row in 1..=4 {
        engine
            .set_cell_value("Sheet1", row, 2, LiteralValue::Number((row * 10) as f64))
            .unwrap();
    }
    // Reads B<A1>, i.e. B3, but only once A1 has been evaluated.
    engine
        .set_cell_formula("Sheet1", 1, 4, parse("=OFFSET($B$1,$A$1-1,0)").unwrap())
        .unwrap();
    engine.evaluate_all().unwrap();

    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 4),
        Some(LiteralValue::Number(30.0))
    );
    let (rebuilds, _) = engine.virtual_dep_recheck_counts();
    assert!(
        rebuilds > 0,
        "a workbook holding a runtime reference must rebuild its vdeps"
    );
}

#[test]
fn an_indirect_workbook_still_rechecks() {
    let mut engine = engine();
    engine
        .set_cell_value("Sheet1", 1, 2, LiteralValue::Number(7.0))
        .unwrap();
    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Text("B1".to_string()))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 4, parse("=INDIRECT($A$1)").unwrap())
        .unwrap();
    engine.evaluate_all().unwrap();

    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 4),
        Some(LiteralValue::Number(7.0))
    );
    let (rebuilds, _) = engine.virtual_dep_recheck_counts();
    assert!(rebuilds > 0, "INDIRECT must rebuild its vdeps");
}

/// A spill that grows during the pass reaches cells no reader was ordered
/// behind when the pass was scheduled. That commit invalidates the formulas
/// whose reads it now covers, which is exactly the signal the guard keys on,
/// so the recheck still runs and the reader still sees the grown spill.
#[test]
fn a_spill_that_grows_during_the_pass_still_rechecks() {
    let mut engine = engine();
    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Number(1.0))
        .unwrap();
    // Spills A1 rows down column D.
    engine
        .set_cell_formula("Sheet1", 1, 4, parse("=SEQUENCE($A$1)").unwrap())
        .unwrap();
    // Reads only the tail of the possible footprint: with a one-row spill
    // nothing in D5:D8 exists, so this reader is not ordered behind D1.
    engine
        .set_cell_formula("Sheet1", 5, 6, parse("=SUM($D$5:$D$8)").unwrap())
        .unwrap();
    engine.evaluate_all().unwrap();
    assert_eq!(
        engine.get_cell_value("Sheet1", 5, 6),
        Some(LiteralValue::Number(0.0)),
        "the spill does not reach D5:D8 yet"
    );

    // Grow it into the reader's range during the next pass.
    let before = engine.virtual_dep_recheck_counts().0;
    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Number(8.0))
        .unwrap();
    engine.evaluate_all().unwrap();
    assert_eq!(
        engine.get_cell_value("Sheet1", 5, 6),
        Some(LiteralValue::Number(26.0)),
        "5+6+7+8"
    );
    assert!(
        engine.virtual_dep_recheck_counts().0 > before,
        "a spill growing into a read during the pass must still be rechecked"
    );
}
