"""Lossless workbook/SheetPort reads for internal callback values."""
import datetime
import pytest
from formualizer import LiteralValue, SheetPortSession, Workbook


def test_native_range_roundtrip_types_and_error_provenance():
    wb = Workbook()
    wb.add_sheet('Data')
    error = {'type': 'Error', 'kind': 'Spill', 'message': 'blocked',
             'sheet': 'Origin', 'row': 2, 'col': 3, 'origin_row': 4, 'origin_col': 5,
             'extra': {'Spill': {'expected_rows': 6, 'expected_cols': 7}}}
    values = [LiteralValue.empty(), LiteralValue.pending(), LiteralValue.int(0),
              LiteralValue.number(0.0), LiteralValue.boolean(False), LiteralValue.text(''),
              LiteralValue.from_object(error), LiteralValue.date(2026, 9, 18)]
    for col, value in enumerate(values, 1):
        wb.set_value('Data', 1, col, value)
    wb.set_temporal_egress('serial')
    row = wb.read_typed_range('Data', 1, 1, 1, len(values))[0]
    assert [v.type_name for v in row] == ['Empty', 'Pending', 'Number', 'Number', 'Boolean', 'Text', 'Error', 'Number']
    assert row[6].to_python() == error
    assert row[7].as_number() == 46283  # Arrow stores dates as Excel serials.
    for col, value in enumerate(row, 1):
        wb.set_value('Data', 2, col, value)
    assert [v.type_name for v in wb.read_typed_range('Data', 2, 1, 2, len(values))[0]] == [v.type_name for v in row]
    assert LiteralValue.from_object(row[1].to_python()).is_pending
    assert wb.read_typed_range('Data', 3, 1, 3, 2)[0][0].is_empty


@pytest.mark.parametrize('bounds', [(0,1,1,1), (1,0,1,1), (2,1,1,1), (1,2,1,1)])
def test_native_range_rejects_invalid_bounds(bounds):
    wb = Workbook()
    wb.add_sheet('Data')
    with pytest.raises(ValueError):
        wb.read_typed_range('Data', *bounds)


def test_native_range_rejects_missing_sheet_and_does_not_evaluate():
    wb = Workbook()
    wb.add_sheet('Data')
    calls = []
    wb.register_function('COUNT_CALL', lambda args: calls.append(1) or 9)
    wb.set_formula('Data', 1, 1, '=COUNT_CALL()')
    wb.read_typed_range('Data', 1, 1, 1, 1)
    assert calls == []
    with pytest.raises(ValueError):
        wb.read_typed_range('Missing', 1, 1, 1, 1)


def test_sheetport_typed_range_shares_workbook_and_can_write_native_values():
    wb = Workbook()
    wb.add_sheet('Data')
    manifest = '''spec: fio
spec_version: "0.3.0"
manifest:
  id: native-range-test
  name: Native Range Test
  workbook:
    uri: memory://native.xlsx
ports:
  - id: values
    dir: in
    shape: range
    location:
      a1: Data!A1:B1
    schema:
      cell_type: number
'''
    session = SheetPortSession.from_manifest_yaml(manifest, wb)
    wb.set_value('Data', 1, 1, 12)
    wb.set_value('Data', 1, 2, 13)
    native = session.read_inputs(typed=True)['values']
    assert [[cell.as_number() for cell in row] for row in native] == [[12, 13]]
    session.write_inputs({'values': [[LiteralValue.number(21), LiteralValue.number(22)]]})
    assert wb.read_typed_range('Data', 1, 1, 1, 2)[0][0].as_number() == 21
    assert session.read_inputs()['values'] == [[21, 22]]


def test_callback_rich_scalar_and_spill_error_roundtrip():
    wb = Workbook()
    wb.add_sheet('Data')
    error = {'type': 'Error', 'kind': 'Na', 'message': 'child detail',
             'sheet': 'Child', 'row': 1, 'col': 2, 'origin_row': 3, 'origin_col': 4,
             'extra': {'Spill': {'expected_rows': 5, 'expected_cols': 6}}}
    wb.register_function('CHILDERR', lambda: LiteralValue.from_object(error), min_args=0, max_args=0)
    wb.register_function('CHILDTABLE', lambda: [[3, LiteralValue.from_object(error)]], min_args=0, max_args=0)
    wb.set_formula('Data', 1, 1, '=CHILDERR()')
    wb.set_formula('Data', 2, 1, '=CHILDTABLE()')
    wb.evaluate_all()
    assert wb.read_typed_range('Data', 1, 1, 1, 1)[0][0].to_python() == error
    assert wb.read_typed_range('Data', 2, 2, 2, 2)[0][0].to_python() == error
    wb.set_formula('Data', 2, 1, '=4')
    wb.evaluate_all()
    assert wb.read_typed_range('Data', 2, 2, 2, 2)[0][0].is_empty


def test_native_error_projection_rejects_invalid_extras_and_preserves_nested_values():
    with pytest.raises(ValueError, match='Invalid error extra'):
        LiteralValue.from_object({'type': 'Error', 'kind': 'Na', 'extra': {'unknown': {}}})
    error = {'type': 'Error', 'kind': 'Value', 'message': 'nested'}
    native = LiteralValue.array([[LiteralValue.pending(), LiteralValue.from_object(error)]])
    assert native.to_python() == [[{'type': 'Pending'}, error]]
    assert LiteralValue.from_object(native).to_python() == native.to_python()
