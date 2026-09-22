use formualizer_common::{ExcelErrorKind, LiteralValue};
use formualizer_eval::engine::FormulaPlaneMode;
use formualizer_workbook::{Workbook, WorkbookConfig};

fn build(mode: FormulaPlaneMode) -> Workbook {
    let config = WorkbookConfig::interactive().with_formula_plane_mode(mode);
    let mut workbook = Workbook::new_with_config(config);
    workbook.add_sheet("S").unwrap();
    for (row, value) in [(1, 10.0), (2, 20.0), (3, 10.0)] {
        workbook
            .set_value("S", row, 1, LiteralValue::Number(value))
            .unwrap();
    }
    let formulas = [
        "=SUM(SEQUENCE(3))",
        "=COUNT(UNIQUE(A1:A3))",
        "=COUNTA(SORT(A1:A3))",
        "=AVERAGE(TRANSPOSE(A1:A3))",
        "=MAX((A1:A3=10)*A1:A3)",
        "=SUM(OFFSET(A1,0,0,3,1))",
        "=SUM(INDIRECT(\"A1:A3\"))",
        "=SUM(SEQUENCE(-1))",
        "=SUM(OFFSET(A1,-1,0))",
        "=SUM(INDIRECT(\"not a reference\"))",
    ];
    for (row, formula) in formulas.into_iter().enumerate() {
        workbook
            .set_formula("S", row as u32 + 1, 2, formula)
            .unwrap();
    }
    workbook.evaluate_all().unwrap();
    workbook
}

#[test]
fn workbook_computed_array_aggregates_preserve_reference_errors_and_mode_parity() {
    let off = build(FormulaPlaneMode::Off);
    let authoritative = build(FormulaPlaneMode::AuthoritativeExperimental);

    for row in 1..=10 {
        assert_eq!(
            authoritative.get_value("S", row, 2),
            off.get_value("S", row, 2),
            "mode mismatch at B{row}"
        );
    }

    let expected = [6.0, 2.0, 3.0, 40.0 / 3.0, 10.0, 40.0, 40.0];
    for (row, expected) in expected.into_iter().enumerate() {
        assert_eq!(
            off.get_value("S", row as u32 + 1, 2),
            Some(LiteralValue::Number(expected))
        );
    }
    assert!(matches!(
        off.get_value("S", 8, 2),
        Some(LiteralValue::Error(error)) if error.kind == ExcelErrorKind::Value
    ));
    for (row, kind) in [(9, ExcelErrorKind::Ref), (10, ExcelErrorKind::Name)] {
        let Some(LiteralValue::Error(error)) = off.get_value("S", row, 2) else {
            panic!("expected an error value at B{row}");
        };
        assert_eq!(error.kind, kind, "error kind at B{row}");
    }
    // B9's #REF! is already message-free; B10's #NAME? is not. See the
    // ignored test below.
    let Some(LiteralValue::Error(error)) = off.get_value("S", 9, 2) else {
        panic!("expected an error value at B9");
    };
    assert_eq!(error.message, None, "error message at B9");
}

/// Split out of the test above by r10 (test hygiene) and left failing-by-
/// record rather than deleted or weakened.
///
/// `=SUM(INDIRECT("not a reference"))` (row 10 of the fixture above) yields
/// `#NAME?` carrying `Some("Undefined name: not a reference")`. Excel's error
/// values have no text payload at all — `#NAME?` is an opaque value and
/// `ERROR.TYPE` returns only its number — so the expectation here (no
/// message) is the correct one and the engine is what diverges. The same
/// assertion fails identically on every commit since 63c78d56; the two other
/// reference errors in the fixture (`#VALUE!` from `SEQUENCE(-1)`, `#REF!`
/// from `OFFSET(A1,-1,0)`) are already message-free, so the gap is specific
/// to the name-resolution path under INDIRECT.
///
/// Un-ignore with the engine fix, not by changing the expectation.
#[test]
#[ignore = "GOD-338 r10: INDIRECT inside a computed-array aggregate returns #NAME? with a message; engine gap since 63c78d56, see the cause register"]
fn workbook_computed_array_aggregate_indirect_name_error_carries_no_message() {
    let off = build(FormulaPlaneMode::Off);
    let Some(LiteralValue::Error(error)) = off.get_value("S", 10, 2) else {
        panic!("expected an error value at B10");
    };
    assert_eq!(error.kind, ExcelErrorKind::Name, "error kind at B10");
    assert_eq!(error.message, None, "error message at B10");
}
