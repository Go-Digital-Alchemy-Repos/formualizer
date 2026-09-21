//! A spill anchor that re-commits the *same* rectangle with the same values
//! must not invalidate its readers a second time.
//!
//! Shape of the Avocet parent's XCALL cells: a custom function returns a fixed
//! rectangle (here 8x5) that a large number of downstream formulas read. When
//! an unrelated scalar input changes, the anchor is dirtied, its footprint is
//! cleared, and the pass re-commits an identical rectangle. Before the fix the
//! clear and the re-commit each queued every reader as an output invalidation,
//! which cost a whole extra replan iteration over the closure.

use crate::engine::{EvalConfig, eval::Engine};
use crate::function::{FnCaps, Function};
use crate::function_registry;
use crate::test_workbook::TestWorkbook;
use crate::traits::{ArgumentHandle, FunctionContext};
use formualizer_common::{ExcelError, LiteralValue};
use formualizer_parse::parser::parse;
use std::sync::Arc;

const SPILL_ROWS: usize = 8;
const SPILL_COLS: usize = 5;
/// Readers of the spill rectangle, chained so each one is its own layer.
const READERS: u32 = 3_000;

/// Stands in for the Python XCALL router: takes one argument (so the anchor
/// depends on the scalar input) and always returns the same rectangle.
#[derive(Debug)]
struct SpillStubFn;

impl Function for SpillStubFn {
    fn caps(&self) -> FnCaps {
        FnCaps::PURE
    }

    fn name(&self) -> &'static str {
        "XCALL_SPILL_STUB"
    }

    fn min_args(&self) -> usize {
        1
    }

    fn arg_schema(&self) -> &'static [crate::args::ArgSchema] {
        use std::sync::LazyLock;
        static SCHEMA: LazyLock<Vec<crate::args::ArgSchema>> =
            LazyLock::new(|| vec![crate::args::ArgSchema::number_lenient_scalar()]);
        &SCHEMA
    }

    fn eval<'a, 'b, 'c>(
        &self,
        _args: &'c [ArgumentHandle<'a, 'b>],
        _ctx: &dyn FunctionContext<'b>,
    ) -> Result<crate::traits::CalcValue<'b>, ExcelError> {
        let rows: Vec<Vec<LiteralValue>> = (0..SPILL_ROWS)
            .map(|r| {
                (0..SPILL_COLS)
                    .map(|c| LiteralValue::Number((r * SPILL_COLS + c) as f64))
                    .collect()
            })
            .collect();
        Ok(crate::traits::CalcValue::Scalar(LiteralValue::Array(rows)))
    }
}

/// `A1` scalar input; `C1` spills 8x5 into C1:G8; `J1..J{READERS}` is a chain
/// where every link reads the whole rectangle.
fn build_engine() -> Engine<TestWorkbook> {
    function_registry::register_function(Arc::new(SpillStubFn));
    let cfg = EvalConfig {
        enable_virtual_dep_telemetry: true,
        ..Default::default()
    };
    let mut engine = Engine::new(TestWorkbook::new(), cfg);
    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Number(1.0))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 3, parse("=XCALL_SPILL_STUB(A1)").unwrap())
        .unwrap();
    for row in 1..=READERS {
        let f = if row == 1 {
            "=SUM($D$2:$G$8)".to_string()
        } else {
            format!("=J{}+SUM($D$2:$G$8)", row - 1)
        };
        engine
            .set_cell_formula("Sheet1", row, 10, parse(&f).unwrap())
            .unwrap();
    }
    engine.evaluate_all().unwrap();
    engine
}

#[test]
fn respill_propagates_dirtiness_once_for_the_whole_rectangle() {
    let mut engine = build_engine();

    let before = engine.dirty_propagation_visits();
    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Number(2.0))
        .unwrap();
    let result = engine.evaluate_all().unwrap();
    let visits = engine.dirty_propagation_visits() - before;
    let telemetry = engine.last_virtual_dep_telemetry();
    println!(
        "re-spill: computed={} propagation_visits={} replan_iterations={} changed_vdeps={}",
        result.computed_vertices,
        visits,
        telemetry.replan_iterations,
        telemetry.changed_vdeps_total
    );

    // The rectangle holds SPILL_ROWS*SPILL_COLS cells, all feeding the same
    // reader component. One multi-source propagation visits that component a
    // small number of times; a per-cell loop visits it once per spill cell,
    // which is what this bound rules out.
    let component = READERS as u64;
    assert!(
        visits < component * (SPILL_ROWS * SPILL_COLS) as u64 / 4,
        "dirty propagation is O(cells x component): {visits} visits for a \
         {component}-vertex component and {} spill cells",
        SPILL_ROWS * SPILL_COLS
    );
    assert_eq!(telemetry.replan_iterations, 0);
    assert_eq!(telemetry.changed_vdeps_total, 0);
    assert!(
        result.computed_vertices <= READERS as usize + 8,
        "readers must evaluate once, got {}",
        result.computed_vertices
    );
    assert_eq!(
        engine.get_cell_value("Sheet1", 8, 7),
        Some(LiteralValue::Number(
            ((SPILL_ROWS - 1) * SPILL_COLS + SPILL_COLS - 1) as f64
        ))
    );
}

#[test]
fn shrinking_the_footprint_clears_the_cells_it_no_longer_covers() {
    let mut engine = build_engine();
    // Sanity: the far corner of the 8x5 rectangle is populated.
    assert_eq!(
        engine.get_cell_value("Sheet1", 8, 7),
        Some(LiteralValue::Number(
            ((SPILL_ROWS - 1) * SPILL_COLS + SPILL_COLS - 1) as f64
        ))
    );

    // Replace the anchor with a formula that spills a strictly smaller
    // rectangle. Previously covered cells must be cleared, and the readers
    // must see the new values.
    engine
        .set_cell_formula("Sheet1", 1, 3, parse("={1,2;3,4}").unwrap())
        .unwrap();
    engine.evaluate_all().unwrap();

    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 3),
        Some(LiteralValue::Number(1.0))
    );
    assert_eq!(
        engine.get_cell_value("Sheet1", 2, 4),
        Some(LiteralValue::Number(4.0))
    );
    // Outside the new 2x2 footprint: cleared.
    assert!(
        matches!(
            engine.get_cell_value("Sheet1", 8, 7),
            None | Some(LiteralValue::Empty)
        ),
        "a shrunk footprint must clear the cells it no longer covers, got {:?}",
        engine.get_cell_value("Sheet1", 8, 7)
    );
    // The reader chain recomputed against the new rectangle.
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 10),
        Some(LiteralValue::Number(4.0))
    );
}

#[test]
fn a_blocked_respill_collapses_to_spill_error_and_clears_the_rectangle() {
    let mut engine = build_engine();

    // Drop a literal inside the rectangle so the next commit cannot spill.
    engine
        .set_cell_value("Sheet1", 4, 5, LiteralValue::Text("blocker".into()))
        .unwrap();
    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Number(3.0))
        .unwrap();
    engine.evaluate_all().unwrap();

    assert!(
        matches!(
            engine.get_cell_value("Sheet1", 1, 3),
            Some(LiteralValue::Error(_))
        ),
        "a blocked re-spill must surface an error at the anchor, got {:?}",
        engine.get_cell_value("Sheet1", 1, 3)
    );
    assert_eq!(
        engine.get_cell_value("Sheet1", 4, 5),
        Some(LiteralValue::Text("blocker".into())),
        "the blocker must survive"
    );
    assert!(
        matches!(
            engine.get_cell_value("Sheet1", 8, 7),
            None | Some(LiteralValue::Empty)
        ),
        "a blocked re-spill must clear the previously covered cells, got {:?}",
        engine.get_cell_value("Sheet1", 8, 7)
    );
}
