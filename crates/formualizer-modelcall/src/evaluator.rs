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

// ------------------------------------------------------------------ Architecture B seam (GOD-383 archB WP0)

/// One cell a compiled run is asked for: `(sheet name, row, col)`, 1-based,
/// the addressing of `Workbook::get_value` (and of `SolveModel::get_value`).
pub type CellAddress = (String, u32, u32);

/// Value-free statistics of one compiled run (`cv_run_stats`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompiledRunStats {
    pub xcalls: u64,
    pub guard_views: u64,
    pub guard_probes: u64,
    /// `None` when no guard view ran (the module's `i64::MIN` sentinel).
    pub guard_max_row_minus_limit: Option<i64>,
    pub t_fresh_s: f64,
    pub t_run_s: f64,
}

/// Cell reads against a finished compiled run's store (`cv_run_read_rect`).
/// An implementation groups the addresses by sheet and reads each sheet's
/// bounding rectangle once. Values follow the architecture-B value law
/// (`docs/modelcall_contract.md`): Blank -> Empty, Num -> Number, Bool ->
/// Boolean, Str -> Text, Err -> Error(kind).
pub trait CompiledCells: Send {
    /// One value per address, in the order given.
    fn read_cells(&self, cells: &[CellAddress]) -> Result<Vec<LiteralValue>, ModelCallError>;
}

/// A finished run's cells shared with a report capture that may outlive the
/// session call (the Python binding's `formualizer.CompiledCells`). The
/// `Mutex` makes the `Send`-only store shareable; no lock is held across a
/// module run (the run has finished when the cells exist).
pub type SharedCompiledCells = Arc<Mutex<Box<dyn CompiledCells>>>;

/// A finished compiled run of a whole workbook (a compiled parent): its
/// declared outputs, a handle for further cell reads (report capture), the
/// route record and the statistics. Holding it keeps the module's run store
/// alive; dropping it frees the store (`cv_run_free`).
pub struct CompiledRun {
    /// Casefolded output key -> typed matrix, in `spec.outputs` order.
    pub outputs: Vec<(String, ChildMatrix)>,
    /// Route record for the receipt's `compiled.routes` (`"compiled"`).
    pub route: Value,
    pub stats: CompiledRunStats,
    pub cells: Box<dyn CompiledCells>,
}

impl CompiledRun {
    pub fn read_cells(&self, cells: &[CellAddress]) -> Result<Vec<LiteralValue>, ModelCallError> {
        self.cells.read_cells(cells)
    }
}

impl fmt::Debug for CompiledRun {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompiledRun")
            .field("outputs", &self.outputs.iter().map(|(key, _)| key.as_str()).collect::<Vec<_>>())
            .field("route", &self.route)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

/// The nested-call surface a compiled module's `MDL.CALLMODEL` sites use:
/// the session's router (`ModelCallRouter::nested(core, [parent identity])`),
/// so children are compiled or engine, with memo and prefetch unchanged.
/// Arguments follow the module -> router law: a scalar is its
/// `LiteralValue`, a rows argument is `LiteralValue::Array`.
pub trait CompiledXcall {
    fn call(
        &mut self,
        target: &LiteralValue,
        block: &LiteralValue,
        output: &LiteralValue,
        tail: &[LiteralValue],
    ) -> Result<ChildMatrix, ModelCallError>;
}

/// What a compiled parent did with one request.
#[derive(Debug)]
pub enum ParentAttempt {
    /// The module ran; the session projects `run.outputs`.
    Compiled(CompiledRun),
    /// No run (or a discarded one): the session builds a fresh engine parent
    /// and records `route` (`engine:<reason>` or `fallback:<reason>`) under
    /// the `parent` key of `compiled.routes`.
    Declined { route: Value },
}

/// Optional compiled parent (architecture B item (c)), consulted by
/// `ModelSession::run` before the engine parent is loaded. `Err` is an
/// infrastructure fault (a router fault during the run, a module panic),
/// not a decline.
///
/// Session contract (package C, `docs/modelcall_contract.md` "Compiled
/// parent"): the session calls `serves` first and consults the parent only
/// when it returns true, the context's `compiled` flag is on and the static
/// checks pass (no goal seek, date system 1900, no calculation
/// normalizations, scalar inputs, operation `client` or an admitted
/// `report`). `inputs` are the admitted parent inputs, declared key ->
/// value, dates as 1900 serials and ints as numbers
/// (`ports::parent_port_literal`), in the admission's effective order.
/// `run` must not record the parent's route in `CompiledChildHook::report`'s
/// `routes`: the session records it under the receipt's `compiled.parent`.
pub trait CompiledParent: Send + Sync {
    /// Whether this registry holds a compiled parent for `workbook_sha256`
    /// (an entry with `role: parent`). A `false` leaves the request exactly
    /// as it was before architecture B: no `compiled.parent` key.
    fn serves(&self, _workbook_sha256: &str) -> bool {
        true
    }

    fn run(
        &self,
        spec: &ModelSpec,
        inputs: &[(String, LiteralValue)],
        context: &CalculationContext,
        xcall: &mut dyn CompiledXcall,
    ) -> Result<ParentAttempt, ModelCallError>;

    /// `{calls, routes}` for the parent, as `CompiledChildHook::report`.
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
