"""Declared XLSX CSE callback outputs order follower consumers on first calculation."""
import json
import struct

import formualizer as fz
import openpyxl
import pytest
from openpyxl.worksheet.formula import ArrayFormula


@pytest.mark.parametrize('cross_sheet', [False, True])
def test_loaded_callback_first_calculation_and_existing_consumer_mutations(tmp_path, cross_sheet):
    source = openpyxl.Workbook()
    consumer = source.active
    consumer.title = 'Sheet1'
    producer = source.create_sheet('Child') if cross_sheet else consumer
    producer['H1'] = 30
    producer['B15'] = ArrayFormula(ref='B15:C22', text='=CALLBACK_TABLE(H1)')
    prefix = 'Child!' if cross_sheet else ''
    consumer['A1'] = f'=VLOOKUP(3,{prefix}B16:C22,2,FALSE)'
    consumer['D1'] = f'={prefix}C18'
    consumer['E1'] = '=IFERROR(A1,-1)'
    consumer['F1'] = '=E1*2'
    path = tmp_path / 'callback.xlsx'
    source.save(path)
    wb = fz.load_workbook(str(path), strategy='eager_all')
    calls = []

    def table(value):
        calls.append(value)
        rate = {'type': 'error', 'kind': 'Na'} if value < 0 else value
        return [['Term', 'Rate']] + [[term, rate if term == 3 else 10 * term] for term in range(1, 8)]

    wb.register_function('callback_table', table, min_args=1, max_args=1)
    for value, expected in [(30, 30), (60, 60), (-1, -1), (90, 90)]:
        if value != 30:
            wb.set_value(producer.title, 1, 8, value)
        wb.evaluate_all()
        assert wb.get_value('Sheet1', 1, 5) == expected
        assert wb.get_value('Sheet1', 1, 6) == expected * 2
        for sheet, row, col in [(producer.title, 18, 3), ('Sheet1', 1, 1), ('Sheet1', 1, 4)]:
            observed = wb.get_value(sheet, row, col)
            if value < 0:
                assert isinstance(observed, dict) and observed['kind'] == 'Na'
            else:
                assert observed == expected
            typed = json.loads(wb.get_typed_value_json(sheet, row, col))
            if value < 0:
                assert typed['type'] == 'Error' and typed['kind'] == 'Na'
            elif typed['type'] == 'Number':
                assert struct.unpack('!d', struct.pack('!Q', typed['bits']))[0] == expected
            else:
                assert typed == {'type': 'Int', 'value': expected}
    assert calls == [30, 60, -1, 90]
