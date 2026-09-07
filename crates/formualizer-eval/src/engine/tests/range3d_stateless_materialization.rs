use crate::arrow_store::{CellIngest, IngestBuilder, OverlayValue, map_error_code};
use crate::engine::range_view::RangeView;
use crate::engine::{CancelToken, DateSystem};
use formualizer_common::{ExcelErrorKind, LiteralValue};

fn old_vertical_materialization(views: &[RangeView<'_>]) -> RangeView<'static> {
    let mut rows = Vec::new();
    for view in views {
        view.for_each_row(&mut |row| {
            rows.push(row.to_vec());
            Ok(())
        })
        .unwrap();
    }
    RangeView::from_owned_rows(rows, DateSystem::Excel1900)
}

fn assert_same_cells(expected: &RangeView<'_>, actual: &RangeView<'_>) {
    assert_eq!(actual.dims(), expected.dims());
    for row in 0..expected.dims().0 {
        for col in 0..expected.dims().1 {
            assert_eq!(actual.get_cell(row, col), expected.get_cell(row, col));
        }
    }
}

#[test]
fn vertical_materialization_matches_cell_reads_across_chunks_types_and_overlays() {
    let mut builder = IngestBuilder::new("Member", 7, 2, DateSystem::Excel1900);
    builder
        .append_row_cells(&[
            CellIngest::Number(1.5),
            CellIngest::Boolean(true),
            CellIngest::Text("base"),
            CellIngest::ErrorCode(map_error_code(ExcelErrorKind::Div)),
            CellIngest::DateSerial(45_001.25),
            CellIngest::DurationSerial(1.5),
            CellIngest::Pending,
        ])
        .unwrap();
    builder
        .append_row_cells(&[
            CellIngest::Empty,
            CellIngest::Boolean(false),
            CellIngest::Text("second"),
            CellIngest::Number(4.0),
            CellIngest::Empty,
            CellIngest::Number(6.0),
            CellIngest::Empty,
        ])
        .unwrap();
    builder
        .append_row_cells(&[
            CellIngest::Number(7.0),
            CellIngest::Empty,
            CellIngest::Text("third"),
            CellIngest::Empty,
            CellIngest::Number(8.0),
            CellIngest::Empty,
            CellIngest::Pending,
        ])
        .unwrap();
    let mut sheet = builder.finish();

    let first_chunk = sheet.columns[0].chunk_mut(0).unwrap();
    first_chunk
        .computed_overlay
        .set(0, OverlayValue::Number(10.0));
    first_chunk.overlay.set(0, OverlayValue::Number(11.0));
    let boolean_chunk = sheet.columns[1].chunk_mut(0).unwrap();
    boolean_chunk
        .computed_overlay
        .set(1, OverlayValue::Boolean(true));
    boolean_chunk.overlay.set(1, OverlayValue::Empty);
    sheet.columns[2]
        .chunk_mut(0)
        .unwrap()
        .computed_overlay
        .set(1, OverlayValue::Text("computed".into()));
    sheet.columns[3]
        .chunk_mut(1)
        .unwrap()
        .overlay
        .set(0, OverlayValue::Error(map_error_code(ExcelErrorKind::Na)));
    sheet.columns[4]
        .chunk_mut(1)
        .unwrap()
        .computed_overlay
        .set(0, OverlayValue::DateTime(45_002.5));
    sheet.columns[5]
        .chunk_mut(0)
        .unwrap()
        .overlay
        .set(1, OverlayValue::Duration(2.25));
    sheet.columns[1]
        .chunk_mut(1)
        .unwrap()
        .computed_overlay
        .set(0, OverlayValue::Error(u8::MAX));

    let member = sheet.range_view(0, 0, 2, 6);
    let expected = old_vertical_materialization(std::slice::from_ref(&member));
    let (actual, summaries) = RangeView::try_from_vertical_views(
        std::slice::from_ref(&member),
        DateSystem::Excel1900,
        true,
    )
    .unwrap();

    assert_same_cells(&expected, &actual);
    assert_eq!(actual.get_cell(0, 0), LiteralValue::Number(11.0));
    assert_eq!(actual.get_cell(0, 1), LiteralValue::Boolean(true));
    assert_eq!(actual.get_cell(0, 2), LiteralValue::Text("base".into()));
    assert!(matches!(
        actual.get_cell(0, 3),
        LiteralValue::Error(error) if error.kind == ExcelErrorKind::Div
    ));
    assert_eq!(actual.get_cell(0, 4), LiteralValue::Number(45_001.25));
    assert_eq!(actual.get_cell(0, 5), LiteralValue::Number(1.5));
    assert_eq!(actual.get_cell(0, 6), LiteralValue::Pending);
    assert_eq!(actual.get_cell(1, 0), LiteralValue::Empty);
    assert_eq!(actual.get_cell(1, 1), LiteralValue::Empty);
    assert_eq!(actual.get_cell(1, 2), LiteralValue::Text("computed".into()));
    assert_eq!(actual.get_cell(2, 4), LiteralValue::Number(45_002.5));
    assert_eq!(actual.get_cell(1, 5), LiteralValue::Number(2.25));
    assert!(matches!(
        actual.get_cell(2, 1),
        LiteralValue::Error(error) if error.kind == ExcelErrorKind::Error
    ));
    assert!(matches!(
        actual.get_cell(2, 3),
        LiteralValue::Error(error) if error.kind == ExcelErrorKind::Na
    ));
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].rows, 3);
    assert_eq!(summaries[0].cells, 21);
}

#[test]
fn vertical_materialization_preserves_member_order_dimensions_and_empty_padding() {
    let mut first_builder = IngestBuilder::new("First", 2, 1, DateSystem::Excel1900);
    first_builder
        .append_row(&[LiteralValue::Number(1.0), LiteralValue::Text("a".into())])
        .unwrap();
    let first = first_builder.finish();

    let mut second_builder = IngestBuilder::new("Second", 3, 1, DateSystem::Excel1900);
    second_builder
        .append_row(&[
            LiteralValue::Number(2.0),
            LiteralValue::Boolean(true),
            LiteralValue::Text("b".into()),
        ])
        .unwrap();
    let second = second_builder.finish();

    let views = vec![
        first.range_view(0, 0, 0, 1),
        first.range_view(1, 1, 0, 0),
        second.range_view(0, 0, 2, 3),
    ];
    let expected = old_vertical_materialization(&views);
    let (actual, summaries) =
        RangeView::try_from_vertical_views(&views, DateSystem::Excel1900, false).unwrap();

    assert_same_cells(&expected, &actual);
    assert_eq!(actual.dims(), (4, 4));
    assert_eq!(actual.get_cell(0, 0), LiteralValue::Number(1.0));
    assert_eq!(actual.get_cell(0, 2), LiteralValue::Empty);
    assert_eq!(actual.get_cell(1, 0), LiteralValue::Number(2.0));
    assert_eq!(actual.get_cell(3, 3), LiteralValue::Empty);
    assert!(summaries.is_empty());
}

#[test]
fn vertical_materialization_keeps_cancellation_attachment_at_the_result_boundary() {
    let member =
        RangeView::from_owned_rows(vec![vec![LiteralValue::Number(1.0)]], DateSystem::Excel1900);
    let expected = old_vertical_materialization(std::slice::from_ref(&member));
    let (actual, _) = RangeView::try_from_vertical_views(
        std::slice::from_ref(&member),
        DateSystem::Excel1900,
        false,
    )
    .unwrap();
    assert_same_cells(&expected, &actual);

    let token = CancelToken::new();
    token.cancel();
    let expected = expected.with_cancel_token(Some(token.clone()));
    let actual = actual.with_cancel_token(Some(token));
    let Some(Err(expected_error)) = expected.iter_row_chunks().next() else {
        panic!("the old materialization should observe the attached cancellation token");
    };
    let Some(Err(actual_error)) = actual.iter_row_chunks().next() else {
        panic!("the new materialization should observe the attached cancellation token");
    };
    assert_eq!(actual_error.kind, expected_error.kind);
}
