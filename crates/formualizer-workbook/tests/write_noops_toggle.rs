//! GOD-383 Trial A T2b through the `Workbook` API: with `FZ_WRITE_NOOPS` on
//! (set via `Engine::set_write_toggles`), an identical `set_formula` and an
//! Empty-over-empty `set_value` recompute nothing, with and without the
//! changelog; with it off they keep the full write path. Values agree.

use formualizer_common::ExcelError;
use formualizer_workbook::{CustomFnOptions, LiteralValue, Workbook, WorkbookConfig};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const SHEET: &str = "Sheet1";

fn workbook(changelog: bool, on: bool) -> (Workbook, Arc<AtomicUsize>) {
    let mut config = WorkbookConfig::ephemeral();
    config.enable_changelog = changelog;
    let mut wb = Workbook::new_with_config(config);
    wb.engine_mut().set_write_toggles(on, on);
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    wb.register_custom_function(
        "myfn",
        CustomFnOptions::default(),
        Arc::new(
            move |args: &[LiteralValue]| -> Result<LiteralValue, ExcelError> {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(args.first().cloned().unwrap_or(LiteralValue::Empty))
            },
        ),
    )
    .unwrap();
    (wb, calls)
}

fn scenario(changelog: bool, on: bool) -> Vec<Option<LiteralValue>> {
    let (mut wb, calls) = workbook(changelog, on);
    wb.set_value(SHEET, 1, 1, LiteralValue::Number(3.0))
        .unwrap();
    wb.set_value(SHEET, 10, 1, LiteralValue::Number(1.0))
        .unwrap();
    wb.set_formula(SHEET, 1, 2, "=A1*2").unwrap();
    wb.set_formula(SHEET, 1, 3, "=MYFN(B1)+COUNTA($A$1:$A$10)")
        .unwrap();
    wb.evaluate_all().unwrap();
    // A first identical re-set may still take the full path (for example
    // when the logged path left non-API formula authorship); from then on
    // the cell holds exactly what `set_formula` installs.
    wb.set_formula(SHEET, 1, 2, "=A1*2").unwrap();
    wb.evaluate_all().unwrap();
    let noops_before = wb.engine().write_toggle_counters().1;
    let before = calls.load(Ordering::SeqCst);

    wb.set_formula(SHEET, 1, 2, "=A1*2").unwrap();
    wb.evaluate_all().unwrap();
    let formula_recompute = calls.load(Ordering::SeqCst) - before;
    assert_eq!(formula_recompute, usize::from(!on));
    assert_eq!(
        wb.engine().write_toggle_counters().1 - noops_before,
        u64::from(on)
    );

    let before = calls.load(Ordering::SeqCst);
    wb.set_value(SHEET, 5, 1, LiteralValue::Empty).unwrap();
    wb.evaluate_all().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst) - before, usize::from(!on));

    // A changed formula still takes the full path.
    wb.set_formula(SHEET, 1, 2, "=A1*5").unwrap();
    wb.evaluate_all().unwrap();
    [(1, 2), (1, 3), (5, 1)]
        .iter()
        .map(|&(r, c)| wb.get_value(SHEET, r, c))
        .collect()
}

#[test]
fn identical_set_formula_and_empty_over_empty_agree_across_toggle() {
    for changelog in [false, true] {
        assert_eq!(
            scenario(changelog, false),
            scenario(changelog, true),
            "changelog={changelog}"
        );
    }
}
