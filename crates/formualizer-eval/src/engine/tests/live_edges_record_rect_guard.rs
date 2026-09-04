//! `record_rect` branch-selection guard (GOD-242).
//!
//! Adopted verbatim (test names and assertions) from the review cycle 1 probe.
//!
//! Covers the two cases the shipped parity test
//! (`record_rect_small_rect_and_member_scan_record_identical_edges`) does not:
//! the `index_covers_members` guard (a membership with two members on one
//! cell, where the index probe would silently drop the shadowed member), and a
//! rect on a sheet that holds no members at all.
//!
//! Law: the public `record_rect` dispatcher records the same edge set a pure
//! member scan would, in both cases.

use crate::engine::live_edges::LiveEdgeCollector;
use crate::engine::{Engine, EvalConfig};
use crate::reference::{CellRef, Coord};
use crate::test_workbook::TestWorkbook;
use formualizer_common::LiteralValue;

fn new_engine() -> Engine<TestWorkbook> {
    Engine::new(TestWorkbook::new(), EvalConfig::default())
}
fn cell(e: &Engine<TestWorkbook>, s: &str, r: u32, c: u32) -> CellRef {
    CellRef::new(
        e.sheet_id(s).expect("sheet exists"),
        Coord::from_excel(r, c, true, true),
    )
}

fn sorted(collector: &LiveEdgeCollector) -> Vec<(u32, u32, bool, u8)> {
    let mut v: Vec<(u32, u32, bool, u8)> = collector
        .take_edge_records()
        .into_iter()
        .map(|e| (e.from, e.to, e.selected, e.mechanisms))
        .collect();
    v.sort_unstable();
    v
}

#[test]
fn record_rect_guard_keeps_a_shadowed_duplicate_member() {
    let mut engine = new_engine();
    engine.add_sheet("Other").unwrap();
    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Number(1.0))
        .unwrap();
    engine.evaluate_all().unwrap();

    let site = cell(&engine, "Other", 1, 1);
    let dup = cell(&engine, "Sheet1", 1, 1);
    // members[1] and members[2] are the SAME cell: `index` can only hold one of
    // them, so the index-probe branch would record a single edge where the
    // member scan records two.  `index_covers_members` must force the scan.
    let members = [site, dup, dup];
    let collector = LiveEdgeCollector::new(&members);
    collector.set_current(0);
    let sheet1 = engine.sheet_id("Sheet1").expect("sheet exists");

    // A 1x1 rect: area (1) < members.len() (3), so without the guard the
    // dispatcher would take the index branch.
    collector.record_rect(sheet1, 0, 0, 0, 0);
    let got = sorted(&collector);

    let scan = LiveEdgeCollector::new(&members);
    scan.set_current(0);
    scan.record_rect_forced_branch(false, sheet1, 0, 0, 0, 0);
    let want = sorted(&scan);

    assert_eq!(
        got, want,
        "record_rect dispatcher dropped a shadowed duplicate member"
    );
    assert_eq!(got.len(), 2, "both duplicate members must get an edge");
}

#[test]
fn record_rect_on_a_sheet_with_no_members_records_nothing_on_either_branch() {
    let mut engine = new_engine();
    engine.add_sheet("Empty").unwrap();
    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Number(1.0))
        .unwrap();
    engine.evaluate_all().unwrap();

    let members: Vec<CellRef> = (1..=4u32).map(|r| cell(&engine, "Sheet1", r, 1)).collect();
    let empty = engine.sheet_id("Empty").expect("sheet exists");

    for by_index in [true, false] {
        let c = LiveEdgeCollector::new(&members);
        c.set_current(0);
        c.record_rect_forced_branch(by_index, empty, 0, 0, 2, 2);
        assert!(
            sorted(&c).is_empty(),
            "member-free sheet recorded an edge (by_index={by_index})"
        );
    }
    // ...and through the dispatcher, for both a small and an oversized rect.
    for rect in [(0u32, 0u32, 0u32, 0u32), (0, 0, 100, 100)] {
        let c = LiveEdgeCollector::new(&members);
        c.set_current(0);
        c.record_rect(empty, rect.0, rect.1, rect.2, rect.3);
        assert!(
            sorted(&c).is_empty(),
            "member-free sheet recorded an edge for {rect:?}"
        );
    }
}
