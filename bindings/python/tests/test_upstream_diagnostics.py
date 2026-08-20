import json

import formualizer as fz


def _runtime_cycle_workbook():
    cfg = fz.EvaluationConfig()
    cfg.cycle_detection = "runtime"
    wb = fz.Workbook(config=fz.WorkbookConfig(eval_config=cfg))
    wb.set_formula("Sheet1", 1, 1, "=B1+1")
    wb.set_formula("Sheet1", 1, 2, "=A1+1")
    return wb


def test_enabled_diagnostics_preserve_status_and_typed_results_on_overflow():
    disabled = _runtime_cycle_workbook()
    enabled = _runtime_cycle_workbook()
    enabled.set_upstream_diagnostics(True, edge_limit=1)

    assert disabled.evaluate_all() is None
    assert enabled.evaluate_all() is None
    assert disabled.get_typed_value_json("Sheet1", 1, 1) == enabled.get_typed_value_json(
        "Sheet1", 1, 1
    )
    assert disabled.get_typed_value_json("Sheet1", 1, 2) == enabled.get_typed_value_json(
        "Sheet1", 1, 2
    )
    dump = json.loads(enabled.upstream_diagnostics_json())
    assert dump["diagnostics"] == {
        "complete": False,
        "overflow": True,
        "named_formula_member_seen": False,
    }
    assert len(dump["stamped_sccs"]) == 1
    assert dump["stamped_sccs"][0]["complete"] is False


def test_complete_diagnostics_and_formula_free_three_dimensional_expansion():
    wb = _runtime_cycle_workbook()
    wb.set_upstream_diagnostics(True)
    wb.evaluate_all()
    dump = json.loads(wb.upstream_diagnostics_json())
    assert dump["diagnostics"]["complete"] is True
    assert dump["stamped_sccs"][0]["members"] == ["Sheet1!A1", "Sheet1!B1"]
    assert len(dump["stamped_sccs"][0]["edges"]) == 2

    inspect = fz.Workbook()
    inspect.add_sheet("Mid")
    inspect.add_sheet("Last")
    inspect.set_value("Sheet1", 1, 1, 1)
    inspect.set_value("Mid", 1, 1, 2)
    inspect.set_value("Last", 1, 1, 3)
    inspect.set_formula("Sheet1", 1, 2, "=SUM(Sheet1:Last!A1)")
    report = inspect.precedents("Sheet1!B1").to_dict()
    reference = report["precedents"][0]["reference"]
    assert reference["kind"] == "ThreeDimensional"
    assert reference["ranges"] == ["Sheet1!A1:A1", "Mid!A1:A1", "Last!A1:A1"]
    assert reference["cell_count"] == 3


def test_typed_getter_and_oversized_diagnostic_limit():
    wb = fz.Workbook()
    wb.set_value("Sheet1", 1, 1, 7)
    assert json.loads(wb.get_typed_value_json("Sheet1", 1, 1)) == {
        "type": "Number",
        "bits": 0x401C000000000000,
    }

    try:
        wb.set_upstream_diagnostics(True, edge_limit=1_000_001)
    except ValueError as error:
        assert "between 1 and 1000000" in str(error)
    else:
        raise AssertionError("oversized production diagnostic limit was accepted")
