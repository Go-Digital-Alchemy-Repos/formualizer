//! Goal seek (Lane B): Brent-Dekker and `run_goal_seeks` against the Python
//! behaviour source. `fixtures/brent_expected.json` and
//! `fixtures/goal_seek_expected.json` were produced by running the parity
//! repository's `_brent_solve` and `solve.run_solves` (see
//! `fixtures/gen_python_expectations.py`); every probe, root and write-back
//! is compared bit for bit.

use formualizer_common::LiteralValue;
use formualizer_modelcall::evaluator::{DefinedRange, NameScope, SolveModel};
use formualizer_modelcall::goal_seek::{BrentStop, brent_solve, run_goal_seeks};
use formualizer_modelcall::{CellRange, ModelCallError};
use serde_json::Value;
use std::collections::HashMap;

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A Python float `repr` parsed back (Rust parses shortest-round-trip text exactly).
fn py_float(value: &Value) -> f64 {
    value.as_str().unwrap().parse::<f64>().unwrap()
}

fn function(name: &str) -> fn(f64) -> f64 {
    match name {
        "cubic" | "cubic_failing_at" => |x| x * x * x - 2.0 * x - 5.0,
        "square2" => |x| x * x - 2.0,
        "rational" => |x| 1.0 / (x + 1.0) - 0.3,
        "linear3" => |x| x - 3.0,
        "steep" => |x| (x - 0.123456789) * 1e6,
        // Python `max(x - 7.5, 0.0)`: the first argument unless the second is larger.
        "flat_then_up" => |x| {
            let shifted = x - 7.5;
            let clipped = if 0.0 > shifted { 0.0 } else { shifted };
            clipped * 1000.0 - 1.0
        },
        other => panic!("unknown function {other}"),
    }
}

#[test]
fn brent_matches_python_probe_for_probe() {
    let cases = fixture("brent_expected.json");
    let mut compared = 0;
    for case in cases.as_array().unwrap() {
        let name = case["function"].as_str().unwrap();
        if name == "cubic_failing_at" {
            continue;
        }
        let f = function(name);
        let mut calls: Vec<f64> = Vec::new();
        let mut objective = |x: f64| -> Result<f64, BrentStop> {
            calls.push(x);
            Ok(f(x))
        };
        let outcome = brent_solve(
            &mut objective,
            py_float(&case["lower"]),
            py_float(&case["upper"]),
            py_float(&case["max_change"]),
            u32::try_from(case["max_iterations"].as_u64().unwrap()).unwrap(),
        );
        let expected_calls: Vec<u64> =
            case["calls"].as_array().unwrap().iter().map(|call| py_float(call).to_bits()).collect();
        let actual_calls: Vec<u64> = calls.iter().map(|call| call.to_bits()).collect();
        assert_eq!(actual_calls, expected_calls, "probe sequence for {case}");
        match case["outcome"].as_str().unwrap() {
            "ok" => {
                let result = outcome.unwrap_or_else(|stop| panic!("{case}: {stop:?}"));
                assert_eq!(result.value.to_bits(), py_float(&case["value"]).to_bits(), "{case}");
                assert_eq!(u64::from(result.iterations), case["iterations"].as_u64().unwrap(), "{case}");
            }
            "max_iterations" => {
                let iterations = u32::try_from(case["iterations"].as_u64().unwrap()).unwrap();
                assert_eq!(outcome, Err(BrentStop::MaxIterations(iterations)), "{case}");
            }
            "no_bracket" => assert_eq!(outcome, Err(BrentStop::NoBracket), "{case}"),
            "zero_slope" => assert_eq!(outcome, Err(BrentStop::ZeroSlope), "{case}"),
            other => panic!("unknown outcome {other}"),
        }
        compared += 1;
    }
    assert_eq!(compared, 12);
}

#[test]
fn brent_target_not_numeric_carries_the_python_iteration_count() {
    let cases = fixture("brent_expected.json");
    let mut compared = 0;
    for case in cases.as_array().unwrap() {
        if case["function"] != "cubic_failing_at" {
            continue;
        }
        let fail_call = case["fail_call"].as_u64().unwrap();
        let mut count = 0u64;
        let mut objective = |x: f64| -> Result<f64, BrentStop> {
            count += 1;
            if count >= fail_call {
                return Err(BrentStop::TargetNotNumeric(0));
            }
            Ok(x * x * x - 2.0 * x - 5.0)
        };
        let outcome = brent_solve(&mut objective, 0.0, 300.0, 1e-12, 100);
        let iterations = u32::try_from(case["iterations"].as_u64().unwrap()).unwrap();
        assert_eq!(outcome, Err(BrentStop::TargetNotNumeric(iterations)), "{case}");
        compared += 1;
    }
    assert_eq!(compared, 3);
}

#[test]
fn brent_known_roots_and_model_errors() {
    // An exact root at an endpoint returns it with zero iterations.
    let mut linear = |x: f64| -> Result<f64, BrentStop> { Ok(x - 4.0) };
    let result = brent_solve(&mut linear, 4.0, 9.0, 1e-9, 25).unwrap();
    assert_eq!((result.value, result.iterations), (4.0, 0));
    // sqrt(2) to the last bit the method reaches with a tight tolerance.
    let mut square = |x: f64| -> Result<f64, BrentStop> { Ok(x * x - 2.0) };
    let root = brent_solve(&mut square, 0.0, 2.0, 1e-15, 200).unwrap().value;
    assert!((root - std::f64::consts::SQRT_2).abs() < 1e-15);
    // A model failure stops at once and is passed through unchanged.
    let failure = ModelCallError::infrastructure("RuntimeError", "engine down");
    let mut failing = |_: f64| -> Result<f64, BrentStop> { Err(BrentStop::Model(failure.clone())) };
    assert_eq!(
        brent_solve(&mut failing, 0.0, 1.0, 1e-9, 25),
        Err(BrentStop::Model(ModelCallError::infrastructure("RuntimeError", "engine down")))
    );
    assert_eq!(BrentStop::ZeroSlope.reason(), "zero_slope");
}

/// The Python `FakeBook`: sheet S, change cell B2, target cell B3 =
/// B2^3 - 2*B2 - 5, the block's labels in H and values in I.
struct FakeModel {
    cells: HashMap<(String, u32, u32), LiteralValue>,
    formulas: HashMap<(String, u32, u32), String>,
    names: Vec<DefinedRange>,
    writes: Vec<(String, u32, u32, LiteralValue)>,
}

fn typed(value: &Value) -> Option<LiteralValue> {
    Some(match value["type"].as_str().unwrap() {
        "none" => return None,
        "bool" => LiteralValue::Boolean(value["value"].as_bool().unwrap()),
        "int" => LiteralValue::Int(value["value"].as_i64().unwrap()),
        "float" => LiteralValue::Number(py_float(&value["value"])),
        "str" => LiteralValue::Text(value["value"].as_str().unwrap().to_owned()),
        other => panic!("unknown type {other}"),
    })
}

impl FakeModel {
    fn from_scenario(scenario: &Value) -> Self {
        let mut cells = HashMap::new();
        for (index, pair) in scenario["labels"].as_array().unwrap().iter().enumerate() {
            let row = u32::try_from(index + 1).unwrap();
            cells.insert(("S".to_owned(), row, 8), LiteralValue::Text(pair[0].as_str().unwrap().to_owned()));
            if let Some(value) = typed(&pair[1]) {
                cells.insert(("S".to_owned(), row, 9), value);
            }
        }
        cells.insert(("S".to_owned(), 2, 2), LiteralValue::Number(1.0));
        let formulas = scenario["formulas"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                let coordinate = |index: usize| u32::try_from(entry[index].as_u64().unwrap()).unwrap();
                (
                    (entry[0].as_str().unwrap().to_owned(), coordinate(1), coordinate(2)),
                    entry[3].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        let names = scenario["names"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                let coordinate = |key: &str| u32::try_from(row[key].as_u64().unwrap()).unwrap();
                DefinedRange {
                    name: row["name"].as_str().unwrap().to_owned(),
                    scope: if row["scope"] == "workbook" {
                        NameScope::Workbook
                    } else {
                        NameScope::Sheet(row["scope_sheet"].as_str().unwrap().to_owned())
                    },
                    range: Some(CellRange {
                        sheet: row["sheet"].as_str().unwrap().to_owned(),
                        start_row: coordinate("start_row"),
                        start_col: coordinate("start_col"),
                        end_row: coordinate("end_row"),
                        end_col: coordinate("end_col"),
                    }),
                }
            })
            .collect();
        let mut model = Self { cells, formulas, names, writes: Vec::new() };
        model.evaluate_all().unwrap();
        model
    }
}

impl SolveModel for FakeModel {
    fn get_value(&self, sheet: &str, row: u32, col: u32) -> Result<LiteralValue, ModelCallError> {
        Ok(self.cells.get(&(sheet.to_owned(), row, col)).cloned().unwrap_or(LiteralValue::Empty))
    }

    fn set_value(&mut self, sheet: &str, row: u32, col: u32, value: LiteralValue) -> Result<(), ModelCallError> {
        self.writes.push((sheet.to_owned(), row, col, value.clone()));
        self.cells.insert((sheet.to_owned(), row, col), value);
        Ok(())
    }

    fn evaluate_all(&mut self) -> Result<(), ModelCallError> {
        let target = match self.cells.get(&("S".to_owned(), 2, 2)) {
            Some(LiteralValue::Number(x)) => LiteralValue::Number(x * x * x - 2.0 * x - 5.0),
            _ => LiteralValue::Text("bad".into()),
        };
        self.cells.insert(("S".to_owned(), 3, 2), target);
        Ok(())
    }

    fn defined_ranges(&self) -> Result<Vec<DefinedRange>, ModelCallError> {
        Ok(self.names.clone())
    }

    fn get_formula(&self, sheet: &str, row: u32, col: u32) -> Result<Option<String>, ModelCallError> {
        Ok(self.formulas.get(&(sheet.to_owned(), row, col)).cloned())
    }
}

/// A write as the Python fake logs it: `[sheet, row, col, kind, repr]`.
fn write_matches(actual: &(String, u32, u32, LiteralValue), expected: &Value) -> bool {
    let (sheet, row, col, value) = actual;
    let same_cell = expected[0] == sheet.as_str()
        && expected[1].as_u64() == Some(u64::from(*row))
        && expected[2].as_u64() == Some(u64::from(*col));
    let repr = expected[4].as_str().unwrap();
    let same_value = match (expected[3].as_str().unwrap(), value) {
        ("number", LiteralValue::Number(number)) => repr.parse::<f64>().unwrap().to_bits() == number.to_bits(),
        ("int", LiteralValue::Int(number)) => repr.parse::<i64>().unwrap() == *number,
        _ => false,
    };
    same_cell && same_value
}

#[test]
fn run_goal_seeks_matches_python_run_solves() {
    let scenarios = fixture("goal_seek_expected.json");
    for scenario in scenarios.as_array().unwrap() {
        let name = scenario["scenario"].as_str().unwrap();
        let mut model = FakeModel::from_scenario(scenario);
        let outcome = run_goal_seeks(&mut model);
        let expected_notes: Vec<&str> =
            scenario["notes"].as_array().unwrap().iter().map(|note| note.as_str().unwrap()).collect();
        match scenario["outcome"].as_str().unwrap() {
            "ok" => {
                let run = outcome.unwrap_or_else(|failure| panic!("{name}: {failure:?}"));
                assert_eq!(run.notes, expected_notes, "{name}");
                let results = scenario["results"].as_object().unwrap();
                assert_eq!(run.results.len(), results.len(), "{name}");
                for (suffix, pair) in results {
                    let result = run.results.get(suffix).unwrap();
                    assert_eq!(result.target_value.to_bits(), py_float(&pair[0]).to_bits(), "{name}");
                    assert_eq!(result.result.to_bits(), py_float(&pair[1]).to_bits(), "{name}");
                }
                let records = scenario["records"].as_array().unwrap();
                assert_eq!(run.records.len(), records.len(), "{name}");
                for (actual, expected) in run.records.iter().zip(records) {
                    assert_eq!(actual["suffix"], expected["suffix"], "{name}");
                    assert_eq!(actual["status"], "converged", "{name}");
                    assert_eq!(actual["iterations"], expected["iterations"], "{name}");
                    assert_eq!(actual["root"].as_f64().unwrap().to_bits(), py_float(&expected["root"]).to_bits());
                    for key in ["cells", "change_cell", "target_cell", "target_value"] {
                        assert!(actual.contains_key(key), "{name}: record lacks {key}");
                    }
                }
            }
            "failed" => {
                let failure = outcome.err().unwrap_or_else(|| panic!("{name}: expected a failure"));
                assert_eq!(failure.notes, expected_notes, "{name}");
                let records = scenario["records"].as_array().unwrap();
                assert_eq!(failure.records.len(), records.len(), "{name}");
                for (actual, expected) in failure.records.iter().zip(records) {
                    for (key, value) in expected.as_object().unwrap() {
                        assert_eq!(actual.get(key), Some(value), "{name}: {key}");
                    }
                    assert_eq!(actual.contains_key("exception_type"), expected.get("exception_type").is_some());
                    assert!(actual.contains_key("diagnostic_observed_cells"), "{name}");
                }
            }
            other => panic!("unknown outcome {other}"),
        }
        let writes = scenario["writes"].as_array().unwrap();
        assert_eq!(model.writes.len(), writes.len(), "{name}: write count");
        for (actual, expected) in model.writes.iter().zip(writes) {
            assert!(write_matches(actual, expected), "{name}: write {actual:?} vs {expected}");
        }
    }
}

#[test]
fn run_goal_seeks_fails_on_collision_engine_error_and_default_get_formula() {
    struct Plain(FakeModel);
    impl SolveModel for Plain {
        fn get_value(&self, sheet: &str, row: u32, col: u32) -> Result<LiteralValue, ModelCallError> {
            self.0.get_value(sheet, row, col)
        }
        fn set_value(&mut self, sheet: &str, row: u32, col: u32, value: LiteralValue) -> Result<(), ModelCallError> {
            self.0.set_value(sheet, row, col, value)
        }
        fn evaluate_all(&mut self) -> Result<(), ModelCallError> {
            self.0.evaluate_all()
        }
        fn defined_ranges(&self) -> Result<Vec<DefinedRange>, ModelCallError> {
            self.0.defined_ranges()
        }
        // `get_formula` left at the contract default.
    }
    let scenarios = fixture("goal_seek_expected.json");
    let converges = &scenarios.as_array().unwrap()[0];
    assert_eq!(converges["scenario"], "converges");

    let failure = run_goal_seeks(&mut Plain(FakeModel::from_scenario(converges))).unwrap_err();
    assert_eq!(failure.notes, ["xsolve_failed:Goal:engine_error"]);
    assert_eq!(failure.records[0]["exception_type"], "NotImplementedError");
    assert!(failure.cause.is_some());

    // Two blocks whose suffixes differ only in case: the second collides.
    let mut model = FakeModel::from_scenario(converges);
    let mut twin = model.names[0].clone();
    twin.name = "Xsolve_GOAL".into();
    model.names.push(twin);
    let failure = run_goal_seeks(&mut model).unwrap_err();
    // Sorted by name: `Xsolve_GOAL` < `Xsolve_Goal`, so it runs first.
    assert_eq!(failure.notes, ["xsolve_ran:GOAL iterations=31", "xsolve_failed:Goal:output_collision"]);
    assert_eq!(failure.records.len(), 2);
    assert_eq!(failure.records[1]["reason"], "output_collision");
    assert!(failure.cause.is_none());

    // A sheet-scope name with the prefix is not a block.
    let mut model = FakeModel::from_scenario(converges);
    model.names[0].scope = NameScope::Sheet("S".into());
    let run = run_goal_seeks(&mut model).unwrap();
    assert!(run.notes.is_empty() && run.results.is_empty());
}
