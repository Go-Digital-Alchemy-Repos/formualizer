//! Lane I: `RetainedModel` + `ModelSession` over in-memory workbooks: pool
//! reuse and `session_reuse`, CL-097 skip counters, warm inheritance, the
//! CP1 finding 2 run-state probe (parent -> child -> grandchild -> leaf with
//! the leaf called twice), deadline cancel mid-leaf, goal seek on a synthetic
//! `Xsolve_` block (pinned cells restored on reuse), exact routing error text
//! (CP1 finding 5) and the report / inspect hooks (CP1 finding 4).

use formualizer_common::{ExcelError, LiteralValue, RangeAddress};
use formualizer_modelcall::context::{CalculationContextSpec, CalculationFlags};
use formualizer_modelcall::evaluator::CompiledAttempt;
use formualizer_modelcall::event::CallStatus;
use formualizer_modelcall::receipt::{PortValue, TimingValue};
use formualizer_modelcall::retained::RetainedModel;
use formualizer_modelcall::router::current_nested_router;
use formualizer_modelcall::session::{ModelSession, ReportHook, SharedWorkbook, WorkbookSource, runtime_workbook_config};
use formualizer_modelcall::spec::OrderedMap;
use formualizer_modelcall::{
    CalculationContext, CompiledChildHook, ModelCallError, ModelPackage, ModelSpec, Operation, PortLocation,
};
use formualizer_workbook::traits::NamedRangeScope;
use formualizer_workbook::{CustomFnOptions, Workbook};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

const SHEET: &str = "Calc";
const SLEEP: Duration = Duration::from_millis(150);

/// One synthetic model: cells (`=...` formula, `n:<number>` literal, `b:true`
/// boolean, else text), one scalar input at A1 (`amount`), one scalar output,
/// optional workbook-scope names.
#[derive(Clone)]
struct Model {
    identity: &'static str,
    cells: Vec<(u32, u32, &'static str)>,
    output: (u32, u32),
    names: Vec<(&'static str, Rect)>,
    slow: bool,
}

/// (row, col, end_row, end_col)
type Rect = (u32, u32, u32, u32);

fn model(identity: &'static str, cells: Vec<(u32, u32, &'static str)>) -> Model {
    Model { identity, cells, output: (2, 1), names: Vec::new(), slow: false }
}

fn a1(row: u32, col: u32) -> String {
    format!("'{SHEET}'!{}{row}", (b'A' + u8::try_from(col - 1).unwrap()) as char)
}

fn location(row: u32, col: u32, name: &str, key: &str, port_id: &str) -> Value {
    json!({"sheet": SHEET, "start_row": row, "start_col": col, "end_row": row, "end_col": col,
           "name": name, "key": key, "port_id": port_id, "shape": "scalar", "date_system": 1900, "date_fields": []})
}

fn spec_json(model: &Model) -> Value {
    let (row, col) = model.output;
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
                {"id": "output_result", "dir": "out", "shape": "scalar", "location": {"a1": a1(row, col)},
                 "schema": {"type": "any"}, "constraints": {"nullable": true}}
            ]
        },
        "inputs": {"amount": location(1, 1, "XINPUT_amount", "amount", "amount")},
        "outputs": {"result": location(row, col, "XOUTPUT_result", "result", "output_result")},
        "defaults": {"amount": 1},
        "goal_seek": [],
        "descriptor": {}
    })
}

struct MemorySource {
    models: HashMap<String, Model>,
    loads: Arc<AtomicUsize>,
}

impl WorkbookSource for MemorySource {
    fn load(&self, spec: &ModelSpec, context: &CalculationContext) -> Result<Workbook, ModelCallError> {
        self.loads.fetch_add(1, Ordering::SeqCst);
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
            let options = CustomFnOptions {
                min_args: 1,
                max_args: Some(1),
                volatile: true,
                thread_safe: false,
                deterministic: false,
                allow_override_builtin: false,
            };
            workbook.register_custom_function("SLOWFN", options, Arc::new(slow)).unwrap();
        }
        workbook.set_value(SHEET, 1, 1, LiteralValue::Number(1.0)).unwrap();
        for (row, col, cell) in &model.cells {
            if cell.starts_with('=') {
                workbook.set_formula(SHEET, *row, *col, cell).unwrap();
            } else if let Some(number) = cell.strip_prefix("n:") {
                workbook.set_value(SHEET, *row, *col, LiteralValue::Number(number.parse().unwrap())).unwrap();
            } else if let Some(flag) = cell.strip_prefix("b:") {
                workbook.set_value(SHEET, *row, *col, LiteralValue::Boolean(flag == "true")).unwrap();
            } else {
                workbook.set_value(SHEET, *row, *col, LiteralValue::Text((*cell).to_owned())).unwrap();
            }
        }
        for (name, (row, col, end_row, end_col)) in &model.names {
            let address = RangeAddress::new(SHEET, *row, *col, *end_row, *end_col).unwrap();
            workbook.define_named_range(name, &address, NamedRangeScope::Workbook).unwrap();
        }
        Ok(workbook)
    }
}

fn package(parent: &Model, children: &[(&str, &Model)]) -> Arc<ModelPackage> {
    let mut children_json = serde_json::Map::new();
    let mut routes = serde_json::Map::new();
    for (route, model) in children {
        children_json.insert(model.identity.to_owned(), spec_json(model));
        routes.insert((*route).to_owned(), Value::String(model.identity.to_owned()));
    }
    Arc::new(
        serde_json::from_value(json!({
            "package_id": "synthetic",
            "parent": spec_json(parent),
            "children": children_json,
            "child_routes": routes,
        }))
        .unwrap(),
    )
}

fn context(deadline_seconds: Option<f64>, operation: Operation, skip_unchanged_writes: bool) -> CalculationContext {
    CalculationContext::from_spec(&CalculationContextSpec {
        now: "2026-09-28T00:00:00+00:00".into(),
        operation,
        random_seed: 147,
        deadline_seconds,
        max_depth: 32,
        flags: CalculationFlags { skip_unchanged_writes, ..CalculationFlags::default() },
    })
    .unwrap()
}

fn source(models: &[&Model]) -> (Arc<MemorySource>, Arc<AtomicUsize>) {
    let loads = Arc::new(AtomicUsize::new(0));
    let source = MemorySource {
        models: models.iter().map(|model| (model.identity.to_owned(), (*model).clone())).collect(),
        loads: loads.clone(),
    };
    (Arc::new(source), loads)
}

fn inputs(amount: i64) -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    map.insert("amount".into(), json!(amount));
    map
}

fn number(value: &PortValue) -> f64 {
    match value {
        PortValue::Scalar(LiteralValue::Number(number)) => *number,
        PortValue::Scalar(LiteralValue::Int(number)) => *number as f64,
        other => panic!("not a number: {other:?}"),
    }
}

fn count(result: &formualizer_modelcall::CalculationResult, key: &str) -> u64 {
    match result.timings.get(key) {
        Some(TimingValue::Count(count)) => count,
        other => panic!("{key}: {other:?}"),
    }
}

fn reuse_list(result: &formualizer_modelcall::CalculationResult, key: &str) -> Vec<String> {
    result.session_reuse[key].as_array().unwrap().iter().map(|value| value.as_str().unwrap().to_owned()).collect()
}

fn call(route: &str) -> &'static str {
    Box::leak(format!("=MDL.CALLMODEL(\"{route}\",0,\"result\",\"amount\",A1)").into_boxed_str())
}

fn parent_and_child() -> (Model, Model) {
    let child = model("child", vec![(2, 1, "=A1*2")]);
    let parent = model("parent", vec![(2, 1, "=MDL.CALLMODEL(\"rates/child\",0,\"result\",\"amount\",A1)*2")]);
    (parent, child)
}

#[test]
fn consecutive_requests_reuse_retained_workbooks_and_skip_unchanged_writes() {
    let (parent, child) = parent_and_child();
    let package = package(&parent, &[("rates/child", &child)]);
    let (source, loads) = source(&[&parent, &child]);
    let retained =
        RetainedModel::new(package.clone(), context(None, Operation::Client, true), false).with_workbook_source(source);

    let run = |amount: i64| {
        let mut session =
            ModelSession::new(package.clone(), context(None, Operation::Client, true)).with_retained(&retained);
        session.calculate(&inputs(amount)).expect("calculates")
    };

    let first = run(3);
    assert_eq!(number(first.outputs.get("result").unwrap()), 12.0);
    assert_eq!(reuse_list(&first, "fresh"), ["parent:sha-parent", "child:sha-child"]);
    assert_eq!(first.session_reuse["reused_count"], json!(0));
    assert_eq!(count(&first, "writes_skipped"), 0);
    assert_eq!(loads.load(Ordering::SeqCst), 2);
    assert!(retained.pool().idle());

    // Same inputs: the parent's input write is skipped, so its call cell stays
    // clean and fires nothing; the retained value answers.
    let second = run(3);
    assert_eq!(number(second.outputs.get("result").unwrap()), 12.0);
    assert_eq!(second.session_reuse["fresh_count"], json!(0));
    assert_eq!(reuse_list(&second, "reused"), ["parent:sha-parent"]);
    assert_eq!(count(&second, "writes_skipped"), 1);
    assert!(second.invocations.is_empty(), "{:?}", second.invocations);

    // A changed input fires the call; the child entry is re-entered.
    let third = run(5);
    assert_eq!(number(third.outputs.get("result").unwrap()), 20.0);
    assert_eq!(reuse_list(&third, "reused"), ["parent:sha-parent", "child:sha-child"]);
    assert_eq!(third.invocations.len(), 1);
    assert_eq!(third.invocations[0].status, CallStatus::Completed);
    assert_eq!(count(&third, "writes_skipped"), 0);
    assert_eq!(loads.load(Ordering::SeqCst), 2, "no workbook was loaded twice");

    let stats = retained.stats();
    assert_eq!(stats["count"], json!(2));
    assert_eq!(retained.close(), 2);
}

#[test]
fn retained_scenarios_and_warm_seal_inherited_events() {
    let (parent, child) = parent_and_child();
    let package = package(&parent, &[("rates/child", &child)]);
    let (source, loads) = source(&[&parent, &child]);
    let retained =
        RetainedModel::new(package.clone(), context(None, Operation::Client, true), true).with_workbook_source(source);
    let report = retained.warm(&[], true).expect("warms");
    assert_eq!(report.warmed(), ["child:sha-child", "parent:sha-parent"]);
    // The parent's warm evaluated one call (default amount 1) on the warmed child.
    assert_eq!(report.models[1].invocations.len(), 1);
    assert_eq!(loads.load(Ordering::SeqCst), 2);

    let run = |amount: i64| {
        let mut session =
            ModelSession::new(package.clone(), context(None, Operation::Client, true)).with_retained(&retained);
        session.calculate(&inputs(amount)).expect("calculates")
    };
    // Inputs equal to the warmed defaults: nothing fires, the warm's call is
    // sealed as inherited.
    let first = run(1);
    assert_eq!(number(first.outputs.get("result").unwrap()), 4.0);
    assert_eq!(reuse_list(&first, "warmed"), ["parent:sha-parent"]);
    assert_eq!(first.invocations.len(), 1);
    assert_eq!(first.invocations[0].status, CallStatus::Inherited);
    assert_eq!(first.invocations[0].inherited_from.as_deref(), Some("warm"));

    // Persistent worker: the next identical request inherits what this one left.
    let second = run(1);
    assert_eq!(reuse_list(&second, "reused"), ["parent:sha-parent"]);
    assert_eq!(second.invocations.len(), 1);
    assert_eq!(second.invocations[0].status, CallStatus::Inherited);
}

/// CP1 finding 2: the in-line child path shares one run state. parent ->
/// child -> grandchild -> leaf, the leaf called twice from the grandchild:
/// four events with stack lengths 1, 2, 3, 3, the second leaf call memoized.
#[test]
fn nested_calls_share_run_state_and_the_repeat_is_memoized() {
    let leaf = model("leaf", vec![(2, 1, "=A1+1")]);
    let grandchild = Model {
        identity: "grandchild",
        cells: vec![(2, 1, "=B2+C2"), (2, 2, call("rates/leaf")), (2, 3, call("rates/leaf"))],
        output: (2, 1),
        names: Vec::new(),
        slow: false,
    };
    let child = model("child", vec![(2, 1, call("rates/grandchild"))]);
    let parent = model("parent", vec![(2, 1, call("rates/child"))]);
    let package = package(&parent, &[("rates/child", &child), ("rates/grandchild", &grandchild), ("rates/leaf", &leaf)]);
    let (source, _) = source(&[&parent, &child, &grandchild, &leaf]);
    let retained =
        RetainedModel::new(package.clone(), context(None, Operation::Client, false), false).with_workbook_source(source);
    // The second request changes the input: the engine skips an identical
    // literal write, so an unchanged request would fire nothing at all.
    for (round, (amount, expected)) in [(3, 8.0), (4, 10.0)].into_iter().enumerate() {
        let mut session = ModelSession::new(package.clone(), context(None, Operation::Client, false)).with_retained(&retained);
        let result = session.calculate(&inputs(amount)).expect("calculates");
        assert_eq!(number(result.outputs.get("result").unwrap()), expected, "round {round}");
        if round == 1 {
            assert_eq!(result.session_reuse["reused_count"], json!(4));
        }
        let lengths: Vec<_> = result.invocations.iter().map(|event| event.stack.len()).collect();
        assert_eq!(lengths, [1, 2, 3, 3], "round {round}");
        let statuses: Vec<_> = result.invocations.iter().map(|event| event.status).collect();
        assert_eq!(
            statuses,
            [CallStatus::Completed, CallStatus::Completed, CallStatus::Completed, CallStatus::Memoized],
            "round {round}"
        );
        assert_eq!(result.invocations[3].memo_of, Some(2));
        assert_eq!(result.invocations[2].stack, ["parent:sha-parent", "child:sha-child", "grandchild:sha-grandchild"]);
    }
}

/// CP1 finding 2, cancel half: a 0.5 s deadline passes while the leaf is
/// evaluating four levels down; every level unwinds, nothing hangs.
#[test]
fn deadline_cancels_mid_leaf_without_hanging() {
    let chain: Vec<(u32, u32, &'static str)> =
        (2..14).map(|row| (row, 1, &*Box::leak(format!("=SLOWFN(A{})", row - 1).into_boxed_str()))).collect();
    let leaf = Model { identity: "leaf", cells: chain, output: (13, 1), names: Vec::new(), slow: true };
    let grandchild = model("grandchild", vec![(2, 1, call("rates/leaf"))]);
    let child = model("child", vec![(2, 1, call("rates/grandchild"))]);
    let parent = model("parent", vec![(2, 1, call("rates/child"))]);
    let package = package(&parent, &[("rates/child", &child), ("rates/grandchild", &grandchild), ("rates/leaf", &leaf)]);
    let (source, _) = source(&[&parent, &child, &grandchild, &leaf]);
    let retained = Arc::new(
        RetainedModel::new(package.clone(), context(None, Operation::Client, false), false).with_workbook_source(source),
    );
    let (done, wait) = mpsc::channel();
    let started = Instant::now();
    let shared = retained.clone();
    std::thread::spawn(move || {
        let mut session =
            ModelSession::new(package, context(Some(0.5), Operation::Client, false)).with_retained(&shared);
        let outcome = session.calculate(&inputs(1));
        let _ = done.send((outcome, session.partial_result()));
    });
    let (outcome, evidence) = wait.recv_timeout(Duration::from_secs(30)).expect("no hang");
    assert_eq!(outcome.unwrap_err(), ModelCallError::deadline());
    assert!(started.elapsed() < SLEEP * 12, "stopped at a layer boundary: {:?}", started.elapsed());
    let lengths: Vec<_> = evidence.invocations.iter().map(|event| event.stack.len()).collect();
    assert_eq!(lengths, [1, 2, 3]);
    assert!(evidence.invocations.iter().all(|event| event.status == CallStatus::InfrastructureError));
    assert!(retained.pool().idle(), "every entry was lent back");
}

/// Goal seek on a synthetic `Xsolve_` block (Lane B's `run_goal_seeks`
/// through the session's `SolveModel`, including `get_formula`): C1 such
/// that C1^2 - A1 = 0. On reuse the change cell is restored from the pin
/// before the next scenario.
#[test]
fn goal_seek_block_solves_and_is_restored_on_reuse() {
    let parent = Model {
        identity: "parent",
        cells: vec![
            (1, 3, "n:0"),
            (2, 3, "=C1*C1-A1"),
            (10, 1, "Run if"),
            (10, 2, "b:true"),
            (11, 1, "Target cell"),
            (11, 2, "=C2"),
            (12, 1, "Target value"),
            (12, 2, "n:0"),
            (13, 1, "By changing"),
            (13, 2, "=C1"),
            (14, 1, "Max change"),
            (14, 2, "n:1e-12"),
            (15, 1, "Max iterations"),
            (15, 2, "n:100"),
            (16, 1, "Lower bound"),
            (16, 2, "n:0"),
            (17, 1, "Upper bound"),
            (17, 2, "n:100"),
        ],
        output: (1, 3),
        names: vec![("Xsolve_Root", (10, 1, 17, 2))],
        slow: false,
    };
    let package = package(&parent, &[]);
    let (source, _) = source(&[&parent]);
    let retained =
        RetainedModel::new(package.clone(), context(None, Operation::Client, true), false).with_workbook_source(source);
    for (amount, expected) in [(4, 2.0), (9, 3.0), (4, 2.0)] {
        let mut session = ModelSession::new(package.clone(), context(None, Operation::Client, true)).with_retained(&retained);
        let result = session.calculate(&inputs(amount)).expect("solves");
        let root = number(result.outputs.get("result").unwrap());
        assert!((root - expected).abs() < 1e-6, "amount {amount}: {root}");
        assert_eq!(result.solvers.len(), 1);
        assert_eq!(result.solvers[0]["status"], json!("converged"));
        assert_eq!(result.solvers[0]["workbook"], json!("parent:sha-parent"));
        assert!(result.diagnostics.iter().any(|note| note.starts_with("xsolve_ran:Root")), "{:?}", result.diagnostics);
    }
    let stats = retained.stats();
    // The block's 16 cells plus the change cell C1.
    assert_eq!(stats["entries"][0]["restore_cells"], json!(17));
}

/// CP1 finding 5: the routing refusals keep Python's exact text.
#[test]
fn routing_error_text_matches_python() {
    let child = model("child", vec![(2, 1, "=A1*2")]);
    let parent = model(
        "parent",
        vec![
            (2, 1, "=MDL.CALLMODEL(1,0,\"result\")"),
            (3, 1, "=MDL.CALLMODEL(\"rates/child\",0,\"NoSuch\",\"amount\",A1)"),
        ],
    );
    let package = package(&parent, &[("rates/child", &child)]);
    let (source, _) = source(&[&parent, &child]);
    let mut session = ModelSession::new(package, context(None, Operation::Client, false)).with_workbook_source(source);
    let result = session.calculate(&inputs(1)).expect("refusals are cell values");
    let errors: Vec<_> = result.invocations.iter().map(|event| event.error.clone().unwrap_or_default()).collect();
    assert!(errors.contains(&"child target must be text".to_owned()), "{errors:?}");
    assert!(
        errors.contains(&"child output selector is not declared: Undeclared output 'NoSuch'".to_owned()),
        "{errors:?}"
    );
    assert!(result.invocations.iter().all(|event| event.status == CallStatus::RoutingError));
}

#[derive(Default)]
struct RecordingHook {
    calls: Mutex<Vec<String>>,
}

impl ReportHook for RecordingHook {
    fn prepare(&self, workbook: &SharedWorkbook) -> Result<(), ModelCallError> {
        let value = workbook.read().unwrap().get_value(SHEET, 1, 1);
        self.calls.lock().unwrap().push(format!("prepare:{value:?}"));
        Ok(())
    }

    fn capture(&self, _workbook: &SharedWorkbook, outputs: &OrderedMap<PortValue>) -> Result<Vec<String>, ModelCallError> {
        self.calls.lock().unwrap().push(format!("capture:{}", number(outputs.get("result").unwrap())));
        Ok(vec!["report_note".into()])
    }

    fn inspect(&self, _workbook: &SharedWorkbook) -> Result<(), ModelCallError> {
        self.calls.lock().unwrap().push("inspect".into());
        Ok(())
    }

    fn inspects(&self) -> bool {
        true
    }
}

/// CP1 finding 4: `report_prepare` before evaluation, `report_capture` after
/// the reads (its notes join the diagnostics), `inspect` for `diagnostic`.
#[test]
fn report_and_inspect_hooks_run_at_their_steps() {
    let (parent, child) = parent_and_child();
    let package = package(&parent, &[("rates/child", &child)]);
    let (source, _) = source(&[&parent, &child]);
    let hook = Arc::new(RecordingHook::default());
    let mut report = ModelSession::new(package.clone(), context(None, Operation::Report, false))
        .with_workbook_source(source.clone())
        .with_report_hook(hook.clone());
    let result = report.calculate(&inputs(3)).unwrap();
    let calls = hook.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls[0].starts_with("prepare:Some("), "{calls:?}");
    assert_eq!(calls[1], "capture:12");
    assert_eq!(result.diagnostics.last().map(String::as_str), Some("report_note"));

    let hook = Arc::new(RecordingHook::default());
    let mut diagnostic = ModelSession::new(package, context(None, Operation::Diagnostic, false))
        .with_workbook_source(source)
        .with_report_hook(hook.clone());
    let result = diagnostic.calculate(&inputs(3)).unwrap();
    assert_eq!(*hook.calls.lock().unwrap(), ["inspect"]);
    assert!(matches!(result.timings.get("inspection_seconds"), Some(TimingValue::Seconds(seconds)) if seconds > 0.0));
}

/// A compiled child for the synthetic `child` model: answers `amount * 2`
/// (`route` `compiled`), declines every other model. With `nested`, it first
/// calls that route through the nested router; with `decline`, it then
/// declines as a module whose nested call returned an error would
/// (`fallback:xcall_result`).
struct DoublingHook {
    calls: AtomicUsize,
    nested: Option<&'static str>,
    decline: bool,
}

impl DoublingHook {
    fn new(nested: Option<&'static str>, decline: bool) -> Arc<Self> {
        Arc::new(Self { calls: AtomicUsize::new(0), nested, decline })
    }
}

impl CompiledChildHook for DoublingHook {
    fn attempt(
        &self,
        spec: &ModelSpec,
        inputs: &[(String, LiteralValue)],
        _output: &PortLocation,
        _stack: &[String],
    ) -> Result<CompiledAttempt, ModelCallError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if spec.identity != "child" {
            return Ok(CompiledAttempt { matrix: None, route: Some(json!("engine:other_child")) });
        }
        let amount = match inputs.iter().find(|(name, _)| name == "amount").map(|(_, value)| value) {
            Some(LiteralValue::Number(value)) => *value,
            Some(LiteralValue::Int(value)) => *value as f64,
            _ => 1.0,
        };
        if let Some(route) = self.nested {
            let router = current_nested_router().expect("a compiled attempt runs under a nested router");
            let args = [
                LiteralValue::Text(route.into()),
                LiteralValue::Number(0.0),
                LiteralValue::Text("result".into()),
                LiteralValue::Text("amount".into()),
                LiteralValue::Number(amount),
            ];
            let _ = router.call(&args);
        }
        if self.decline {
            return Ok(CompiledAttempt { matrix: None, route: Some(json!("fallback:xcall_result")) });
        }
        Ok(CompiledAttempt { matrix: Some(vec![vec![LiteralValue::Number(amount * 2.0)]]), route: Some(json!("compiled")) })
    }

    fn report(&self) -> serde_json::Map<String, Value> {
        serde_json::Map::new()
    }
}

fn compiled_context() -> CalculationContext {
    CalculationContext::from_spec(&CalculationContextSpec {
        now: "2026-09-28T00:00:00+00:00".into(),
        operation: Operation::Client,
        random_seed: 147,
        deadline_seconds: None,
        max_depth: 32,
        flags: CalculationFlags { compiled: true, ..CalculationFlags::default() },
    })
    .unwrap()
}

/// CP2 item 4: `RetainedModel::warm` routes the parent's calls through the
/// compiled child it was given (Python's warm `CalculationSession` builds a
/// `CompiledRoute`), and the warm events a request inherits carry `route`.
#[test]
fn warm_consults_the_compiled_child_and_inherited_events_carry_route() {
    let (parent, child) = parent_and_child();
    let package = package(&parent, &[("rates/child", &child)]);
    let (memory, _) = source(&[&parent, &child]);
    let hook = DoublingHook::new(None, false);
    let mut retained = RetainedModel::new(package.clone(), compiled_context(), false).with_workbook_source(memory);
    retained.set_compiled_child(Some(hook.clone()));
    let report = retained.warm(&[], true).expect("warms");
    assert_eq!(report.warmed(), ["child:sha-child", "parent:sha-parent"]);
    assert_eq!(hook.calls.load(Ordering::SeqCst), 1, "the parent's one call went to the compiled child");
    let warm_events = &report.models[1].invocations;
    assert_eq!(warm_events.len(), 1);
    assert_eq!(warm_events[0].route, Some(json!("compiled")));

    // A request whose inputs equal the warmed defaults inherits the warm's
    // event, route included.
    let mut session = ModelSession::new(package.clone(), compiled_context()).with_retained(&retained);
    session.set_compiled_child(Some(hook.clone()));
    let result = session.calculate(&inputs(1)).expect("calculates");
    assert_eq!(number(result.outputs.get("result").unwrap()), 4.0);
    assert_eq!(result.invocations.len(), 1);
    assert_eq!(result.invocations[0].status, CallStatus::Inherited);
    assert_eq!(result.invocations[0].route, Some(json!("compiled")));

    // Without a hook the warm evaluates on the engine and records no route.
    let (memory, _) = source(&[&parent, &child]);
    let plain = RetainedModel::new(package, compiled_context(), false).with_workbook_source(memory);
    let report = plain.warm(&[], true).expect("warms");
    assert_eq!(report.models[1].invocations[0].route, None);
}

/// CP2 item 5b: a nested call that faults while the compiled child runs
/// fails the request at once (`CompiledRoute.attempt`'s `fallback:fault`),
/// instead of falling back to the engine and firing the faulting call again.
#[test]
fn nested_fault_during_compiled_attempt_fails_without_engine_fallback() {
    let (parent, child) = parent_and_child();
    let leaf = model("leaf", vec![(2, 1, "=A1+1")]);
    let package = package(&parent, &[("rates/child", &child), ("rates/leaf", &leaf)]);
    // The leaf is declared but cannot load: its call is an infrastructure fault.
    let (memory, loads) = source(&[&parent, &child]);
    let hook = DoublingHook::new(Some("rates/leaf"), true);
    let mut session = ModelSession::new(package, compiled_context()).with_workbook_source(memory);
    session.set_compiled_child(Some(hook.clone()));
    let error = session.calculate(&inputs(3)).expect_err("the nested fault fails the run");
    match &error {
        ModelCallError::Infrastructure { kind, message } => {
            assert_eq!(kind, "CallbackInfrastructureError");
            assert!(message.starts_with("child callback infrastructure fault"), "{message}");
        }
        other => panic!("unexpected error {other:?}"),
    }
    assert_eq!(hook.calls.load(Ordering::SeqCst), 1);
    assert_eq!(loads.load(Ordering::SeqCst), 1, "only the parent loaded: the child never ran on the engine");
    let evidence = session.partial_result();
    let child_event = evidence
        .invocations
        .iter()
        .find(|event| event.child.as_deref() == Some("child:sha-child"))
        .expect("the parent's call event");
    assert_eq!(child_event.route, Some(json!("fallback:fault")));
    let leaf = LiteralValue::Text("rates/leaf".into());
    let leaf_events = evidence.invocations.iter().filter(|event| event.target == leaf).count();
    assert_eq!(leaf_events, 1, "the faulting call fired once");
}
