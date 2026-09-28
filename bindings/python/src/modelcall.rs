//! Python surface of the child-model call path (GOD-383).
//!
//! `RetainedModel(package_json, context_json, *, retain_scenarios=False)`
//! holds one package's retained workbooks (parent and children, loaded on
//! first use or by `warm`), their pinned goal-seek cells and CL-097 write
//! records; the Python pool leases it to one request at a time.
//! `warm(inputs=None)`, `close()`, `stats()`, `forget_scenarios()`.
//!
//! `ModelSession(package_json, context_json, retained=None, *,
//! compiled_child=None)` is one request (deadline measured from
//! construction). `calculate(inputs, report_prepare=None,
//! report_capture=None, inspect=None)` evaluates one scenario with the GIL
//! released and returns a dict with the keys in
//! `formualizer_modelcall::receipt::result_keys` (memo counters under the
//! receipt key `xcall_memo` this round), plus `report_cells`,
//! `conditional_results`, `formula_counts` when `report_capture` returned them
//! and `inspection` when `inspect` returned something.
//!
//! Every value in the dict is in receipt form, `snapshot._plain` of what the
//! Python runtime held: int / float / str / bool / None, `{"type": "date" |
//! "datetime" | "time", "value": iso}`, errors as the dict
//! `LiteralValue.to_python()` gives, lists for tuples. (The hooks receive
//! native values: `report_capture(workbook, outputs)` gets outputs as
//! `literal_to_py` builds them.)
//!
//! A failed run raises `ModelCalculationError` (a `RuntimeError`) with
//! `error_type` (the Python exception name the runtime raised, e.g.
//! `TimeoutError`), `error_message`, and `evidence` (the same dict shape,
//! outputs empty, diagnostics ending with `Type: message`).
//!
//! Hooks: `report_prepare(workbook)` runs after admission and before
//! evaluation (operation `report`; the write record is cleared after it),
//! `report_capture(workbook, outputs)` after goal seek and the output reads,
//! `inspect(workbook)` after capture (operation `diagnostic`, timed as
//! `inspection_seconds`); `workbook` is a `Workbook` over the session's own
//! parent. `ModelSession.workbook()` returns the parent after `calculate`.
//!
//! Compiled child (`flags.compiled`): `compiled_child` (constructor keyword or
//! `set_compiled_child(hook)`) is an object with `attempt(identity,
//! workbook_sha256, inputs, output, stack, xcall)` and `report()`. `attempt`
//! returns `(matrix_or_None, route_or_None)`; `xcall` is a callable routing
//! the compiled child's own calls (`xcall(target, block, output, *tail)`).
//!
//! `register_import_aliases(workbook, callback)` binds a Python callable
//! under `MDL.CALLMODEL` and every imported name in
//! `formualizer_modelcall::IMPORTED_CALL_NAMES`, the one table of those names.

use formualizer_common::LiteralValue;
use formualizer_modelcall::context::CalculationContextSpec;
use formualizer_modelcall::evaluator::{CompiledAttempt, CompiledChildHook};
use formualizer_modelcall::event::ModelCallEvent;
use formualizer_modelcall::ports::WireValue;
use formualizer_modelcall::receipt::{
    CalculationResult, PortValue, TimingValue, py_date_iso, py_datetime_iso, py_time_iso, result_keys,
};
use formualizer_modelcall::retained::RetainedModel;
use formualizer_modelcall::router::{ModelCallRouter, current_nested_router};
use formualizer_modelcall::session::{ModelSession, ReportHook, SharedWorkbook, failure_kind};
use formualizer_modelcall::spec::{ModelSpec, OrderedMap, PortLocation};
use formualizer_modelcall::{CalculationContext, ModelCallError, ModelPackage, call_function_names};
use pyo3::exceptions::{PyNotImplementedError, PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{
    PyAny, PyBool, PyDate, PyDateTime, PyDict, PyFloat, PyInt, PyList, PyString, PyTime, PyTuple,
};
use std::sync::{Arc, Mutex};

use crate::sheetport::json_to_py;
use crate::value::{PyLiteralValue, literal_to_py, py_to_literal};
use crate::workbook::PyWorkbook;

pyo3::create_exception!(formualizer, ModelCalculationError, PyRuntimeError);

fn to_py_err(error: ModelCallError) -> PyErr {
    match error {
        ModelCallError::Routing(message) => PyValueError::new_err(message),
        ModelCallError::NotImplemented(what) => PyNotImplementedError::new_err(what),
        other @ ModelCallError::Infrastructure { .. } => PyRuntimeError::new_err(other.to_string()),
    }
}

/// A Python exception as a request failure (`Type: message`).
fn from_py_err(py: Python<'_>, error: &PyErr) -> ModelCallError {
    let kind = error.get_type(py).name().map(|name| name.to_string()).unwrap_or_else(|_| "Exception".into());
    let message = error.value(py).str().map(|text| text.to_string()).unwrap_or_default();
    if kind == "ChildRoutingError" {
        ModelCallError::routing(message)
    } else {
        ModelCallError::infrastructure(kind, message)
    }
}

/// A dict or JSON text, as JSON text.
fn json_text(value: &Bound<'_, PyAny>) -> PyResult<String> {
    if let Ok(text) = value.cast::<PyString>() {
        return Ok(text.to_str()?.to_owned());
    }
    let json = value.py().import("json")?;
    json.call_method1("dumps", (value,))?.extract()
}

/// Any JSON-serialisable Python value (a str is a JSON string, not JSON text).
fn py_json(value: &Bound<'_, PyAny>) -> PyResult<serde_json::Value> {
    let text: String = value.py().import("json")?.call_method1("dumps", (value,))?.extract()?;
    serde_json::from_str(&text).map_err(|error| PyValueError::new_err(error.to_string()))
}

// ---------------------------------------------------------------------------
// Python -> request values
// ---------------------------------------------------------------------------

/// A Python request value as the runtime's object model.
fn py_to_wire(value: &Bound<'_, PyAny>) -> PyResult<WireValue> {
    if let Ok(literal) = value.extract::<PyRef<'_, PyLiteralValue>>() {
        return Ok(WireValue::Literal(literal.inner.clone()));
    }
    if value.is_none() {
        return Ok(WireValue::None);
    }
    if value.is_instance_of::<PyBool>() {
        return Ok(WireValue::Bool(value.extract()?));
    }
    if value.is_instance_of::<PyInt>() {
        return Ok(match value.extract::<i64>() {
            Ok(number) => WireValue::Int(number),
            Err(_) => WireValue::Float(value.extract::<f64>()?),
        });
    }
    if value.is_instance_of::<PyFloat>() {
        return Ok(WireValue::Float(value.extract()?));
    }
    if value.is_instance_of::<PyString>() {
        return Ok(WireValue::Str(value.extract()?));
    }
    if value.is_instance_of::<PyDateTime>() || value.is_instance_of::<PyDate>() || value.is_instance_of::<PyTime>() {
        return Ok(match py_to_literal(value)? {
            LiteralValue::DateTime(stamp) => WireValue::DateTime(stamp),
            LiteralValue::Date(day) => WireValue::Date(day),
            LiteralValue::Time(clock) => WireValue::Time(clock),
            other => WireValue::Literal(other),
        });
    }
    if let Ok(dict) = value.cast::<PyDict>() {
        if dict.contains("type")? {
            if let Ok(literal) = py_to_literal(value) {
                return Ok(WireValue::Literal(literal));
            }
        }
        let mut entries = Vec::with_capacity(dict.len());
        for (key, item) in dict.iter() {
            entries.push((key.str()?.to_string(), py_to_wire(&item)?));
        }
        return Ok(WireValue::Dict(OrderedMap(entries)));
    }
    if value.is_instance_of::<PyList>() || value.is_instance_of::<PyTuple>() {
        let mut items = Vec::new();
        for item in value.try_iter()? {
            items.push(py_to_wire(&item?)?);
        }
        return Ok(WireValue::List(items));
    }
    if let Ok(literal) = py_to_literal(value) {
        return Ok(WireValue::Literal(literal));
    }
    Err(PyTypeError::new_err(format!(
        "unsupported request value type: {}",
        value.get_type().name()?
    )))
}

fn request_inputs(inputs: &Bound<'_, PyAny>) -> PyResult<Vec<(String, WireValue)>> {
    let dict = match inputs.cast::<PyDict>() {
        Ok(dict) => dict.clone(),
        Err(_) => {
            let parsed = inputs.py().import("json")?.call_method1("loads", (json_text(inputs)?,))?;
            parsed.cast::<PyDict>().map_err(|_| PyTypeError::new_err("inputs must be a dict"))?.clone()
        }
    };
    let mut pairs = Vec::with_capacity(dict.len());
    for (key, value) in dict.iter() {
        let name: String = key.extract().map_err(|_| PyTypeError::new_err("input names must be text"))?;
        pairs.push((name, py_to_wire(&value)?));
    }
    Ok(pairs)
}

// ---------------------------------------------------------------------------
// Result -> Python
// ---------------------------------------------------------------------------

fn typed(py: Python<'_>, value: &LiteralValue) -> PyResult<Py<PyAny>> {
    Ok(Py::new(py, PyLiteralValue::from(value.clone()))?.into_any())
}

fn rows_to_py(
    py: Python<'_>,
    rows: &[Vec<LiteralValue>],
    cell: fn(Python<'_>, &LiteralValue) -> PyResult<Py<PyAny>>,
) -> PyResult<Py<PyAny>> {
    let outer = PyList::empty(py);
    for row in rows {
        let inner = PyList::empty(py);
        for value in row {
            inner.append(cell(py, value)?)?;
        }
        outer.append(inner)?;
    }
    Ok(outer.into_any().unbind())
}

/// Native Python value (`literal_to_py`), what the hooks receive.
fn native(py: Python<'_>, value: &LiteralValue) -> PyResult<Py<PyAny>> {
    literal_to_py(py, value)
}

/// Receipt form: `snapshot._plain(literal.to_python())`.
fn plain(py: Python<'_>, value: &LiteralValue) -> PyResult<Py<PyAny>> {
    let tagged = |kind: &str, text: String| -> PyResult<Py<PyAny>> {
        let dict = PyDict::new(py);
        dict.set_item("type", kind)?;
        dict.set_item("value", text)?;
        Ok(dict.into_any().unbind())
    };
    match value {
        LiteralValue::Date(day) => tagged("date", py_date_iso(day)),
        LiteralValue::DateTime(stamp) => tagged("datetime", py_datetime_iso(stamp)),
        LiteralValue::Time(clock) => tagged("time", py_time_iso(clock)),
        LiteralValue::Array(rows) => rows_to_py(py, rows, plain),
        other => literal_to_py(py, other),
    }
}

fn port_value_to_py(
    py: Python<'_>,
    value: &PortValue,
    cell: fn(Python<'_>, &LiteralValue) -> PyResult<Py<PyAny>>,
) -> PyResult<Py<PyAny>> {
    match value {
        PortValue::Scalar(value) => cell(py, value),
        PortValue::Range(rows) => rows_to_py(py, rows, cell),
        PortValue::Record(fields) => {
            let dict = PyDict::new(py);
            for (key, value) in fields.iter() {
                dict.set_item(key, cell(py, value)?)?;
            }
            Ok(dict.into_any().unbind())
        }
        PortValue::Row(values) => {
            let list = PyList::empty(py);
            for value in values {
                list.append(cell(py, value)?)?;
            }
            Ok(list.into_any().unbind())
        }
        PortValue::Table(rows) => {
            let list = PyList::empty(py);
            for row in rows {
                let dict = PyDict::new(py);
                for (key, value) in row.iter() {
                    dict.set_item(key, cell(py, value)?)?;
                }
                list.append(dict)?;
            }
            Ok(list.into_any().unbind())
        }
    }
}

fn ports_to_py(
    py: Python<'_>,
    ports: &OrderedMap<PortValue>,
    cell: fn(Python<'_>, &LiteralValue) -> PyResult<Py<PyAny>>,
) -> PyResult<Py<PyAny>> {
    let dict = PyDict::new(py);
    for (key, value) in ports.iter() {
        dict.set_item(key, port_value_to_py(py, value, cell)?)?;
    }
    Ok(dict.into_any().unbind())
}

fn event_to_py(py: Python<'_>, event: &ModelCallEvent) -> PyResult<Py<PyAny>> {
    let dict = PyDict::new(py);
    dict.set_item("index", event.index)?;
    dict.set_item("parent", &event.parent)?;
    dict.set_item("target", plain(py, &event.target)?)?;
    dict.set_item("output", plain(py, &event.output)?)?;
    dict.set_item("stack", PyList::new(py, &event.stack)?)?;
    dict.set_item("status", event.status.as_str())?;
    if let Some(child) = &event.child {
        dict.set_item("child", child)?;
    }
    if let Some(inputs) = &event.inputs {
        let map = PyDict::new(py);
        for (name, value) in inputs {
            map.set_item(name, plain(py, value)?)?;
        }
        dict.set_item("inputs", map)?;
    }
    if let Some(source) = event.memo_of {
        dict.set_item("memo_of", source)?;
    }
    if let Some(matrix) = &event.matrix {
        dict.set_item("matrix", rows_to_py(py, matrix, plain)?)?;
    }
    if let Some(error) = &event.error {
        dict.set_item("error", error)?;
    }
    if let Some(returned) = &event.returned_error {
        dict.set_item("returned_error", plain(py, &LiteralValue::Error(returned.clone()))?)?;
    }
    if event.prefetch {
        dict.set_item("prefetch", true)?;
    }
    if let Some(route) = &event.route {
        dict.set_item("route", json_to_py(py, route)?)?;
    }
    if let Some(from) = &event.inherited_from {
        dict.set_item("inherited_from", from)?;
    }
    if let Some(from) = &event.held_from {
        dict.set_item("held_from", from)?;
    }
    Ok(dict.into_any().unbind())
}

fn events_to_py(py: Python<'_>, events: &[ModelCallEvent]) -> PyResult<Py<PyAny>> {
    let list = PyList::empty(py);
    for event in events {
        list.append(event_to_py(py, event)?)?;
    }
    Ok(list.into_any().unbind())
}

fn map_to_py(py: Python<'_>, map: &serde_json::Map<String, serde_json::Value>) -> PyResult<Py<PyAny>> {
    json_to_py(py, &serde_json::Value::Object(map.clone()))
}

fn result_dict<'py>(py: Python<'py>, result: &CalculationResult) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item(result_keys::OUTPUTS, ports_to_py(py, &result.outputs, plain)?)?;
    dict.set_item(result_keys::TYPED_OUTPUTS, ports_to_py(py, &result.typed_outputs, plain)?)?;
    dict.set_item(result_keys::EFFECTIVE_INPUTS, ports_to_py(py, &result.effective_inputs, plain)?)?;
    dict.set_item(result_keys::INVOCATIONS, events_to_py(py, &result.invocations)?)?;
    let timings = PyDict::new(py);
    for (key, value) in result.timings.0.iter() {
        match value {
            TimingValue::Count(count) => timings.set_item(key, *count)?,
            TimingValue::Seconds(seconds) => timings.set_item(key, *seconds)?,
        }
    }
    dict.set_item(result_keys::TIMINGS, timings)?;
    dict.set_item(result_keys::FAULTS, events_to_py(py, &result.faults)?)?;
    let solvers = PyList::empty(py);
    for record in &result.solvers {
        solvers.append(map_to_py(py, record)?)?;
    }
    dict.set_item(result_keys::SOLVERS, solvers)?;
    dict.set_item(result_keys::DIAGNOSTICS, PyList::new(py, &result.diagnostics)?)?;
    dict.set_item(result_keys::SESSION_REUSE, map_to_py(py, &result.session_reuse)?)?;
    let memo = match &result.call_memo {
        Some(report) => serde_json::to_value(report).map_err(|error| PyValueError::new_err(error.to_string()))?,
        None => serde_json::Value::Object(serde_json::Map::new()),
    };
    dict.set_item(result_keys::CALL_MEMO, json_to_py(py, &memo)?)?;
    dict.set_item(result_keys::COMPILED, map_to_py(py, &result.compiled)?)?;
    Ok(dict)
}

// ---------------------------------------------------------------------------
// Hooks backed by Python callables
// ---------------------------------------------------------------------------

/// What the hooks returned in the last `calculate`.
#[derive(Default)]
struct HookResults {
    report: Option<Py<PyAny>>,
    inspection: Option<Py<PyAny>>,
}

struct PyReportHook {
    prepare: Option<Py<PyAny>>,
    capture: Option<Py<PyAny>>,
    inspect: Option<Py<PyAny>>,
    results: Arc<Mutex<HookResults>>,
}

fn shared_workbook(py: Python<'_>, workbook: &SharedWorkbook) -> PyResult<Py<PyWorkbook>> {
    Py::new(py, PyWorkbook::from_shared(workbook.clone()))
}

impl ReportHook for PyReportHook {
    fn prepare(&self, workbook: &SharedWorkbook) -> Result<(), ModelCallError> {
        let Some(prepare) = &self.prepare else { return Ok(()) };
        Python::attach(|py| {
            let workbook = shared_workbook(py, workbook).map_err(|error| from_py_err(py, &error))?;
            prepare.call1(py, (workbook,)).map_err(|error| from_py_err(py, &error))?;
            Ok(())
        })
    }

    fn capture(&self, workbook: &SharedWorkbook, outputs: &OrderedMap<PortValue>) -> Result<Vec<String>, ModelCallError> {
        let Some(capture) = &self.capture else { return Ok(Vec::new()) };
        Python::attach(|py| {
            let run = || -> PyResult<Vec<String>> {
                let workbook = shared_workbook(py, workbook)?;
                let outputs = ports_to_py(py, outputs, native)?;
                let report = capture.call1(py, (workbook, outputs))?;
                let bound = report.bind(py);
                let mut notes = Vec::new();
                if let Ok(dict) = bound.cast::<PyDict>() {
                    if let Some(diagnostics) = dict.get_item("diagnostics")? {
                        for note in diagnostics.try_iter()? {
                            notes.push(note?.str()?.to_string());
                        }
                    }
                }
                if let Ok(mut results) = self.results.lock() {
                    results.report = Some(report);
                }
                Ok(notes)
            };
            run().map_err(|error| from_py_err(py, &error))
        })
    }

    fn inspect(&self, workbook: &SharedWorkbook) -> Result<(), ModelCallError> {
        let Some(inspect) = &self.inspect else { return Ok(()) };
        Python::attach(|py| {
            let run = || -> PyResult<()> {
                let workbook = shared_workbook(py, workbook)?;
                let inspection = inspect.call1(py, (workbook,))?;
                if let Ok(mut results) = self.results.lock() {
                    results.inspection = (!inspection.is_none(py)).then_some(inspection);
                }
                Ok(())
            };
            run().map_err(|error| from_py_err(py, &error))
        })
    }

    fn inspects(&self) -> bool {
        self.inspect.is_some()
    }
}

/// Add the hook keys the hooks produced to a result dict.
fn add_hook_results(py: Python<'_>, dict: &Bound<'_, PyDict>, results: &HookResults) -> PyResult<()> {
    if let Some(report) = &results.report
        && let Ok(report) = report.bind(py).cast::<PyDict>()
    {
        for key in [result_keys::REPORT_CELLS, result_keys::CONDITIONAL_RESULTS, result_keys::FORMULA_COUNTS] {
            if let Some(value) = report.get_item(key)? {
                dict.set_item(key, value)?;
            }
        }
    }
    if let Some(inspection) = &results.inspection {
        dict.set_item(result_keys::INSPECTION, inspection.clone_ref(py))?;
    }
    Ok(())
}

struct PyCompiledHook {
    target: Py<PyAny>,
}

fn location_to_py(py: Python<'_>, location: &PortLocation) -> PyResult<Py<PyAny>> {
    let value = serde_json::to_value(location).map_err(|error| PyValueError::new_err(error.to_string()))?;
    json_to_py(py, &value)
}

impl CompiledChildHook for PyCompiledHook {
    fn attempt(
        &self,
        spec: &ModelSpec,
        inputs: &[(String, LiteralValue)],
        output: &PortLocation,
        stack: &[String],
    ) -> Result<CompiledAttempt, ModelCallError> {
        let router = current_nested_router();
        Python::attach(|py| {
            let run = || -> PyResult<CompiledAttempt> {
                let map = PyDict::new(py);
                for (name, value) in inputs {
                    map.set_item(name, native(py, value)?)?;
                }
                let xcall: Py<PyAny> = match &router {
                    Some(router) => Py::new(py, PyNestedCall { router: router.clone() })?.into_any(),
                    None => py.None(),
                };
                let result = self.target.call_method1(
                    py,
                    "attempt",
                    (
                        &spec.identity,
                        &spec.workbook_sha256,
                        map,
                        location_to_py(py, output)?,
                        PyTuple::new(py, stack)?,
                        xcall,
                    ),
                )?;
                let bound = result.bind(py);
                if bound.is_none() {
                    return Ok(CompiledAttempt::default());
                }
                let (matrix, route): (Bound<'_, PyAny>, Bound<'_, PyAny>) = bound.extract()?;
                let matrix = if matrix.is_none() {
                    None
                } else {
                    let mut rows = Vec::new();
                    for row in matrix.try_iter()? {
                        let mut cells = Vec::new();
                        for cell in row?.try_iter()? {
                            cells.push(py_to_literal(&cell?)?);
                        }
                        rows.push(cells);
                    }
                    Some(rows)
                };
                let route = if route.is_none() { None } else { Some(py_json(&route)?) };
                Ok(CompiledAttempt { matrix, route })
            };
            run().map_err(|error| from_py_err(py, &error))
        })
    }

    fn report(&self) -> serde_json::Map<String, serde_json::Value> {
        Python::attach(|py| {
            self.target
                .call_method0(py, "report")
                .and_then(|report| py_json(report.bind(py)))
                .ok()
                .and_then(|value| value.as_object().cloned())
                .unwrap_or_default()
        })
    }
}

/// The router a compiled child calls its own children through.
#[pyclass(name = "ModelCallRouter", module = "formualizer.formualizer_py")]
pub struct PyNestedCall {
    router: ModelCallRouter,
}

#[pymethods]
impl PyNestedCall {
    #[pyo3(signature = (*args))]
    fn __call__(&self, py: Python<'_>, args: &Bound<'_, PyTuple>) -> PyResult<Py<PyAny>> {
        let args = args.iter().map(|arg| py_to_literal(&arg)).collect::<PyResult<Vec<_>>>()?;
        let router = self.router.clone();
        let value = py.detach(move || router.call(&args));
        match value {
            LiteralValue::Array(rows) => rows_to_py(py, &rows, typed),
            other => typed(py, &other),
        }
    }
}

// ---------------------------------------------------------------------------
// ModelSession
// ---------------------------------------------------------------------------

fn parse_package(package_json: &str) -> PyResult<ModelPackage> {
    ModelPackage::from_json(package_json).map_err(|error| PyValueError::new_err(error.to_string()))
}

fn parse_context(context: &Bound<'_, PyAny>) -> PyResult<CalculationContext> {
    let spec: CalculationContextSpec =
        serde_json::from_str(&json_text(context)?).map_err(|error| PyValueError::new_err(error.to_string()))?;
    CalculationContext::from_spec(&spec).map_err(to_py_err)
}

fn calculation_error(py: Python<'_>, error: &ModelCallError, evidence: Bound<'_, PyDict>) -> PyResult<PyErr> {
    let exception = ModelCalculationError::new_err(error.to_string());
    let value = exception.value(py);
    value.setattr("error_type", failure_kind(error))?;
    let message = match error {
        ModelCallError::Infrastructure { message, .. } => message.clone(),
        other => other.to_string(),
    };
    value.setattr("error_message", message)?;
    value.setattr("evidence", evidence)?;
    Ok(exception)
}

/// One package's retained workbooks (`sessions.SessionPool` for one package).
#[pyclass(name = "RetainedModel", module = "formualizer.formualizer_py")]
pub struct PyRetainedModel {
    model: RetainedModel,
}

#[pymethods]
impl PyRetainedModel {
    /// `package_json`: the package JSON. `context_json`: a dict or JSON text
    /// (`now`, `random_seed`, `flags`; the warm clock and seed, and the
    /// flags every load uses, e.g. `skip_unchanged_writes`).
    /// `retain_scenarios`: a persistent worker (`WORKBOOK_SESSION_PERSIST`):
    /// each completed request's scenario is what the next one inherits.
    #[new]
    #[pyo3(signature = (package_json, context_json, *, retain_scenarios = false))]
    fn new(package_json: &str, context_json: &Bound<'_, PyAny>, retain_scenarios: bool) -> PyResult<Self> {
        let package = parse_package(package_json)?;
        let context = parse_context(context_json)?;
        Ok(Self { model: RetainedModel::new(Arc::new(package), context, retain_scenarios) })
    }

    /// Load and evaluate every model not yet retained (children first, then
    /// the parent with `inputs`, default scenario when None). Returns
    /// `{warmed, warm_timings, warm_invocations, warm_session_timings,
    /// invocations}` (`invocations`: identity -> the warm run's events).
    #[pyo3(signature = (inputs = None))]
    fn warm<'py>(&self, py: Python<'py>, inputs: Option<&Bound<'py, PyAny>>) -> PyResult<Bound<'py, PyDict>> {
        let inputs = match inputs {
            Some(inputs) if !inputs.is_none() => request_inputs(inputs)?,
            _ => Vec::new(),
        };
        let model = &self.model;
        let report = py.detach(|| model.warm(&inputs, true)).map_err(|error| {
            let evidence = PyDict::new(py);
            calculation_error(py, &error, evidence).unwrap_or_else(|failure| failure)
        })?;
        let dict = PyDict::new(py);
        dict.set_item("warmed", PyList::new(py, report.warmed())?)?;
        let seconds = PyDict::new(py);
        let counts = PyDict::new(py);
        let timings = PyDict::new(py);
        let events = PyDict::new(py);
        for model in &report.models {
            seconds.set_item(&model.identity, model.seconds)?;
            counts.set_item(&model.identity, model.invocations.len())?;
            timings.set_item(&model.identity, timings_to_py(py, &model.timings)?)?;
            events.set_item(&model.identity, events_to_py(py, &model.invocations)?)?;
        }
        dict.set_item("warm_timings", seconds)?;
        dict.set_item("warm_invocations", counts)?;
        dict.set_item("warm_session_timings", timings)?;
        dict.set_item("invocations", events)?;
        Ok(dict)
    }

    /// Forget every retained workbook; returns how many there were.
    fn close(&self) -> usize {
        self.model.close()
    }

    /// Keep the workbooks, inherit nothing from what ran (after a failure).
    fn forget_scenarios(&self) {
        self.model.pool().forget_scenarios();
    }

    /// Entries, flags and warm timings (plain JSON).
    fn stats(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        json_to_py(py, &self.model.stats())
    }

    fn __len__(&self) -> usize {
        self.model.pool().len()
    }
}

fn timings_to_py<'py>(py: Python<'py>, timings: &formualizer_modelcall::receipt::Timings) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    for (key, value) in timings.0.iter() {
        match value {
            TimingValue::Count(count) => dict.set_item(key, *count)?,
            TimingValue::Seconds(seconds) => dict.set_item(key, *seconds)?,
        }
    }
    Ok(dict)
}

/// One calculation request over a pinned model package.
#[pyclass(name = "ModelSession", module = "formualizer.formualizer_py")]
pub struct PyModelSession {
    session: ModelSession,
    results: Arc<Mutex<HookResults>>,
    /// Keeps the retained model alive while this request runs on it.
    _retained: Option<Py<PyRetainedModel>>,
}

#[pymethods]
impl PyModelSession {
    /// `package_json`: the package JSON (`package.py`). `context_json`: a dict
    /// or JSON text with `now` (timezone-aware ISO 8601), `operation`,
    /// `random_seed`, `deadline_seconds` (from construction), `max_depth` and
    /// `flags`. `retained`: the `RetainedModel` this request leases.
    #[new]
    #[pyo3(signature = (package_json, context_json, retained = None, *, compiled_child = None))]
    fn new(
        py: Python<'_>,
        package_json: &str,
        context_json: &Bound<'_, PyAny>,
        retained: Option<Py<PyRetainedModel>>,
        compiled_child: Option<Py<PyAny>>,
    ) -> PyResult<Self> {
        let package = parse_package(package_json)?;
        let context = parse_context(context_json)?;
        let mut session = ModelSession::new(Arc::new(package), context);
        if let Some(retained) = &retained {
            session.set_retained(Some(&retained.borrow(py).model));
        }
        if let Some(target) = compiled_child {
            session.set_compiled_child(Some(Arc::new(PyCompiledHook { target })));
        }
        Ok(Self { session, results: Arc::new(Mutex::new(HookResults::default())), _retained: retained })
    }

    /// Calculate one scenario; returns the result dict or raises
    /// `ModelCalculationError` carrying `evidence`.
    #[pyo3(signature = (inputs, report_prepare = None, report_capture = None, inspect = None))]
    fn calculate<'py>(
        &mut self,
        py: Python<'py>,
        inputs: &Bound<'py, PyAny>,
        report_prepare: Option<Py<PyAny>>,
        report_capture: Option<Py<PyAny>>,
        inspect: Option<Py<PyAny>>,
    ) -> PyResult<Bound<'py, PyDict>> {
        let inputs = request_inputs(inputs)?;
        if let Ok(mut results) = self.results.lock() {
            *results = HookResults::default();
        }
        let hook: Option<Arc<dyn ReportHook>> =
            if report_prepare.is_some() || report_capture.is_some() || inspect.is_some() {
                Some(Arc::new(PyReportHook {
                    prepare: report_prepare,
                    capture: report_capture,
                    inspect,
                    results: self.results.clone(),
                }))
            } else {
                None
            };
        self.session.set_report_hook(hook);
        let session = &mut self.session;
        let outcome = py.detach(|| session.calculate_wire(&inputs));
        // The hooks' Python objects must not outlive this call through the session.
        self.session.set_report_hook(None);
        match outcome {
            Ok(result) => {
                let dict = result_dict(py, &result)?;
                if let Ok(results) = self.results.lock() {
                    add_hook_results(py, &dict, &results)?;
                }
                Ok(dict)
            }
            Err(error) => {
                let evidence = result_dict(py, &self.session.partial_result())?;
                Err(calculation_error(py, &error, evidence)?)
            }
        }
    }

    /// Evidence of the last run (as `calculate` returns, outputs empty).
    fn partial_result<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        result_dict(py, &self.session.partial_result())
    }

    /// The parent workbook of the last `calculate`, or None.
    fn workbook(&self, py: Python<'_>) -> PyResult<Option<Py<PyWorkbook>>> {
        self.session.workbook().map(|workbook| shared_workbook(py, workbook)).transpose()
    }

    /// What `report_capture` returned in the last `calculate`, or None.
    #[getter]
    fn report(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.results.lock().ok().and_then(|results| results.report.as_ref().map(|report| report.clone_ref(py)))
    }

    /// The compiled-child hook (`attempt`/`report`), or None to clear it.
    fn set_compiled_child(&mut self, hook: Option<Py<PyAny>>) {
        self.session
            .set_compiled_child(hook.map(|target| Arc::new(PyCompiledHook { target }) as Arc<dyn CompiledChildHook>));
    }

    /// Cancel every workbook this request has open (thread-safe).
    fn cancel(&self) {
        self.session.cancel();
    }
}

/// Register `callback` on `workbook` under `MDL.CALLMODEL` and every imported
/// alias, with the options `RouterBinding.register` uses today. An occupied
/// name is unregistered first.
#[pyfunction]
#[pyo3(signature = (workbook, callback, *, immutable_fresh = true))]
fn register_import_aliases(workbook: &Bound<'_, PyAny>, callback: &Bound<'_, PyAny>, immutable_fresh: bool) -> PyResult<Vec<&'static str>> {
    let py = workbook.py();
    let mut names = Vec::new();
    for name in call_function_names() {
        // Refused when not registered; that is the expected case.
        let _ = workbook.call_method1("unregister_function", (name,));
        let options = PyDict::new(py);
        options.set_item("min_args", 3)?;
        options.set_item("max_args", py.None())?;
        options.set_item("volatile", !immutable_fresh)?;
        options.set_item("deterministic", immutable_fresh)?;
        options.set_item("thread_safe", false)?;
        workbook.call_method("register_function", (name, callback), Some(&options))?;
        names.push(name);
    }
    Ok(names)
}

/// Every name the call function is registered under, ours first.
#[pyfunction]
fn call_model_function_names() -> Vec<&'static str> {
    call_function_names().collect()
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyModelSession>()?;
    m.add_class::<PyRetainedModel>()?;
    m.add_class::<PyNestedCall>()?;
    m.add("ModelCalculationError", m.py().get_type::<ModelCalculationError>())?;
    m.add_function(wrap_pyfunction!(register_import_aliases, m)?)?;
    m.add_function(wrap_pyfunction!(call_model_function_names, m)?)?;
    Ok(())
}
