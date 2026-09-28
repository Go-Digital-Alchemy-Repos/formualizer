//! Lane A: one request (`runtime.CalculationSession`): load, admission,
//! evaluate, goal seek, read, timings, invocations, deadline watchdog and the
//! evidence of a failed run.
//!
//! Every workbook this request opens is its own `Workbook`: the parent, and
//! one fresh load per child call. A call handler (`router::ModelCallRouter`)
//! never touches the workbook it runs inside; the only thing anything outside
//! an evaluation does to a workbook is set its cancel flag (the deadline
//! watchdog, a fault in a child call, `ModelSession::cancel`).
//!
//! Not ported here (they stay in Python this round): the session pool and
//! warm (`session_reuse` is always empty and sealing adds no held or inherited
//! events), engine identity verification, and the `diagnostic` operation's
//! inspection capture (Python reads it from `ModelSession.workbook()`).

use chrono::Utc;
use formualizer_common::{ExcelError, ExcelErrorKind, LiteralValue};
use formualizer_eval::engine::named_range::{NameScope as EngineScope, NamedDefinition};
use formualizer_eval::engine::{CancelToken as EngineCancel, CycleDetection, DeterministicMode, EvalConfig};
use formualizer_eval::timezone::TimeZoneSpec;
use formualizer_workbook::{CalamineAdapter, IoError, LoadStrategy, Workbook, WorkbookConfig, XlsxPathSource};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::thread::JoinHandle;
use std::time::Instant;

use crate::evaluator::{
    CancelToken, ChildEvaluator, ChildMatrix, ChildOutcome, ChildRequest, CompiledChildHook, DefinedRange, NameScope,
    SolveModel,
};
use crate::event::{CallStatus, ModelCallEvent};
use crate::goal_seek::run_goal_seeks;
use crate::import_boundary::GOAL_SEEK_BLOCK_PREFIX;
use crate::memo::ModelCallMemo;
use crate::ports::{PortError, PortSession, WireValue, py_repr, read_typed_matrix, wire_to_port_value};
use crate::prefetch::{Prefetcher, sibling_plan};
use crate::receipt::{CalculationResult, MemoReport, PortValue, TimingValue, Timings, timing_keys};
use crate::router::{ModelCallRouter, register_call_handler, with_nested_router};
use crate::spec::{CellRange, ModelPackage, ModelSpec, OrderedMap};
use crate::{CalculationContext, ModelCallError, Operation};

/// A workbook the session shares with its caller (the Python binding wraps
/// the same `Arc` as a `Workbook` object for report capture).
pub type SharedWorkbook = Arc<RwLock<Workbook>>;

/// Loads a pinned workbook (`sessions.load_workbook_session`'s load step).
pub trait WorkbookSource: Send + Sync {
    fn load(&self, spec: &ModelSpec, context: &CalculationContext) -> Result<Workbook, ModelCallError>;
}

/// The workbook configuration the runtime loads with: the binding's
/// `WorkbookConfig(eval_config=EvaluationConfig())` with `enable_parallel =
/// False`, `cycle_detection = 'runtime'` and `workbook_seed = seed`
/// (`merge_python_eval_config` over the interactive base).
pub fn runtime_workbook_config(seed: u64) -> WorkbookConfig {
    let mut python = EvalConfig::default();
    python.cycle.detection = CycleDetection::Runtime;
    python.enable_parallel = false;
    python.workbook_seed = seed;
    let mut config = WorkbookConfig::interactive();
    let base = &mut config.eval;
    base.enable_parallel = python.enable_parallel;
    base.max_threads = python.max_threads;
    base.range_expansion_limit = python.range_expansion_limit;
    base.workbook_seed = python.workbook_seed;
    base.case_sensitive_names = python.case_sensitive_names;
    base.case_sensitive_tables = python.case_sensitive_tables;
    base.warmup = python.warmup.clone();
    base.date_system = python.date_system;
    base.formula_plane_mode = python.formula_plane_mode;
    base.evaluation_budgets = python.evaluation_budgets.clone();
    base.cycle = python.cycle;
    base.speculative_chain = python.speculative_chain;
    base.spec_chain_read_guard = python.spec_chain_read_guard;
    config
}

/// Loads `spec.workbook_path` with Calamine after checking its sha256, then
/// applies the descriptor's `calculation_normalizations`.
#[derive(Debug, Clone, Copy, Default)]
pub struct PathWorkbookSource;

impl WorkbookSource for PathWorkbookSource {
    fn load(&self, spec: &ModelSpec, context: &CalculationContext) -> Result<Workbook, ModelCallError> {
        let path = std::path::Path::new(&spec.workbook_path);
        let bytes = std::fs::read(path).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => ModelCallError::infrastructure(
                "FileNotFoundError",
                format!("[Errno 2] No such file or directory: {}", py_repr(&spec.workbook_path)),
            ),
            _ => ModelCallError::infrastructure("OSError", error.to_string()),
        })?;
        let digest = Sha256::digest(&bytes);
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        if hex != spec.workbook_sha256 {
            return Err(ModelCallError::infrastructure(
                "ValueError",
                "workbook bytes differ from pinned package identity",
            ));
        }
        drop(bytes);
        let adapter = CalamineAdapter::open_path_with_source(path, XlsxPathSource::SharedFile).map_err(|error| {
            ModelCallError::infrastructure("OSError", format!("open failed with XlsxPathSource.SHARED_FILE: {error}"))
        })?;
        let mut workbook =
            Workbook::from_reader(adapter, LoadStrategy::EagerAll, runtime_workbook_config(context.random_seed))
                .map_err(|error| ModelCallError::infrastructure("OSError", format!("load failed: {error}")))?;
        apply_normalization_edits(&mut workbook, spec)?;
        Ok(workbook)
    }
}

/// `pdf_export.adapter.apply_normalization_edits(workbook, descriptor['calculation_normalizations'])`.
pub fn apply_normalization_edits(workbook: &mut Workbook, spec: &ModelSpec) -> Result<(), ModelCallError> {
    let Some(edits) = spec.descriptor.get("calculation_normalizations").and_then(Value::as_array) else {
        return Ok(());
    };
    for edit in edits {
        let text = |key: &str| edit.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
        let number = |key: &str| {
            edit.get(key).and_then(Value::as_u64).and_then(|value| u32::try_from(value).ok()).unwrap_or_default()
        };
        workbook
            .set_formula(&text("sheet"), number("row"), number("col"), &text("formula"))
            .map_err(|error| ModelCallError::infrastructure("RuntimeError", error.to_string()))?;
    }
    Ok(())
}

/// Report capture for `operation = report` (`pdf_export.snapshot`), supplied
/// by the Python binding. `prepare` runs after admission and before
/// evaluation (`prepare_report_conditions`); `capture` runs after the outputs
/// are read and returns the report's diagnostics.
pub trait ReportHook: Send + Sync {
    fn prepare(&self, workbook: &SharedWorkbook) -> Result<(), ModelCallError>;
    fn capture(
        &self,
        workbook: &SharedWorkbook,
        outputs: &OrderedMap<PortValue>,
    ) -> Result<Vec<String>, ModelCallError>;
}

fn engine_error(error: IoError) -> ModelCallError {
    match error {
        IoError::Engine(error) => ModelCallError::infrastructure("ExcelEvaluationError", error.to_string()),
        other => ModelCallError::infrastructure("RuntimeError", other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Per-request state shared with the call handlers
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub(crate) struct RunState {
    pub(crate) invocations: Vec<ModelCallEvent>,
    /// Indices into `invocations` of faulted events (Python appends the same
    /// dict to `faults`, so a fault event is read back from the list).
    pub(crate) fault_indices: Vec<usize>,
    pub(crate) solvers: Vec<Map<String, Value>>,
    pub(crate) diagnostics: Vec<String>,
    pub(crate) timings: Timings,
    pub(crate) memo: Option<ModelCallMemo>,
}

impl RunState {
    pub(crate) fn faults(&self) -> Vec<ModelCallEvent> {
        self.fault_indices.iter().filter_map(|index| self.invocations.get(*index).cloned()).collect()
    }

    fn first_fault_error(&self) -> Option<String> {
        self.fault_indices
            .first()
            .and_then(|index| self.invocations.get(*index))
            .map(|event| event.error.clone().unwrap_or_default())
    }
}

/// One request's core: what `CalculationSession` holds that its routers use.
pub(crate) struct RunCore {
    pub(crate) package: Arc<ModelPackage>,
    pub(crate) context: CalculationContext,
    source: Arc<dyn WorkbookSource>,
    compiled: Option<Arc<dyn CompiledChildHook>>,
    state: Mutex<RunState>,
    pub(crate) prefetch: Mutex<Option<Prefetcher>>,
    active: Mutex<Vec<(u64, Arc<AtomicBool>)>>,
    next_id: AtomicU64,
    deadline_cancelled: AtomicBool,
    pub(crate) closed: AtomicBool,
    cancel: CancelToken,
}

/// A loaded, tracked workbook and its port session.
pub(crate) struct Loaded {
    pub(crate) workbook: Workbook,
    pub(crate) ports: PortSession,
    pub(crate) flag: Arc<AtomicBool>,
    pub(crate) id: u64,
}

struct Watchdog {
    stop: mpsc::Sender<()>,
    handle: JoinHandle<()>,
}

impl Watchdog {
    fn stop(self) {
        let _ = self.stop.send(());
        let _ = self.handle.join();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl RunCore {
    pub(crate) fn new(
        package: Arc<ModelPackage>,
        context: CalculationContext,
        source: Arc<dyn WorkbookSource>,
        compiled: Option<Arc<dyn CompiledChildHook>>,
        cancel: CancelToken,
    ) -> Self {
        let mut timings = Timings::base();
        let compiled = if context.flags.compiled {
            timings.set(timing_keys::COMPILED_SECONDS, TimingValue::Seconds(0.0));
            compiled
        } else {
            None
        };
        let memo = context.flags.call_memo.then(ModelCallMemo::new);
        Self {
            package,
            context,
            source,
            compiled,
            state: Mutex::new(RunState { timings, memo, ..RunState::default() }),
            prefetch: Mutex::new(None),
            active: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(0),
            deadline_cancelled: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            cancel,
        }
    }

    pub(crate) fn state(&self) -> MutexGuard<'_, RunState> {
        lock(&self.state)
    }

    /// Build the sibling prefetcher when the flag, the memo and a plan allow.
    fn init_prefetch(&self, evaluator: Arc<dyn ChildEvaluator>) {
        if !self.context.prefetch_enabled() {
            return;
        }
        if let Ok(Some(plan)) = sibling_plan(&self.package.parent) {
            *lock(&self.prefetch) =
                Some(Prefetcher::new(plan, self.package.clone(), self.context.clone(), evaluator));
        }
    }

    fn start_watchdog(self: &Arc<Self>) -> Option<Watchdog> {
        let deadline = self.context.deadline?;
        let (stop, wait) = mpsc::channel::<()>();
        let core = Arc::clone(self);
        let handle = std::thread::Builder::new()
            .name("calculation-deadline".into())
            .spawn(move || {
                let timeout = deadline.saturating_duration_since(Instant::now());
                if matches!(wait.recv_timeout(timeout), Err(RecvTimeoutError::Timeout)) {
                    core.deadline_cancelled.store(true, Ordering::SeqCst);
                    for (_, flag) in lock(&core.active).iter() {
                        flag.store(true, Ordering::SeqCst);
                    }
                }
            })
            .ok()?;
        Some(Watchdog { stop, handle })
    }

    fn track(&self, flag: &Arc<AtomicBool>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        lock(&self.active).push((id, flag.clone()));
        let hook = flag.clone();
        self.cancel.on_cancel(move || hook.store(true, Ordering::SeqCst));
        id
    }

    /// `_release`: stop watching the workbook and reset its cancel flag.
    pub(crate) fn release(&self, id: u64, flag: &AtomicBool) {
        lock(&self.active).retain(|(existing, _)| *existing != id);
        flag.store(false, Ordering::SeqCst);
    }

    fn release_all(&self) {
        let released: Vec<_> = std::mem::take(&mut *lock(&self.active));
        for (_, flag) in released {
            flag.store(false, Ordering::SeqCst);
        }
    }

    pub(crate) fn check_deadline(&self) -> Result<(), ModelCallError> {
        self.context.check_deadline()
    }

    fn add_seconds(&self, key: &str, started: Instant) {
        self.state().timings.add_seconds(key, started.elapsed().as_secs_f64());
    }

    fn count_writes(&self, ports: &PortSession) {
        if let Some(stats) = ports.write_stats {
            let mut state = self.state();
            for (key, value) in stats.entries() {
                state.timings.add_count(key, value);
            }
        }
    }

    /// `resolve_child(target)`: the child's version id and spec.
    pub(crate) fn resolve_child(&self, target: &LiteralValue) -> Result<(String, &ModelSpec), ModelCallError> {
        let LiteralValue::Text(target) = target else {
            return Err(ModelCallError::routing("child target must be text"));
        };
        let version = self
            .package
            .child_routes
            .get(target)
            .ok_or_else(|| ModelCallError::routing("child target is not in pinned package routes"))?;
        let spec = self
            .package
            .children
            .get(version)
            .ok_or_else(|| ModelCallError::routing("pinned child package is unavailable"))?;
        Ok((version.clone(), spec))
    }

    /// `load(spec, stack)`: a fresh load with the call function registered
    /// for this caller stack, tracked for cancellation, graph prepared.
    pub(crate) fn load(self: &Arc<Self>, spec: &ModelSpec, stack: &[String]) -> Result<Loaded, ModelCallError> {
        self.check_deadline()?;
        let started = Instant::now();
        let mut workbook = self.source.load(spec, &self.context)?;
        workbook
            .set_deterministic_mode(DeterministicMode::Enabled {
                timestamp_utc: self.context.now.with_timezone(&Utc),
                timezone: TimeZoneSpec::FixedOffsetSeconds(0),
            })
            .map_err(engine_error)?;
        let flag = Arc::new(AtomicBool::new(false));
        let router = ModelCallRouter::bound(Arc::clone(self), stack.to_vec(), flag.clone());
        register_call_handler(&mut workbook, router)?;
        let id = self.track(&flag);
        let prepared = (|| {
            let preparation_started = Instant::now();
            workbook.prepare_graph_all().map_err(engine_error)?;
            self.add_seconds("preparation_seconds", preparation_started);
            PortSession::new(&mut workbook, spec, true, self.context.flags.skip_unchanged_writes)
                .map_err(PortError::into_error)
        })();
        match prepared {
            Ok(ports) => {
                self.add_seconds("load_seconds", started);
                Ok(Loaded { workbook, ports, flag, id })
            }
            Err(error) => {
                self.release(id, &flag);
                Err(error)
            }
        }
    }

    fn raise_for_cancellation(&self, error: &ExcelError) -> Result<(), ModelCallError> {
        if error.kind != ExcelErrorKind::Cancelled {
            return Ok(());
        }
        if self.deadline_cancelled.load(Ordering::SeqCst) {
            return Err(ModelCallError::deadline());
        }
        if let Some(first) = self.state().first_fault_error() {
            return Err(ModelCallError::infrastructure(
                "CallbackInfrastructureError",
                format!("child callback infrastructure fault: {first}"),
            ));
        }
        Ok(())
    }

    fn fault_after(&self, suffix: Option<&str>) -> Result<(), ModelCallError> {
        let state = self.state();
        if let Some(first) = state.first_fault_error() {
            let message = match suffix {
                Some(suffix) => suffix.to_owned(),
                None => format!("child callback infrastructure fault: {first}"),
            };
            return Err(ModelCallError::infrastructure("CallbackInfrastructureError", message));
        }
        Ok(())
    }

    /// `evaluate(workbook)`.
    pub(crate) fn evaluate(&self, workbook: &mut Workbook, flag: &Arc<AtomicBool>) -> Result<(), ModelCallError> {
        self.check_deadline()?;
        let started = Instant::now();
        // `Workbook.evaluate_all` resets the flag before it starts.
        flag.store(false, Ordering::SeqCst);
        let outcome = workbook.evaluate_all_cancellable(EngineCancel::from_flag(flag.clone()));
        self.add_seconds("evaluation_seconds", started);
        match outcome {
            Ok(_) => {}
            Err(IoError::Engine(error)) => {
                self.raise_for_cancellation(&error)?;
                return Err(ModelCallError::infrastructure("ExcelEvaluationError", error.to_string()));
            }
            Err(other) => return Err(ModelCallError::infrastructure("RuntimeError", other.to_string())),
        }
        self.check_deadline()?;
        self.fault_after(None)
    }

    /// `solve(workbook, identity)`: goal seek over the evaluated workbook.
    pub(crate) fn solve(&self, workbook: &mut Workbook, flag: &Arc<AtomicBool>, identity: &str) -> Result<(), ModelCallError> {
        let started = Instant::now();
        let mut model = WorkbookSolveModel { workbook, flag: flag.clone(), cancelled: None };
        let has_blocks = match model.defined_ranges() {
            Ok(ranges) => ranges
                .iter()
                .any(|range| range.scope == NameScope::Workbook && range.name.starts_with(GOAL_SEEK_BLOCK_PREFIX)),
            Err(error) => {
                self.add_seconds("solver_seconds", started);
                return Err(error);
            }
        };
        // Python's run_solves finds no block and returns nothing; the goal
        // seek (Lane B) is only entered when a block exists.
        let (notes, records) = if has_blocks {
            match run_goal_seeks(&mut model) {
                Ok(run) => (run.notes, run.records),
                Err(failure) => {
                    {
                        let mut state = self.state();
                        state.solvers.extend(failure.records.into_iter().map(|record| with_workbook(record, identity)));
                        state.diagnostics.extend(failure.notes);
                    }
                    self.add_seconds("solver_seconds", started);
                    if let Some(cancelled) = model.cancelled.take() {
                        self.raise_for_cancellation(&cancelled)?;
                    }
                    return Err(ModelCallError::infrastructure(
                        "RequiredSolverFailure",
                        "required workbook solver failed",
                    ));
                }
            }
        } else {
            (Vec::new(), Vec::new())
        };
        self.check_deadline()?;
        self.fault_after(Some("child callback infrastructure fault during solve"))?;
        {
            let mut state = self.state();
            state.solvers.extend(records.into_iter().map(|record| with_workbook(record, identity)));
            state.diagnostics.extend(notes);
        }
        self.add_seconds("solver_seconds", started);
        Ok(())
    }

    fn router_event_for(&self, event: Option<usize>, identity: &str) -> Option<usize> {
        let index = event?;
        let state = self.state();
        let last = state.invocations.last()?;
        (last.index == index && last.status == CallStatus::Started && last.child.as_deref() == Some(identity))
            .then_some(index)
    }

    /// `calculate_child(spec, inputs, output, stack)`. `event` is the router's
    /// started event, whose `route` the compiled hook may set.
    pub(crate) fn calculate_child(
        self: &Arc<Self>,
        spec: &ModelSpec,
        inputs: &[(String, LiteralValue)],
        output: &LiteralValue,
        stack: &[String],
        event: Option<usize>,
    ) -> Result<ChildMatrix, ModelCallError> {
        let started = Instant::now();
        let LiteralValue::Text(selector) = output else {
            return Err(ModelCallError::routing("child output selector must be text"));
        };
        let location = spec.resolve_output(selector).ok_or_else(|| {
            ModelCallError::routing(format!(
                "child output selector is not declared: Undeclared output {}",
                py_repr(selector)
            ))
        })?;
        if let Some(hook) = &self.compiled {
            let router_event = self.router_event_for(event, &spec.model_identity());
            let attempt_started = Instant::now();
            let nested = ModelCallRouter::nested(Arc::clone(self), stack.to_vec());
            let attempt = with_nested_router(nested, || hook.attempt(spec, inputs, location, stack));
            self.add_seconds(timing_keys::COMPILED_SECONDS, attempt_started);
            let attempt = attempt?;
            if let (Some(index), Some(route)) = (router_event, attempt.route) {
                if let Some(event) = self.state().invocations.get_mut(index) {
                    event.route = Some(route);
                }
            }
            if let Some(matrix) = attempt.matrix {
                self.add_seconds("child_seconds", started);
                return Ok(matrix);
            }
        }
        let mut loaded = self.load(spec, stack)?;
        let result = (|| {
            let admission_started = Instant::now();
            let wire: Vec<(String, WireValue)> =
                inputs.iter().map(|(name, value)| (name.clone(), WireValue::from_literal(value))).collect();
            let admitted = loaded.ports.write_scenario(&mut loaded.workbook, spec, &wire, false);
            self.add_seconds("admission_seconds", admission_started);
            self.count_writes(&loaded.ports);
            admitted.map_err(|error| {
                if error.is_admission() {
                    ModelCallError::routing(format!(
                        "child inputs do not satisfy the pinned interface: {}: {}",
                        error.kind(),
                        error.message()
                    ))
                } else {
                    error.into_error()
                }
            })?;
            self.evaluate(&mut loaded.workbook, &loaded.flag)?;
            let caller = stack.last().map(String::as_str).unwrap_or_default();
            self.solve(&mut loaded.workbook, &loaded.flag, caller)?;
            let matrix = read_typed_matrix(&loaded.workbook, &location.range).map_err(PortError::into_error)?;
            if matrix.iter().flatten().any(|value| matches!(value, LiteralValue::Pending)) {
                return Err(ModelCallError::infrastructure("RuntimeError", "child evaluation returned Pending"));
            }
            Ok(matrix)
        })();
        self.release(loaded.id, &loaded.flag);
        drop(loaded);
        if result.is_ok() {
            self.add_seconds("child_seconds", started);
        }
        result
    }

    /// `xcall_memo_report()`: memo counters plus the prefetch report.
    fn memo_report(&self) -> Option<MemoReport> {
        let prefetched = lock(&self.prefetch).as_ref().and_then(Prefetcher::report);
        let mut report = self.state().memo.as_ref().and_then(ModelCallMemo::report)?;
        report.prefetch = prefetched;
        Some(report)
    }

    fn compiled_report(&self) -> Map<String, Value> {
        if !self.context.flags.compiled {
            return Map::new();
        }
        match &self.compiled {
            Some(hook) => hook.report(),
            None => {
                let mut report = Map::new();
                report.insert("calls".into(), Value::from(0));
                report.insert("routes".into(), Value::Array(Vec::new()));
                report
            }
        }
    }

    /// `close()`: abandon flights, then leave every workbook reusable.
    fn close(&self) {
        let mut state_timings = Timings::default();
        if let Some(prefetch) = lock(&self.prefetch).as_mut() {
            prefetch.abandon(&mut state_timings);
        }
        if !state_timings.0.is_empty() {
            let mut state = self.state();
            for (key, value) in state_timings.0.0 {
                match value {
                    TimingValue::Seconds(seconds) => state.timings.add_seconds(&key, seconds),
                    TimingValue::Count(count) => state.timings.add_count(&key, count),
                }
            }
        }
        self.closed.store(true, Ordering::SeqCst);
        self.release_all();
    }
}

fn with_workbook(mut record: Map<String, Value>, identity: &str) -> Map<String, Value> {
    record.insert("workbook".into(), Value::String(identity.to_owned()));
    record
}

/// The goal-seek surface over a loaded workbook (`SolveModel`).
struct WorkbookSolveModel<'a> {
    workbook: &'a mut Workbook,
    flag: Arc<AtomicBool>,
    /// The engine error of a cancelled re-evaluation, if one happened.
    cancelled: Option<ExcelError>,
}

impl SolveModel for WorkbookSolveModel<'_> {
    fn get_value(&self, sheet: &str, row: u32, col: u32) -> Result<LiteralValue, ModelCallError> {
        Ok(self.workbook.get_value(sheet, row, col).unwrap_or(LiteralValue::Empty))
    }

    fn set_value(&mut self, sheet: &str, row: u32, col: u32, value: LiteralValue) -> Result<(), ModelCallError> {
        self.workbook
            .set_value(sheet, row, col, value)
            .map_err(|error| ModelCallError::infrastructure("RuntimeError", error.to_string()))
    }

    fn evaluate_all(&mut self) -> Result<(), ModelCallError> {
        self.flag.store(false, Ordering::SeqCst);
        match self.workbook.evaluate_all_cancellable(EngineCancel::from_flag(self.flag.clone())) {
            Ok(_) => Ok(()),
            Err(IoError::Engine(error)) => {
                let converted = ModelCallError::infrastructure("ExcelEvaluationError", error.to_string());
                if error.kind == ExcelErrorKind::Cancelled {
                    self.cancelled = Some(error);
                }
                Err(converted)
            }
            Err(other) => Err(ModelCallError::infrastructure("RuntimeError", other.to_string())),
        }
    }

    fn defined_ranges(&self) -> Result<Vec<DefinedRange>, ModelCallError> {
        let engine = self.workbook.engine();
        Ok(engine
            .named_ranges_snapshot()
            .into_iter()
            .map(|entry| {
                let scope = match entry.scope {
                    EngineScope::Workbook => NameScope::Workbook,
                    EngineScope::Sheet(id) => NameScope::Sheet(engine.sheet_name(id).to_owned()),
                };
                let range = match &entry.definition {
                    NamedDefinition::Cell(cell) => Some(CellRange {
                        sheet: engine.sheet_name(cell.sheet_id).to_owned(),
                        start_row: cell.coord.row() + 1,
                        start_col: cell.coord.col() + 1,
                        end_row: cell.coord.row() + 1,
                        end_col: cell.coord.col() + 1,
                    }),
                    NamedDefinition::Range(range) if range.start.sheet_id == range.end.sheet_id => Some(CellRange {
                        sheet: engine.sheet_name(range.start.sheet_id).to_owned(),
                        start_row: range.start.coord.row() + 1,
                        start_col: range.start.coord.col() + 1,
                        end_row: range.end.coord.row() + 1,
                        end_col: range.end.coord.col() + 1,
                    }),
                    _ => None,
                };
                DefinedRange { name: entry.name, scope, range }
            })
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Sub-requests (prefetch flights)
// ---------------------------------------------------------------------------

/// Evaluates one child call as its own sub-request (a prefetch flight): own
/// events, own memo, prefetch off, own timings and deadline watchdog.
#[derive(Clone)]
pub struct SubRequestEvaluator {
    source: Arc<dyn WorkbookSource>,
    compiled: Option<Arc<dyn CompiledChildHook>>,
}

impl SubRequestEvaluator {
    pub fn new(source: Arc<dyn WorkbookSource>, compiled: Option<Arc<dyn CompiledChildHook>>) -> Self {
        Self { source, compiled }
    }
}

impl ChildEvaluator for SubRequestEvaluator {
    fn evaluate_child(&self, request: &ChildRequest) -> ChildOutcome {
        let Some(spec) = request.spec() else {
            return ChildOutcome::failed(ModelCallError::routing("pinned child package is unavailable"));
        };
        let mut context = request.context.clone();
        context.flags.prefetch = false;
        let core = Arc::new(RunCore::new(
            request.package.clone(),
            context,
            self.source.clone(),
            self.compiled.clone(),
            request.cancel.clone(),
        ));
        let watchdog = core.start_watchdog();
        let output = LiteralValue::Text(request.output.clone());
        let result = core.calculate_child(spec, &request.inputs, &output, &request.stack, None);
        if let Some(watchdog) = watchdog {
            watchdog.stop();
        }
        core.close();
        let state = core.state();
        ChildOutcome {
            result,
            invocations: state.invocations.clone(),
            faults: state.faults(),
            timings: state.timings.clone(),
            solvers: state.solvers.clone(),
            diagnostics: state.diagnostics.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// The session
// ---------------------------------------------------------------------------

pub struct ModelSession {
    package: Arc<ModelPackage>,
    context: CalculationContext,
    compiled_child: Option<Arc<dyn CompiledChildHook>>,
    cancel: CancelToken,
    source: Arc<dyn WorkbookSource>,
    report_hook: Option<Arc<dyn ReportHook>>,
    core: Option<Arc<RunCore>>,
    workbook: Option<SharedWorkbook>,
    effective_inputs: OrderedMap<PortValue>,
    failure: Option<ModelCallError>,
}

impl ModelSession {
    pub fn new(package: Arc<ModelPackage>, context: CalculationContext) -> Self {
        Self {
            package,
            context,
            compiled_child: None,
            cancel: CancelToken::new(),
            source: Arc::new(PathWorkbookSource),
            report_hook: None,
            core: None,
            workbook: None,
            effective_inputs: OrderedMap::default(),
            failure: None,
        }
    }

    /// Consulted before engine evaluation when `context.flags.compiled`.
    pub fn with_compiled_child(mut self, hook: Arc<dyn CompiledChildHook>) -> Self {
        self.compiled_child = Some(hook);
        self
    }

    /// Replace the pinned-path loader (tests, injected factories).
    pub fn with_workbook_source(mut self, source: Arc<dyn WorkbookSource>) -> Self {
        self.source = source;
        self
    }

    /// Report capture for `operation = report`.
    pub fn with_report_hook(mut self, hook: Arc<dyn ReportHook>) -> Self {
        self.report_hook = Some(hook);
        self
    }

    pub fn set_report_hook(&mut self, hook: Option<Arc<dyn ReportHook>>) {
        self.report_hook = hook;
    }

    pub fn set_compiled_child(&mut self, hook: Option<Arc<dyn CompiledChildHook>>) {
        self.compiled_child = hook;
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

    /// The parent workbook of the last `calculate` (evaluated state), for
    /// report capture and inspection after the run.
    pub fn workbook(&self) -> Option<&SharedWorkbook> {
        self.workbook.as_ref()
    }

    /// Thread-safe: cancel every workbook this request has open.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn cancel_token(&self) -> &CancelToken {
        &self.cancel
    }

    fn evaluator(&self) -> Arc<dyn ChildEvaluator> {
        Arc::new(SubRequestEvaluator::new(self.source.clone(), self.compiled_child.clone()))
    }

    /// Calculate the parent for one scenario. `inputs` are client wire values
    /// (decoded by `ports`, `decode_wire=True`). JSON objects are read without
    /// key order; the binding calls [`Self::calculate_wire`] with the request's
    /// own order.
    pub fn calculate(&mut self, inputs: &Map<String, Value>) -> Result<CalculationResult, ModelCallError> {
        let wire: Vec<(String, WireValue)> =
            inputs.iter().map(|(name, value)| (name.clone(), WireValue::from_json(value))).collect();
        self.calculate_wire(&wire)
    }

    /// `calculate` over request values in their own order.
    pub fn calculate_wire(&mut self, inputs: &[(String, WireValue)]) -> Result<CalculationResult, ModelCallError> {
        let core = Arc::new(RunCore::new(
            self.package.clone(),
            self.context.clone(),
            self.source.clone(),
            self.compiled_child.clone(),
            self.cancel.clone(),
        ));
        core.init_prefetch(self.evaluator());
        self.core = Some(core.clone());
        self.workbook = None;
        self.effective_inputs = OrderedMap::default();
        self.failure = None;
        let watchdog = core.start_watchdog();
        let result = self.run(&core, inputs);
        if let Some(watchdog) = watchdog {
            watchdog.stop();
        }
        core.close();
        if let Err(error) = &result {
            self.failure = Some(error.clone());
        }
        result
    }

    fn trim_trailing_null_rows(&self) -> bool {
        match self.package.report.as_object() {
            Some(report) if report.contains_key("_profile_name") => {
                report.get("trim_trailing_null_rows") == Some(&Value::Bool(true))
            }
            _ => true,
        }
    }

    fn run(&mut self, core: &Arc<RunCore>, inputs: &[(String, WireValue)]) -> Result<CalculationResult, ModelCallError> {
        let started = Instant::now();
        let package = self.package.clone();
        let spec = &package.parent;
        let identity = spec.model_identity();
        let stack = vec![identity.clone()];
        let Loaded { workbook, mut ports, flag, .. } = core.load(spec, &stack)?;
        let shared: SharedWorkbook = Arc::new(RwLock::new(workbook));
        self.workbook = Some(shared.clone());
        let write_guard = || shared.write().unwrap_or_else(std::sync::PoisonError::into_inner);

        let admission_started = Instant::now();
        let effective = ports.write_scenario(&mut write_guard(), spec, inputs, true);
        core.add_seconds("admission_seconds", admission_started);
        core.count_writes(&ports);
        let effective = effective.map_err(PortError::into_error)?;
        self.effective_inputs =
            OrderedMap(effective.iter().map(|(key, value)| (key.clone(), wire_to_port_value(value))).collect());

        let report = (self.context.operation == Operation::Report).then(|| self.report_hook.clone()).flatten();
        if let Some(hook) = &report {
            hook.prepare(&shared)?;
            if let Some(record) = ports.write_record.as_mut() {
                record.clear();
            }
        }
        core.evaluate(&mut write_guard(), &flag)?;
        core.solve(&mut write_guard(), &flag, &identity)?;

        let capture_started = Instant::now();
        let (effective_inputs, typed_outputs, outputs) = {
            let mut workbook = write_guard();
            let effective = ports.read_inputs(&mut workbook, spec).map_err(PortError::into_error)?;
            let typed = ports.read_typed_outputs(&mut workbook, spec).map_err(PortError::into_error)?;
            let outputs = ports
                .read_outputs(&mut workbook, spec, self.trim_trailing_null_rows())
                .map_err(PortError::into_error)?;
            (effective, typed, outputs)
        };
        self.effective_inputs = effective_inputs.clone();
        {
            let mut state = core.state();
            state.diagnostics.extend(ports.ignored_inputs.iter().map(|name| format!("ignored_input:{name}")));
            if self.context.operation == Operation::Report {
                state.diagnostics.extend(ports.defaulted_inputs.iter().map(|name| format!("defaulted_input:{name}")));
            }
        }
        if let Some(hook) = &report {
            let notes = hook.capture(&shared, &outputs)?;
            core.state().diagnostics.extend(notes);
        }
        core.add_seconds("capture_seconds", capture_started);
        let mut state = core.state();
        state.timings.set(timing_keys::TOTAL_SECONDS, TimingValue::Seconds(started.elapsed().as_secs_f64()));
        let result = CalculationResult {
            outputs,
            typed_outputs,
            effective_inputs,
            invocations: state.invocations.clone(),
            faults: state.faults(),
            timings: state.timings.clone(),
            solvers: state.solvers.clone(),
            diagnostics: state.diagnostics.clone(),
            session_reuse: Map::new(),
            call_memo: None,
            compiled: Map::new(),
        };
        drop(state);
        Ok(CalculationResult { call_memo: core.memo_report(), compiled: core.compiled_report(), ..result })
    }

    /// Evidence for a failed run (`runtime.calculate`'s except branch): the
    /// effective inputs so far, the invocations, goal-seek records, timings,
    /// memo and compiled reports, and the diagnostics with the failure line
    /// (`Type: message`) appended.
    pub fn partial_result(&self) -> CalculationResult {
        let Some(core) = &self.core else { return CalculationResult::default() };
        let (invocations, faults, timings, solvers, mut diagnostics) = {
            let state = core.state();
            (
                state.invocations.clone(),
                state.faults(),
                state.timings.clone(),
                state.solvers.clone(),
                state.diagnostics.clone(),
            )
        };
        if let Some(failure) = &self.failure {
            diagnostics.push(failure_line(failure));
        }
        CalculationResult {
            outputs: OrderedMap::default(),
            typed_outputs: OrderedMap::default(),
            effective_inputs: self.effective_inputs.clone(),
            invocations,
            faults,
            timings,
            solvers,
            diagnostics,
            session_reuse: Map::new(),
            call_memo: core.memo_report(),
            compiled: core.compiled_report(),
        }
    }
}

/// `type(exc).__name__ + ': ' + str(exc)` for a request failure.
pub fn failure_line(error: &ModelCallError) -> String {
    match error {
        ModelCallError::Routing(message) => format!("ChildRoutingError: {message}"),
        other => other.to_string(),
    }
}

/// The Python exception type name of a request failure.
pub fn failure_kind(error: &ModelCallError) -> &str {
    match error {
        ModelCallError::Routing(_) => "ChildRoutingError",
        ModelCallError::Infrastructure { kind, .. } => kind,
        ModelCallError::NotImplemented(_) => "NotImplementedError",
    }
}

/// Flight evaluation for Lane B: a sub-session over the same package with
/// prefetch off.
impl ChildEvaluator for ModelSession {
    fn evaluate_child(&self, request: &ChildRequest) -> ChildOutcome {
        SubRequestEvaluator::new(self.source.clone(), self.compiled_child.clone()).evaluate_child(request)
    }
}
