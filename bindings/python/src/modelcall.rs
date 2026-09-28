//! Python surface of the child-model call path (GOD-383).
//!
//! `ModelSession(package_json, context)` wraps `formualizer_modelcall`'s
//! session; `calculate(inputs)` returns a dict with the keys in
//! `formualizer_modelcall::receipt::result_keys` (memo counters under the
//! receipt key `xcall_memo` this round). `register_import_aliases(workbook,
//! callback)` binds a Python callable under `MDL.CALLMODEL` and every imported
//! name in `formualizer_modelcall::IMPORTED_CALL_NAMES`, the one table of
//! those names.
//!
//! Lane 0 stub: `calculate` raises `NotImplementedError` until Lane A lands
//! the session, and the result conversion goes through JSON; Lane A replaces
//! it with native value conversion (`LiteralValue` -> Python objects, as
//! `read_typed_matrix` produces today).

use formualizer_modelcall::receipt::{CalculationResult, result_keys};
use formualizer_modelcall::session::ModelSession;
use formualizer_modelcall::context::CalculationContextSpec;
use formualizer_modelcall::{CalculationContext, ModelCallError, ModelPackage, call_function_names};
use pyo3::exceptions::{PyNotImplementedError, PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict, PyString};
use std::sync::Arc;

fn to_py_err(error: ModelCallError) -> PyErr {
    match error {
        ModelCallError::Routing(message) => PyValueError::new_err(message),
        ModelCallError::NotImplemented(what) => PyNotImplementedError::new_err(what),
        other @ ModelCallError::Infrastructure { .. } => PyRuntimeError::new_err(other.to_string()),
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

fn to_py_json(py: Python<'_>, value: &serde_json::Value) -> PyResult<Py<PyAny>> {
    let text = serde_json::to_string(value).map_err(|error| PyValueError::new_err(error.to_string()))?;
    Ok(py.import("json")?.call_method1("loads", (text,))?.unbind())
}

fn result_dict<'py>(py: Python<'py>, result: &CalculationResult) -> PyResult<Bound<'py, PyDict>> {
    let json = |value: serde_json::Result<serde_json::Value>| -> PyResult<Py<PyAny>> {
        let value = value.map_err(|error| PyValueError::new_err(error.to_string()))?;
        to_py_json(py, &value)
    };
    let dict = PyDict::new(py);
    dict.set_item(result_keys::OUTPUTS, json(serde_json::to_value(&result.outputs))?)?;
    dict.set_item(result_keys::TYPED_OUTPUTS, json(serde_json::to_value(&result.typed_outputs))?)?;
    dict.set_item(result_keys::EFFECTIVE_INPUTS, json(serde_json::to_value(&result.effective_inputs))?)?;
    dict.set_item(result_keys::INVOCATIONS, json(serde_json::to_value(&result.invocations))?)?;
    dict.set_item(result_keys::TIMINGS, json(serde_json::to_value(&result.timings))?)?;
    dict.set_item(result_keys::FAULTS, json(serde_json::to_value(&result.faults))?)?;
    dict.set_item(result_keys::SOLVERS, json(serde_json::to_value(&result.solvers))?)?;
    dict.set_item(result_keys::DIAGNOSTICS, json(serde_json::to_value(&result.diagnostics))?)?;
    dict.set_item(result_keys::SESSION_REUSE, json(serde_json::to_value(&result.session_reuse))?)?;
    let memo = match &result.call_memo {
        Some(report) => serde_json::to_value(report),
        None => Ok(serde_json::Value::Object(serde_json::Map::new())),
    };
    dict.set_item(result_keys::CALL_MEMO, json(memo)?)?;
    dict.set_item(result_keys::COMPILED, json(serde_json::to_value(&result.compiled))?)?;
    Ok(dict)
}

/// One calculation request over a pinned model package.
#[pyclass(name = "ModelSession", module = "formualizer.formualizer_py")]
pub struct PyModelSession {
    session: ModelSession,
}

#[pymethods]
impl PyModelSession {
    /// `package_json`: the package JSON (`package.py`). `context`: a dict or
    /// JSON text with `now` (timezone-aware ISO 8601), `operation`,
    /// `random_seed`, `deadline_seconds`, `max_depth` and `flags`.
    #[new]
    fn new(package_json: &str, context: &Bound<'_, PyAny>) -> PyResult<Self> {
        let package = ModelPackage::from_json(package_json).map_err(|error| PyValueError::new_err(error.to_string()))?;
        let spec: CalculationContextSpec = serde_json::from_str(&json_text(context)?)
            .map_err(|error| PyValueError::new_err(error.to_string()))?;
        let context = CalculationContext::from_spec(&spec).map_err(to_py_err)?;
        Ok(Self { session: ModelSession::new(Arc::new(package), context) })
    }

    /// Calculate one scenario; returns the result dict.
    fn calculate<'py>(&mut self, py: Python<'py>, inputs: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyDict>> {
        let inputs: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&json_text(inputs)?)
            .map_err(|error| PyValueError::new_err(error.to_string()))?;
        let result = self.session.calculate(&inputs).map_err(to_py_err)?;
        result_dict(py, &result)
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
    m.add_function(wrap_pyfunction!(register_import_aliases, m)?)?;
    m.add_function(wrap_pyfunction!(call_model_function_names, m)?)?;
    Ok(())
}
