"""Declared XLSX CSE callback outputs order follower consumers on first calculation."""
import json
import struct
import zipfile
import xml.etree.ElementTree as ET

import formualizer as fz
import openpyxl
import pytest
from openpyxl.worksheet.formula import ArrayFormula


@pytest.mark.parametrize('cross_sheet', [False, True])
@pytest.mark.parametrize('saved_end', [None, 16, 22, 30])
@pytest.mark.parametrize('targeted', [False, True])
def test_loaded_callback_first_calculation_and_existing_consumer_mutations(tmp_path, cross_sheet, saved_end, targeted):
    source = openpyxl.Workbook()
    consumer = source.active
    consumer.title = 'Sheet1'
    producer = source.create_sheet('Child') if cross_sheet else consumer
    producer['J1'] = 30
    producer['H1'] = '=J1*1'
    producer['B15'] = ArrayFormula(ref=f'B15:C{saved_end or 22}', text='=CALLBACK_TABLE(H1)')
    prefix = 'Child!' if cross_sheet else ''
    consumer['A1'] = f'=VLOOKUP(3,{prefix}B16:C22,2,FALSE)'
    consumer['D1'] = f'={prefix}C18'
    consumer['E1'] = '=IFERROR(A1,-1)'
    consumer['F1'] = '=E1*2'
    path = tmp_path / 'callback.xlsx'
    source.save(path)
    if saved_end is not None:
        with zipfile.ZipFile(path) as archive:
            parts = {name: archive.read(name) for name in archive.namelist()}
        ns = '{http://schemas.openxmlformats.org/spreadsheetml/2006/main}'
        producer_part = 'xl/worksheets/sheet2.xml' if cross_sheet else 'xl/worksheets/sheet1.xml'
        tree = ET.fromstring(parts[producer_part])
        next(c for c in tree.iter(ns+'c') if c.get('r') == 'B15').set('cm', '1')
        parts[producer_part] = ET.tostring(tree)
        parts['xl/metadata.xml'] = b'''<metadata xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:xda="http://schemas.microsoft.com/office/spreadsheetml/2017/dynamicarray"><metadataTypes count="1"><metadataType name="XLDAPR"/></metadataTypes><futureMetadata name="XLDAPR" count="1"><bk><extLst><ext uri="{bdbb8cdc-fa1e-496e-a857-3c3f30c029c3}"><xda:dynamicArrayProperties fDynamic="1" fCollapsed="0"/></ext></extLst></bk></futureMetadata><cellMetadata count="1"><bk><rc t="1" v="0"/></bk></cellMetadata></metadata>'''
        with zipfile.ZipFile(path, 'w', zipfile.ZIP_DEFLATED) as archive:
            for name, contents in parts.items():
                archive.writestr(name, contents)

        ct = ET.fromstring(parts['[Content_Types].xml'])
        ET.SubElement(ct, '{http://schemas.openxmlformats.org/package/2006/content-types}Override', PartName='/xl/metadata.xml', ContentType='application/vnd.openxmlformats-officedocument.spreadsheetml.sheetMetadata+xml')
        parts['[Content_Types].xml'] = ET.tostring(ct)
        rels = ET.fromstring(parts['xl/_rels/workbook.xml.rels'])
        ET.SubElement(rels, '{http://schemas.openxmlformats.org/package/2006/relationships}Relationship', Id='rIdDynamicMetadata', Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/sheetMetadata', Target='metadata.xml')
        parts['xl/_rels/workbook.xml.rels'] = ET.tostring(rels)
        with zipfile.ZipFile(path, 'w', zipfile.ZIP_DEFLATED) as archive:
            for name, contents in parts.items(): archive.writestr(name, contents)

    config = fz.EvaluationConfig()
    config.workbook_seed = 147
    config.cycle_detection = 'runtime'
    wb = fz.Workbook.from_path(str(path), config=fz.WorkbookConfig(eval_config=config))
    wb.prepare_graph()
    calls = []

    def table(value):
        calls.append(value)
        rate = {'type': 'error', 'kind': 'Na'} if value < 0 else value
        return [['Term', 'Rate']] + [[term, rate if term == 3 else 10 * term] for term in range(1, 8)]

    wb.register_function('callback_table', table, min_args=1, max_args=1)
    for value, expected in [(30, 30), (60, 60), (-1, -1), (90, 90)]:
        if value != 30:
            wb.set_value(producer.title, 1, 10, value)
        if targeted:
            wb.evaluate_cells([('Sheet1',1,6)])
        else:
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
