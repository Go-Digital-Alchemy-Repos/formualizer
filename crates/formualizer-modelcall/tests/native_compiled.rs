//! Architecture B (GOD-383 WP2): the native compiled-module host against a
//! hand-written `CV_NATIVE_ABI = 1` stub (`tests/fixtures/native_stub/`,
//! built here with cargo into CARGO_TARGET_TMPDIR, once per workbook sha).
//!
//! Covers: the value law and route strings of `compiled/adapter.py`
//! (`compiled`, `engine:<reason>`, `fallback:<reason>`), every decline
//! reason the host produces, a panicking nested-call handler surfacing as an
//! infrastructure error (contract F3), a compiled parent running a compiled
//! child re-entrantly on one thread and two hooks sharing one module (F4),
//! `read_cells` over known rectangles, and the ranged port law.

use formualizer_common::{ExcelError, ExcelErrorKind, LiteralValue};
use formualizer_modelcall::compiled::{NativeCompiledHook, NativeDecline, NativeModule, NativeRegistryEntry};
use formualizer_modelcall::context::{CalculationContextSpec, CalculationFlags};
use formualizer_modelcall::evaluator::CompiledAttempt;
use formualizer_modelcall::{
    CalculationContext, ChildMatrix, CompiledChildHook, CompiledParent, CompiledXcall, ModelCallError, ModelSpec,
    Operation, ParentAttempt,
};
use serde_json::{Value, json};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

const PARENT: &str = "stub-parent";
const CHILD: &str = "stub-child";
const PLAIN: &str = "stub-a";

// ------------------------------------------------------------------ fixture

/// Build the stub for `sha` (serialised; one build per sha per process).
fn stub_library(sha: &str) -> PathBuf {
    static BUILDS: Mutex<BTreeMap<String, PathBuf>> = Mutex::new(BTreeMap::new());
    let mut builds = BUILDS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(path) = builds.get(sha) {
        return path.clone();
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native_stub/Cargo.toml");
    let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("native_stub").join(sha);
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let output = Command::new(cargo)
        .args(["build", "--offline", "--locked", "--manifest-path"])
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target)
        .env("STUB_WORKBOOK_SHA", sha)
        .env_remove("RUSTFLAGS")
        .output()
        .expect("cargo runs");
    assert!(output.status.success(), "stub build failed: {}", String::from_utf8_lossy(&output.stderr));
    let file = format!("{}native_stub{}", std::env::consts::DLL_PREFIX, std::env::consts::DLL_SUFFIX);
    let path = target.join("debug").join(file);
    assert!(path.is_file(), "stub library missing at {}", path.display());
    builds.insert(sha.to_owned(), path.clone());
    path
}

fn entry(sha: &str) -> NativeRegistryEntry {
    NativeRegistryEntry { native_path: stub_library(sha), engine_commit: "stub-engine".into(), manifest_sha256: "m1".into() }
}

fn hook(shas: &[&str]) -> NativeCompiledHook {
    NativeCompiledHook::new(shas.iter().map(|sha| ((*sha).to_owned(), entry(sha))).collect())
}

fn location(row: u32, col: u32, end_row: u32, end_col: u32, key: &str, port_id: &str, shape: &str) -> Value {
    json!({"sheet": "Calc", "start_row": row, "start_col": col, "end_row": end_row, "end_col": end_col,
           "name": format!("X_{key}"), "key": key, "port_id": port_id, "shape": shape,
           "date_system": 1900, "date_fields": []})
}

/// The stub workbook's spec: ports amount (A1), mode (A2), table (A10:C11,
/// ranged); outputs result (C1:D2) and total (A5).
fn spec_json(sha: &str) -> Value {
    json!({
        "identity": format!("model-{sha}"),
        "workbook_path": format!("memory://{sha}"),
        "workbook_sha256": sha,
        "manifest": {
            "spec": "fio", "spec_version": "0.3.0",
            "manifest": {"id": sha, "name": sha, "workbook": {"uri": "memory://stub.xlsx", "locale": "en-US", "date_system": 1900}},
            "ports": [
                {"id": "amount", "dir": "in", "shape": "scalar", "location": {"a1": "Calc!A1"}, "schema": {"type": "any"}},
                {"id": "mode", "dir": "in", "shape": "scalar", "location": {"a1": "Calc!A2"}, "schema": {"type": "any"}},
                {"id": "table", "dir": "in", "shape": "range", "location": {"a1": "Calc!A10:C11"},
                 "schema": {"kind": "range", "cell_type": "any"}},
                {"id": "output_result", "dir": "out", "shape": "range", "location": {"a1": "Calc!C1:D2"},
                 "schema": {"kind": "range", "cell_type": "any"}},
                {"id": "output_total", "dir": "out", "shape": "scalar", "location": {"a1": "Calc!A5"}, "schema": {"type": "any"}}
            ]
        },
        "inputs": {
            "amount": location(1, 1, 1, 1, "Amount", "amount", "scalar"),
            "mode": location(2, 1, 2, 1, "Mode", "mode", "scalar"),
            "table": location(10, 1, 11, 3, "Table", "table", "range")
        },
        "outputs": {
            "result": location(1, 3, 2, 4, "Result", "output_result", "range"),
            "total": location(5, 1, 5, 1, "Total", "output_total", "scalar")
        },
        "defaults": {"Amount": 1, "Mode": "plain", "Table": [[1, 2, 3], [4, 5, 6]]},
        "goal_seek": [],
        "descriptor": {}
    })
}

fn spec(sha: &str) -> ModelSpec {
    serde_json::from_value(spec_json(sha)).expect("spec parses")
}

fn context_with(deadline_seconds: Option<f64>) -> CalculationContext {
    CalculationContext::from_spec(&CalculationContextSpec {
        now: "2026-09-29T12:00:00+00:00".into(),
        operation: Operation::Client,
        random_seed: 1,
        deadline_seconds,
        max_depth: 8,
        flags: CalculationFlags { compiled: true, ..CalculationFlags::default() },
    })
    .expect("context")
}

fn context() -> CalculationContext {
    context_with(None)
}

fn num(value: f64) -> LiteralValue {
    LiteralValue::Number(value)
}

fn text(value: &str) -> LiteralValue {
    LiteralValue::Text(value.to_owned())
}

fn inputs(amount: LiteralValue, mode: &str) -> Vec<(String, LiteralValue)> {
    vec![("amount".into(), amount), ("mode".into(), text(mode))]
}

/// A nested-call handler with a fixed answer that records its arguments.
struct FixedXcall {
    answer: Result<ChildMatrix, ModelCallError>,
    calls: Vec<Vec<LiteralValue>>,
}

impl FixedXcall {
    fn new(answer: Result<ChildMatrix, ModelCallError>) -> Self {
        Self { answer, calls: Vec::new() }
    }
}

impl CompiledXcall for FixedXcall {
    fn call(
        &mut self,
        target: &LiteralValue,
        block: &LiteralValue,
        output: &LiteralValue,
        tail: &[LiteralValue],
    ) -> Result<ChildMatrix, ModelCallError> {
        let mut args = vec![target.clone(), block.clone(), output.clone()];
        args.extend(tail.iter().cloned());
        self.calls.push(args);
        self.answer.clone()
    }
}

struct PanicXcall;

impl CompiledXcall for PanicXcall {
    fn call(&mut self, _: &LiteralValue, _: &LiteralValue, _: &LiteralValue, _: &[LiteralValue]) -> Result<ChildMatrix, ModelCallError> {
        panic!("handler panic");
    }
}

fn child(
    hook: &NativeCompiledHook,
    spec: &ModelSpec,
    inputs: &[(String, LiteralValue)],
    xcall: &mut dyn CompiledXcall,
) -> Result<CompiledAttempt, ModelCallError> {
    let output = spec.resolve_output("result").expect("declared output");
    hook.attempt_child(spec, inputs, output, &context(), xcall, &|| 0)
}

fn route(attempt: &CompiledAttempt) -> &str {
    attempt.route.as_ref().and_then(Value::as_str).expect("a route")
}

fn no_xcall() -> FixedXcall {
    FixedXcall::new(Err(ModelCallError::infrastructure("RuntimeError", "unexpected nested call")))
}

// ------------------------------------------------------------------ child path

#[test]
fn child_answers_the_output_matrix_with_route_compiled() {
    let hook = hook(&[PLAIN]);
    let spec = spec(PLAIN);
    let attempt = child(&hook, &spec, &inputs(LiteralValue::Int(3), "plain"), &mut no_xcall()).unwrap();
    assert_eq!(route(&attempt), "compiled");
    assert_eq!(attempt.matrix, Some(vec![vec![num(3.0), num(6.0)], vec![LiteralValue::Boolean(true), text("text")]]));
    let total = spec.resolve_output("total").unwrap();
    let attempt = hook.attempt_child(&spec, &inputs(num(3.0), "plain"), total, &context(), &mut no_xcall(), &|| 0).unwrap();
    assert_eq!(attempt.matrix, Some(vec![vec![num(24.0)]]), "3 + sum of the default table");
    // A supplied error kind that is in the error lane comes back kind-only.
    let attempt = child(&hook, &spec, &inputs(num(3.0), "err_num"), &mut no_xcall()).unwrap();
    assert_eq!(route(&attempt), "compiled");
    assert_eq!(attempt.matrix.unwrap()[0][0], LiteralValue::Error(ExcelError::new(ExcelErrorKind::Num)));
    // TODAY is the request clock's UTC whole-day serial.
    let attempt = child(&hook, &spec, &inputs(num(1.0), "today"), &mut no_xcall()).unwrap();
    assert_eq!(attempt.matrix.unwrap()[0][0], num(46294.0));
    let report = CompiledChildHook::report(&hook);
    assert_eq!(report["calls"], json!(4));
    assert_eq!(report["routes"], json!(["compiled", "compiled", "compiled", "compiled"]));
    assert_eq!(CompiledParent::report(&hook)["calls"], json!(0));
}

#[test]
fn nested_call_gets_router_arguments_and_its_answer_is_spliced() {
    let hook = hook(&[PLAIN]);
    let spec = spec(PLAIN);
    let mut xcall = FixedXcall::new(Ok(vec![vec![num(7.5), text("x")]]));
    let attempt = child(&hook, &spec, &inputs(num(2.0), "nested"), &mut xcall).unwrap();
    assert_eq!(route(&attempt), "compiled");
    assert_eq!(attempt.matrix, Some(vec![vec![num(7.5), num(1.0)], vec![num(2.0), LiteralValue::Empty]]));
    assert_eq!(xcall.calls, vec![vec![text("rates/child"), num(0.0), text("result"), text("amount"), num(20.0)]]);
    // A rows argument reaches the router as an array.
    let mut xcall = FixedXcall::new(Ok(vec![vec![LiteralValue::Empty]]));
    let attempt = child(&hook, &spec, &inputs(num(2.0), "nested_rows"), &mut xcall).unwrap();
    assert_eq!(route(&attempt), "compiled");
    let table = LiteralValue::Array(vec![
        vec![num(1.0), num(2.0), num(3.0)],
        vec![num(4.0), num(5.0), num(6.0)],
    ]);
    assert_eq!(xcall.calls[0][1], table);
    assert_eq!(attempt.matrix.unwrap()[0][0], LiteralValue::Empty);
}

#[test]
fn every_decline_reason_is_recorded() {
    let hook = hook(&[PLAIN]);
    let base = spec(PLAIN);
    let date = LiteralValue::Date(chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap());
    let ok = || FixedXcall::new(Ok(vec![vec![num(1.0)]]));
    let cases: Vec<(&str, Vec<(String, LiteralValue)>, FixedXcall)> = vec![
        ("engine:admission", inputs(date, "plain"), ok()),
        ("engine:admission", vec![("unknown".into(), num(1.0))], ok()),
        ("engine:admission", inputs(num(f64::NAN), "plain"), ok()),
        ("fallback:stub_decline", inputs(num(1.0), "decline"), ok()),
        ("fallback:probe_violation", inputs(num(1.0), "violation"), ok()),
        ("fallback:exception", inputs(num(1.0), "panic"), ok()),
        ("fallback:exception", inputs(num(1.0), "badrect"), ok()),
        ("fallback:mode", inputs(num(1.0), "no such mode"), ok()),
        ("fallback:non_finite", inputs(num(1.0), "nonfinite"), ok()),
        ("fallback:output_lane", inputs(num(1.0), "err_div"), ok()),
        ("fallback:xcall_lane", inputs(num(1.0), "nested"), FixedXcall::new(Ok(vec![vec![LiteralValue::Int(1)]]))),
        (
            "fallback:xcall_error",
            inputs(num(1.0), "nested"),
            FixedXcall::new(Ok(vec![vec![LiteralValue::Error(ExcelError::new(ExcelErrorKind::Na))]])),
        ),
        ("fallback:xcall_error", inputs(num(1.0), "nested"), FixedXcall::new(Ok(vec![vec![LiteralValue::Pending]]))),
        ("fallback:xcall_error", inputs(num(1.0), "nested"), FixedXcall::new(Err(ModelCallError::routing("no route")))),
        ("fallback:xcall_result", inputs(num(1.0), "nested"), FixedXcall::new(Ok(vec![vec![num(1.0)], vec![]]))),
    ];
    for (expected, inputs, mut xcall) in cases {
        let attempt = child(&hook, &base, &inputs, &mut xcall).unwrap();
        assert_eq!(route(&attempt), expected, "{inputs:?}");
        assert!(attempt.matrix.is_none());
    }

    let mut edits: Vec<(&str, Value)> = Vec::new();
    let mut unregistered = spec_json("stub-unregistered");
    unregistered["workbook_sha256"] = json!("stub-unregistered");
    edits.push(("engine:not_registered", unregistered));
    let mut v = spec_json(PLAIN);
    v["manifest"]["manifest"]["workbook"]["date_system"] = json!(1904);
    edits.push(("engine:date_system", v));
    let mut v = spec_json(PLAIN);
    v["descriptor"] = json!({"calculation_normalizations": [{"sheet": "Calc"}]});
    edits.push(("engine:descriptor_edits", v));
    let mut v = spec_json(PLAIN);
    v["goal_seek"] = json!([{"name": "Xsolve_Rate", "sheet": "Calc", "start_row": 15, "start_col": 1, "end_row": 18, "end_col": 2}]);
    edits.push(("engine:solvers", v));
    let mut v = spec_json(PLAIN);
    v["inputs"]["extra"] = location(3, 1, 3, 1, "Extra", "extra", "scalar");
    edits.push(("engine:port_contract", v));
    let mut v = spec_json(PLAIN);
    v["manifest"]["ports"][0]["schema"] = json!({"type": "number"});
    edits.push(("engine:port_contract", v));
    let mut v = spec_json(PLAIN);
    v["defaults"] = json!({"Amount": 1, "Table": [[1, 2, 3], [4, 5, 6]]});
    edits.push(("engine:port_defaults", v));
    let mut v = spec_json(PLAIN);
    v["outputs"]["result"] = location(1, 3, 2, 5, "Result", "output_result", "range");
    edits.push(("engine:shape_mismatch", v));
    for (expected, value) in edits {
        let edited: ModelSpec = serde_json::from_value(value).unwrap();
        let attempt = child(&hook, &edited, &inputs(num(1.0), "plain"), &mut no_xcall()).unwrap();
        assert_eq!(route(&attempt), expected);
    }

    // The registry key must be the module's own workbook.
    let mut registry = BTreeMap::new();
    registry.insert("stub-b".to_owned(), entry(PLAIN));
    let wrong = NativeCompiledHook::new(registry);
    assert_eq!(wrong.module("stub-b").unwrap_err(), NativeDecline::KeyMismatch);
    let mut v = spec_json(PLAIN);
    v["workbook_sha256"] = json!("stub-b");
    let attempt = child(&wrong, &serde_json::from_value(v).unwrap(), &inputs(num(1.0), "plain"), &mut no_xcall()).unwrap();
    assert_eq!(route(&attempt), "engine:key_mismatch");
    // Without a nested router on this thread the trait entry declines before running.
    let output = base.resolve_output("result").unwrap();
    let attempt = CompiledChildHook::attempt(&hook, &base, &inputs(num(1.0), "plain"), output, &[]).unwrap();
    assert_eq!(route(&attempt), "engine:no_router");
}

#[test]
fn handler_faults_are_infrastructure_errors_not_aborts() {
    let hook = hook(&[PLAIN]);
    let spec = spec(PLAIN);
    let error = child(&hook, &spec, &inputs(num(1.0), "nested"), &mut PanicXcall).unwrap_err();
    let ModelCallError::Infrastructure { kind, message } = &error else { panic!("{error:?}") };
    assert_eq!(kind, "CallbackInfrastructureError");
    assert!(message.starts_with("child callback infrastructure fault: PanicException: handler panic"), "{message}");
    let mut failing = FixedXcall::new(Err(ModelCallError::infrastructure("RuntimeError", "engine failed")));
    let error = child(&hook, &spec, &inputs(num(1.0), "nested"), &mut failing).unwrap_err();
    assert_eq!(
        error,
        ModelCallError::infrastructure("CallbackInfrastructureError", "child callback infrastructure fault: RuntimeError: engine failed")
    );
    // A router fault recorded during the run (the fault count grew).
    let faults = Cell::new(0);
    let output = spec.resolve_output("result").unwrap();
    let mut counting = FixedXcall::new(Ok(vec![vec![num(1.0)]]));
    let attempt = hook
        .attempt_child(&spec, &inputs(num(1.0), "plain"), output, &context(), &mut counting, &|| {
            faults.set(faults.get() + 1);
            faults.get() - 1
        })
        .unwrap();
    assert_eq!(route(&attempt), "fallback:fault");
    assert!(attempt.matrix.is_none());
    // A deadline passed after the run.
    let error = hook
        .attempt_child(&spec, &inputs(num(1.0), "plain"), output, &context_with(Some(0.0)), &mut no_xcall(), &|| 0)
        .unwrap_err();
    assert_eq!(error, ModelCallError::deadline());
    let routes = CompiledChildHook::report(&hook)["routes"].clone();
    assert_eq!(routes, json!(["fallback:fault", "fallback:fault", "fallback:fault", "fallback:deadline"]));
    // The parent path: same law.
    let error = CompiledParent::run(&hook, &spec, &inputs(num(1.0), "nested"), &context(), &mut PanicXcall).unwrap_err();
    assert!(matches!(error, ModelCallError::Infrastructure { ref kind, .. } if kind == "CallbackInfrastructureError"));
    assert_eq!(CompiledParent::report(&hook)["routes"], json!(["fallback:fault"]));
}

// ------------------------------------------------------------------ parent path, F4

/// The parent's nested-call handler: runs the child module through the same
/// hook, on this thread, while the parent's `cv_run` is on the stack.
struct ChildXcall<'a> {
    hook: &'a NativeCompiledHook,
    child: ModelSpec,
    thread: std::thread::ThreadId,
    routes: Vec<String>,
}

impl CompiledXcall for ChildXcall<'_> {
    fn call(
        &mut self,
        _target: &LiteralValue,
        _block: &LiteralValue,
        output: &LiteralValue,
        tail: &[LiteralValue],
    ) -> Result<ChildMatrix, ModelCallError> {
        assert_eq!(std::thread::current().id(), self.thread, "nested call on the parent's thread");
        let LiteralValue::Text(selector) = output else { panic!("selector") };
        let inputs: Vec<(String, LiteralValue)> = tail
            .chunks(2)
            .map(|pair| match pair {
                [LiteralValue::Text(name), value] => (name.clone(), value.clone()),
                _ => panic!("tail pairs"),
            })
            .collect();
        let location = self.child.resolve_output(selector).expect("child output");
        let attempt = self.hook.attempt_child(&self.child, &inputs, location, &context(), &mut no_xcall(), &|| 0)?;
        self.routes.push(attempt.route.as_ref().and_then(Value::as_str).unwrap_or_default().to_owned());
        attempt.matrix.ok_or_else(|| ModelCallError::routing("child declined"))
    }
}

fn compiled(attempt: ParentAttempt) -> formualizer_modelcall::CompiledRun {
    match attempt {
        ParentAttempt::Compiled(run) => run,
        ParentAttempt::Declined { route } => panic!("declined: {route}"),
    }
}

#[test]
fn compiled_parent_runs_a_compiled_child_reentrantly_on_one_thread() {
    let hook = hook(&[PARENT, CHILD]);
    let parent = spec(PARENT);
    let mut xcall = ChildXcall { hook: &hook, child: spec(CHILD), thread: std::thread::current().id(), routes: Vec::new() };
    let run = compiled(CompiledParent::run(&hook, &parent, &inputs(num(3.0), "twice"), &context(), &mut xcall).unwrap());
    assert_eq!(run.route, json!("compiled"));
    // result: [[child(amount=30)[0][0], rows], [cols, child(amount=40)[0][0]]]; the child's plain result is 2x2.
    assert_eq!(run.outputs[0], ("result".to_owned(), vec![vec![num(30.0), num(2.0)], vec![num(2.0), num(40.0)]]));
    assert_eq!(run.outputs[1], ("total".to_owned(), vec![vec![num(24.0)]]));
    assert_eq!(run.stats.xcalls, 2);
    assert_eq!(run.stats.guard_max_row_minus_limit, None);
    assert_eq!(xcall.routes, ["compiled", "compiled"]);
    assert_eq!(CompiledChildHook::report(&hook)["routes"], json!(["compiled", "compiled"]));
    assert_eq!(CompiledParent::report(&hook)["routes"], json!(["compiled"]));
    // A parent decline records under the parent's routes and runs nothing further.
    let declined = CompiledParent::run(&hook, &parent, &inputs(num(3.0), "decline"), &context(), &mut xcall).unwrap();
    assert!(matches!(declined, ParentAttempt::Declined { ref route } if route == "fallback:stub_decline"));
    let declined = CompiledParent::run(&hook, &spec(PLAIN), &inputs(num(3.0), "plain"), &context(), &mut xcall).unwrap();
    assert!(matches!(declined, ParentAttempt::Declined { ref route } if route == "engine:not_registered"));
    hook.clear_routes();
    assert_eq!(CompiledParent::report(&hook)["calls"], json!(0));
}

#[test]
fn two_hooks_share_one_module_per_sha_and_identity_is_checked() {
    let registry = json!({PLAIN: {"native_path": stub_library(PLAIN), "engine_commit": "stub-engine", "manifest_sha256": "m1"}});
    let first = NativeCompiledHook::from_registry_json(&registry.to_string()).unwrap();
    let second = NativeCompiledHook::from_registry_json(&registry.to_string()).unwrap();
    let a = first.module(PLAIN).unwrap();
    let b = second.module(PLAIN).unwrap();
    assert!(Arc::ptr_eq(&a, &b), "one module per sha per process");
    assert!(Arc::ptr_eq(&a, &NativeModule::cached(PLAIN, &entry(PLAIN)).unwrap()));
    assert_eq!(a.port_names(), ["amount", "mode", "table"]);
    let spec = spec(PLAIN);
    for hook in [&first, &second] {
        let attempt = child(hook, &spec, &inputs(num(5.0), "plain"), &mut no_xcall()).unwrap();
        assert_eq!(attempt.matrix.unwrap()[0][1], num(10.0));
    }
    // Same sha, another manifest or another file: never swapped in.
    let other_manifest = NativeRegistryEntry { manifest_sha256: "m2".into(), ..entry(PLAIN) };
    assert_eq!(NativeModule::cached(PLAIN, &other_manifest).unwrap_err(), NativeDecline::ModuleIdentity);
    let copy = Path::new(env!("CARGO_TARGET_TMPDIR")).join("native_stub").join("copy-of-stub-a");
    std::fs::copy(stub_library(PLAIN), &copy).unwrap();
    let other_path = NativeRegistryEntry { native_path: copy, ..entry(PLAIN) };
    assert_eq!(NativeModule::cached(PLAIN, &other_path).unwrap_err(), NativeDecline::ModuleIdentity);
    let mut registry = BTreeMap::new();
    registry.insert(PLAIN.to_owned(), other_path);
    let third = NativeCompiledHook::new(registry);
    let attempt = child(&third, &spec, &inputs(num(5.0), "plain"), &mut no_xcall()).unwrap();
    assert_eq!(route(&attempt), "engine:module_identity");
}

#[test]
fn read_cells_reads_known_rectangles_by_sheet() {
    let hook = hook(&[PARENT]);
    let run = compiled(CompiledParent::run(&hook, &spec(PARENT), &inputs(num(4.0), "plain"), &context(), &mut no_xcall()).unwrap());
    let cells = [
        ("Calc".to_owned(), 1, 1),
        ("Report".to_owned(), 2, 2),
        ("Calc".to_owned(), 10, 3),
        ("Report".to_owned(), 3, 3),
        ("Report".to_owned(), 1, 1),
        ("calc".to_owned(), 2, 1),
        ("Calc".to_owned(), 20, 6),
        ("Calc".to_owned(), 2, 4),
    ];
    let values = run.read_cells(&cells).unwrap();
    assert_eq!(
        values,
        vec![
            num(4.0),
            num(5.0),
            num(3.0),
            LiteralValue::Error(ExcelError::new(ExcelErrorKind::Na)),
            text("label"),
            text("plain"),
            LiteralValue::Empty,
            text("text"),
        ]
    );
    assert!(run.read_cells(&[("Nowhere".to_owned(), 1, 1)]).is_err());
    assert!(run.read_cells(&[("Calc".to_owned(), 0, 1)]).is_err());
    assert!(run.read_cells(&[("Calc".to_owned(), 21, 1)]).is_err(), "outside the module's sheet");
    // The run can be read from another thread (CompiledCells: Send).
    let moved = std::thread::spawn(move || run.read_cells(&[("Calc".to_owned(), 5, 1)]).unwrap()).join().unwrap();
    assert_eq!(moved, vec![num(4.0 + 21.0)]);
}

#[test]
fn ranged_port_takes_exactly_its_declared_rectangle() {
    let hook = hook(&[PLAIN]);
    let spec = spec(PLAIN);
    let table = |rows: Vec<Vec<LiteralValue>>| ("table".to_owned(), LiteralValue::Array(rows));
    let mut given = inputs(num(1.0), "table");
    given.push(table(vec![
        vec![num(10.0), num(20.0), LiteralValue::Int(30)],
        vec![num(40.0), LiteralValue::Empty, text("t")],
    ]));
    let attempt = child(&hook, &spec, &given, &mut no_xcall()).unwrap();
    assert_eq!(route(&attempt), "compiled");
    assert_eq!(attempt.matrix, Some(vec![vec![num(60.0), num(40.0)], vec![text("t"), LiteralValue::Empty]]));
    let declined = [
        table(vec![vec![num(1.0), num(2.0)], vec![num(3.0), num(4.0)], vec![num(5.0), num(6.0)]]),
        table(vec![vec![num(1.0), num(2.0), num(3.0)]]),
        table(vec![vec![num(1.0), num(2.0), num(3.0)], vec![num(4.0), num(5.0), LiteralValue::Pending]]),
        ("table".to_owned(), num(1.0)),
    ];
    for port in declined {
        let mut given = inputs(num(1.0), "table");
        given.push(port);
        let attempt = child(&hook, &spec, &given, &mut no_xcall()).unwrap();
        assert_eq!(route(&attempt), "engine:admission");
    }
    // A declared ranged port whose rectangle is not the module's port.
    let mut v = spec_json(PLAIN);
    v["inputs"]["table"] = location(10, 1, 12, 3, "Table", "table", "range");
    let moved: ModelSpec = serde_json::from_value(v).unwrap();
    let attempt = child(&hook, &moved, &inputs(num(1.0), "table"), &mut no_xcall()).unwrap();
    assert_eq!(route(&attempt), "engine:port_contract");
}
