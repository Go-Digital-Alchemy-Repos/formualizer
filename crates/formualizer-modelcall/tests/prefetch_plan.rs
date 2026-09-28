//! Sibling plan and sibling vectors (Lane B) against the Python behaviour
//! source: `fixtures/prefetch_plan.xlsx` is a synthetic parent written with
//! openpyxl, and `fixtures/prefetch_expected.json` is what the parity
//! repository's `sibling_plan_for_path` and `sibling_vectors` returned for it
//! (`fixtures/gen_python_expectations.py`).

use formualizer_common::LiteralValue;
use formualizer_modelcall::ModelSpec;
use formualizer_modelcall::prefetch::{
    PlanValue, PositionKind, call_arguments, literal, sibling_plan, sibling_plan_for_path, sibling_vectors,
};
use serde_json::{Value, json};
use std::path::PathBuf;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR")))
}

fn expected() -> Value {
    serde_json::from_str(&std::fs::read_to_string(fixture_path("prefetch_expected.json")).unwrap()).unwrap()
}

fn typed_literal(value: &Value) -> LiteralValue {
    match value["type"].as_str().unwrap() {
        "bool" => LiteralValue::Boolean(value["value"].as_bool().unwrap()),
        "int" => LiteralValue::Int(value["value"].as_i64().unwrap()),
        "float" => LiteralValue::Number(value["value"].as_str().unwrap().parse().unwrap()),
        "str" => LiteralValue::Text(value["value"].as_str().unwrap().to_owned()),
        other => panic!("not a literal: {other}"),
    }
}

fn typed_plan_value(value: &Value) -> PlanValue {
    match value["type"].as_str().unwrap() {
        "dynamic" => PlanValue::Dynamic,
        "absent" => PlanValue::Absent,
        _ => PlanValue::Constant(typed_literal(value)),
    }
}

/// Exact equality: type and bits (the Python `typed` spelling).
fn same_literal(left: &LiteralValue, right: &LiteralValue) -> bool {
    match (left, right) {
        (LiteralValue::Number(a), LiteralValue::Number(b)) => a.to_bits() == b.to_bits(),
        _ => left == right,
    }
}

#[test]
fn plan_matches_python_on_the_fixture_workbook() {
    let expected = expected();
    let plan = sibling_plan_for_path(&fixture_path("prefetch_plan.xlsx")).unwrap();
    assert_eq!(plan.counts(), expected["counts"]);
    let groups = expected["groups"].as_array().unwrap();
    assert_eq!(plan.groups.len(), groups.len());
    for (group, want) in plan.groups.iter().zip(groups) {
        assert_eq!(group.target, want["target"].as_str().unwrap());
        assert_eq!(group.output, want["output"].as_str().unwrap());
        let cells: Vec<&str> = want["cells"].as_array().unwrap().iter().map(|cell| cell.as_str().unwrap()).collect();
        assert_eq!(group.cells, cells);
        let positions = want["positions"].as_array().unwrap();
        assert_eq!(group.positions.len(), positions.len(), "{}", group.target);
        for (position, want) in group.positions.iter().zip(positions) {
            assert_eq!(position.name, want[0].as_str().unwrap());
            assert_eq!(position.kind.as_str(), want[1].as_str().unwrap());
            let values: Vec<PlanValue> = want[2].as_array().unwrap().iter().map(typed_plan_value).collect();
            assert_eq!(position.values.len(), values.len());
            for (actual, wanted) in position.values.iter().zip(&values) {
                let same = match (actual, wanted) {
                    (PlanValue::Constant(a), PlanValue::Constant(b)) => same_literal(a, b),
                    _ => actual == wanted,
                };
                assert!(same, "{}: {actual:?} vs {wanted:?}", position.name);
            }
        }
    }
}

#[test]
fn sibling_vectors_match_python() {
    let expected = expected();
    let plan = sibling_plan_for_path(&fixture_path("prefetch_plan.xlsx")).unwrap();
    let observations = expected["vectors"].as_array().unwrap();
    assert_eq!(observations.len(), 11);
    for observation in observations {
        let inputs: Vec<(String, LiteralValue)> = observation["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pair| (pair[0].as_str().unwrap().to_owned(), typed_literal(&pair[1])))
            .collect();
        let vectors = sibling_vectors(
            &plan,
            observation["target"].as_str().unwrap(),
            observation["output"].as_str().unwrap(),
            &inputs,
        );
        let wanted = observation["vectors"].as_array().unwrap();
        assert_eq!(vectors.len(), wanted.len(), "{observation}");
        for (vector, want) in vectors.iter().zip(wanted) {
            let want = want.as_array().unwrap();
            assert_eq!(vector.len(), want.len(), "{observation}");
            for ((name, value), pair) in vector.iter().zip(want) {
                assert_eq!(name, pair[0].as_str().unwrap(), "{observation}");
                assert!(same_literal(value, &typed_literal(&pair[1])), "{observation}: {name} {value:?} vs {}", pair[1]);
            }
        }
    }
}

#[test]
fn sibling_plan_for_spec_is_cached_and_none_without_groups() {
    let spec = |path: &str, sha: &str| -> ModelSpec {
        serde_json::from_value(json!({
            "identity": "parent", "workbook_path": path, "workbook_sha256": sha,
            "manifest": {}, "inputs": {}, "outputs": {}
        }))
        .unwrap()
    };
    let path = fixture_path("prefetch_plan.xlsx");
    let plan = sibling_plan(&spec(path.to_str().unwrap(), "lane-b-fixture")).unwrap().unwrap();
    assert_eq!(plan.groups.len(), 4);
    // Cached by sha256: a later spec with the same sha never reads its path.
    let cached = sibling_plan(&spec("/nonexistent.xlsx", "lane-b-fixture")).unwrap().unwrap();
    assert_eq!(cached, plan);
    assert!(sibling_plan(&spec("/nonexistent.xlsx", "lane-b-missing")).is_err());
}

#[test]
fn call_arguments_reads_every_alias_and_skips_unterminated_calls() {
    let calls = call_arguments(r#"=MDL.CALLMODEL("a/b", 0, "Out", "N", -1.5) + _xlfn.CS.SPARK.XCALL("c", A1:B2, "x")"#);
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].len(), 5);
    assert_eq!(literal(&calls[0][0]), Some(LiteralValue::Text("a/b".into())));
    assert_eq!(literal(&calls[0][4]), Some(LiteralValue::Number(-1.5)));
    assert_eq!(literal(&calls[1][1]), None);
    // Nested parentheses and array constants stay inside one argument.
    let calls = call_arguments(r#"=CS.SPARK.XCALL("t", {1,2;3,4}, "o", "K", SUM(A1, (B2)))"#);
    assert_eq!(calls[0].len(), 5);
    assert!(call_arguments(r#"=SUM(1, 2)"#).is_empty());
}

#[test]
fn literals_follow_the_python_rules() {
    let one = |formula: &str| {
        let calls = call_arguments(&format!("=MDL.CALLMODEL({formula})"));
        literal(&calls[0][0])
    };
    assert_eq!(one("12"), Some(LiteralValue::Int(12)));
    assert_eq!(one("-0"), Some(LiteralValue::Int(0)));
    assert_eq!(one("1.5E+3"), Some(LiteralValue::Number(1500.0)));
    assert_eq!(one("-0.0").map(|value| matches!(value, LiteralValue::Number(n) if n.is_sign_negative())), Some(true));
    assert_eq!(one(r#""a""b""#), Some(LiteralValue::Text("a\"b".into())));
    assert_eq!(one("TRUE"), Some(LiteralValue::Boolean(true)));
    assert_eq!(one("A1"), None);
    assert_eq!(one("1+1"), None);
    assert_eq!(PositionKind::Partial.as_str(), "partial");
}
