//! Architecture B item (c) over package B's native stub module
//! (`tests/fixtures/native_stub/`, built here as in `native_compiled.rs`):
//! `ModelSession` runs the stub parent compiled with `NativeCompiledHook` as
//! both parent and child hook, its two `MDL.CALLMODEL` calls go through the
//! session's router to the compiled stub child (stack `[parent]`), and a
//! forced decline (`mode = "decline"`) on a report run falls back to a fresh
//! engine parent that `report_prepare` then prepares (F1).

use formualizer_common::LiteralValue;
use formualizer_modelcall::compiled::{NativeCompiledHook, NativeRegistryEntry};
use formualizer_modelcall::context::{CalculationContextSpec, CalculationFlags};
use formualizer_modelcall::evaluator::{CompiledChildHook, CompiledParent, SharedCompiledCells};
use formualizer_modelcall::event::CallStatus;
use formualizer_modelcall::receipt::PortValue;
use formualizer_modelcall::session::{ModelSession, ReportHook, SharedWorkbook, WorkbookSource, runtime_workbook_config};
use formualizer_modelcall::spec::OrderedMap;
use formualizer_modelcall::{CalculationContext, ModelCallError, ModelPackage, ModelSpec, Operation};
use formualizer_workbook::Workbook;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

const PARENT: &str = "cstub-parent";
const CHILD: &str = "cstub-child";

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
    builds.insert(sha.to_owned(), path.clone());
    path
}

fn hook() -> Arc<NativeCompiledHook> {
    let entry = |sha: &str| NativeRegistryEntry {
        native_path: stub_library(sha),
        engine_commit: "stub-engine".into(),
        manifest_sha256: "m1".into(),
    };
    Arc::new(NativeCompiledHook::new([PARENT, CHILD].iter().map(|sha| ((*sha).to_owned(), entry(sha))).collect()))
}

fn location(row: u32, col: u32, end_row: u32, end_col: u32, key: &str, port_id: &str, shape: &str) -> Value {
    json!({"sheet": "Calc", "start_row": row, "start_col": col, "end_row": end_row, "end_col": end_col,
           "name": format!("X_{key}"), "key": key, "port_id": port_id, "shape": shape,
           "date_system": 1900, "date_fields": []})
}

/// The stub workbook's spec (as `native_compiled.rs`): amount A1, mode A2,
/// table A10:C11 (ranged); outputs result C1:D2 and total A5.
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

fn package() -> Arc<ModelPackage> {
    Arc::new(
        serde_json::from_value(json!({
            "package_id": "stub",
            "parent": spec_json(PARENT),
            "children": {"child": spec_json(CHILD)},
            "child_routes": {"rates/child": "child"},
        }))
        .unwrap(),
    )
}

/// The engine form of the stub parent (only the fallback loads it); the
/// child is never loaded (it runs compiled).
#[derive(Default)]
struct Source {
    loads: Mutex<Vec<String>>,
}

impl WorkbookSource for Source {
    fn load(&self, spec: &ModelSpec, context: &CalculationContext) -> Result<Workbook, ModelCallError> {
        self.loads.lock().unwrap().push(spec.workbook_sha256.clone());
        if spec.workbook_sha256 != PARENT {
            return Err(ModelCallError::infrastructure("FileNotFoundError", "no engine child in this test"));
        }
        let mut workbook = Workbook::new_with_config(runtime_workbook_config(context.random_seed));
        workbook.add_sheet("Calc").unwrap();
        workbook.set_formula("Calc", 1, 3, "=MDL.CALLMODEL(\"rates/child\",0,\"result\",\"amount\",A1*10)").unwrap();
        workbook.set_formula("Calc", 5, 1, "=A1+SUM(A10:C11)").unwrap();
        Ok(workbook)
    }
}

fn context(operation: Operation) -> CalculationContext {
    CalculationContext::from_spec(&CalculationContextSpec {
        now: "2026-09-29T12:00:00+00:00".into(),
        operation,
        random_seed: 1,
        deadline_seconds: None,
        max_depth: 8,
        flags: CalculationFlags { compiled: true, ..CalculationFlags::default() },
    })
    .unwrap()
}

fn session(operation: Operation, source: &Arc<Source>, hook: &Arc<NativeCompiledHook>) -> ModelSession {
    ModelSession::new(package(), context(operation))
        .with_workbook_source(source.clone())
        .with_compiled_child(hook.clone() as Arc<dyn CompiledChildHook>)
        .with_compiled_parent(hook.clone() as Arc<dyn CompiledParent>)
}

fn inputs(mode: &str) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("Amount".into(), json!(3));
    map.insert("Mode".into(), json!(mode));
    map
}

fn num(value: f64) -> LiteralValue {
    LiteralValue::Number(value)
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
        let value = workbook.read().unwrap().get_value("Calc", 5, 1);
        self.calls.lock().unwrap().push(format!("capture:workbook:{value:?}"));
        Ok(Vec::new())
    }

    fn capture_compiled(&self, cells: &SharedCompiledCells, _outputs: &OrderedMap<PortValue>) -> Result<Vec<String>, ModelCallError> {
        let values = cells.lock().unwrap().read_cells(&[("Calc".to_owned(), 5, 1), ("Calc".to_owned(), 1, 3)])?;
        self.calls.lock().unwrap().push(format!("capture:compiled:{values:?}"));
        Ok(Vec::new())
    }

    fn captures_compiled(&self) -> bool {
        true
    }
}

#[test]
fn stub_parent_runs_compiled_with_compiled_children_through_the_router() {
    let source = Arc::new(Source::default());
    let hook = hook();
    let report = Arc::new(RecordingHook::default());
    let mut session = session(Operation::Report, &source, &hook).with_report_hook(report.clone());
    session.set_report_conditions_ok(true);
    let result = session.calculate(&inputs("twice")).expect("compiled parent");

    assert_eq!(result.typed_outputs.get("Result"), Some(&PortValue::Range(vec![vec![num(30.0), num(2.0)], vec![num(2.0), num(40.0)]])));
    assert_eq!(result.outputs.get("Total"), Some(&PortValue::Scalar(num(24.0))));
    assert_eq!(result.effective_inputs.get("Amount"), Some(&PortValue::Scalar(num(3.0))));
    assert_eq!(result.effective_inputs.get("Mode"), Some(&PortValue::Scalar(LiteralValue::Text("twice".into()))));
    assert_eq!(
        result.effective_inputs.get("Table"),
        Some(&PortValue::Range(vec![vec![num(1.0), num(2.0), num(3.0)], vec![num(4.0), num(5.0), num(6.0)]]))
    );
    let identity = package().parent.model_identity();
    assert_eq!(result.invocations.len(), 2);
    assert!(result.invocations.iter().all(|event| event.stack == [identity.clone()] && event.status == CallStatus::Completed));
    assert!(result.invocations.iter().all(|event| event.route == Some(json!("compiled"))));
    assert_eq!(result.compiled.get("parent"), Some(&json!("compiled")));
    assert_eq!(result.compiled.get("parent_xcalls"), Some(&json!(2)));
    assert_eq!(result.compiled.get("parent_loaded"), Some(&json!(false)));
    assert_eq!(result.compiled.get("routes"), Some(&json!(["compiled", "compiled"])));
    assert!(source.loads.lock().unwrap().is_empty(), "no engine workbook loaded");
    assert_eq!(*report.calls.lock().unwrap(), ["capture:compiled:[Number(24.0), Number(30.0)]"]);
    // The cells outlive the calculate call (the Python CompiledCells holds them).
    let cells = session.compiled_cells().expect("cells").clone();
    assert_eq!(cells.lock().unwrap().read_cells(&[("Calc".to_owned(), 2, 4)]).unwrap(), [num(40.0)]);
}

#[test]
fn forced_stub_decline_on_a_report_run_falls_back_and_prepares_the_engine_parent() {
    let source = Arc::new(Source::default());
    let hook = hook();
    let report = Arc::new(RecordingHook::default());
    let mut session = session(Operation::Report, &source, &hook).with_report_hook(report.clone());
    session.set_report_conditions_ok(true);
    let result = session.calculate(&inputs("decline")).expect("engine fallback");
    assert_eq!(result.compiled.get("parent"), Some(&json!("fallback:stub_decline")));
    assert_eq!(result.compiled.get("parent_loaded"), Some(&json!(true)));
    assert_eq!(*source.loads.lock().unwrap(), [PARENT]);
    assert_eq!(*report.calls.lock().unwrap(), ["prepare:workbook", "capture:workbook:Some(Number(24.0))"]);
    // The engine parent's own call went to the compiled child.
    assert_eq!(result.invocations.len(), 1);
    assert_eq!(result.invocations[0].route, Some(json!("compiled")));
    assert_eq!(result.compiled.get("routes"), Some(&json!(["compiled"])));
    assert!(session.compiled_cells().is_none());
}
