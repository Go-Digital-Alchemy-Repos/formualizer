"""Explicit dynamic FIO types preserve spreadsheet scalar kinds."""
import pytest
from formualizer import LiteralValue, SheetPortConstraintError, SheetPortSession, Workbook


def session_for(schema='type: any', constraints='nullable: true', default=''):
    wb = Workbook()
    wb.add_sheet('Data')
    manifest = f'''spec: fio
spec_version: "0.3.0"
manifest: {{ id: any-tests, name: Any Tests }}
ports:
  - id: input
    dir: in
    shape: scalar
    location: {{ a1: Data!A1 }}
    schema: {{ {schema} }}
    constraints: {{ {constraints} }}
{default}
  - id: output
    dir: out
    shape: scalar
    location: {{ a1: Data!A1 }}
    schema: {{ type: any }}
    constraints: {{ nullable: true }}
'''
    return wb, SheetPortSession.from_manifest_yaml(manifest, wb)


@pytest.mark.parametrize('value,kind', [(0, 'Number'), ('', 'Text'), ('007', 'Text'),
                                       (False, 'Boolean'), (None, 'Empty')])
def test_any_writes_preserve_spreadsheet_kinds(value, kind):
    wb, session = session_for()
    session.write_inputs({'input': value})
    assert session.read_outputs(typed=True)['output'].type_name == kind
    assert session.read_outputs()['output'] == value
    assert wb.read_typed_range('Data', 1, 1, 1, 1)[0][0].type_name == kind


def test_any_preserves_rich_spreadsheet_error_output():
    _, session = session_for()
    error = {'type': 'Error', 'kind': 'Na', 'message': 'child note',
             'extra': {'Spill': {'expected_rows': 3, 'expected_cols': 2}}}
    session.write_inputs({'input': LiteralValue.from_object(error)})
    assert session.read_outputs(typed=True)['output'].to_python() == error


@pytest.mark.parametrize('value', [LiteralValue.pending(), LiteralValue.array([[LiteralValue.number(1)]])])
def test_any_rejects_incomplete_and_array_cell_values(value):
    _, session = session_for()
    with pytest.raises(SheetPortConstraintError):
        session.write_inputs({'input': value})


def test_any_nullable_and_numeric_constraints_remain_enforced():
    _, session = session_for(constraints='nullable: false, min: 1, max: 4')
    for value in [None, 0, 5, '2', False, LiteralValue.error('Na', None)]:
        with pytest.raises(SheetPortConstraintError):
            session.write_inputs({'input': value})
    session.write_inputs({'input': 2})
    assert session.read_outputs()['output'] == 2


def test_any_exact_enumeration_and_pattern_constraints_remain_enforced():
    _, session = session_for(constraints='nullable: true, enum: [0, "zero", false]')
    for value in [0, 'zero', False]:
        session.write_inputs({'input': value})
    with pytest.raises(SheetPortConstraintError):
        session.write_inputs({'input': '0'})
    _, session = session_for(constraints='nullable: true, pattern: "^word$"')
    session.write_inputs({'input': 'word'})
    with pytest.raises(SheetPortConstraintError):
        session.write_inputs({'input': 'other'})


@pytest.mark.parametrize('default,value', [('    default: "007"', '007'), ('    default: 0', 0), ('    default: false', False)])
def test_any_json_defaults_keep_original_scalar_kind(default, value):
    _, session = session_for(default=default)
    assert session.read_inputs()['input'] == value


def test_existing_strict_string_still_rejects_numeric_zero():
    _, session = session_for(schema='type: string')
    with pytest.raises(SheetPortConstraintError):
        session.write_inputs({'input': 0})


def test_any_range_preserves_heterogeneous_error_and_empty_cells():
    wb = Workbook()
    wb.add_sheet('Data')
    manifest = '''spec: fio
spec_version: "0.3.0"
manifest: { id: any-range, name: Any Range }
ports:
  - id: values
    dir: in
    shape: range
    location: { a1: Data!A1:C2 }
    schema: { cell_type: any }
    constraints: { nullable: true }
  - id: output
    dir: out
    shape: range
    location: { a1: Data!A1:C2 }
    schema: { cell_type: any }
    constraints: { nullable: true }
'''
    session = SheetPortSession.from_manifest_yaml(manifest, wb)
    error = {'type': 'Error', 'kind': 'Value', 'message': 'range note'}
    session.write_inputs({'values': [[0, '', False], [None, '007', LiteralValue.from_object(error)]]})
    rows = session.read_outputs(typed=True)['output']
    assert [[cell.type_name for cell in row] for row in rows] == [
        ['Number', 'Text', 'Boolean'], ['Empty', 'Text', 'Error']]
    assert rows[1][2].to_python() == error
    assert session.read_outputs()['output'] == [[0, '', False], [None, '007', error]]
    with pytest.raises(SheetPortConstraintError):
        session.write_inputs({'values': [[1, 2, 3], [4, 5, LiteralValue.pending()]]})


def test_any_output_rejects_pending_and_accepts_nonnullable_error():
    wb, session = session_for(constraints='nullable: false')
    error = {'type': 'Error', 'kind': 'Na', 'message': 'not a blank'}
    session.write_inputs({'input': LiteralValue.from_object(error)})
    assert session.read_outputs()['output'] == error
    wb.set_value('Data', 1, 1, LiteralValue.pending())
    with pytest.raises(SheetPortConstraintError):
        session.read_outputs(typed=True)
