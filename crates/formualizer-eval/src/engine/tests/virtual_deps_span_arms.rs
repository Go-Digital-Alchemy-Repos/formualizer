//! Coverage for the `DynamicRefCollector` 3-D span arms (`virtual_deps.rs`).
//!
//! Adopted verbatim (test names and assertions) from the GOD-242 review cycle 1
//! probe: the review measured that the shipped regression test
//! `three_dimensional_span_after_value_writes_before_first_evaluation_equals_explicit_member_sum`
//! PASSES with the `virtual_deps.rs` half of the fix reverted, i.e. those arms
//! shipped unexercised.
//!
//! Law: the collector records, for a 3-D reference, exactly the
//! registration-order member window's regions, and nothing for a `#REF!` span.

use crate::engine::virtual_deps::DynamicRefCollector;
use crate::engine::{Engine, EvalConfig};
use crate::formula_plane::region_index::Region;
use crate::test_workbook::TestWorkbook;
use crate::traits::EvaluationContext;
use formualizer_common::LiteralValue;
use formualizer_parse::parser::ReferenceType;

fn engine_with_tabs() -> Engine<TestWorkbook> {
    let mut e = Engine::new(TestWorkbook::new(), EvalConfig::default());
    for s in ["Acct1", "Acct2", "Acct3", "Zed"] {
        e.add_sheet(s).unwrap();
    }
    for s in ["Acct1", "Acct2", "Acct3", "Zed"] {
        e.set_cell_value(s, 1, 2, LiteralValue::Number(1.0))
            .unwrap();
        e.set_cell_value(s, 5, 2, LiteralValue::Number(2.0))
            .unwrap();
    }
    e.evaluate_all().unwrap();
    e
}

fn regions(c: &DynamicRefCollector<'_, TestWorkbook>) -> Vec<Region> {
    let mut v: Vec<Region> = c
        .collected_regions
        .lock()
        .unwrap()
        .iter()
        .copied()
        .collect();
    v.sort_by_key(|r| format!("{r:?}"));
    v
}

#[test]
fn dynamic_collector_cell3d_collects_one_point_per_member_sheet() {
    let e = engine_with_tabs();
    let c = DynamicRefCollector::new(&e, "Sheet1");
    let r = ReferenceType::Cell3D {
        sheet_first: "Acct1".to_string(),
        sheet_last: "Acct3".to_string(),
        row: 1,
        col: 2,
        row_abs: true,
        col_abs: true,
    };
    c.resolve_range_view(&r, "Sheet1").unwrap();
    let got = regions(&c);
    let want: Vec<Region> = ["Acct1", "Acct2", "Acct3"]
        .iter()
        .map(|s| Region::rect(e.graph.sheet_id(s).unwrap(), 0, 0, 1, 1).normalized())
        .collect();
    let mut want = want;
    want.sort_by_key(|r| format!("{r:?}"));
    assert_eq!(got, want, "Cell3D must collect exactly the member window");
}

#[test]
fn dynamic_collector_range3d_unbounded_column_uses_the_used_extent() {
    let e = engine_with_tabs();
    let c = DynamicRefCollector::new(&e, "Sheet1");
    let r = ReferenceType::Range3D {
        sheet_first: "Acct1".to_string(),
        sheet_last: "Acct2".to_string(),
        start_row: None,
        start_col: Some(2),
        end_row: None,
        end_col: Some(2),
        start_row_abs: true,
        start_col_abs: true,
        end_row_abs: true,
        end_col_abs: true,
    };
    c.resolve_range_view(&r, "Sheet1").unwrap();
    let got = regions(&c);
    let mut want: Vec<Region> = ["Acct1", "Acct2"]
        .iter()
        .map(|s| Region::rect(e.graph.sheet_id(s).unwrap(), 0, 4, 1, 1).normalized())
        .collect();
    want.sort_by_key(|r| format!("{r:?}"));
    assert_eq!(
        got, want,
        "Range3D over a whole column must collect the engine used extent, per member, and no sheet outside the window"
    );
}

#[test]
fn dynamic_collector_missing_endpoint_collects_nothing() {
    let e = engine_with_tabs();
    let c = DynamicRefCollector::new(&e, "Sheet1");
    let r = ReferenceType::Cell3D {
        sheet_first: "Acct1".to_string(),
        sheet_last: "NoSuchTab".to_string(),
        row: 1,
        col: 2,
        row_abs: true,
        col_abs: true,
    };
    let _ = c.resolve_range_view(&r, "Sheet1");
    assert!(
        regions(&c).is_empty(),
        "a #REF! span must not leave a partial member window in the collector"
    );
}
