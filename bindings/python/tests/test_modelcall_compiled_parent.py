"""Architecture B (GOD-383) binding smoke: ModelSession.set_native_compiled,
the compiled parent receipt keys, the registry's role and report_conditions_simple
fields, and formualizer.CompiledCells.

Synthetic parent -> child package written with ``Workbook.to_xlsx_bytes`` (no
client content). Without a loadable native module the parent's registry entry
declines to the engine, so the values must equal the plain engine arm and the
receipt must say why (``compiled.parent``). The stub-module run of a compiled
parent is covered by the crate tests (``tests/compiled_parent.rs``,
``tests/native_compiled.rs``).
"""

import datetime
import hashlib
import json

import pytest

import formualizer as fz

SHEET = "Calc"
CALL = '=MDL.CALLMODEL("rates/child",0,"result","amount",A1)+MDL.CALLMODEL("rates/child",0,"result","amount",A1)'


def _write(tmp_path, name, formula):
    wb = fz.Workbook()
    wb.add_sheet(SHEET)
    wb.set_value(SHEET, 1, 1, 1)
    wb.set_formula(SHEET, 2, 1, formula)
    path = tmp_path / f"{name}.xlsx"
    path.write_bytes(bytes(wb.to_xlsx_bytes()))
    return str(path), hashlib.sha256(path.read_bytes()).hexdigest()


def _loc(row, key, port_id):
    return {"sheet": SHEET, "start_row": row, "start_col": 1, "end_row": row, "end_col": 1,
            "name": "X_" + key, "key": key, "port_id": port_id, "shape": "scalar",
            "date_system": 1900, "date_fields": []}


def _spec(identity, path, sha):
    return {
        "identity": identity, "workbook_path": path, "workbook_sha256": sha,
        "manifest": {
            "spec": "fio", "spec_version": "0.3.0",
            "manifest": {"id": "model-" + identity, "name": identity,
                         "workbook": {"uri": "file://" + path, "locale": "en-US", "date_system": 1900}},
            "ports": [
                {"id": "amount", "dir": "in", "shape": "scalar", "location": {"a1": f"'{SHEET}'!A1"},
                 "schema": {"type": "any"}, "constraints": {"nullable": True}},
                {"id": "output_result", "dir": "out", "shape": "scalar", "location": {"a1": f"'{SHEET}'!A2"},
                 "schema": {"type": "any"}, "constraints": {"nullable": True}},
            ],
        },
        "inputs": {"amount": _loc(1, "amount", "amount")},
        "outputs": {"result": _loc(2, "result", "output_result")},
        "defaults": {"amount": 1},
        "goal_seek": [],
        "descriptor": {},
    }


@pytest.fixture()
def package(tmp_path):
    parent_path, parent_sha = _write(tmp_path, "parent", CALL)
    child_path, child_sha = _write(tmp_path, "child", "=A1*2")
    document = {
        "package_id": "synthetic",
        "parent": _spec("parent", parent_path, parent_sha),
        "children": {"child": _spec("child", child_path, child_sha)},
        "child_routes": {"rates/child": "child"},
    }
    return json.dumps(document), parent_sha


def _context(operation="client"):
    return {"now": "2026-09-29T00:00:00+00:00", "operation": operation, "flags": {"compiled": True}}


def _registry(parent_sha, role="parent", **extra):
    entry = {"native_path": "/nonexistent/libcv_native.so", "engine_commit": "unknown",
             "manifest_sha256": "0" * 64, "role": role}
    entry.update(extra)
    return json.dumps({parent_sha: entry})


def test_native_registry_declines_to_the_engine_with_its_route(package):
    document, parent_sha = package
    engine = fz.ModelSession(document, _context()).calculate({"amount": 3})
    assert engine["outputs"] == {"result": 12.0}
    assert "parent" not in engine["compiled"]

    session = fz.ModelSession(document, _context())
    session.set_native_compiled(_registry(parent_sha))
    result = session.calculate({"amount": 3})
    for key in ("outputs", "typed_outputs", "effective_inputs"):
        assert result[key] == engine[key], key
    assert [e["status"] for e in result["invocations"]] == [e["status"] for e in engine["invocations"]]
    route = result["compiled"]["parent"]
    assert route.startswith(("engine:", "fallback:")), route
    assert result["compiled"]["parent_loaded"] is True
    assert "parent_xcalls" not in result["compiled"]
    assert session.compiled_cells() is None
    assert session.workbook() is not None

    session.set_native_compiled(None)
    assert "parent" not in session.calculate({"amount": 3})["compiled"]


def test_child_only_registry_records_no_parent_route(package):
    document, parent_sha = package
    session = fz.ModelSession(document, _context())
    session.set_native_compiled(_registry(parent_sha, role="child"))
    result = session.calculate({"amount": 3})
    assert "parent" not in result["compiled"]
    assert result["outputs"] == {"result": 12.0}


def test_report_run_refuses_without_the_conditions_rule_and_prepares_the_engine_parent(package):
    document, parent_sha = package
    seen = []

    def prepare(workbook):
        seen.append(("prepare", type(workbook).__name__))

    def capture(workbook, outputs):
        seen.append(("capture", type(workbook).__name__))
        return {"diagnostics": ["captured"], "report_cells": {}}

    session = fz.ModelSession(document, _context("report"))
    session.set_native_compiled(_registry(parent_sha))
    refused = session.calculate({"amount": 3}, report_prepare=prepare, report_capture=capture)
    assert refused["compiled"]["parent"] == "engine:report_conditions"
    assert seen == [("prepare", "Workbook"), ("capture", "Workbook")]

    with pytest.raises(TypeError):
        session.calculate({"amount": 3}, report_conditions_ok=True)   # the registry decides, not the caller

    seen.clear()
    session.set_native_compiled(_registry(parent_sha, report_conditions_simple=True))
    admitted = session.calculate({"amount": 3}, report_prepare=prepare, report_capture=capture)
    # No loadable module: the attempt declines and the engine parent is prepared (F1).
    assert admitted["compiled"]["parent"] != "engine:report_conditions"
    assert admitted["outputs"] == refused["outputs"]
    assert seen == [("prepare", "Workbook"), ("capture", "Workbook")]
    assert "captured" in admitted["diagnostics"]


def test_compiled_cells_is_exported_and_normalises_engine_temporals():
    assert fz.CompiledCells.__module__ == "formualizer.formualizer_py"
    with pytest.raises(TypeError):
        fz.CompiledCells()
    assert fz.CompiledCells.serial(datetime.date(2024, 3, 1)) == 45352.0
    assert fz.CompiledCells.serial(datetime.datetime(2024, 3, 1, 12, 0)) == 45352.5
    assert fz.CompiledCells.serial(datetime.time(6, 0)) == 0.25
    assert fz.CompiledCells.serial(datetime.timedelta(hours=36)) == 1.5
    assert fz.CompiledCells.serial(7.0) == 7.0
    assert fz.CompiledCells.serial("x") == "x"
    assert fz.CompiledCells.serial(None) is None


def test_bad_registry_is_refused(package):
    document, _ = package
    session = fz.ModelSession(document, _context())
    with pytest.raises(RuntimeError):
        session.set_native_compiled("[]")
