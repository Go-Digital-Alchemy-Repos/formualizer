//! Lane A: one request (`runtime.CalculationSession`): load or reuse,
//! admission, evaluate, goal seek, read, timings, invocations, deadline
//! watchdog, sealing. Lane 0 placeholder: construction is real; `calculate`
//! returns `ModelCallError::NotImplemented`.

use serde_json::{Map, Value};
use std::sync::Arc;

use crate::evaluator::{CancelToken, ChildEvaluator, ChildOutcome, ChildRequest, CompiledChildHook};
use crate::receipt::CalculationResult;
use crate::spec::ModelPackage;
use crate::{CalculationContext, ModelCallError};

pub struct ModelSession {
    package: Arc<ModelPackage>,
    context: CalculationContext,
    compiled_child: Option<Arc<dyn CompiledChildHook>>,
    cancel: CancelToken,
}

impl ModelSession {
    pub fn new(package: Arc<ModelPackage>, context: CalculationContext) -> Self {
        Self { package, context, compiled_child: None, cancel: CancelToken::new() }
    }

    /// Consulted before engine evaluation when `context.flags.compiled`.
    pub fn with_compiled_child(mut self, hook: Arc<dyn CompiledChildHook>) -> Self {
        self.compiled_child = Some(hook);
        self
    }

    pub fn package(&self) -> &Arc<ModelPackage> {
        &self.package
    }

    pub fn context(&self) -> &CalculationContext {
        &self.context
    }

    pub fn compiled_child(&self) -> Option<&Arc<dyn CompiledChildHook>> {
        self.compiled_child.as_ref()
    }

    /// Thread-safe: cancel every workbook this request has open.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn cancel_token(&self) -> &CancelToken {
        &self.cancel
    }

    /// Calculate the parent for one scenario. `inputs` are client wire values
    /// (decoded by `ports`, `decode_wire=True`).
    pub fn calculate(&mut self, _inputs: &Map<String, Value>) -> Result<CalculationResult, ModelCallError> {
        Err(ModelCallError::NotImplemented("session::ModelSession::calculate"))
    }

    /// Evidence for a failed run (`runtime.calculate`'s except branch).
    pub fn partial_result(&self) -> CalculationResult {
        CalculationResult::default()
    }
}

/// Flight evaluation for Lane B: a sub-session over the same package with
/// prefetch off.
impl ChildEvaluator for ModelSession {
    fn evaluate_child(&self, _request: &ChildRequest) -> ChildOutcome {
        ChildOutcome::failed(ModelCallError::NotImplemented("session::ModelSession::evaluate_child"))
    }
}
