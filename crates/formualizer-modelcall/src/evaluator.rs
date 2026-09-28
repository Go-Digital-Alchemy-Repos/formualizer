//! The seams between lanes: child evaluation, the compiled-child hook, the
//! goal-seek model and cancellation.

use formualizer_common::LiteralValue;
use serde_json::{Map, Value};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::event::ModelCallEvent;
use crate::key::InputPairs;
use crate::receipt::Timings;
use crate::spec::{CellRange, ModelPackage, ModelSpec, PortLocation};
use crate::{CalculationContext, ModelCallError};

/// A child's typed result: rows of engine values (`read_typed_matrix`).
pub type ChildMatrix = Vec<Vec<LiteralValue>>;

type CancelHook = Box<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct CancelInner {
    cancelled: AtomicBool,
    hooks: Mutex<Vec<CancelHook>>,
}

/// Thread-safe cancellation shared by a request and its flights.
///
/// An evaluator registers one hook per workbook it opens (`on_cancel`), each
/// calling that workbook's `cancel()`; `cancel()` flips the flag and runs every
/// hook. A hook registered after cancellation runs at once. This is the only
/// way anything outside an evaluation touches its workbooks.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<CancelInner>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst)
    }

    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::SeqCst);
        if let Ok(hooks) = self.0.hooks.lock() {
            for hook in hooks.iter() {
                hook();
            }
        }
    }

    /// Register a hook; it runs exactly once, now if already cancelled.
    pub fn on_cancel(&self, hook: impl Fn() + Send + Sync + 'static) {
        let Ok(mut hooks) = self.0.hooks.lock() else { return };
        // Checked under the lock `cancel` takes after setting the flag, so a
        // hook is either in the list `cancel` runs or run here, never both.
        if self.is_cancelled() {
            drop(hooks);
            hook();
        } else {
            hooks.push(Box::new(hook));
        }
    }
}

impl fmt::Debug for CancelToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("CancelToken").field("cancelled", &self.is_cancelled()).finish()
    }
}

/// One child call evaluated as its own sub-request (a prefetch flight, or any
/// caller that wants the Python `CalculationSession(package, replace(context,
/// xcall_prefetch=False)).calculate_child(...)` semantics): its own invocation
/// list (the flight's started event is NOT included; the caller owns it), own
/// memo, prefetch off, own timings. Owned so it can move to a thread.
#[derive(Debug, Clone)]
pub struct ChildRequest {
    pub package: Arc<ModelPackage>,
    /// Key into `package.children` (the child's version id).
    pub child_version: String,
    pub inputs: InputPairs,
    /// Output selector text (the router has checked it is text).
    pub output: String,
    /// Caller stack plus the child's `identity:sha256`.
    pub stack: Vec<String>,
    /// The request's context with `flags.prefetch = false`.
    pub context: CalculationContext,
    pub cancel: CancelToken,
}

impl ChildRequest {
    pub fn spec(&self) -> Option<&ModelSpec> {
        self.package.children.get(&self.child_version)
    }
}

/// What a sub-request produced: its result and everything its receipt needs.
#[derive(Debug, Clone)]
pub struct ChildOutcome {
    pub result: Result<ChildMatrix, ModelCallError>,
    /// Sealed events of the sub-request, indexed from 0 in its own list.
    pub invocations: Vec<ModelCallEvent>,
    pub faults: Vec<ModelCallEvent>,
    pub timings: Timings,
    pub solvers: Vec<Map<String, Value>>,
    pub diagnostics: Vec<String>,
}

impl ChildOutcome {
    pub fn failed(error: ModelCallError) -> Self {
        Self {
            result: Err(error),
            invocations: Vec::new(),
            faults: Vec::new(),
            timings: Timings::default(),
            solvers: Vec::new(),
            diagnostics: Vec::new(),
        }
    }
}

/// Evaluates a child as a separate sub-request. Implemented by Lane A's
/// session; consumed by Lane B's prefetch. Must be callable from any thread.
///
/// Reentrancy: an implementation loads or reuses a *separate* `Workbook` for
/// the child and never touches the caller's workbook.
pub trait ChildEvaluator: Send + Sync {
    fn evaluate_child(&self, request: &ChildRequest) -> ChildOutcome;
}

/// What the compiled-child hook did with one call.
#[derive(Debug, Clone, Default)]
pub struct CompiledAttempt {
    /// `Some` answers the call; `None` declines (the engine path runs).
    pub matrix: Option<ChildMatrix>,
    /// Route record stored on the call's event (`event['route']`).
    pub route: Option<Value>,
}

/// Optional compiled child (`compiled.adapter.CompiledRoute.attempt`), a
/// Python callable in production, consulted before engine evaluation when
/// `flags.compiled` is on.
pub trait CompiledChildHook: Send + Sync {
    fn attempt(
        &self,
        spec: &ModelSpec,
        inputs: &[(String, LiteralValue)],
        output: &PortLocation,
        stack: &[String],
    ) -> Result<CompiledAttempt, ModelCallError>;

    /// `CompiledRoute.report()`: `{calls, routes}`.
    fn report(&self) -> Map<String, Value>;
}

/// Scope of a defined name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameScope {
    Workbook,
    Sheet(String),
}

/// One defined name as the workbook reports it (`_named_range_rows`).
#[derive(Debug, Clone, PartialEq)]
pub struct DefinedRange {
    pub name: String,
    pub scope: NameScope,
    /// `None` for a name that is not a bounded rectangle.
    pub range: Option<CellRange>,
}

/// The workbook surface goal seek needs (Lane A implements it over a child's
/// or the parent's own `Workbook`; Lane B's tests implement it over a fake).
pub trait SolveModel {
    fn get_value(&self, sheet: &str, row: u32, col: u32) -> Result<LiteralValue, ModelCallError>;
    fn set_value(&mut self, sheet: &str, row: u32, col: u32, value: LiteralValue) -> Result<(), ModelCallError>;
    fn evaluate_all(&mut self) -> Result<(), ModelCallError>;
    fn defined_ranges(&self) -> Result<Vec<DefinedRange>, ModelCallError>;

    /// The cell's formula text as the workbook spells it (`Workbook.get_formula`),
    /// `None` for a cell without a formula. Goal seek reads its `Target cell`
    /// and `By changing` parameters this way (`_resolve_solve_reference`).
    ///
    /// Lane B addition (additive, default provided so existing implementors
    /// compile): the default fails, which makes every block that needs it an
    /// `engine_error` failure, so an implementor over a real workbook must
    /// override it.
    fn get_formula(&self, _sheet: &str, _row: u32, _col: u32) -> Result<Option<String>, ModelCallError> {
        Err(ModelCallError::NotImplemented("SolveModel::get_formula"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn cancel_runs_hooks_including_late_ones() {
        let token = CancelToken::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let early = calls.clone();
        token.on_cancel(move || {
            early.fetch_add(1, Ordering::SeqCst);
        });
        let shared = token.clone();
        shared.cancel();
        assert!(token.is_cancelled());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let late = calls.clone();
        token.on_cancel(move || {
            late.fetch_add(10, Ordering::SeqCst);
        });
        assert_eq!(calls.load(Ordering::SeqCst), 11);
    }
}
