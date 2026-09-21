//! Re-sending an input a cell already holds must be a no-op end to end:
//! nothing is journalled, no dependent is dirtied, and undo history is
//! unaffected. See `Engine::is_unchanged_literal_write`.

use formualizer_common::ExcelError;
use formualizer_workbook::{CustomFnOptions, LiteralValue, Workbook, WorkbookConfig};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const SHEET: &str = "Sheet1";

fn workbook(changelog: bool) -> (Workbook, Arc<AtomicUsize>) {
    let mut config = WorkbookConfig::ephemeral();
    config.enable_changelog = changelog;
    let mut wb = Workbook::new_with_config(config);

    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    wb.register_custom_function(
        "myfn",
        CustomFnOptions::default(),
        Arc::new(move |args: &[LiteralValue]| -> Result<LiteralValue, ExcelError> {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(args.first().cloned().unwrap_or(LiteralValue::Empty))
        }),
    )
    .unwrap();

    (wb, calls)
}

/// A1 literal, B1 `=A1*2`, C1 `=MYFN(B1)` — evaluated once.
fn build(wb: &mut Workbook) {
    wb.set_value(SHEET, 1, 1, LiteralValue::Number(3.0)).unwrap();
    wb.set_formula(SHEET, 1, 2, "=A1*2").unwrap();
    wb.set_formula(SHEET, 1, 3, "=MYFN(B1)").unwrap();
    wb.evaluate_all().unwrap();
    assert_eq!(wb.get_value(SHEET, 1, 2), Some(LiteralValue::Number(6.0)));
    assert_eq!(wb.get_value(SHEET, 1, 3), Some(LiteralValue::Number(6.0)));
}

fn unchanged_write_does_not_recompute(changelog: bool) {
    let (mut wb, calls) = workbook(changelog);
    build(&mut wb);
    let baseline = calls.load(Ordering::SeqCst);
    assert_eq!(baseline, 1);

    wb.set_value(SHEET, 1, 1, LiteralValue::Number(3.0)).unwrap();
    wb.evaluate_all().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), baseline);
    assert_eq!(wb.get_value(SHEET, 1, 2), Some(LiteralValue::Number(6.0)));
    assert_eq!(wb.get_value(SHEET, 1, 3), Some(LiteralValue::Number(6.0)));

    wb.set_value(SHEET, 1, 1, LiteralValue::Number(4.0)).unwrap();
    wb.evaluate_all().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), baseline + 1);
    assert_eq!(wb.get_value(SHEET, 1, 3), Some(LiteralValue::Number(8.0)));
}

#[test]
fn unchanged_set_value_is_a_no_op_without_changelog() {
    unchanged_write_does_not_recompute(false);
}

#[test]
fn unchanged_set_value_is_a_no_op_with_changelog() {
    unchanged_write_does_not_recompute(true);
}

#[test]
fn unchanged_write_inside_an_action_is_a_no_op() {
    let (mut wb, calls) = workbook(true);
    build(&mut wb);
    let baseline = calls.load(Ordering::SeqCst);

    wb.action("rewrite-inputs", |action| {
        action.set_value(SHEET, 1, 1, LiteralValue::Number(3.0))
    })
    .unwrap();
    wb.evaluate_all().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), baseline);
    assert_eq!(wb.get_value(SHEET, 1, 3), Some(LiteralValue::Number(6.0)));

    wb.action("change-inputs", |action| {
        action.set_value(SHEET, 1, 1, LiteralValue::Number(5.0))
    })
    .unwrap();
    wb.evaluate_all().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), baseline + 1);
    assert_eq!(wb.get_value(SHEET, 1, 3), Some(LiteralValue::Number(10.0)));
}

#[test]
fn unchanged_batch_cells_are_dropped_from_set_values() {
    let (mut wb, calls) = workbook(true);
    build(&mut wb);
    let baseline = calls.load(Ordering::SeqCst);

    // A1 keeps its value; A2 is new, and feeds nothing.
    wb.set_values(
        SHEET,
        1,
        1,
        &[
            vec![LiteralValue::Number(3.0)],
            vec![LiteralValue::Number(9.0)],
        ],
    )
    .unwrap();
    wb.evaluate_all().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), baseline);
    assert_eq!(wb.get_value(SHEET, 2, 1), Some(LiteralValue::Number(9.0)));
    assert_eq!(wb.get_value(SHEET, 1, 3), Some(LiteralValue::Number(6.0)));
}

/// An unchanged write between a real change and an undo must leave history
/// intact. The skipped write journals no cell event, so its action is an empty
/// compound: undoing it restores nothing, exactly as undoing the unchanged
/// `SetValue { old: 5, new: 5 }` it replaced used to, and the next undo still
/// reaches the real change.
#[test]
fn undo_after_an_unchanged_write_keeps_history_intact() {
    let (mut wb, _calls) = workbook(true);
    build(&mut wb);

    wb.action("change", |action| {
        action.set_value(SHEET, 1, 1, LiteralValue::Number(5.0))
    })
    .unwrap();
    wb.evaluate_all().unwrap();
    assert_eq!(wb.get_value(SHEET, 1, 1), Some(LiteralValue::Number(5.0)));

    wb.action("rewrite-same", |action| {
        action.set_value(SHEET, 1, 1, LiteralValue::Number(5.0))
    })
    .unwrap();

    // Undoing the no-op action changes nothing.
    wb.undo().unwrap();
    wb.evaluate_all().unwrap();
    assert_eq!(wb.get_value(SHEET, 1, 1), Some(LiteralValue::Number(5.0)));
    assert_eq!(wb.get_value(SHEET, 1, 3), Some(LiteralValue::Number(10.0)));

    // Undoing again reaches the real change.
    wb.undo().unwrap();
    wb.evaluate_all().unwrap();
    assert_eq!(wb.get_value(SHEET, 1, 1), Some(LiteralValue::Number(3.0)));
    assert_eq!(wb.get_value(SHEET, 1, 3), Some(LiteralValue::Number(6.0)));

    wb.redo().unwrap();
    wb.evaluate_all().unwrap();
    assert_eq!(wb.get_value(SHEET, 1, 1), Some(LiteralValue::Number(5.0)));
    assert_eq!(wb.get_value(SHEET, 1, 3), Some(LiteralValue::Number(10.0)));
}

/// The fast path keys on the vertex kind, so a formula cell overwritten with a
/// literal equal to its own cached result must still become a literal.
#[test]
fn overwriting_a_formula_with_its_cached_result_is_not_skipped() {
    let (mut wb, _calls) = workbook(true);
    build(&mut wb);

    wb.set_value(SHEET, 1, 2, LiteralValue::Number(6.0)).unwrap();
    wb.set_value(SHEET, 1, 1, LiteralValue::Number(10.0))
        .unwrap();
    wb.evaluate_all().unwrap();
    assert_eq!(
        wb.get_value(SHEET, 1, 2),
        Some(LiteralValue::Number(6.0)),
        "B1 must be a literal now, not =A1*2"
    );
}
