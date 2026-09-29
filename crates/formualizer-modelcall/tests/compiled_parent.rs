//! Architecture B item (c): the compiled parent inside `ModelSession`.
//!
//! A stub `CompiledParent` computes the same parent the engine evaluates
//! (`A2 = MDL.CALLMODEL(child, amount) + MDL.CALLMODEL(child, amount)`, the
//! repeat a memo hit) through the session's router, so every test compares
//! the compiled arm with the engine arm of the same package: outputs, typed
//! outputs, effective inputs (a date input included), invocations with stack
//! `[parent]`, the memo, and the `compiled` receipt (`parent`,
//! `parent_xcalls`, `parent_loaded`). Declines and static refusals fall back
//! to the engine parent; on a report run `report_prepare` runs only on that
//! engine parent (F1) and the capture receives the compiled cells otherwise.

use formualizer_common::LiteralValue;
use formualizer_modelcall::context::{CalculationContextSpec, CalculationFlags};
use formualizer_modelcall::evaluator::{
    CellAddress, CompiledCells, CompiledParent, CompiledRun, CompiledRunStats, CompiledXcall, ParentAttempt,
    SharedCompiledCells,
};
use formualizer_modelcall::event::CallStatus;
use formualizer_modelcall::receipt::{CalculationResult, PortValue};
use formualizer_modelcall::session::{ModelSession, ReportHook, SharedWorkbook, WorkbookSource, runtime_workbook_config};
use formualizer_modelcall::spec::OrderedMap;
use formualizer_modelcall::{CalculationContext, ModelCallError, ModelPackage, ModelSpec, Operation};
use formualizer_workbook::Workbook;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const SHEET: &str = "Calc";
const PARENT: &str = "parent:sha-parent";
const CALL: &str = "=MDL.CALLMODEL(\"rates/child\",0,\"result\",\"amount\",A1)+MDL.CALLMODEL(\"rates/child\",0,\"result\",\"amount\",A1)";

fn location(row: u32, key: &str, port_id: &str) -> Value {
    json!({"sheet": SHEET, "start_row": row, "start_col": 1, "end_row": row, "end_col": 1,
           "name": format!("XINPUT_{key}"), "key": key, "port_id": port_id, "shape": "scalar",
           "date_system": 1900, "date_fields": []})
}

fn spec_json(identity: &str, with_start: bool, extra: &Value) -> Value {
    let mut ports = vec![json!({"id": "amount", "dir": "in", "shape": "scalar", "location": {"a1": format!("'{SHEET}'!A1")},
                                "schema": {"type": "any"}, "constraints": {"nullable": true}})];
    let mut inputs = Map::new();
    inputs.insert("amount".into(), location(1, "amount", "amount"));
    let mut defaults = Map::new();
    defaults.insert("amount".into(), json!(1));
    if with_start {
        ports.push(json!({"id": "start", "dir": "in", "shape": "scalar", "location": {"a1": format!("'{SHEET}'!A3")},
                          "schema": {"type": "date"}, "constraints": {"nullable": true}}));
        inputs.insert("start".into(), location(3, "start", "start"));
        defaults.insert("start".into(), json!({"$date": "2024-01-01"}));
    }
    ports.push(json!({"id": "output_result", "dir": "out", "shape": "scalar",
                      "location": {"a1": format!("'{SHEET}'!A2")},
                      "schema": {"type": "any"}, "constraints": {"nullable": true}}));
    let mut output = location(2, "result", "output_result");
    output["name"] = json!("XOUTPUT_result");
    let mut spec = json!({
        "identity": identity,
        "workbook_path": format!("memory://{identity}"),
        "workbook_sha256": format!("sha-{identity}"),
        "manifest": {
            "spec": "fio", "spec_version": "0.3.0",
            "manifest": {"id": format!("model-{identity}"), "name": identity,
                         "workbook": {"uri": "memory://model.xlsx", "locale": "en-US", "date_system": 1900}},
            "ports": ports
        },
        "inputs": inputs,
        "outputs": {"result": output},
        "defaults": defaults,
        "goal_seek": [],
        "descriptor": {}
    });
    if let (Some(target), Some(extra)) = (spec.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            target.insert(key.clone(), value.clone());
        }
    }
    spec
}

fn package(parent_extra: &Value) -> Arc<ModelPackage> {
    Arc::new(
        serde_json::from_value(json!({
            "package_id": "synthetic",
            "parent": spec_json("parent", true, parent_extra),
            "children": {"child": spec_json("child", false, &json!({}))},
            "child_routes": {"rates/child": "child"},
        }))
        .unwrap(),
    )
}

/// In-memory workbooks; counts loads per identity.
#[derive(Default)]
struct Source {
    loads: Mutex<HashMap<String, usize>>,
}

impl Source {
    fn loads(&self, identity: &str) -> usize {
        self.loads.lock().unwrap().get(identity).copied().unwrap_or(0)
    }
}

impl WorkbookSource for Source {
    fn load(&self, spec: &ModelSpec, context: &CalculationContext) -> Result<Workbook, ModelCallError> {
        *self.loads.lock().unwrap().entry(spec.identity.clone()).or_insert(0) += 1;
        let mut workbook = Workbook::new_with_config(runtime_workbook_config(context.random_seed));
        workbook.add_sheet(SHEET).unwrap();
        workbook.set_value(SHEET, 1, 1, LiteralValue::Number(1.0)).unwrap();
        if spec.identity == "parent" {
            workbook.set_formula(SHEET, 2, 1, CALL).unwrap();
        } else {
            workbook.set_formula(SHEET, 2, 1, "=A1*2").unwrap();
        }
        Ok(workbook)
    }
}

fn context(operation: Operation) -> CalculationContext {
    CalculationContext::from_spec(&CalculationContextSpec {
        now: "2026-09-29T00:00:00+00:00".into(),
        operation,
        random_seed: 147,
        deadline_seconds: None,
        max_depth: 32,
        flags: CalculationFlags { compiled: true, ..CalculationFlags::default() },
    })
    .unwrap()
}

fn inputs() -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("amount".into(), json!(3));
    map.insert("start".into(), json!("2024-03-01"));
    map
}

/// The compiled store of the stub: `(sheet, row, col) -> value`.
struct StubCells(HashMap<CellAddress, LiteralValue>);

impl CompiledCells for StubCells {
    fn read_cells(&self, cells: &[CellAddress]) -> Result<Vec<LiteralValue>, ModelCallError> {
        Ok(cells.iter().map(|cell| self.0.get(cell).cloned().unwrap_or(LiteralValue::Empty)).collect())
    }
}

/// The parent workbook as a compiled module would run it.
struct StubParent {
    decline: Option<&'static str>,
    serves: bool,
    fail: bool,
    runs: AtomicUsize,
}

impl StubParent {
    fn new() -> Self {
        Self { decline: None, serves: true, fail: false, runs: AtomicUsize::new(0) }
    }
}

fn number(value: &LiteralValue) -> f64 {
    match value {
        LiteralValue::Number(number) => *number,
        #[expect(clippy::cast_precision_loss, reason = "test values are small")]
        LiteralValue::Int(number) => *number as f64,
        other => panic!("not a number: {other:?}"),
    }
}

impl CompiledParent for StubParent {
    fn serves(&self, workbook_sha256: &str) -> bool {
        self.serves && workbook_sha256 == "sha-parent"
    }

    fn run(
        &self,
        _spec: &ModelSpec,
        inputs: &[(String, LiteralValue)],
        _context: &CalculationContext,
        xcall: &mut dyn CompiledXcall,
    ) -> Result<ParentAttempt, ModelCallError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let input = |name: &str| inputs.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone());
        let amount = input("amount").unwrap();
        let start = input("start").unwrap();
        assert_eq!(amount, LiteralValue::Number(3.0), "ints reach the module as numbers");
        assert_eq!(start, LiteralValue::Number(45352.0), "dates reach the module as 1900 serials");
        let tail = [LiteralValue::Text("amount".into()), amount.clone()];
        let target = LiteralValue::Text("rates/child".into());
        let (block, output) = (LiteralValue::Number(0.0), LiteralValue::Text("result".into()));
        let first = xcall.call(&target, &block, &output, &tail)?;
        let second = xcall.call(&target, &block, &output, &tail)?;
        if self.fail {
            return Err(ModelCallError::infrastructure("RuntimeError", "module panicked"));
        }
        if let Some(reason) = self.decline {
            return Ok(ParentAttempt::Declined { route: Value::String(format!("fallback:{reason}")) });
        }
        let result = LiteralValue::Number(number(&first[0][0]) + number(&second[0][0]));
        let mut cells = HashMap::new();
        cells.insert((SHEET.to_owned(), 1, 1), amount);
        cells.insert((SHEET.to_owned(), 2, 1), result.clone());
        cells.insert((SHEET.to_owned(), 3, 1), start);
        Ok(ParentAttempt::Compiled(CompiledRun {
            outputs: vec![("result".into(), vec![vec![result]])],
            route: Value::String("compiled".into()),
            stats: CompiledRunStats { xcalls: 2, ..CompiledRunStats::default() },
            cells: Box::new(StubCells(cells)),
        }))
    }

    fn report(&self) -> Map<String, Value> {
        Map::new()
    }
}

#[derive(Default)]
struct RecordingHook {
    calls: Mutex<Vec<String>>,
}

impl ReportHook for RecordingHook {
    fn prepare(&self, _workbook: &SharedWorkbook) -> Result<(), ModelCallError> {
        self.calls.lock().unwrap().push("prepare:workbook".into());
        Ok(())
    }

    fn capture(&self, workbook: &SharedWorkbook, _outputs: &OrderedMap<PortValue>) -> Result<Vec<String>, ModelCallError> {
        let value = workbook.read().unwrap().get_value(SHEET, 2, 1);
        self.calls.lock().unwrap().push(format!("capture:workbook:{value:?}"));
        Ok(vec!["captured".into()])
    }

    fn capture_compiled(
        &self,
        cells: &SharedCompiledCells,
        _outputs: &OrderedMap<PortValue>,
    ) -> Result<Vec<String>, ModelCallError> {
        let value = cells.lock().unwrap().read_cells(&[(SHEET.to_owned(), 2, 1)])?;
        self.calls.lock().unwrap().push(format!("capture:compiled:{:?}", value[0]));
        Ok(vec!["captured".into()])
    }

    fn captures_compiled(&self) -> bool {
        true
    }
}

fn session(package: &Arc<ModelPackage>, operation: Operation, source: &Arc<Source>) -> ModelSession {
    ModelSession::new(package.clone(), context(operation)).with_workbook_source(source.clone())
}

fn engine_arm(operation: Operation) -> CalculationResult {
    let source = Arc::new(Source::default());
    session(&package(&json!({})), operation, &source).calculate(&inputs()).expect("engine arm")
}

fn assert_same_values(compiled: &CalculationResult, engine: &CalculationResult) {
    assert_eq!(compiled.outputs, engine.outputs);
    assert_eq!(compiled.typed_outputs, engine.typed_outputs);
    assert_eq!(compiled.effective_inputs, engine.effective_inputs);
    let calls = |result: &CalculationResult| {
        result
            .invocations
            .iter()
            .map(|event| (event.status, event.stack.clone(), event.matrix.clone(), event.inputs.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(calls(compiled), calls(engine));
}

#[test]
fn compiled_parent_serves_the_request_through_the_router() {
    let package = package(&json!({}));
    let source = Arc::new(Source::default());
    let stub = Arc::new(StubParent::new());
    let mut compiled = session(&package, Operation::Client, &source).with_compiled_parent(stub.clone());
    let result = compiled.calculate(&inputs()).expect("compiled arm");
    let engine = engine_arm(Operation::Client);

    assert_same_values(&result, &engine);
    assert_eq!(result.outputs.get("result"), Some(&PortValue::Scalar(LiteralValue::Number(12.0))));
    let start = chrono::NaiveDate::from_ymd_opt(2024, 3, 1).unwrap();
    assert_eq!(result.effective_inputs.get("start"), Some(&PortValue::Scalar(LiteralValue::Date(start))));
    assert_eq!(result.effective_inputs.get("amount"), Some(&PortValue::Scalar(LiteralValue::Number(3.0))));
    let statuses: Vec<_> = result.invocations.iter().map(|event| event.status).collect();
    assert_eq!(statuses, [CallStatus::Completed, CallStatus::Memoized]);
    assert!(result.invocations.iter().all(|event| event.stack == [PARENT]));
    assert_eq!(result.invocations[1].memo_of, Some(0));
    let memo = result.call_memo.as_ref().expect("memo report");
    assert_eq!((memo.hits, memo.misses, memo.stores), (1, 1, 1));

    assert_eq!(result.compiled.get("parent"), Some(&json!("compiled")));
    assert_eq!(result.compiled.get("parent_xcalls"), Some(&json!(2)));
    assert_eq!(result.compiled.get("parent_loaded"), Some(&json!(false)));
    assert_eq!(source.loads("parent"), 0, "no engine parent loaded");
    assert_eq!(source.loads("child"), 1);
    assert_eq!(stub.runs.load(Ordering::SeqCst), 1);
    assert!(compiled.workbook().is_none());
    assert!(compiled.compiled_cells().is_some());
    assert!(result.timings.0.get("compiled_seconds").is_some());
}

#[test]
fn a_declined_attempt_falls_back_to_a_fresh_engine_parent() {
    let package = package(&json!({}));
    let source = Arc::new(Source::default());
    let stub = Arc::new(StubParent { decline: Some("forced"), ..StubParent::new() });
    let mut session = session(&package, Operation::Client, &source).with_compiled_parent(stub);
    let result = session.calculate(&inputs()).expect("fallback arm");
    assert_same_values(&result, &engine_arm(Operation::Client));
    assert_eq!(result.compiled.get("parent"), Some(&json!("fallback:forced")));
    assert_eq!(result.compiled.get("parent_loaded"), Some(&json!(true)));
    assert!(result.compiled.get("parent_xcalls").is_none());
    // The discarded attempt's calls are not in the receipt: two, from the engine parent.
    assert_eq!(result.invocations.len(), 2);
    assert_eq!(source.loads("parent"), 1);
    assert!(session.workbook().is_some());
    assert!(session.compiled_cells().is_none());
}

#[test]
fn report_run_prepares_only_an_engine_parent_and_captures_compiled_cells() {
    let package = package(&json!({}));
    let engine = engine_arm(Operation::Report);

    // Admitted by the conditions rule: compiled, no prepare, capture over cells.
    let source = Arc::new(Source::default());
    let hook = Arc::new(RecordingHook::default());
    let mut compiled = session(&package, Operation::Report, &source)
        .with_compiled_parent(Arc::new(StubParent::new()))
        .with_report_hook(hook.clone());
    compiled.set_report_conditions_ok(true);
    let result = compiled.calculate(&inputs()).expect("compiled report");
    assert_same_values(&result, &engine);
    assert_eq!(result.compiled.get("parent"), Some(&json!("compiled")));
    assert_eq!(*hook.calls.lock().unwrap(), ["capture:compiled:Number(12.0)"]);
    let mut expected = engine.diagnostics.clone();
    expected.push("captured".into());
    assert_eq!(result.diagnostics, expected);

    // Forced decline on a report run: fallback, prepare + capture on the engine parent.
    let source = Arc::new(Source::default());
    let hook = Arc::new(RecordingHook::default());
    let mut declined = session(&package, Operation::Report, &source)
        .with_compiled_parent(Arc::new(StubParent { decline: Some("forced"), ..StubParent::new() }))
        .with_report_hook(hook.clone());
    declined.set_report_conditions_ok(true);
    let result = declined.calculate(&inputs()).expect("fallback report");
    assert_same_values(&result, &engine);
    assert_eq!(result.compiled.get("parent"), Some(&json!("fallback:forced")));
    assert_eq!(*hook.calls.lock().unwrap(), ["prepare:workbook", "capture:workbook:Some(Number(12.0))"]);
    assert!(result.diagnostics.contains(&"captured".to_owned()));
}

#[test]
fn static_refusals_record_an_engine_route_without_running_the_module() {
    let normalization = json!({"descriptor": {"calculation_normalizations": [{"sheet": SHEET, "row": 9, "col": 9, "formula": "=1"}]}});
    let cases: [(Operation, bool, Value, &str); 3] = [
        (Operation::Report, false, json!({}), "engine:report_conditions"),
        (Operation::Diagnostic, true, json!({}), "engine:operation:diagnostic"),
        (Operation::Client, true, normalization, "engine:calculation_normalizations"),
    ];
    for (operation, conditions_ok, extra, route) in cases {
        let package = package(&extra);
        let source = Arc::new(Source::default());
        let stub = Arc::new(StubParent::new());
        let mut session = session(&package, operation, &source).with_compiled_parent(stub.clone());
        session.set_report_conditions_ok(conditions_ok);
        let compiled = match session.calculate(&inputs()) {
            Ok(result) => result.compiled,
            Err(_) => session.partial_result().compiled,
        };
        assert_eq!(compiled.get("parent"), Some(&json!(route)), "{route}");
        assert_eq!(compiled.get("parent_loaded"), Some(&json!(true)), "{route}");
        assert_eq!(stub.runs.load(Ordering::SeqCst), 0, "{route}");
    }
}

#[test]
fn an_unserved_parent_leaves_the_receipt_as_before() {
    let package = package(&json!({}));
    let source = Arc::new(Source::default());
    let stub = Arc::new(StubParent { serves: false, ..StubParent::new() });
    let result =
        session(&package, Operation::Client, &source).with_compiled_parent(stub.clone()).calculate(&inputs()).unwrap();
    assert!(result.compiled.get("parent").is_none());
    assert_eq!(stub.runs.load(Ordering::SeqCst), 0);
    assert_same_values(&result, &engine_arm(Operation::Client));
}

#[test]
fn a_module_error_fails_the_request_with_its_route_on_the_evidence() {
    let package = package(&json!({}));
    let source = Arc::new(Source::default());
    let stub = Arc::new(StubParent { fail: true, ..StubParent::new() });
    let mut session = session(&package, Operation::Client, &source).with_compiled_parent(stub);
    let error = session.calculate(&inputs()).unwrap_err();
    assert!(error.to_string().contains("module panicked"), "{error}");
    let evidence = session.partial_result();
    assert_eq!(evidence.compiled.get("parent"), Some(&json!("fallback:error")));
    assert_eq!(evidence.invocations.len(), 2);
    assert_eq!(source.loads("parent"), 0);
}

/// Prerequisite refactor: `admit_scenario` + `apply` is `write_scenario`
/// (same returned effective inputs, cells, per-scenario names, CL-097 stats),
/// and an admission error is the same error.
#[test]
fn admission_split_matches_write_scenario() {
    use formualizer_modelcall::ports::{PortSession, WireValue, admit_scenario};
    let package = package(&json!({}));
    let spec = &package.parent;
    let wire: Vec<(String, WireValue)> = inputs().iter().map(|(key, value)| (key.clone(), WireValue::from_json(value))).collect();
    let make = || Source::default().load(spec, &context(Operation::Client)).unwrap();
    let (mut left, mut right) = (make(), make());
    let mut written = PortSession::new(&mut left, spec, true, true).unwrap();
    let mut applied = PortSession::new(&mut right, spec, true, true).unwrap();
    for _ in 0..2 {
        let expected = written.write_scenario(&mut left, spec, &wire, true).unwrap();
        let admitted = admit_scenario(spec, &wire, true).unwrap();
        assert_eq!(admitted.returned(spec).unwrap(), expected);
        assert_eq!(applied.apply(&mut right, spec, &admitted).unwrap(), expected);
        for row in 1..=3 {
            assert_eq!(left.get_value(SHEET, row, 1), right.get_value(SHEET, row, 1), "row {row}");
        }
        assert_eq!(written.defaulted_inputs, applied.defaulted_inputs);
        assert_eq!(written.ignored_inputs, applied.ignored_inputs);
        assert_eq!(written.write_stats, applied.write_stats);
    }
    let bad = vec![("nope".to_owned(), WireValue::Int(1))];
    let from_write = written.write_scenario(&mut left, spec, &bad, true).unwrap_err();
    assert_eq!(admit_scenario(spec, &bad, true).unwrap_err(), from_write);
}
