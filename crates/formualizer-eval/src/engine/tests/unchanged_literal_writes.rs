//! Rewriting a cell with the literal it already holds must not dirty the cell's
//! dependents. See `Engine::is_unchanged_literal_write`.

use crate::engine::{Engine, EvalConfig};
use crate::function::Function;
use crate::test_workbook::TestWorkbook;
use crate::traits::{ArgumentHandle, CalcValue, FunctionContext};
use formualizer_common::{ExcelError, LiteralValue};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug)]
struct CountingPassthrough {
    calls: Arc<AtomicUsize>,
}

impl Function for CountingPassthrough {
    fn name(&self) -> &'static str {
        "MYFN"
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

fn engine_with_counter() -> (Engine<TestWorkbook>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let workbook = TestWorkbook::new().with_function(Arc::new(CountingPassthrough {
        calls: Arc::clone(&calls),
    }));
    let mut config = EvalConfig::default();
    config.enable_parallel = false;
    (Engine::new(workbook, config), calls)
}

/// A1 literal, B1 `=A1*2`, C1 `=MYFN(B1)`.
#[test]
fn rewriting_a_cell_with_its_own_value_does_not_recompute_dependents() {
    let (mut engine, calls) = engine_with_counter();

    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Number(3.0))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 2, formualizer_parse::parser::parse("=A1*2").unwrap())
        .unwrap();
    engine
        .set_cell_formula(
            "Sheet1",
            1,
            3,
            formualizer_parse::parser::parse("=MYFN(B1)").unwrap(),
        )
        .unwrap();

    engine.evaluate_all().unwrap();
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 2),
        Some(LiteralValue::Number(6.0))
    );
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 3),
        Some(LiteralValue::Number(6.0))
    );
    let after_first = calls.load(Ordering::SeqCst);
    assert_eq!(after_first, 1);

    // Rewrite A1 with the value it already holds: nothing downstream re-runs.
    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Number(3.0))
        .unwrap();
    engine.evaluate_all().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), after_first);
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 2),
        Some(LiteralValue::Number(6.0))
    );
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 3),
        Some(LiteralValue::Number(6.0))
    );

    // An Int that normalizes to the stored Number is still the same literal.
    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Int(3))
        .unwrap();
    engine.evaluate_all().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), after_first);

    // A different value does re-run the dependents.
    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Number(4.0))
        .unwrap();
    engine.evaluate_all().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), after_first + 1);
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 3),
        Some(LiteralValue::Number(8.0))
    );
}

/// The fast path keys on the vertex kind, so a formula cell overwritten with a
/// literal equal to its own cached result must still become a literal.
#[test]
fn overwriting_a_formula_with_its_cached_result_is_not_skipped() {
    let (mut engine, _calls) = engine_with_counter();

    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Number(3.0))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 1, 2, formualizer_parse::parser::parse("=A1*2").unwrap())
        .unwrap();
    engine.evaluate_all().unwrap();
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 2),
        Some(LiteralValue::Number(6.0))
    );
    assert!(
        engine.get_cell("Sheet1", 1, 2).is_some_and(|(ast, _)| ast.is_some()),
        "B1 must be a formula before the overwrite"
    );

    engine
        .set_cell_value("Sheet1", 1, 2, LiteralValue::Number(6.0))
        .unwrap();

    assert!(
        engine
            .get_cell("Sheet1", 1, 2)
            .is_none_or(|(ast, _)| ast.is_none()),
        "writing a literal over a formula must drop the formula"
    );

    // And the cell now behaves as a literal: changing A1 leaves it at 6.
    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Number(10.0))
        .unwrap();
    engine.evaluate_all().unwrap();
    assert_eq!(
        engine.get_cell_value("Sheet1", 1, 2),
        Some(LiteralValue::Number(6.0))
    );
}

/// NaN is never equal to itself, so a NaN rewrite cannot take the fast path.
#[test]
fn nan_rewrite_is_never_treated_as_unchanged() {
    let (mut engine, _calls) = engine_with_counter();
    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Number(f64::NAN))
        .unwrap();
    assert!(!engine.is_unchanged_literal_write("Sheet1", 1, 1, &LiteralValue::Number(f64::NAN)));
}

/// An empty cell rewritten as empty keeps the full write path, because writing
/// `Empty` still has to create the vertex and stays undo-symmetric.
#[test]
fn empty_writes_are_never_skipped() {
    let (mut engine, _calls) = engine_with_counter();
    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Empty)
        .unwrap();
    assert!(!engine.is_unchanged_literal_write("Sheet1", 1, 1, &LiteralValue::Empty));
}
