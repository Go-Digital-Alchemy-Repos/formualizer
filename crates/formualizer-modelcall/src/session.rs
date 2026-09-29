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
//! Lane I: a session built `with_retained(model)` loads through the
//! model's pool (`sessions.SessionPool`): a retained workbook is re-entered
//! (`_reuse`: cancel reset, clock, router rebound, pinned goal-seek cells
//! restored, port session re-entered so CL-097 skips unchanged writes), a
//! fresh load is admitted, `session_reuse` is reported and invocations are
//! sealed with held and inherited events (`runtime.sealed_invocations`).
//! Hooks: `report_prepare` / `report_capture` (operation `report`) and
//! `inspect` (operation `diagnostic`) run on the parent through [`ReportHook`].
//!
//! Not ported here (they stay in Python this round): engine identity
//! verification, prefetch slot pools (flights always load fresh).

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
    CancelToken, CellAddress, ChildEvaluator, ChildMatrix, ChildOutcome, ChildRequest, CompiledChildHook, CompiledParent,
    CompiledRun, CompiledXcall, DefinedRange, NameScope, ParentAttempt, SharedCompiledCells, SolveModel,
};
use crate::event::{CallStatus, ModelCallEvent};
use crate::goal_seek::run_goal_seeks;
use crate::import_boundary::GOAL_SEEK_BLOCK_PREFIX;
use crate::memo::ModelCallMemo;
use crate::ports::{
    Admitted, PortError, PortSession, WireValue, WrittenCells, admit_scenario, parent_port_literal, port_value_from_grid,
    port_value_from_written_grid, project_inputs, written_cells,
    project_outputs, py_repr, read_typed_matrix, wire_to_port_value,
};
use crate::prefetch::{Prefetcher, sibling_plan};
use crate::receipt::{CalculationResult, MemoReport, PortValue, TimingValue, Timings, compiled_keys, timing_keys};
use crate::retained::{Acquired, Admission, RetainedModel, RetainedPool, ReuseState, restore_pinned_cells, solver_written_cells};
use crate::router::{ModelCallRouter, RouterSlot, register_call_handler, with_nested_router};
use crate::spec::{CellRange, ModelPackage, ModelSpec, OrderedMap, PortLocation};
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
///
/// Architecture B: a compiled parent run has no workbook. `prepare` never
/// runs on it (F1: report conditions are prepared only on an engine parent,
/// including the one a declined compiled attempt falls back to);
/// `capture_compiled` receives the run's cells instead. A hook that does not
/// `captures_compiled` makes a report run refuse the compiled parent
/// (`engine:report_capture`).
pub trait ReportHook: Send + Sync {
    fn prepare(&self, workbook: &SharedWorkbook) -> Result<(), ModelCallError>;
    fn capture(
        &self,
        workbook: &SharedWorkbook,
        outputs: &OrderedMap<PortValue>,
    ) -> Result<Vec<String>, ModelCallError>;
    /// `capture` over a compiled parent run's cells.
    fn capture_compiled(
        &self,
        _cells: &SharedCompiledCells,
        _outputs: &OrderedMap<PortValue>,
    ) -> Result<Vec<String>, ModelCallError> {
        Err(ModelCallError::infrastructure("RuntimeError", "report hook cannot capture a compiled parent"))
    }
    /// Whether `capture_compiled` is implemented.
    fn captures_compiled(&self) -> bool {
        false
    }
    /// `capture_inspection(workbook, spec)` for operation `diagnostic`, after
    /// the report capture; timed as `inspection_seconds`. The hook keeps what
    /// it captured (the binding returns it as `inspection`).
    fn inspect(&self, _workbook: &SharedWorkbook) -> Result<(), ModelCallError> {
        Ok(())
    }
    /// Whether `inspect` does anything (no timing otherwise).
    fn inspects(&self) -> bool {
        false
    }
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
    /// Lane I: what this request did with the retained pool.
    pub(crate) reuse: ReuseState,
    /// Architecture B: the compiled parent's record for `compiled` (route,
    /// xcalls, whether an engine parent was loaded, discarded child routes).
    pub(crate) parent: Option<ParentRecord>,
}

/// The compiled parent's part of the `compiled` receipt map.
#[derive(Debug, Clone, Default)]
pub(crate) struct ParentRecord {
    route: Value,
    xcalls: Option<u64>,
    loaded: bool,
    /// `routes[start..end]` of the child hook's report belong to a discarded
    /// compiled parent attempt.
    discarded: Option<(usize, usize)>,
}

impl RunState {
    pub(crate) fn faults(&self) -> Vec<ModelCallEvent> {
        self.fault_indices.iter().filter_map(|index| self.invocations.get(*index).cloned()).collect()
    }

    /// The error of the first fault recorded after the first `before` faults
    /// (`session.faults[faults]['error']` in `CompiledRoute.attempt`).
    pub(crate) fn fault_error_since(&self, before: usize) -> Option<String> {
        self.fault_indices
            .get(before)
            .and_then(|index| self.invocations.get(*index))
            .map(|event| event.error.clone().unwrap_or_default())
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
    /// Lane I: the retained model's pool, when the session runs on one.
    pool: Option<Arc<RetainedPool>>,
    state: Mutex<RunState>,
    pub(crate) prefetch: Mutex<Option<Prefetcher>>,
    active: Mutex<Vec<(u64, Arc<AtomicBool>)>>,
    next_id: AtomicU64,
    deadline_cancelled: AtomicBool,
    pub(crate) closed: AtomicBool,
    cancel: CancelToken,
}

/// A loaded (or re-entered), tracked workbook and its port session.
pub(crate) struct Loaded {
    pub(crate) workbook: SharedWorkbook,
    pub(crate) ports: PortSession,
    pub(crate) flag: Arc<AtomicBool>,
    pub(crate) id: u64,
    /// The call binding registered on the workbook.
    pub(crate) slot: Arc<RouterSlot>,
    /// The pool entry, when the workbook is retained.
    pub(crate) entry: Option<u64>,
}

fn write(workbook: &SharedWorkbook) -> std::sync::RwLockWriteGuard<'_, Workbook> {
    workbook.write().unwrap_or_else(std::sync::PoisonError::into_inner)
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
        pool: Option<Arc<RetainedPool>>,
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
        let reuse = ReuseState::new(pool.is_some());
        Self {
            package,
            context,
            source,
            compiled,
            pool,
            state: Mutex::new(RunState { timings, memo, reuse, ..RunState::default() }),
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
            // Lane P/X2: sibling scenarios run as up to prefetch_max parallel
            // chunks, each on its own loaded child.
            *lock(&self.prefetch) = Some(
                Prefetcher::new(plan, self.package.clone(), self.context.clone(), evaluator)
                    .with_batch_evaluator(Arc::new(crate::batch_child::BatchChildEvaluator::new(
                        self.source.clone(),
                        self.compiled.clone(),
                    ))),
            );
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

    /// `load(spec, stack)`: re-enter a retained workbook, or load fresh with
    /// the call function registered for this caller stack, tracked for
    /// cancellation, graph prepared (and admitted when a pool is present).
    pub(crate) fn load(self: &Arc<Self>, spec: &ModelSpec, stack: &[String]) -> Result<Loaded, ModelCallError> {
        self.check_deadline()?;
        let started = Instant::now();
        if let Some(pool) = &self.pool
            && let Some(acquired) = pool.acquire(spec, self.context.random_seed)?
        {
            let serial = acquired.serial;
            let note = acquired.note();
            match self.reenter(acquired, spec, stack) {
                Ok(loaded) => {
                    self.state().reuse.note_reused(note);
                    self.add_seconds("load_seconds", started);
                    return Ok(loaded);
                }
                Err(error) => {
                    pool.discard(serial);
                    return Err(error);
                }
            }
        }
        let flag = Arc::new(AtomicBool::new(false));
        let (workbook, ports, slot) = self.open_fresh(spec, Some(stack), &flag)?;
        let id = self.track(&flag);
        let shared: SharedWorkbook = Arc::new(RwLock::new(workbook));
        let mut entry = None;
        if let Some(pool) = &self.pool {
            let admitted = (|| {
                let restore = {
                    let mut guard = write(&shared);
                    let model = WorkbookSolveModel { workbook: &mut guard, flag: flag.clone(), cancelled: None };
                    solver_written_cells(&model)?
                };
                let admission = Admission { workbook: shared.clone(), slot: slot.clone(), flag: flag.clone(), restore };
                pool.admit(spec, self.context.random_seed, admission)
            })();
            match admitted {
                Ok(serial) => {
                    self.state().reuse.note_admitted(serial, &spec.model_identity());
                    entry = Some(serial);
                }
                Err(error) => {
                    self.release(id, &flag);
                    slot.unbind();
                    return Err(error);
                }
            }
        }
        self.state().reuse.note_fresh(spec.model_identity());
        self.add_seconds("load_seconds", started);
        Ok(Loaded { workbook: shared, ports, flag, id, slot, entry })
    }

    /// The fresh-load steps (`sessions.load_workbook_session`): source load,
    /// clock, call binding (bound to this core for `stack` when given),
    /// `prepare_graph`, port session with a write record when asked for.
    pub(crate) fn open_fresh(
        self: &Arc<Self>,
        spec: &ModelSpec,
        stack: Option<&[String]>,
        flag: &Arc<AtomicBool>,
    ) -> Result<(Workbook, PortSession, Arc<RouterSlot>), ModelCallError> {
        let mut workbook = self.source.load(spec, &self.context)?;
        workbook
            .set_deterministic_mode(DeterministicMode::Enabled {
                timestamp_utc: self.context.now.with_timezone(&Utc),
                timezone: TimeZoneSpec::FixedOffsetSeconds(0),
            })
            .map_err(engine_error)?;
        let slot = RouterSlot::new();
        if let Some(stack) = stack {
            slot.bind(ModelCallRouter::bound(Arc::clone(self), stack.to_vec(), flag.clone()));
        }
        let prepared = (|| {
            register_call_handler(&mut workbook, slot.clone())?;
            let preparation_started = Instant::now();
            workbook.prepare_graph_all().map_err(engine_error)?;
            self.add_seconds("preparation_seconds", preparation_started);
            PortSession::new(&mut workbook, spec, true, self.context.flags.skip_unchanged_writes)
                .map_err(PortError::into_error)
        })();
        match prepared {
            Ok(ports) => Ok((workbook, ports, slot)),
            Err(error) => {
                // CL-095: nobody receives this workbook.
                slot.unbind();
                Err(error)
            }
        }
    }

    /// `_reuse(entry, spec, stack)`: re-enter a retained workbook as this
    /// run's own scenario. No second prepare and no second registration.
    fn reenter(self: &Arc<Self>, acquired: Acquired, spec: &ModelSpec, stack: &[String]) -> Result<Loaded, ModelCallError> {
        let Acquired { serial, workbook, slot, flag, restore, ports, .. } = acquired;
        let id = self.track(&flag);
        flag.store(false, Ordering::SeqCst);
        let result = (|| {
            let mut guard = write(&workbook);
            guard
                .set_deterministic_mode(DeterministicMode::Enabled {
                    timestamp_utc: self.context.now.with_timezone(&Utc),
                    timezone: TimeZoneSpec::FixedOffsetSeconds(0),
                })
                .map_err(engine_error)?;
            slot.bind(ModelCallRouter::bound(Arc::clone(self), stack.to_vec(), flag.clone()));
            let mut ports = match ports {
                Some(ports) => ports,
                // Lent back without its port session (a failed run): rebuild.
                None => PortSession::new(&mut guard, spec, false, self.context.flags.skip_unchanged_writes)
                    .map_err(PortError::into_error)?,
            };
            restore_pinned_cells(&mut guard, &restore, Some(&mut ports))?;
            ports.reenter();
            Ok(ports)
        })();
        match result {
            Ok(ports) => Ok(Loaded { workbook, ports, flag, id, slot, entry: Some(serial) }),
            Err(error) => {
                self.release(id, &flag);
                slot.unbind();
                Err(error)
            }
        }
    }

    /// `_release(workbook)`: stop watching it; a pool entry is unbound and
    /// lent back with its port session, a request-owned load is dropped.
    pub(crate) fn release_loaded(&self, loaded: Loaded) {
        let Loaded { ports, flag, id, slot, entry, .. } = loaded;
        self.release(id, &flag);
        slot.unbind();
        if let (Some(serial), Some(pool)) = (entry, &self.pool) {
            self.state().reuse.note_released(serial);
            pool.release(serial, Some(ports));
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
            let faults_before = self.state().fault_indices.len();
            let nested = ModelCallRouter::nested(Arc::clone(self), stack.to_vec());
            let attempt = with_nested_router(nested, || hook.attempt(spec, inputs, location, stack));
            self.add_seconds(timing_keys::COMPILED_SECONDS, attempt_started);
            let mut attempt = attempt?;
            // CompiledRoute.attempt (R1 #7): a nested call that faulted during
            // the compiled run fails the request now, as the engine path would
            // after evaluating the child, instead of re-firing it on the engine.
            let fault = self.state().fault_error_since(faults_before);
            if fault.is_some() {
                attempt.route = Some(Value::String("fallback:fault".to_owned()));
            }
            if let (Some(index), Some(route)) = (router_event, attempt.route)
                && let Some(event) = self.state().invocations.get_mut(index)
            {
                event.route = Some(route);
            }
            if let Some(error) = fault {
                return Err(ModelCallError::infrastructure(
                    "CallbackInfrastructureError",
                    format!("child callback infrastructure fault: {error}"),
                ));
            }
            if let Some(matrix) = attempt.matrix {
                self.add_seconds("child_seconds", started);
                return Ok(matrix);
            }
        }
        let mut loaded = self.load(spec, stack)?;
        let result = (|| {
            let mut workbook = write(&loaded.workbook);
            let admission_started = Instant::now();
            let wire: Vec<(String, WireValue)> =
                inputs.iter().map(|(name, value)| (name.clone(), WireValue::from_literal(value))).collect();
            let admitted = loaded.ports.write_scenario(&mut workbook, spec, &wire, false);
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
            let evaluation_start = self.state().invocations.len();
            self.evaluate(&mut workbook, &loaded.flag)?;
            let caller = stack.last().map(String::as_str).unwrap_or_default();
            self.solve(&mut workbook, &loaded.flag, caller)?;
            let matrix = read_typed_matrix(&workbook, &location.range).map_err(PortError::into_error)?;
            if matrix.iter().flatten().any(|value| matches!(value, LiteralValue::Pending)) {
                return Err(ModelCallError::infrastructure("RuntimeError", "child evaluation returned Pending"));
            }
            if let Some(serial) = loaded.entry {
                let mut state = self.state();
                let state = &mut *state;
                state.reuse.record_evaluation(serial, &state.invocations, stack, evaluation_start);
            }
            Ok(matrix)
        })();
        // This child is no longer evaluating; the watchdog stops watching it.
        self.release_loaded(loaded);
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
        let mut report = match &self.compiled {
            Some(hook) => hook.report(),
            None => {
                let mut report = Map::new();
                report.insert("calls".into(), Value::from(0));
                report.insert("routes".into(), Value::Array(Vec::new()));
                report
            }
        };
        let parent = self.state().parent.clone();
        if let Some(parent) = parent {
            if let Some((start, end)) = parent.discarded
                && let Some(Value::Array(routes)) = report.get_mut(compiled_keys::ROUTES)
            {
                let end = end.min(routes.len());
                let start = start.min(end);
                routes.drain(start..end);
                let dropped = (end - start) as u64;
                if let Some(calls) = report.get(compiled_keys::CALLS).and_then(Value::as_u64) {
                    report.insert(compiled_keys::CALLS.into(), Value::from(calls.saturating_sub(dropped)));
                }
            }
            report.insert(compiled_keys::PARENT.into(), parent.route);
            if let Some(xcalls) = parent.xcalls {
                report.insert(compiled_keys::PARENT_XCALLS.into(), Value::from(xcalls));
            }
            report.insert(compiled_keys::PARENT_LOADED.into(), Value::Bool(parent.loaded));
        }
        report
    }

    fn set_parent(&self, record: ParentRecord) {
        self.state().parent = Some(record);
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
        // `_release_sessions()`: anything still lent out goes back (without a
        // port session, which the next acquire rebuilds).
        if let Some(pool) = &self.pool {
            let acquired = std::mem::take(&mut self.state().reuse.acquired);
            for serial in acquired {
                pool.release(serial, None);
            }
        }
    }

    /// The sealed invocation list (`sealed_invocations`).
    pub(crate) fn sealed_invocations(&self) -> Vec<ModelCallEvent> {
        let state = self.state();
        state.reuse.seal(&state.invocations)
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

    fn get_formula(&self, sheet: &str, row: u32, col: u32) -> Result<Option<String>, ModelCallError> {
        Ok(self.workbook.get_formula(sheet, row, col))
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
            None,
            request.cancel.clone(),
        ));
        let watchdog = core.start_watchdog();
        let output = LiteralValue::Text(request.output.clone());
        let result = crate::compiled::with_request_context(&core.context, || {
            core.calculate_child(spec, &request.inputs, &output, &request.stack, None)
        });
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
    /// Architecture B: the compiled parent (consulted before the engine parent).
    compiled_parent: Option<Arc<dyn CompiledParent>>,
    /// The cells of the last compiled parent run (report capture, gates).
    compiled_cells: Option<SharedCompiledCells>,
    cancel: CancelToken,
    source: Arc<dyn WorkbookSource>,
    pool: Option<Arc<RetainedPool>>,
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
            compiled_parent: None,
            compiled_cells: None,
            cancel: CancelToken::new(),
            source: Arc::new(PathWorkbookSource),
            pool: None,
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

    /// Run on a retained model's workbooks (the Python pool's lease): loads
    /// go through its pool and use its loader.
    pub fn with_retained(mut self, retained: &RetainedModel) -> Self {
        self.set_retained(Some(retained));
        self
    }

    pub fn set_retained(&mut self, retained: Option<&RetainedModel>) {
        match retained {
            Some(retained) => {
                self.pool = Some(retained.pool.clone());
                self.source = retained.source.clone();
            }
            None => self.pool = None,
        }
    }

    /// Report capture for `operation = report` (and inspection for
    /// `operation = diagnostic`).
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

    /// Architecture B: consult `hook` for the parent before loading the engine
    /// parent (when `flags.compiled` is on and `hook.serves` the parent sha).
    pub fn with_compiled_parent(mut self, hook: Arc<dyn CompiledParent>) -> Self {
        self.compiled_parent = Some(hook);
        self
    }

    pub fn set_compiled_parent(&mut self, hook: Option<Arc<dyn CompiledParent>>) {
        self.compiled_parent = hook;
    }

    /// The cells of the last `calculate`'s compiled parent run, when the
    /// parent ran compiled (`workbook()` is then `None`).
    pub fn compiled_cells(&self) -> Option<&SharedCompiledCells> {
        self.compiled_cells.as_ref()
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
    /// report capture and inspection after the run. With a retained model
    /// this is the pool's workbook: read it before the next request.
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

    /// `calculate` over request values in their own order. The request
    /// context is set for this thread (`compiled::with_request_context`) so a
    /// native compiled child called from the engine parent has its clock.
    pub fn calculate_wire(&mut self, inputs: &[(String, WireValue)]) -> Result<CalculationResult, ModelCallError> {
        let context = self.context.clone();
        crate::compiled::with_request_context(&context, || self.calculate_request(inputs))
    }

    fn calculate_request(&mut self, inputs: &[(String, WireValue)]) -> Result<CalculationResult, ModelCallError> {
        self.workbook = None;
        self.compiled_cells = None;
        self.effective_inputs = OrderedMap::default();
        self.failure = None;
        let mut core = self.new_core();
        let mut parent_record: Option<ParentRecord> = None;
        match self.parent_plan() {
            ParentPlan::Engine => {}
            ParentPlan::Refused(reason) => {
                parent_record = Some(ParentRecord { route: Value::String(format!("engine:{reason}")), ..ParentRecord::default() });
            }
            ParentPlan::Compiled(hook) => {
                let before = self.child_routes_len();
                let watchdog = core.start_watchdog();
                let outcome = self.run_compiled(&core, hook.as_ref(), inputs);
                if let Some(watchdog) = watchdog {
                    watchdog.stop();
                }
                core.close();
                match outcome {
                    Ok(CompiledOutcome::Served(result)) => return Ok(*result),
                    Ok(CompiledOutcome::Declined(route)) => {
                        // Discard the attempt: a fresh request core for the engine parent.
                        let after = self.child_routes_len();
                        self.compiled_cells = None;
                        self.effective_inputs = OrderedMap::default();
                        parent_record = Some(ParentRecord { route, discarded: Some((before, after)), ..ParentRecord::default() });
                        core = self.new_core();
                    }
                    Err(error) => {
                        self.failure = Some(error.clone());
                        return Err(error);
                    }
                }
            }
        }
        if let Some(mut record) = parent_record {
            record.loaded = true;
            core.set_parent(record);
        }
        let watchdog = core.start_watchdog();
        let mut parent: Option<Loaded> = None;
        let result = self.run(&core, inputs, &mut parent);
        if let Some(watchdog) = watchdog {
            watchdog.stop();
        }
        if let Some(parent) = parent.take() {
            core.release_loaded(parent);
        }
        core.close();
        if let Err(error) = &result {
            self.failure = Some(error.clone());
        }
        result
    }

    /// A request core for this session's package, prefetch initialised; it
    /// becomes the core `partial_result` reads.
    fn new_core(&mut self) -> Arc<RunCore> {
        let core = Arc::new(RunCore::new(
            self.package.clone(),
            self.context.clone(),
            self.source.clone(),
            self.compiled_child.clone(),
            self.pool.clone(),
            self.cancel.clone(),
        ));
        core.init_prefetch(self.evaluator());
        self.core = Some(core.clone());
        core
    }

    /// How many child routes the compiled-child hook has reported so far.
    fn child_routes_len(&self) -> usize {
        if !self.context.flags.compiled {
            return 0;
        }
        self.compiled_child
            .as_ref()
            .and_then(|hook| hook.report().get(compiled_keys::ROUTES).and_then(Value::as_array).map(Vec::len))
            .unwrap_or(0)
    }

    /// Whether (and why not) the parent runs compiled (architecture B item (c)).
    fn parent_plan(&self) -> ParentPlan {
        let Some(hook) = self.compiled_parent.clone() else { return ParentPlan::Engine };
        let spec = &self.package.parent;
        if !self.context.flags.compiled || !hook.serves(&spec.workbook_sha256) {
            return ParentPlan::Engine;
        }
        let refused = |reason: &str| ParentPlan::Refused(reason.to_owned());
        if !spec.goal_seek.is_empty() {
            return refused("goal_seek");
        }
        match self.context.operation {
            Operation::Client => {}
            Operation::Report => {
                if !hook.report_conditions_simple(&spec.workbook_sha256) {
                    return refused("report_conditions");
                }
                if self.report_hook.as_ref().is_some_and(|hook| !hook.captures_compiled()) {
                    return refused("report_capture");
                }
            }
            Operation::Diagnostic => return refused("operation:diagnostic"),
        }
        if spec.date_system() != Some(1900) {
            return refused("date_system");
        }
        // No `calculation_normalizations` refusal (integration, 2026-09-29): a
        // compiled parent is gated in parent mode against the engine parent
        // with the descriptor's edits applied, as parity whole_model serves a
        // gated parent artifact; the edits stay a child refusal
        // (`descriptor_edits`, compiled.rs `eligible`).
        let admitted_inputs = spec.inputs.iter().all(|(_, location)| matches!(location.shape.as_str(), "scalar" | "range"));
        let readable_outputs =
            spec.outputs.iter().all(|(_, location)| matches!(location.shape.as_str(), "scalar" | "record" | "range"));
        if !admitted_inputs || !readable_outputs {
            return refused("port_contract");
        }
        ParentPlan::Compiled(hook)
    }

    /// One compiled parent attempt on `core` (architecture B item (c)):
    /// admission without a workbook, the module run with the router as its
    /// `MDL.CALLMODEL` handler (stack `[parent]`), outputs and effective
    /// inputs read from the run's cells and projected as the engine path
    /// projects them, report capture over the cells. A decline returns the
    /// route; a nested-call fault or a hook error fails the request.
    fn run_compiled(
        &mut self,
        core: &Arc<RunCore>,
        hook: &dyn CompiledParent,
        inputs: &[(String, WireValue)],
    ) -> Result<CompiledOutcome, ModelCallError> {
        let started = Instant::now();
        let package = self.package.clone();
        let spec = &package.parent;
        let identity = spec.model_identity();
        let declined = |route: &str| Ok(CompiledOutcome::Declined(Value::String(route.to_owned())));

        let admission_started = Instant::now();
        // The engine path raises the admission error with its own text.
        let Ok(admitted) = admit_scenario(spec, inputs, true) else { return declined("engine:admission:invalid") };
        let Some(module_inputs) = module_inputs(spec, &admitted) else {
            return declined("engine:admission:value_type");
        };
        core.add_seconds("admission_seconds", admission_started);
        if let Ok(returned) = admitted.returned(spec) {
            self.effective_inputs =
                OrderedMap(returned.iter().map(|(key, value)| (key.clone(), wire_to_port_value(value))).collect());
        }

        let mut xcall = RouterXcall { router: ModelCallRouter::nested(Arc::clone(core), vec![identity]) };
        let attempt_started = Instant::now();
        let attempt = hook.run(spec, &module_inputs, &core.context, &mut xcall);
        core.add_seconds(timing_keys::COMPILED_SECONDS, attempt_started);
        let fault = core.state().first_fault_error();
        if let Some(error) = fault {
            // CompiledRoute.attempt: a nested call that faulted during the
            // compiled run fails the request, as the engine parent would.
            core.set_parent(ParentRecord { route: Value::from("fallback:fault"), ..ParentRecord::default() });
            return Err(ModelCallError::infrastructure(
                "CallbackInfrastructureError",
                format!("child callback infrastructure fault: {error}"),
            ));
        }
        let run = match attempt {
            Ok(ParentAttempt::Compiled(run)) => run,
            Ok(ParentAttempt::Declined { route }) => return Ok(CompiledOutcome::Declined(route)),
            Err(error) => {
                let deadline = matches!(&error, ModelCallError::Infrastructure { kind, .. } if kind == "TimeoutError");
                let route = if deadline { "fallback:deadline" } else { "fallback:error" };
                core.set_parent(ParentRecord { route: Value::from(route), ..ParentRecord::default() });
                return Err(error);
            }
        };
        if core.check_deadline().is_err() {
            // No late answer: the engine path refuses at its first deadline check.
            return declined("fallback:deadline");
        }

        let capture_started = Instant::now();
        let CompiledRun { outputs: module_outputs, route, stats, cells } = run;
        let Some((typed_outputs, outputs, effective_inputs)) =
            project_compiled(spec, &admitted, &module_outputs, cells.as_ref(), self.trim_trailing_null_rows())
        else {
            return declined("fallback:projection");
        };
        let cells: SharedCompiledCells = Arc::new(Mutex::new(cells));
        self.compiled_cells = Some(cells.clone());
        self.effective_inputs = effective_inputs.clone();
        {
            let mut state = core.state();
            state.diagnostics.extend(admitted.ignored_inputs.iter().map(|name| format!("ignored_input:{name}")));
            if self.context.operation == Operation::Report {
                state.diagnostics.extend(admitted.defaulted_inputs.iter().map(|name| format!("defaulted_input:{name}")));
            }
        }
        let report = (self.context.operation == Operation::Report).then(|| self.report_hook.clone()).flatten();
        if let Some(hook) = &report {
            let notes = hook.capture_compiled(&cells, &outputs)?;
            core.state().diagnostics.extend(notes);
        }
        core.add_seconds("capture_seconds", capture_started);
        core.set_parent(ParentRecord { route, xcalls: Some(stats.xcalls), loaded: false, discarded: None });
        core.state().timings.set(timing_keys::TOTAL_SECONDS, TimingValue::Seconds(started.elapsed().as_secs_f64()));
        let invocations = core.sealed_invocations();
        if let Some(pool) = &self.pool
            && pool.retain_scenarios()
        {
            core.state().reuse.retain_scenario(pool, &invocations);
        }
        let state = core.state();
        let result = CalculationResult {
            outputs,
            typed_outputs,
            effective_inputs,
            invocations,
            faults: state.faults(),
            timings: state.timings.clone(),
            solvers: state.solvers.clone(),
            diagnostics: state.diagnostics.clone(),
            session_reuse: state.reuse.report(),
            call_memo: None,
            compiled: Map::new(),
        };
        drop(state);
        Ok(CompiledOutcome::Served(Box::new(CalculationResult {
            call_memo: core.memo_report(),
            compiled: core.compiled_report(),
            ..result
        })))
    }

    fn trim_trailing_null_rows(&self) -> bool {
        match self.package.report.as_object() {
            Some(report) if report.contains_key("_profile_name") => {
                report.get("trim_trailing_null_rows") == Some(&Value::Bool(true))
            }
            _ => true,
        }
    }

    fn run(
        &mut self,
        core: &Arc<RunCore>,
        inputs: &[(String, WireValue)],
        parent: &mut Option<Loaded>,
    ) -> Result<CalculationResult, ModelCallError> {
        let started = Instant::now();
        let package = self.package.clone();
        let spec = &package.parent;
        let identity = spec.model_identity();
        let stack = vec![identity.clone()];
        let loaded = parent.insert(core.load(spec, &stack)?);
        let shared = loaded.workbook.clone();
        let flag = loaded.flag.clone();
        self.workbook = Some(shared.clone());

        let admission_started = Instant::now();
        let effective = loaded.ports.write_scenario(&mut write(&shared), spec, inputs, true);
        core.add_seconds("admission_seconds", admission_started);
        core.count_writes(&loaded.ports);
        let effective = effective.map_err(PortError::into_error)?;
        self.effective_inputs =
            OrderedMap(effective.iter().map(|(key, value)| (key.clone(), wire_to_port_value(value))).collect());

        let report = (self.context.operation == Operation::Report).then(|| self.report_hook.clone()).flatten();
        if let Some(hook) = &report {
            hook.prepare(&shared)?;
            if let Some(record) = loaded.ports.write_record.as_mut() {
                // Report condition formulas are written outside the port
                // surface; the record no longer vouches for this workbook.
                record.clear();
            }
        }
        core.evaluate(&mut write(&shared), &flag)?;
        core.solve(&mut write(&shared), &flag, &identity)?;

        let capture_started = Instant::now();
        let (effective_inputs, typed_outputs, outputs) = {
            let mut workbook = write(&shared);
            let ports = &mut loaded.ports;
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
            state.diagnostics.extend(loaded.ports.ignored_inputs.iter().map(|name| format!("ignored_input:{name}")));
            if self.context.operation == Operation::Report {
                state.diagnostics.extend(loaded.ports.defaulted_inputs.iter().map(|name| format!("defaulted_input:{name}")));
            }
        }
        if let Some(hook) = &report {
            let notes = hook.capture(&shared, &outputs)?;
            core.state().diagnostics.extend(notes);
        }
        core.add_seconds("capture_seconds", capture_started);
        if self.context.operation == Operation::Diagnostic
            && let Some(hook) = self.report_hook.clone().filter(|hook| hook.inspects())
        {
            let inspection_started = Instant::now();
            hook.inspect(&shared)?;
            core.add_seconds("inspection_seconds", inspection_started);
            core.check_deadline()?;
        }
        core.state().timings.set(timing_keys::TOTAL_SECONDS, TimingValue::Seconds(started.elapsed().as_secs_f64()));
        let invocations = core.sealed_invocations();
        if let Some(pool) = &self.pool
            && pool.retain_scenarios()
        {
            core.state().reuse.retain_scenario(pool, &invocations);
        }
        let state = core.state();
        let result = CalculationResult {
            outputs,
            typed_outputs,
            effective_inputs,
            invocations,
            faults: state.faults(),
            timings: state.timings.clone(),
            solvers: state.solvers.clone(),
            diagnostics: state.diagnostics.clone(),
            session_reuse: state.reuse.report(),
            call_memo: None,
            compiled: Map::new(),
        };
        drop(state);
        Ok(CalculationResult { call_memo: core.memo_report(), compiled: core.compiled_report(), ..result })
    }

    /// Evidence for a failed run (`runtime.calculate`'s except branch): the
    /// effective inputs so far, the sealed invocations, goal-seek records,
    /// timings, session reuse, memo and compiled reports, and the diagnostics
    /// with the failure line (`Type: message`) appended.
    pub fn partial_result(&self) -> CalculationResult {
        let Some(core) = &self.core else { return CalculationResult::default() };
        let invocations = core.sealed_invocations();
        let (faults, timings, solvers, mut diagnostics, session_reuse) = {
            let state = core.state();
            (state.faults(), state.timings.clone(), state.solvers.clone(), state.diagnostics.clone(), state.reuse.report())
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
            session_reuse,
            call_memo: core.memo_report(),
            compiled: core.compiled_report(),
        }
    }
}

/// What `ModelSession::parent_plan` decided.
enum ParentPlan {
    /// No compiled parent for this request: the engine parent, no record.
    Engine,
    /// A static refusal: the engine parent, route `engine:<reason>`.
    Refused(String),
    Compiled(Arc<dyn CompiledParent>),
}

/// What one compiled parent attempt produced.
enum CompiledOutcome {
    Served(Box<CalculationResult>),
    /// Discarded; the route (`engine:<r>` or `fallback:<r>`).
    Declined(Value),
}

/// The router as a compiled module's `MDL.CALLMODEL` handler (package B S2):
/// an array result is `Ok(rows)`; a call that recorded a router fault is an
/// infrastructure error carrying the fault's event error (the request fails,
/// the call is not re-fired on an engine parent); any other non-array answer
/// (a routing `#REF!`) is `Err(Routing)`, which the hook declines
/// `fallback:xcall_error`.
struct RouterXcall {
    router: ModelCallRouter,
}

impl CompiledXcall for RouterXcall {
    fn call(
        &mut self,
        target: &LiteralValue,
        block: &LiteralValue,
        output: &LiteralValue,
        tail: &[LiteralValue],
    ) -> Result<ChildMatrix, ModelCallError> {
        let mut args = Vec::with_capacity(3 + tail.len());
        args.extend([target.clone(), block.clone(), output.clone()]);
        args.extend(tail.iter().cloned());
        let before = self.router.fault_count();
        let value = self.router.call(&args);
        if let Some(error) = self.router.fault_error_since(before) {
            return Err(ModelCallError::infrastructure(
                "CallbackInfrastructureError",
                format!("child callback infrastructure fault: {error}"),
            ));
        }
        match value {
            LiteralValue::Array(rows) => Ok(rows),
            LiteralValue::Error(error) => {
                Err(ModelCallError::routing(error.message.unwrap_or_else(|| error.kind.to_string())))
            }
            other => Err(ModelCallError::routing(format!("child call returned a non-array value: {other:?}"))),
        }
    }
}

/// The admitted parent inputs as the module receives them: declared key ->
/// `parent_port_literal(value)`, in the admission's effective order. `None`
/// when a value has no module form (Lane D `admission:value_type`).
fn module_inputs(spec: &ModelSpec, admitted: &Admitted) -> Option<Vec<(String, LiteralValue)>> {
    let mut inputs = Vec::with_capacity(admitted.effective.len());
    for (key, _) in &admitted.effective {
        let location = spec.inputs.get(&crate::key::casefold(key))?;
        let value = admitted.by_port.iter().find(|(id, _)| *id == location.port_id).map(|(_, value)| value)?;
        inputs.push((location.key.clone(), parent_port_literal(value).ok()?));
    }
    Some(inputs)
}

/// Every cell of `location`'s rectangle, row-major.
fn rectangle(location: &PortLocation) -> Vec<CellAddress> {
    let range = &location.range;
    (range.start_row..=range.end_row)
        .flat_map(|row| (range.start_col..=range.end_col).map(move |col| (range.sheet.clone(), row, col)))
        .collect()
}

fn grid(location: &PortLocation, values: Vec<LiteralValue>) -> ChildMatrix {
    let width = location.range.cols() as usize;
    values.chunks(width.max(1)).map(<[LiteralValue]>::to_vec).collect()
}

type Projected = (OrderedMap<PortValue>, OrderedMap<PortValue>, OrderedMap<PortValue>);

/// `(typed_outputs, outputs, effective_inputs)` of a compiled parent run:
/// the module's declared outputs, and the input rectangles read from the
/// run's cells, projected exactly as the engine path projects its SheetPort
/// reads. `None` when a port cannot be read (the caller declines).
fn project_compiled(
    spec: &ModelSpec,
    admitted: &Admitted,
    module_outputs: &[(String, ChildMatrix)],
    cells: &dyn crate::evaluator::CompiledCells,
    trim: bool,
) -> Option<Projected> {
    let mut outputs = std::collections::BTreeMap::new();
    for (folded, location) in spec.outputs.iter() {
        let matrix = module_outputs
            .iter()
            .find(|(key, _)| key == folded)
            .map(|(_, matrix)| matrix.clone())
            .map_or_else(|| cells.read_cells(&rectangle(location)).ok().map(|values| grid(location, values)), Some)?;
        outputs.insert(location.port_id.clone(), port_value_from_grid(spec, location, &matrix)?);
    }
    let mut inputs = std::collections::BTreeMap::new();
    for (_, location) in spec.inputs.iter() {
        let values = cells.read_cells(&rectangle(location)).ok()?;
        // A cell the admission wrote reads back as the engine types the write:
        // a date / datetime formats it, anything else clears its style
        // (`ports::written_cells`, `ports::engine_temporal`).
        let written = match admitted.by_port.iter().find(|(id, _)| *id == location.port_id) {
            Some((_, value)) => written_cells(location, value)?,
            None => WrittenCells::new(),
        };
        let value = port_value_from_written_grid(spec, location, &grid(location, values), &written)?;
        inputs.insert(location.port_id.clone(), value);
    }
    let (typed, client) = project_outputs(spec, &outputs, trim).ok()?;
    let effective = project_inputs(spec, &inputs).ok()?;
    Some((typed, client, effective))
}

/// One model of a `RetainedModel::warm` (the loop body of
/// `SessionPool.warm`): fresh load, admit, bind a warm run of its own, write
/// the default scenario, evaluate, unbind, release. Returns the warm run's
/// events and timings.
pub(crate) fn warm_model(
    retained: &RetainedModel,
    spec: &ModelSpec,
    inputs: &[(String, WireValue)],
) -> Result<(Vec<ModelCallEvent>, Timings), ModelCallError> {
    let mut context = retained.context.clone();
    context.operation = Operation::Client;
    context.flags.prefetch = false;
    context.deadline = None;
    let core = Arc::new(RunCore::new(
        retained.package.clone(),
        context,
        retained.source.clone(),
        retained.compiled.clone(),
        Some(retained.pool.clone()),
        CancelToken::new(),
    ));
    let flag = Arc::new(AtomicBool::new(false));
    let (workbook, mut ports, slot) = core.open_fresh(spec, None, &flag)?;
    let shared: SharedWorkbook = Arc::new(RwLock::new(workbook));
    let restore = {
        let mut guard = write(&shared);
        let model = WorkbookSolveModel { workbook: &mut guard, flag: flag.clone(), cancelled: None };
        solver_written_cells(&model)
    };
    let restore = match restore {
        Ok(restore) => restore,
        Err(error) => {
            slot.unbind();
            return Err(error);
        }
    };
    let admission = Admission { workbook: shared.clone(), slot: slot.clone(), flag: flag.clone(), restore };
    let serial = match retained.pool.admit(spec, retained.context.random_seed, admission) {
        Ok(serial) => serial,
        Err(error) => {
            slot.unbind();
            return Err(error);
        }
    };
    slot.bind(ModelCallRouter::bound(Arc::clone(&core), vec![spec.model_identity()], flag.clone()));
    let outcome = (|| {
        let mut guard = write(&shared);
        ports.write_scenario(&mut guard, spec, inputs, true).map_err(PortError::into_error)?;
        flag.store(false, Ordering::SeqCst);
        match guard.evaluate_all_cancellable(EngineCancel::from_flag(flag.clone())) {
            Ok(_) => {}
            Err(error) => return Err(engine_error(error)),
        }
        if let Some(first) = core.state().first_fault_error() {
            return Err(ModelCallError::infrastructure(
                "RuntimeError",
                format!("child callback infrastructure fault while warming: {first}"),
            ));
        }
        Ok(())
    })();
    // The warm owns no workbook once it ends: the binding stays registered,
    // pointing at nothing, until a request binds it.
    slot.unbind();
    core.close();
    if let Err(error) = outcome {
        retained.pool.discard(serial);
        return Err(error);
    }
    retained.pool.release(serial, Some(ports));
    let state = core.state();
    let events = state.invocations.clone();
    retained.pool.set_warm_invocations(serial, events.clone(), false);
    Ok((events, state.timings.clone()))
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
