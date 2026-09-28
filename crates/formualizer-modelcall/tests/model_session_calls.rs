//! ModelSession over in-memory parent/child/grandchild workbooks: routing,
//! memo, error mapping, admission and the risk-3 reentrancy check (a
//! deadline cancel across three nested evaluations must not deadlock).

use formualizer_common::{ExcelError, ExcelErrorKind, LiteralValue};
use formualizer_modelcall::context::{CalculationContextSpec, CalculationFlags};
use formualizer_modelcall::event::CallStatus;
use formualizer_modelcall::receipt::{PortValue, TimingValue};
use formualizer_modelcall::session::{ModelSession, WorkbookSource, runtime_workbook_config};
use formualizer_modelcall::{CalculationContext, ModelCallError, ModelPackage, ModelSpec};
use formualizer_workbook::{CustomFnOptions, Workbook};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const SHEET: &str = "Calc";
const CHAIN: u32 = 12;
const SLEEP: Duration = Duration::from_millis(200);

/// One synthetic model: cells, one scalar input at A1 and one output.
#[derive(Clone)]
struct Model {
    identity: &'static str,
    cells: Vec<(u32, u32, &'static str)>,
    output: (u32, u32, u32, u32),
    slow: bool,
}

fn a1(row: u32, col: u32) -> String {
    format!("'{SHEET}'!{}{row}", (b'A' + u8::try_from(col - 1).unwrap()) as char)
}

fn location(row: u32, col: u32, end_row: u32, end_col: u32, name: &str, key: &str, port_id: &str, shape: &str) -> Value {
    json!({"sheet": SHEET, "start_row": row, "start_col": col, "end_row": end_row, "end_col": end_col,
           "name": name, "key": key, "port_id": port_id, "shape": shape, "date_system": 1900, "date_fields": []})
}

fn spec_json(model: &Model, descriptor: Value) -> Value {
    let (row, col, end_row, end_col) = model.output;
    let output_shape = if row == end_row && col == end_col { "scalar" } else { "range" };
    let output_a1 = if output_shape == "scalar" {
        a1(row, col)
    } else {
        format!("{}:{}", a1(row, col), a1(end_row, end_col).rsplit('!').next().unwrap())
    };
    let output_schema = if output_shape == "scalar" { json!({"type": "any"}) } else { json!({"kind": "range", "cell_type": "any"}) };
    let mut output_location = location(row, col, end_row, end_col, "XOUTPUT_result", "result", "output_result", output_shape);
    if output_shape == "range" {
        output_location["headers"] = json!(["Label", "Value"]);
    }
    json!({
        "identity": model.identity,
        "workbook_path": format!("memory://{}", model.identity),
        "workbook_sha256": format!("sha-{}", model.identity),
        "manifest": {
            "spec": "fio", "spec_version": "0.3.0",
            "manifest": {"id": format!("model-{}", model.identity), "name": model.identity,
                         "workbook": {"uri": "memory://model.xlsx", "locale": "en-US", "date_system": 1900}},
            "ports": [
                {"id": "amount", "dir": "in", "shape": "scalar", "location": {"a1": a1(1, 1)},
                 "schema": {"type": "any"}, "constraints": {"nullable": true}},
                {"id": "output_result", "dir": "out", "shape": output_shape, "location": {"a1": output_a1},
                 "schema": output_schema, "constraints": {"nullable": true}}
            ]
        },
        "inputs": {"amount": location(1, 1, 1, 1, "XINPUT_amount", "amount", "amount", "scalar")},
        "outputs": {"result": output_location},
        "defaults": {"amount": 1},
        "goal_seek": [],
        "descriptor": descriptor
    })
}

struct MemorySource {
    models: HashMap<String, Model>,
    load_delay: HashMap<String, Duration>,
}

impl WorkbookSource for MemorySource {
    fn load(&self, spec: &ModelSpec, context: &CalculationContext) -> Result<Workbook, ModelCallError> {
        if let Some(delay) = self.load_delay.get(&spec.identity) {
            std::thread::sleep(*delay);
        }
        let model = self
            .models
            .get(&spec.identity)
            .ok_or_else(|| ModelCallError::infrastructure("FileNotFoundError", format!("no model {}", spec.identity)))?;
        let mut workbook = Workbook::new_with_config(runtime_workbook_config(context.random_seed));
        workbook.add_sheet(SHEET).unwrap();
        if model.slow {
            let slow = |args: &[LiteralValue]| -> Result<LiteralValue, ExcelError> {
                std::thread::sleep(SLEEP);
                match args.first() {
                    Some(LiteralValue::Number(value)) => Ok(LiteralValue::Number(value + 1.0)),
                    Some(LiteralValue::Int(value)) => Ok(LiteralValue::Number(*value as f64 + 1.0)),
                    _ => Ok(LiteralValue::Number(1.0)),
                }
            };
            let options = CustomFnOptions { min_args: 1, max_args: Some(1), volatile: true, thread_safe: false, deterministic: false, allow_override_builtin: false };
            workbook.register_custom_function("SLOWFN", options, Arc::new(slow)).unwrap();
        }
        workbook.set_value(SHEET, 1, 1, LiteralValue::Number(1.0)).unwrap();
        for (row, col, cell) in &model.cells {
            if let Some(formula) = cell.strip_prefix('=') {
                workbook.set_formula(SHEET, *row, *col, &format!("={formula}")).unwrap();
            } else {
                workbook.set_value(SHEET, *row, *col, LiteralValue::Text((*cell).to_owned())).unwrap();
            }
        }
        Ok(workbook)
    }
}

fn package(parent: &Model, children: &[(&str, &Model)], descriptor: Value) -> ModelPackage {
    let mut children_json = serde_json::Map::new();
    let mut routes = serde_json::Map::new();
    for (route, model) in children {
        children_json.insert(model.identity.to_owned(), spec_json(model, json!({})));
        routes.insert((*route).to_owned(), Value::String(model.identity.to_owned()));
    }
    serde_json::from_value(json!({
        "package_id": "synthetic",
        "parent": spec_json(parent, descriptor),
        "children": children_json,
        "child_routes": routes,
    }))
    .unwrap()
}

fn context(deadline_seconds: Option<f64>, max_depth: u32) -> CalculationContext {
    CalculationContext::from_spec(&CalculationContextSpec {
        now: "2026-09-28T00:00:00+00:00".into(),
        operation: Default::default(),
        random_seed: 147,
        deadline_seconds,
        max_depth,
        flags: CalculationFlags::default(),
    })
    .unwrap()
}

fn session(package: ModelPackage, context: CalculationContext, models: &[&Model], delays: &[(&str, Duration)]) -> ModelSession {
    let source = MemorySource {
        models: models.iter().map(|model| (model.identity.to_owned(), (*model).clone())).collect(),
        load_delay: delays.iter().map(|(name, delay)| ((*name).to_owned(), *delay)).collect(),
    };
    ModelSession::new(Arc::new(package), context).with_workbook_source(Arc::new(source))
}

fn number(value: &PortValue) -> f64 {
    match value {
        PortValue::Scalar(LiteralValue::Number(number)) => *number,
        PortValue::Scalar(LiteralValue::Int(number)) => *number as f64,
        other => panic!("not a number: {other:?}"),
    }
}

fn doubler() -> Model {
    Model { identity: "child", cells: vec![(2, 1, "=A1*2")], output: (2, 1, 2, 1), slow: false }
}

fn inputs(amount: i64) -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    map.insert("amount".into(), json!(amount));
    map
}

#[test]
fn parent_calls_child_and_the_repeat_is_memoized() {
    let parent = Model {
        identity: "parent",
        cells: vec![(
            2,
            1,
            "=MDL.CALLMODEL(\"rates/child\",0,\"result\",\"amount\",A1)+_xldudf_CS_SPARK_XCALL(\"rates/child\",0,\"result\",\"amount\",A1)",
        )],
        output: (2, 1, 2, 1),
        slow: false,
    };
    let child = doubler();
    let package = package(&parent, &[("rates/child", &child)], json!({}));
    let mut session = session(package, context(None, 32), &[&parent, &child], &[]);
    let result = session.calculate(&inputs(3)).expect("calculates");
    assert_eq!(number(result.outputs.get("result").unwrap()), 12.0);
    let statuses: Vec<_> = result.invocations.iter().map(|event| event.status).collect();
    assert_eq!(statuses, [CallStatus::Completed, CallStatus::Memoized]);
    assert_eq!(result.invocations[1].memo_of, Some(0));
    assert_eq!(result.invocations[0].child.as_deref(), Some("child:sha-child"));
    assert_eq!(result.invocations[0].stack, ["parent:sha-parent"]);
    let memo = result.call_memo.expect("memo report");
    assert_eq!((memo.hits, memo.misses, memo.stores), (1, 1, 1));
    let keys: Vec<_> = result.timings.0.keys().collect();
    assert_eq!(
        keys,
        [
            "load_seconds", "evaluation_seconds", "solver_seconds", "child_seconds", "preparation_seconds",
            "admission_seconds", "capture_seconds", "inspection_seconds", "total_seconds"
        ]
    );
    assert!(result.faults.is_empty() && result.session_reuse.is_empty() && result.compiled.is_empty());
    assert!(matches!(result.timings.get("child_seconds"), Some(TimingValue::Seconds(seconds)) if seconds > 0.0));
}

#[test]
fn cycle_depth_and_unknown_routes_return_ref_errors() {
    let looping = Model {
        identity: "child",
        cells: vec![(2, 1, "=MDL.CALLMODEL(\"rates/child\",0,\"result\",\"amount\",A1)")],
        output: (2, 1, 2, 1),
        slow: false,
    };
    let parent = Model {
        identity: "parent",
        cells: vec![
            (2, 1, "=MDL.CALLMODEL(\"rates/child\",0,\"result\",\"amount\",A1)"),
            (3, 1, "=CS.SPARK.XCALL(\"nowhere\",0,\"result\")"),
        ],
        output: (2, 1, 2, 1),
        slow: false,
    };
    let package = package(&parent, &[("rates/child", &looping)], json!({}));
    let mut session = session(package, context(None, 32), &[&parent, &looping], &[]);
    let result = session.calculate(&inputs(3)).expect("routing errors are cell values, not failures");
    let errors: Vec<_> = result
        .invocations
        .iter()
        .map(|event| (event.status, event.error.clone().unwrap_or_default()))
        .collect();
    assert!(errors.contains(&(CallStatus::RoutingError, "active workbook child cycle rejected".into())), "{errors:?}");
    assert!(errors.contains(&(CallStatus::RoutingError, "child target is not in pinned package routes".into())), "{errors:?}");
    let refused = result.invocations.iter().find(|event| event.status == CallStatus::RoutingError).unwrap();
    assert_eq!(refused.returned_error.as_ref().unwrap().kind, ExcelErrorKind::Ref);

    let child = doubler();
    let parent = Model {
        identity: "parent",
        cells: vec![(2, 1, "=MDL.CALLMODEL(\"rates/child\",0,\"result\",\"amount\",A1)")],
        output: (2, 1, 2, 1),
        slow: false,
    };
    let package = package_for_depth(&parent, &child);
    let mut session = session(package, context(None, 1), &[&parent, &child], &[]);
    let result = session.calculate(&inputs(3)).unwrap();
    assert_eq!(result.invocations[0].error.as_deref(), Some("maximum child depth exceeded"));
}

fn package_for_depth(parent: &Model, child: &Model) -> ModelPackage {
    package(parent, &[("rates/child", child)], json!({}))
}

#[test]
fn a_child_fault_fails_the_run_with_calc_and_evidence() {
    let parent = Model {
        identity: "parent",
        cells: vec![(2, 1, "=MDL.CALLMODEL(\"rates/child\",0,\"result\",\"amount\",A1)")],
        output: (2, 1, 2, 1),
        slow: false,
    };
    let child = doubler();
    let package = package(&parent, &[("rates/child", &child)], json!({}));
    // The child is not in the source: every load is an infrastructure fault.
    let mut session = session(package, context(None, 32), &[&parent], &[]);
    let error = session.calculate(&inputs(3)).unwrap_err();
    assert_eq!(
        error,
        ModelCallError::infrastructure(
            "CallbackInfrastructureError",
            "child callback infrastructure fault: FileNotFoundError: no model child"
        )
    );
    let evidence = session.partial_result();
    assert_eq!(evidence.invocations[0].status, CallStatus::InfrastructureError);
    assert_eq!(evidence.invocations[0].returned_error.as_ref().unwrap().kind, ExcelErrorKind::Calc);
    assert_eq!(evidence.faults.len(), 1);
    assert!(evidence.diagnostics.last().unwrap().starts_with("CallbackInfrastructureError: "));
    assert!(evidence.timings.get("total_seconds").is_none());
}

/// Risk 3: parent -> child -> grandchild, the grandchild slow enough that
/// the deadline passes while all three evaluations are live. The watchdog
/// cancels every workbook; each level unwinds with the deadline error and
/// nothing waits on a lock another level holds.
#[test]
fn deadline_cancel_through_three_nested_evaluations_does_not_deadlock() {
    let chain: Vec<(u32, u32, &'static str)> = (2..CHAIN + 2).map(|row| (row, 1, slow_formula(row))).collect();
    let grandchild = Model { identity: "grandchild", cells: chain, output: (CHAIN + 1, 1, CHAIN + 1, 1), slow: true };
    let child = Model {
        identity: "child",
        cells: vec![(2, 1, "=MDL.CALLMODEL(\"rates/grandchild\",0,\"result\",\"amount\",A1)")],
        output: (2, 1, 2, 1),
        slow: false,
    };
    let parent = Model {
        identity: "parent",
        cells: vec![(2, 1, "=MDL.CALLMODEL(\"rates/child\",0,\"result\",\"amount\",A1)")],
        output: (2, 1, 2, 1),
        slow: false,
    };
    let package = package(&parent, &[("rates/child", &child), ("rates/grandchild", &grandchild)], json!({}));
    let mut session = session(package, context(Some(0.4), 32), &[&parent, &child, &grandchild], &[]);
    let (done, wait) = mpsc::channel();
    let started = Instant::now();
    std::thread::spawn(move || {
        let outcome = session.calculate(&inputs(1));
        let evidence = session.partial_result();
        let _ = done.send((outcome, evidence));
    });
    let (outcome, evidence) = wait.recv_timeout(Duration::from_secs(20)).expect("no deadlock: the run returned");
    let elapsed = started.elapsed();
    assert_eq!(outcome.unwrap_err(), ModelCallError::deadline());
    assert!(elapsed < SLEEP * CHAIN / 2, "stopped at a layer boundary, took {elapsed:?}");
    let statuses: Vec<_> = evidence.invocations.iter().map(|event| event.status).collect();
    assert_eq!(statuses, [CallStatus::InfrastructureError, CallStatus::InfrastructureError]);
    assert_eq!(evidence.invocations[1].stack, ["parent:sha-parent", "child:sha-child"]);
    assert_eq!(
        evidence.invocations[1].error.as_deref(),
        Some("TimeoutError: calculation deadline exceeded")
    );
    assert_eq!(evidence.diagnostics.last().map(String::as_str), Some("TimeoutError: calculation deadline exceeded"));
}

/// A deadline that passes while a nested child is still loading: the
/// grandchild load refuses at its deadline check, no evaluation hangs.
#[test]
fn deadline_during_a_nested_load_unwinds() {
    let grandchild = doubler_named("grandchild");
    let child = Model {
        identity: "child",
        cells: vec![(2, 1, "=MDL.CALLMODEL(\"rates/grandchild\",0,\"result\",\"amount\",A1)")],
        output: (2, 1, 2, 1),
        slow: false,
    };
    let parent = Model {
        identity: "parent",
        cells: vec![(2, 1, "=MDL.CALLMODEL(\"rates/child\",0,\"result\",\"amount\",A1)")],
        output: (2, 1, 2, 1),
        slow: false,
    };
    let package = package(&parent, &[("rates/child", &child), ("rates/grandchild", &grandchild)], json!({}));
    let mut session = session(
        package,
        context(Some(0.3), 32),
        &[&parent, &child, &grandchild],
        &[("child", Duration::from_millis(500))],
    );
    let (done, wait) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(session.calculate(&inputs(1)));
    });
    let outcome = wait.recv_timeout(Duration::from_secs(20)).expect("no deadlock");
    assert_eq!(outcome.unwrap_err(), ModelCallError::deadline());
}

fn doubler_named(identity: &'static str) -> Model {
    Model { identity, cells: vec![(2, 1, "=A1*2")], output: (2, 1, 2, 1), slow: false }
}

fn slow_formula(row: u32) -> &'static str {
    Box::leak(format!("=SLOWFN(A{})", row - 1).into_boxed_str())
}

#[test]
fn admission_rejects_undeclared_inputs_and_ignore_policy_records_them() {
    let child = doubler();
    let parent = doubler_named("parent");
    let reject = package(&parent, &[("rates/child", &child)], json!({}));
    let mut session_reject = session(reject, context(None, 32), &[&parent, &child], &[]);
    let mut request = inputs(3);
    request.insert("bogus".into(), json!(1));
    let error = session_reject.calculate(&request).unwrap_err();
    assert_eq!(error, ModelCallError::infrastructure("ValueError", "Undeclared input 'bogus'"));

    let ignore = package(&parent, &[("rates/child", &child)], json!({"unknown_input_policy": "ignore"}));
    let mut session_ignore = session(ignore, context(None, 32), &[&parent, &child], &[]);
    let result = session_ignore.calculate(&request).unwrap();
    assert_eq!(number(result.outputs.get("result").unwrap()), 6.0);
    assert_eq!(result.diagnostics, ["ignored_input:bogus"]);
    assert_eq!(number(result.effective_inputs.get("amount").unwrap()), 3.0);
}

#[test]
fn ranged_output_projects_a_client_table() {
    let parent = Model {
        identity: "parent",
        cells: vec![(3, 1, "Label"), (3, 2, "Value"), (4, 1, "Double"), (4, 2, "=A1*2"), (5, 1, "Triple"), (5, 2, "=A1*3")],
        output: (3, 1, 6, 2),
        slow: false,
    };
    let package = package(&parent, &[], json!({}));
    let mut session = session(package, context(None, 32), &[&parent], &[]);
    let result = session.calculate(&inputs(2)).unwrap();
    let PortValue::Table(rows) = result.outputs.get("result").unwrap() else { panic!("table") };
    // The trailing all-blank row 6 is trimmed from the client wire.
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].keys().collect::<Vec<_>>(), ["Label", "Value"]);
    assert_eq!(rows[1].get("Label"), Some(&LiteralValue::Text("Triple".into())));
    let PortValue::Range(typed) = result.typed_outputs.get("result").unwrap() else { panic!("typed range") };
    assert_eq!(typed.len(), 4);
}
