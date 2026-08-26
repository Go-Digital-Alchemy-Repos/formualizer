#!/usr/bin/env python3
"""GOD-187aa2 S020 workbook census and binding gate.

The worker imports Formualizer only inside a fresh wheel-specific virtual
environment.  The XLSX formula universe and cached values come directly from
OOXML, independently of the engine.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import math
import os
import posixpath
import shutil
import signal
import statistics
import struct
import subprocess
import sys
import time
import xml.etree.ElementTree as ET
import zipfile
from collections import Counter, defaultdict
from pathlib import Path
from typing import Any, Iterable


ROOT = Path("/home/deploy/work/god187aa")
PINNED_WORKBOOK_SHA256 = "ff82b0802d66bfb46ccbda4d22f9080f6e7aaea89c01174c2bf8301611bde62f"
PINNED_CENSUS_SHA256 = "37d7c21571a85274ce8e9ff0f3887d989185282c22b55bf1f8924bfcb1d73227"
CLOCK_EPOCH_UTC = 1786636800
CLOCK_OFFSET_SECONDS = -14400
REL_TOL = 1e-9
ABS_TOL = 1e-9
C107 = "ILL_XOUTPUT!C107"
CENSUS_SHEET = C107.split("!", 1)[0]
C107_CACHED_RAW = "1.2781180157575278E-3"
C107_ANSWER_KEY = 0.00127811801575753

MAIN_NS = "http://schemas.openxmlformats.org/spreadsheetml/2006/main"
REL_NS = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
PKG_REL_NS = "http://schemas.openxmlformats.org/package/2006/relationships"
Q_C = f"{{{MAIN_NS}}}c"
Q_F = f"{{{MAIN_NS}}}f"
Q_V = f"{{{MAIN_NS}}}v"
Q_IS = f"{{{MAIN_NS}}}is"
Q_T = f"{{{MAIN_NS}}}t"


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    with temporary.open("w", encoding="utf-8") as handle:
        json.dump(value, handle, indent=2, sort_keys=True, allow_nan=False)
        handle.write("\n")
    temporary.replace(path)


def require_under_root(path: Path) -> Path:
    resolved = path.resolve()
    if resolved != ROOT and ROOT not in resolved.parents:
        raise ValueError(f"path must remain under {ROOT}: {resolved}")
    return resolved


def verify_environment() -> None:
    expected = {
        "PIP_CACHE_DIR": str(ROOT / ".pip-cache"),
        "TMPDIR": str(ROOT / ".tmp"),
    }
    failures = {
        key: {"expected": value, "actual": os.environ.get(key)}
        for key, value in expected.items()
        if os.environ.get(key) != value
    }
    if failures:
        raise RuntimeError(f"binding environment is not exported: {failures}")


def verify_pinned_inputs(workbook: Path, sealed: Path | None = None) -> dict[str, str]:
    measured = {"workbook_sha256": sha256_file(workbook)}
    if measured["workbook_sha256"] != PINNED_WORKBOOK_SHA256:
        raise RuntimeError(
            "STOP workbook hash mismatch: "
            f"expected {PINNED_WORKBOOK_SHA256}, measured {measured['workbook_sha256']}"
        )
    if sealed is not None:
        measured["sealed_census_sha256"] = sha256_file(sealed)
        if measured["sealed_census_sha256"] != PINNED_CENSUS_SHA256:
            raise RuntimeError(
                "STOP census-oracle hash mismatch: "
                f"expected {PINNED_CENSUS_SHA256}, measured {measured['sealed_census_sha256']}"
            )
    return measured


def sorted_coordinate_sha256(coordinates: Iterable[str]) -> str:
    payload = "".join(f"{coordinate}\n" for coordinate in sorted(coordinates)).encode("utf-8")
    return hashlib.sha256(payload).hexdigest()


def _relationship_target(base: str, target: str) -> str:
    if target.startswith("/"):
        return target.lstrip("/")
    return posixpath.normpath(posixpath.join(posixpath.dirname(base), target))


def _sheet_parts(archive: zipfile.ZipFile) -> list[tuple[str, str]]:
    workbook_part = "xl/workbook.xml"
    workbook_root = ET.fromstring(archive.read(workbook_part))
    relationships_root = ET.fromstring(archive.read("xl/_rels/workbook.xml.rels"))
    targets = {
        relationship.attrib["Id"]: _relationship_target(workbook_part, relationship.attrib["Target"])
        for relationship in relationships_root.findall(f"{{{PKG_REL_NS}}}Relationship")
    }
    sheets = []
    for sheet in workbook_root.findall(f".//{{{MAIN_NS}}}sheet"):
        relationship_id = sheet.attrib[f"{{{REL_NS}}}id"]
        sheets.append((sheet.attrib["name"], targets[relationship_id]))
    return sheets


def _shared_strings(archive: zipfile.ZipFile) -> list[str]:
    part = "xl/sharedStrings.xml"
    if part not in archive.namelist():
        return []
    values: list[str] = []
    for event, element in ET.iterparse(archive.open(part), events=("end",)):
        if element.tag == f"{{{MAIN_NS}}}si":
            values.append("".join(node.text or "" for node in element.iter(Q_T)))
            element.clear()
    return values


def _cached_value(cell: ET.Element, shared_strings: list[str]) -> dict[str, Any]:
    value_element = cell.find(Q_V)
    cell_type = cell.attrib.get("t", "n")
    if value_element is None:
        return {"present": False, "type": "missing"}
    raw = value_element.text or ""
    if raw == "":
        return {"present": True, "type": "empty", "raw": raw, "value": None}
    if cell_type in {"n", ""}:
        try:
            number = float(raw)
        except ValueError:
            return {"present": True, "type": "invalid_numeric", "raw": raw}
        return {"present": True, "type": "number", "raw": raw, "value": number}
    if cell_type == "b":
        return {"present": True, "type": "boolean", "raw": raw, "value": raw == "1"}
    if cell_type == "e":
        return {"present": True, "type": "error", "raw": raw, "value": raw}
    if cell_type == "s":
        try:
            value = shared_strings[int(raw)]
        except (ValueError, IndexError):
            return {"present": True, "type": "invalid_shared_string", "raw": raw}
        return {"present": True, "type": "text", "raw": raw, "value": value}
    if cell_type in {"str", "inlineStr"}:
        if cell_type == "inlineStr":
            inline = cell.find(Q_IS)
            text = "" if inline is None else "".join(node.text or "" for node in inline.iter(Q_T))
        else:
            text = raw
        return {"present": True, "type": "text", "raw": raw, "value": text}
    if cell_type == "d":
        return {"present": True, "type": "date_text", "raw": raw, "value": raw}
    return {"present": True, "type": f"unknown:{cell_type}", "raw": raw, "value": raw}


def parse_ooxml_formula_universe(workbook: Path) -> dict[str, Any]:
    records: list[dict[str, Any]] = []
    formula_kinds: Counter[str] = Counter()
    shared_strings_count = 0
    with zipfile.ZipFile(workbook) as archive:
        shared_strings = _shared_strings(archive)
        shared_strings_count = len(shared_strings)
        for sheet_name, sheet_part in _sheet_parts(archive):
            with archive.open(sheet_part) as handle:
                for _, cell in ET.iterparse(handle, events=("end",)):
                    if cell.tag != Q_C:
                        continue
                    formula = cell.find(Q_F)
                    if formula is not None:
                        cell_ref = cell.attrib.get("r")
                        if not cell_ref:
                            raise RuntimeError(f"formula cell without coordinate in sheet {sheet_name}")
                        formula_type = formula.attrib.get("t", "normal")
                        if formula_type == "shared":
                            detail = "shared_anchor" if (formula.text or "").strip() else "shared_follower"
                        elif formula_type == "array":
                            detail = "array_anchor"
                        else:
                            detail = formula_type
                        formula_kinds[detail] += 1
                        records.append(
                            {
                                "coordinate": f"{sheet_name}!{cell_ref}",
                                "sheet": sheet_name,
                                "cell": cell_ref,
                                "formula_ooxml_kind": detail,
                                "formula_ref": formula.attrib.get("ref"),
                                "cached": _cached_value(cell, shared_strings),
                            }
                        )
                    cell.clear()
    records.sort(key=lambda record: record["coordinate"])
    coordinates = [record["coordinate"] for record in records]
    duplicates = sorted(coordinate for coordinate, count in Counter(coordinates).items() if count != 1)
    if duplicates:
        raise RuntimeError(f"duplicate physical formula coordinates: {duplicates}")
    return {
        "records": records,
        "coordinates": coordinates,
        "count": len(coordinates),
        "sorted_coordinate_sha256": sorted_coordinate_sha256(coordinates),
        "formula_element_kinds": dict(sorted(formula_kinds.items())),
        "shared_strings_count": shared_strings_count,
        "definition": {
            "population": "Every worksheet <c> element with a direct <f> child.",
            "shared_formulas": "Both shared anchors and empty shared followers carry <f> and are included at their physical coordinates.",
            "array_formulas": "Only physical <c><f t=\"array\"> anchors are included. Cells named only by the array ref but lacking their own <f> are excluded.",
            "engine_join": "One exact sheet-name and 1-based A1-coordinate lookup per physical OOXML formula coordinate, regardless of the engine's internal array representation.",
            "hash_encoding": "Coordinates sorted by Unicode code point, UTF-8 encoded, newline after every coordinate including the last.",
        },
    }


def bits_to_float(bits: int) -> float:
    return struct.unpack(">d", struct.pack(">Q", bits))[0]


def engine_numeric(engine: dict[str, Any]) -> float | None:
    if engine.get("type") == "Int":
        return float(engine["value"])
    if engine.get("type") == "Number":
        return bits_to_float(engine["bits"])
    return None


def sanitize_engine(engine: dict[str, Any]) -> dict[str, Any]:
    if engine.get("type") == "Error":
        return {"type": "Error", "kind": engine.get("kind")}
    return engine


def cached_error_kind(raw: str) -> str:
    return {
        "#REF!": "Ref",
        "#VALUE!": "Value",
        "#DIV/0!": "Div",
        "#NAME?": "Name",
        "#N/A": "NA",
        "#NUM!": "Num",
        "#NULL!": "Null",
        "#SPILL!": "Spill",
        "#CALC!": "Calc",
    }.get(raw, raw)


def compare_value(engine: dict[str, Any], cached: dict[str, Any]) -> dict[str, Any]:
    if not cached["present"]:
        return {"comparable": False, "match": None, "class": "excluded_no_cached_v"}
    engine_number = engine_numeric(engine)
    if cached["type"] == "number" and engine_number is not None:
        cached_number = float(cached["value"])
        delta = engine_number - cached_number
        absolute_delta = abs(delta)
        scale = max(abs(engine_number), abs(cached_number))
        relative_delta = 0.0 if absolute_delta == 0.0 else (math.inf if scale == 0.0 else absolute_delta / scale)
        matches = math.isclose(engine_number, cached_number, rel_tol=REL_TOL, abs_tol=ABS_TOL)
        return {
            "comparable": True,
            "match": matches,
            "class": "numeric_match" if matches else "numeric_mismatch",
            "delta": delta,
            "absolute_delta": absolute_delta,
            "relative_delta": relative_delta,
        }
    if engine.get("type") == "Error" and cached["type"] == "error":
        matches = engine.get("kind") == cached_error_kind(str(cached.get("value")))
        return {
            "comparable": True,
            "match": matches,
            "class": "error_match" if matches else "error_mismatch",
        }
    engine_normalized: tuple[str, Any]
    engine_type = engine.get("type")
    if engine_type == "Boolean":
        engine_normalized = ("boolean", engine.get("value"))
    elif engine_type == "Text":
        engine_normalized = ("text", engine.get("value"))
    elif engine_type == "Empty":
        engine_normalized = ("empty", None)
    elif engine_type == "Error":
        engine_normalized = ("error", engine.get("kind"))
    else:
        engine_normalized = (str(engine_type), engine)
    cached_normalized = (cached["type"], cached.get("value"))
    matches = engine_normalized == cached_normalized
    same_type = engine_normalized[0] == cached_normalized[0]
    return {
        "comparable": True,
        "match": matches,
        "class": ("nonnumeric_match" if matches else "nonnumeric_mismatch") if same_type else "type_mismatch",
    }


def _comparison_summary(records: list[dict[str, Any]]) -> dict[str, Any]:
    classes = Counter(record["comparison"]["class"] for record in records)
    mismatches = [
        record["coordinate"]
        for record in records
        if record["comparison"]["comparable"] and record["comparison"]["match"] is False
    ]
    type_mismatches = [
        record["coordinate"] for record in records if record["comparison"]["class"] == "type_mismatch"
    ]
    no_cached = [
        record["coordinate"]
        for record in records
        if record["comparison"]["class"] == "excluded_no_cached_v"
    ]
    return {
        "rel_tol": REL_TOL,
        "abs_tol": ABS_TOL,
        "class_counts": dict(sorted(classes.items())),
        "comparable_count": sum(1 for record in records if record["comparison"]["comparable"]),
        "mismatch_count": len(mismatches),
        "mismatch_coordinates": mismatches,
        "type_mismatch_count": len(type_mismatches),
        "type_mismatch_coordinates": type_mismatches,
        "no_cached_v_count": len(no_cached),
        "no_cached_v_coordinates": no_cached,
    }


def worker(workbook_path: Path, output_path: Path) -> int:
    verify_environment()
    measured = verify_pinned_inputs(workbook_path)
    oracle = parse_ooxml_formula_universe(workbook_path)

    import formualizer as fz  # Imported only in the fresh wheel environment.

    load_started = time.perf_counter()
    workbook = fz.Workbook.load_path(str(workbook_path), strategy="eager_all")
    workbook.set_deterministic_clock(
        dt.datetime.fromtimestamp(CLOCK_EPOCH_UTC, tz=dt.timezone.utc),
        CLOCK_OFFSET_SECONDS,
    )
    load_seconds = time.perf_counter() - load_started

    evaluation_started = time.perf_counter()
    evaluation_failure: dict[str, Any] | None = None
    try:
        workbook.evaluate_all()
    except Exception as error:
        evaluation_error_type = getattr(fz, "ExcelEvaluationError", None)
        if evaluation_error_type is None or not isinstance(error, evaluation_error_type):
            raise
        evaluation_failure = {
            "exception_type": type(error).__name__,
            "kind": getattr(error, "kind", getattr(error, "excel_kind", None)),
            "message": str(error),
        }
    evaluation_seconds = time.perf_counter() - evaluation_started

    output_records: list[dict[str, Any]] = []
    census_errors: defaultdict[str, list[str]] = defaultdict(list)
    all_engine_errors: defaultdict[str, list[str]] = defaultdict(list)
    for source in oracle["records"]:
        cell_ref = source["cell"]
        index = 0
        while index < len(cell_ref) and cell_ref[index].isalpha():
            index += 1
        column_letters, row_text = cell_ref[:index], cell_ref[index:]
        column = 0
        for letter in column_letters.upper():
            column = column * 26 + (ord(letter) - ord("A") + 1)
        row = int(row_text)
        typed = json.loads(workbook.get_typed_value_json(source["sheet"], row, column))
        typed = sanitize_engine(typed)
        if typed.get("type") == "Error":
            error_kind = str(typed.get("kind"))
            all_engine_errors[error_kind].append(source["coordinate"])
            if source["sheet"] == CENSUS_SHEET:
                census_errors[error_kind].append(source["coordinate"])
        comparison = compare_value(typed, source["cached"])
        record = {
            "coordinate": source["coordinate"],
            "formula_ooxml_kind": source["formula_ooxml_kind"],
            "engine": typed,
            "cached": source["cached"],
            "comparison": comparison,
        }
        numeric_value = engine_numeric(typed)
        if numeric_value is not None:
            record["engine_numeric_value"] = numeric_value
        output_records.append(record)

    output_coordinates = [record["coordinate"] for record in output_records]
    universe_set = set(oracle["coordinates"])
    output_set = set(output_coordinates)
    duplicates = sorted(coordinate for coordinate, count in Counter(output_coordinates).items() if count != 1)
    coverage = {
        "expected_count": oracle["count"],
        "output_record_count": len(output_coordinates),
        "output_unique_count": len(output_set),
        "missing_count": len(universe_set - output_set),
        "missing_coordinates": sorted(universe_set - output_set),
        "extra_count": len(output_set - universe_set),
        "extra_coordinates": sorted(output_set - universe_set),
        "duplicate_count": len(duplicates),
        "duplicate_coordinates": duplicates,
    }
    coverage["exact"] = all(
        coverage[key] == 0 for key in ("missing_count", "extra_count", "duplicate_count")
    ) and coverage["expected_count"] == coverage["output_record_count"]

    result = {
        "schema_version": 1,
        "workbook_sha256": measured["workbook_sha256"],
        "engine": {
            "package": "formualizer",
            "version": getattr(fz, "__version__", "unknown"),
        },
        "clock_pin": {
            "epoch_utc": CLOCK_EPOCH_UTC,
            "utc_iso": dt.datetime.fromtimestamp(CLOCK_EPOCH_UTC, tz=dt.timezone.utc).isoformat(),
            "offset_seconds": CLOCK_OFFSET_SECONDS,
        },
        "runtime": {
            "load_and_immediate_clock_pin_seconds": load_seconds,
            "evaluate_all_wall_seconds": evaluation_seconds,
            "evaluate_all_completed": evaluation_failure is None,
            "evaluate_all_failure": evaluation_failure,
        },
        "formula_universe": {
            key: value for key, value in oracle.items() if key not in {"records", "coordinates"}
        },
        "universe_coverage": coverage,
        "error_census_population": {
            "sheet": CENSUS_SHEET,
            "definition": "All physical OOXML formula coordinates on the Ledger result sheet used by the sealed GOD-187i census.",
        },
        "error_cells_by_class": {
            key: sorted(value) for key, value in sorted(census_errors.items())
        },
        "error_count": sum(len(value) for value in census_errors.values()),
        "all_engine_error_cells_by_class": {
            key: sorted(value) for key, value in sorted(all_engine_errors.items())
        },
        "all_engine_error_count": sum(len(value) for value in all_engine_errors.values()),
        "comparison": _comparison_summary(output_records),
        "cells": output_records,
    }
    write_json(output_path, result)
    return 0 if coverage["exact"] else 2


def parse_gnu_time(path: Path) -> dict[str, Any]:
    fields: dict[str, str] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        if ": " in line:
            key, value = line.strip().split(": ", 1)
            fields[key] = value
    rss = fields.get("Maximum resident set size (kbytes)")
    return {
        "maximum_rss_kib": int(rss) if rss is not None else None,
        "elapsed_wall_clock": fields.get("Elapsed (wall clock) time (h:mm:ss or m:ss)"),
        "user_seconds": fields.get("User time (seconds)"),
        "system_seconds": fields.get("System time (seconds)"),
        "exit_status": int(fields["Exit status"]) if "Exit status" in fields else None,
        "source": str(path),
    }


def run_in_fresh_venv(args: argparse.Namespace) -> int:
    verify_environment()
    workbook = require_under_root(Path(args.workbook))
    wheel = require_under_root(Path(args.wheel))
    output = require_under_root(Path(args.output))
    venv = require_under_root(Path(args.venv))
    timing = require_under_root(Path(args.timing))
    log = require_under_root(Path(args.log))
    verify_pinned_inputs(workbook)
    wheel_hash = sha256_file(wheel)
    if wheel_hash != args.wheel_sha256:
        raise RuntimeError(
            f"STOP wheel hash mismatch: expected {args.wheel_sha256}, measured {wheel_hash}"
        )
    if venv.exists():
        raise RuntimeError(f"fresh venv path already exists: {venv}")
    for path in (output, timing, log):
        path.parent.mkdir(parents=True, exist_ok=True)

    subprocess.run(
        [sys.executable, "-m", "venv", "--without-pip", str(venv)], check=True
    )
    pip_driver = ROOT / "venv-probe-baseline-preserved" / "bin" / "python"
    if not pip_driver.is_file():
        raise RuntimeError(f"accepted aa1 clone-local pip driver is missing: {pip_driver}")
    install = subprocess.run(
        [
            str(pip_driver),
            "-m",
            "pip",
            "--python",
            str(venv / "bin" / "python"),
            "install",
            "--no-index",
            "--no-deps",
            str(wheel),
        ],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    log.write_text(
        "[clone-local external pip driver; target venv remains pip-less]\n"
        + install.stdout,
        encoding="utf-8",
    )
    if install.returncode != 0:
        return install.returncode

    command = [
        "/usr/bin/time",
        "-v",
        "-o",
        str(timing),
        str(venv / "bin" / "python"),
        str(Path(__file__).resolve()),
        "worker",
        "--workbook",
        str(workbook),
        "--output",
        str(output),
    ]
    with log.open("a", encoding="utf-8") as handle:
        handle.write("\n[worker]\n")
        handle.flush()
        process = subprocess.Popen(
            command,
            stdout=handle,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        try:
            return_code = process.wait(timeout=1800)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
            handle.write("STOP: census worker exceeded the 30-minute limit\n")
            return 124

    if output.exists() and timing.exists():
        result = json.loads(output.read_text(encoding="utf-8"))
        result["wheel"] = {"path": str(wheel), "sha256": wheel_hash}
        result["process_measurement"] = parse_gnu_time(timing)
        result["process_measurement"]["wrapped_command"] = "worker process including one evaluate_all call"
        write_json(output, result)
    return return_code


def load_run(path: Path) -> tuple[dict[str, Any], dict[str, dict[str, Any]]]:
    run = json.loads(path.read_text(encoding="utf-8"))
    cells = {cell["coordinate"]: cell for cell in run["cells"]}
    return run, cells


def error_map(run: dict[str, Any]) -> dict[str, str]:
    return {
        coordinate: error_class
        for error_class, coordinates in run["error_cells_by_class"].items()
        for coordinate in coordinates
    }


def all_engine_error_map(run: dict[str, Any]) -> dict[str, str]:
    return {
        coordinate: error_class
        for error_class, coordinates in run["all_engine_error_cells_by_class"].items()
        for coordinate in coordinates
    }


def matching_set(cells: dict[str, dict[str, Any]]) -> set[str]:
    return {
        coordinate
        for coordinate, cell in cells.items()
        if cell["comparison"]["comparable"] and cell["comparison"]["match"] is True
    }


def mismatching_set(cells: dict[str, dict[str, Any]]) -> set[str]:
    return {
        coordinate
        for coordinate, cell in cells.items()
        if cell["comparison"]["comparable"] and cell["comparison"]["match"] is False
    }


def _cell_numeric(cell: dict[str, Any]) -> float | None:
    if "engine_numeric_value" in cell:
        return float(cell["engine_numeric_value"])
    return engine_numeric(cell["engine"])


def baseline_gate(run_path: Path, sealed_path: Path, output_path: Path) -> int:
    verify_pinned_inputs(ROOT / "s020-private" / "original.xlsx", sealed_path)
    run, cells = load_run(run_path)
    sealed = json.loads(sealed_path.read_text(encoding="utf-8"))
    expected = set(sealed["coordinates"])
    errors = error_map(run)
    actual = set(errors)
    c107 = cells.get(C107)
    c107_engine = _cell_numeric(c107) if c107 else None
    c107_cached = c107["cached"].get("value") if c107 else None
    result = {
        "verdict": "PASS",
        "checks": {
            "evaluate_all_completed": run["runtime"]["evaluate_all_completed"],
            "universe_coverage_exact": run["universe_coverage"]["exact"],
            "error_census_exact": run["error_cells_by_class"] == {"Ref": sorted(expected)},
            "sealed_coordinate_set_equal": actual == expected,
            "c107_engine_zero": c107_engine == 0.0,
            "c107_cached_reproduced": c107 is not None
            and c107["cached"].get("raw") == C107_CACHED_RAW
            and math.isclose(float(c107_cached), float(C107_CACHED_RAW), rel_tol=0.0, abs_tol=0.0),
        },
        "sealed_ref_count": len(expected),
        "actual_error_cells_by_class": run["error_cells_by_class"],
        "coordinate_symmetric_difference": sorted(actual ^ expected),
        "coordinate_symmetric_difference_count": len(actual ^ expected),
        "c107": {"engine": c107_engine, "cached": c107_cached},
        "engine_vs_cached_mismatch_count": run["comparison"]["mismatch_count"],
        "historical_ledger_answer_key_mismatch_count": sealed["value_mismatch_count"],
        "historical_count_comparator_continuity_claim": False,
    }
    if not all(result["checks"].values()):
        result["verdict"] = "STOP-PLATFORM-DIVERGENCE"
    write_json(output_path, result)
    return 0 if result["verdict"] == "PASS" else 3


def percentile(values: list[float], fraction: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    index = min(len(ordered) - 1, max(0, math.ceil(fraction * len(ordered)) - 1))
    return ordered[index]


def deviations(cells: dict[str, dict[str, Any]], coordinates: set[str]) -> dict[str, Any]:
    absolute = [
        float(cells[coordinate]["comparison"]["absolute_delta"])
        for coordinate in sorted(coordinates)
        if coordinate in cells and "absolute_delta" in cells[coordinate]["comparison"]
    ]
    relative = [
        float(cells[coordinate]["comparison"]["relative_delta"])
        for coordinate in sorted(coordinates)
        if coordinate in cells and "relative_delta" in cells[coordinate]["comparison"]
    ]
    return {
        "numeric_delta_count": len(absolute),
        "absolute_delta": {
            "min": min(absolute) if absolute else None,
            "p50": percentile(absolute, 0.50),
            "p90": percentile(absolute, 0.90),
            "p99": percentile(absolute, 0.99),
            "max": max(absolute) if absolute else None,
            "mean": statistics.fmean(absolute) if absolute else None,
        },
        "relative_delta": {
            "min": min(relative) if relative else None,
            "p50": percentile(relative, 0.50),
            "p90": percentile(relative, 0.90),
            "p99": percentile(relative, 0.99),
            "max": max(relative) if relative else None,
        },
    }


def collateral_detail(
    baseline_cells: dict[str, dict[str, Any]],
    fixed_cells: dict[str, dict[str, Any]],
    coordinates: Iterable[str],
) -> list[dict[str, Any]]:
    details = []
    for coordinate in sorted(coordinates):
        fixed = fixed_cells[coordinate]
        comparison = fixed["comparison"]
        detail: dict[str, Any] = {
            "coordinate": coordinate,
            "type_or_class": comparison["class"],
        }
        if "delta" in comparison:
            detail["numeric_delta"] = comparison["delta"]
        else:
            detail["baseline_type"] = baseline_cells[coordinate]["engine"].get("type")
            detail["fixed_type"] = fixed["engine"].get("type")
        details.append(detail)
    return details


def compare_runs(baseline_path: Path, fixed_path: Path, sealed_path: Path, output_path: Path) -> int:
    verify_pinned_inputs(ROOT / "s020-private" / "original.xlsx", sealed_path)
    baseline, baseline_cells = load_run(baseline_path)
    fixed, fixed_cells = load_run(fixed_path)
    sealed = json.loads(sealed_path.read_text(encoding="utf-8"))
    heal_coordinates = set(sealed["coordinates"])
    baseline_census_errors = error_map(baseline)
    fixed_census_errors = error_map(fixed)
    baseline_all_errors = all_engine_error_map(baseline)
    fixed_all_errors = all_engine_error_map(fixed)
    baseline_match = matching_set(baseline_cells)
    fixed_match = matching_set(fixed_cells)
    baseline_mismatch = mismatching_set(baseline_cells)
    fixed_mismatch = mismatching_set(fixed_cells)

    healed = baseline_mismatch & fixed_match
    newly_mismatched = baseline_match & fixed_mismatch
    unchanged_match = baseline_match & fixed_match
    unchanged_mismatch = baseline_mismatch & fixed_mismatch
    other_state_change = set(baseline_cells) - (
        healed | newly_mismatched | unchanged_match | unchanged_mismatch
    )
    heal_exceptions = sorted(heal_coordinates - fixed_match)
    new_error_coordinates = sorted(set(fixed_all_errors) - set(baseline_all_errors))
    residual_errors = [
        {"coordinate": coordinate, "class": fixed_census_errors[coordinate]}
        for coordinate in sorted(fixed_census_errors)
    ]
    c107_baseline = _cell_numeric(baseline_cells[C107])
    c107_fixed = _cell_numeric(fixed_cells[C107])
    c107_cached = float(fixed_cells[C107]["cached"]["value"])

    checks = {
        "baseline_evaluate_all_completed": baseline["runtime"]["evaluate_all_completed"],
        "fixed_evaluate_all_completed": fixed["runtime"]["evaluate_all_completed"],
        "baseline_universe_coverage_exact": baseline["universe_coverage"]["exact"],
        "fixed_universe_coverage_exact": fixed["universe_coverage"]["exact"],
        "same_formula_universe": baseline["formula_universe"]["count"]
        == fixed["formula_universe"]["count"]
        and baseline["formula_universe"]["sorted_coordinate_sha256"]
        == fixed["formula_universe"]["sorted_coordinate_sha256"],
        "zero_ref_cells": "Ref" not in fixed["error_cells_by_class"],
        "zero_residual_errors": not fixed_census_errors,
        "all_heal_coordinates_match": not heal_exceptions,
        "c107_matches_cached": c107_fixed is not None
        and math.isclose(c107_fixed, c107_cached, rel_tol=REL_TOL, abs_tol=ABS_TOL),
        "c107_matches_answer_key": c107_fixed is not None
        and math.isclose(c107_fixed, C107_ANSWER_KEY, rel_tol=REL_TOL, abs_tol=ABS_TOL),
        "no_new_errors": not new_error_coordinates,
        "no_newly_mismatched_cells": not newly_mismatched,
    }
    if not checks["no_newly_mismatched_cells"]:
        verdict = "FAIL-COLLATERAL"
        exit_code = 4
    elif not all(checks.values()):
        verdict = "FAIL-HEAL"
        exit_code = 5
    else:
        verdict = "PASS"
        exit_code = 0
    result = {
        "verdict": verdict,
        "checks": checks,
        "heal_coordinates": {
            "expected_count": len(heal_coordinates),
            "match_count": len(heal_coordinates & fixed_match),
            "exception_count": len(heal_exceptions),
            "exception_coordinates": heal_exceptions,
            "deviation_distribution": deviations(fixed_cells, heal_coordinates),
        },
        "errors": {
            "baseline_count": len(baseline_census_errors),
            "fixed_count": len(fixed_census_errors),
            "fixed_error_cells_by_class": fixed["error_cells_by_class"],
            "residual_errors": residual_errors,
            "new_error_count": len(new_error_coordinates),
            "new_error_coordinates": new_error_coordinates,
            "all_engine_baseline_count": len(baseline_all_errors),
            "all_engine_fixed_count": len(fixed_all_errors),
            "all_engine_removed_error_coordinates": sorted(
                set(baseline_all_errors) - set(fixed_all_errors)
            ),
        },
        "c107": {
            "baseline_engine": c107_baseline,
            "fixed_engine": c107_fixed,
            "fixed_engine_type_or_class": fixed_cells[C107]["comparison"]["class"],
            "cached": c107_cached,
            "answer_key": C107_ANSWER_KEY,
            "fixed_minus_cached": None if c107_fixed is None else c107_fixed - c107_cached,
            "fixed_minus_answer_key": None
            if c107_fixed is None
            else c107_fixed - C107_ANSWER_KEY,
        },
        "full_engine_vs_cached": {
            "baseline_mismatch_count": baseline["comparison"]["mismatch_count"],
            "fixed_mismatch_count": fixed["comparison"]["mismatch_count"],
            "healed_count": len(healed),
            "healed_coordinates": sorted(healed),
            "newly_mismatched_count": len(newly_mismatched),
            "newly_mismatched": collateral_detail(
                baseline_cells, fixed_cells, newly_mismatched
            ),
            "unchanged_match_count": len(unchanged_match),
            "unchanged_match_coordinates": sorted(unchanged_match),
            "unchanged_mismatch_count": len(unchanged_mismatch),
            "unchanged_mismatch_coordinates": sorted(unchanged_mismatch),
            "other_state_change_count": len(other_state_change),
            "other_state_change_coordinates": sorted(other_state_change),
        },
        "historical_context": {
            "prior_ledger_answer_key_mismatch_count": sealed["value_mismatch_count"],
            "comparator_policy": "GOD-158 Ledger answer key, not engine versus workbook cache",
            "continuity_claim": False,
        },
    }
    write_json(output_path, result)
    return exit_code


def model_gate(
    sealed: set[str],
    baseline_errors: dict[str, str],
    fixed_errors: dict[str, str],
    fixed_match: set[str],
    baseline_match: set[str],
) -> dict[str, Any]:
    newly_mismatched = baseline_match - fixed_match
    checks = {
        "baseline_exact": baseline_errors == {coordinate: "Ref" for coordinate in sealed},
        "zero_ref": all(kind != "Ref" for kind in fixed_errors.values()),
        "all_heal_match": sealed <= fixed_match,
        "no_new_errors": not (set(fixed_errors) - set(baseline_errors)),
        "no_newly_mismatched": not newly_mismatched,
    }
    return {
        "checks": checks,
        "newly_mismatched": sorted(newly_mismatched),
        "overall": all(checks.values()),
    }


def self_test(output_path: Path) -> int:
    sealed = {"Heal!A1", "Heal!A2"}
    nonheal_error = "Clean!B1"
    nonheal_value = "Clean!B2"
    baseline_errors = {coordinate: "Ref" for coordinate in sealed}
    baseline_match = {nonheal_error, nonheal_value}
    conforming_fixed_match = sealed | baseline_match

    def numeric_matches(engine_value: float, cached_value: float) -> bool:
        engine = {"type": "Number", "bits": struct.unpack(">Q", struct.pack(">d", engine_value))[0]}
        cached = {"present": True, "type": "number", "raw": repr(cached_value), "value": cached_value}
        return compare_value(engine, cached)["match"] is True

    conforming_heal_match = numeric_matches(1.0, 1.0)
    engine_perturbed_match = numeric_matches(1.0 * (1.0 + 1e-6), 1.0)
    cached_perturbed_match = numeric_matches(1.0, 1.0 * (1.0 + 1e-6))
    nonheal_perturbed_match = numeric_matches(2.0 * (1.0 + 1e-6), 2.0)
    assert conforming_heal_match
    assert not engine_perturbed_match
    assert not cached_perturbed_match
    assert not nonheal_perturbed_match

    cases: dict[str, dict[str, Any]] = {}
    cases["conforming"] = model_gate(
        sealed, baseline_errors, {}, conforming_fixed_match, baseline_match
    )
    cases["mutant_a_heal_forced_error"] = model_gate(
        sealed,
        baseline_errors,
        {"Heal!A1": "Ref"},
        conforming_fixed_match - {"Heal!A1"},
        baseline_match,
    )
    cases["mutant_b_heal_engine_perturbed"] = model_gate(
        sealed,
        baseline_errors,
        {},
        conforming_fixed_match if engine_perturbed_match else conforming_fixed_match - {"Heal!A1"},
        baseline_match,
    )
    cases["mutant_b_heal_engine_perturbed"]["numeric_model"] = {
        "engine_relative_perturbation": 1e-6,
        "tolerance_match": engine_perturbed_match,
    }
    cases["mutant_c_heal_cached_perturbed"] = model_gate(
        sealed,
        baseline_errors,
        {},
        conforming_fixed_match if cached_perturbed_match else conforming_fixed_match - {"Heal!A2"},
        baseline_match,
    )
    cases["mutant_c_heal_cached_perturbed"]["numeric_model"] = {
        "cached_relative_perturbation": 1e-6,
        "tolerance_match": cached_perturbed_match,
    }
    cases["mutant_d_clean_nonheal_forced_error"] = model_gate(
        sealed,
        baseline_errors,
        {nonheal_error: "Value"},
        conforming_fixed_match - {nonheal_error},
        baseline_match,
    )
    cases["mutant_e_clean_nonheal_new_mismatch"] = model_gate(
        sealed,
        baseline_errors,
        {},
        conforming_fixed_match
        if nonheal_perturbed_match
        else conforming_fixed_match - {nonheal_value},
        baseline_match,
    )
    cases["mutant_e_clean_nonheal_new_mismatch"]["numeric_model"] = {
        "engine_relative_perturbation": 1e-6,
        "tolerance_match": nonheal_perturbed_match,
    }
    expectations = {
        "conforming": (True, None),
        "mutant_a_heal_forced_error": (False, "zero_ref"),
        "mutant_b_heal_engine_perturbed": (False, "all_heal_match"),
        "mutant_c_heal_cached_perturbed": (False, "all_heal_match"),
        "mutant_d_clean_nonheal_forced_error": (False, "no_new_errors"),
        "mutant_e_clean_nonheal_new_mismatch": (False, "no_newly_mismatched"),
    }
    attested = True
    for name, (expected_overall, expected_failed_gate) in expectations.items():
        case = cases[name]
        case["expected_overall"] = expected_overall
        case["expected_failed_gate"] = expected_failed_gate
        case["attested"] = case["overall"] is expected_overall and (
            expected_failed_gate is None or case["checks"][expected_failed_gate] is False
        )
        attested = attested and case["attested"]
    result = {
        "schema_version": 1,
        "model_only_no_real_input_mutation": True,
        "tolerances": {"relative": REL_TOL, "absolute": ABS_TOL},
        "all_six_cases_attested": attested,
        "conforming_passed": cases["conforming"]["overall"],
        "five_mutants_failed": sum(
            not case["overall"] for name, case in cases.items() if name != "conforming"
        )
        == 5,
        "cases": cases,
    }
    write_json(output_path, result)
    return 0 if attested else 6


def oracle_command(workbook: Path, sealed: Path, output: Path) -> int:
    hashes = verify_pinned_inputs(workbook, sealed)
    oracle = parse_ooxml_formula_universe(workbook)
    sealed_data = json.loads(sealed.read_text(encoding="utf-8"))
    coordinate_set = set(oracle["coordinates"])
    missing_sealed = sorted(set(sealed_data["coordinates"]) - coordinate_set)
    result = {
        "schema_version": 1,
        **hashes,
        "formula_universe": {
            key: value for key, value in oracle.items() if key not in {"records", "coordinates"}
        },
        "sealed_ref_coordinates_in_universe": {
            "expected": len(sealed_data["coordinates"]),
            "present": len(set(sealed_data["coordinates"]) & coordinate_set),
            "missing_count": len(missing_sealed),
            "missing_coordinates": missing_sealed,
        },
        "c107_in_universe": C107 in coordinate_set,
    }
    write_json(output, result)
    return 0 if not missing_sealed and result["c107_in_universe"] else 7


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    oracle_parser = subparsers.add_parser("oracle")
    oracle_parser.add_argument("--workbook", required=True)
    oracle_parser.add_argument("--sealed", required=True)
    oracle_parser.add_argument("--output", required=True)

    test_parser = subparsers.add_parser("self-test")
    test_parser.add_argument("--output", required=True)

    run_parser = subparsers.add_parser("run")
    run_parser.add_argument("--wheel", required=True)
    run_parser.add_argument("--wheel-sha256", required=True)
    run_parser.add_argument("--workbook", required=True)
    run_parser.add_argument("--output", required=True)
    run_parser.add_argument("--venv", required=True)
    run_parser.add_argument("--timing", required=True)
    run_parser.add_argument("--log", required=True)

    worker_parser = subparsers.add_parser("worker")
    worker_parser.add_argument("--workbook", required=True)
    worker_parser.add_argument("--output", required=True)

    baseline_parser = subparsers.add_parser("baseline-gate")
    baseline_parser.add_argument("--run", required=True)
    baseline_parser.add_argument("--sealed", required=True)
    baseline_parser.add_argument("--output", required=True)

    compare_parser = subparsers.add_parser("compare")
    compare_parser.add_argument("--baseline", required=True)
    compare_parser.add_argument("--fixed", required=True)
    compare_parser.add_argument("--sealed", required=True)
    compare_parser.add_argument("--output", required=True)
    return parser


def main() -> int:
    args = build_parser().parse_args()
    verify_environment()
    if args.command == "oracle":
        return oracle_command(
            require_under_root(Path(args.workbook)),
            require_under_root(Path(args.sealed)),
            require_under_root(Path(args.output)),
        )
    if args.command == "self-test":
        return self_test(require_under_root(Path(args.output)))
    if args.command == "run":
        return run_in_fresh_venv(args)
    if args.command == "worker":
        return worker(
            require_under_root(Path(args.workbook)), require_under_root(Path(args.output))
        )
    if args.command == "baseline-gate":
        return baseline_gate(
            require_under_root(Path(args.run)),
            require_under_root(Path(args.sealed)),
            require_under_root(Path(args.output)),
        )
    if args.command == "compare":
        return compare_runs(
            require_under_root(Path(args.baseline)),
            require_under_root(Path(args.fixed)),
            require_under_root(Path(args.sealed)),
            require_under_root(Path(args.output)),
        )
    raise AssertionError(args.command)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(f"{type(error).__name__}: {error}", file=sys.stderr)
        raise
