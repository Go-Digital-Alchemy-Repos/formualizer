//! Lane P: sibling scenarios on ONE loaded child (the one-load
//! [`ChildBatchEvaluator`]).
//!
//! A prefetch dispatch hands over several sibling sub-requests that share one
//! child version, caller stack and output and differ only in inputs. The
//! per-flight path ([`crate::session::SubRequestEvaluator`]) loads the child
//! once per scenario (Income: five sibling loads, peak RSS 7.3 GB).
//! [`BatchChildEvaluator`] loads it once and runs every scenario on that
//! workbook, returning per scenario the same [`ChildOutcome`] the per-flight
//! path returns: the same result matrix, events, faults, goal-seek records and
//! diagnostics (timings differ: only the first scenario pays `load_seconds`
//! and `preparation_seconds`).
//!
//! Scenario loop (SheetPort `BatchExecutor`'s pattern, on this crate's own
//! primitives): per scenario, admit through the child's `PortSession` (which
//! writes every declared input, defaults included, and restores the formula
//! defaults on every write after the first), `evaluate_all`, goal seek, read
//! the output rectangle, then put back every cell goal seek wrote
//! (`sessions.solver_written_cells` / `restore_pinned_cells` in Python).
//! SheetPort's `BatchExecutor::run` itself is not used: it admits through
//! SheetPort's own `write_inputs` (not the `ports.py` rules: aliases,
//! unknown-input policy, defaults, CL-105), evaluates a targeted `RecalcPlan`
//! instead of `evaluate_all`, has no goal-seek step between evaluate and
//! read, and fails the whole batch on one scenario's error. Each of those
//! would break outcome equality with the per-flight path.
//!
//! Trust: a scenario that fails in any way drops the loaded child and the
//! next scenario loads a fresh one (Python: a slot whose flight failed is not
//! lent again). Grandchild calls made while a scenario evaluates go through
//! the normal router and load their own workbooks.
//!
//! Requests that do not share `(package, child_version, stack)` are split
//! into groups; outcomes come back in request order. Each group runs as up
//! to `context.flags.prefetch_max` parallel chunks, each chunk on its own
//! loaded child (Python's concurrent prefetch flights: Income's five
//! siblings ran serially on one child at twice the Python wall time). A
//! chunk batches several scenarios on one load only when the group exceeds
//! `prefetch_max`.

use formualizer_common::{ExcelError, ExcelErrorKind, LiteralValue};
use formualizer_eval::engine::CancelToken as EngineCancel;
use formualizer_eval::engine::named_range::{NameScope as EngineScope, NamedDefinition};
use formualizer_workbook::{IoError, Workbook};
use serde_json::{Map, Value};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Instant;

use crate::ModelCallError;
use crate::evaluator::{
    CancelToken, ChildMatrix, ChildOutcome, ChildRequest, CompiledChildHook, DefinedRange, NameScope, SolveModel,
};
use crate::goal_seek::run_goal_seeks;
use crate::import_boundary::GOAL_SEEK_BLOCK_PREFIX;
use crate::memo::ModelCallMemo;
use crate::ports::{PortError, WireValue, py_repr, read_typed_matrix};
use crate::prefetch::ChildBatchEvaluator;
use crate::receipt::timing_keys;
use crate::router::{ModelCallRouter, with_nested_router};
use crate::session::{Loaded, RunCore, RunState, SharedWorkbook, WorkbookSource};
use crate::spec::{CellRange, ModelSpec};

/// One loaded child per group of sibling scenarios.
#[derive(Clone)]
pub struct BatchChildEvaluator {
    source: Arc<dyn WorkbookSource>,
    compiled: Option<Arc<dyn CompiledChildHook>>,
}

impl BatchChildEvaluator {
    /// The same arguments as [`crate::session::SubRequestEvaluator::new`].
    pub fn new(source: Arc<dyn WorkbookSource>, compiled: Option<Arc<dyn CompiledChildHook>>) -> Self {
        Self { source, compiled }
    }
}

impl ChildBatchEvaluator for BatchChildEvaluator {
    fn evaluate_children(&self, requests: &[ChildRequest]) -> Vec<ChildOutcome> {
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for (index, request) in requests.iter().enumerate() {
            let group = groups.iter_mut().find(|group| {
                let first = &requests[group[0]];
                Arc::ptr_eq(&first.package, &request.package)
                    && first.child_version == request.child_version
                    && first.stack == request.stack
            });
            match group {
                Some(group) => group.push(index),
                None => groups.push(vec![index]),
            }
        }
        let mut outcomes: Vec<Option<ChildOutcome>> = (0..requests.len()).map(|_| None).collect();
        for group in groups {
            let flights = requests[group[0]].context.flags.prefetch_max.max(1) as usize;
            for (indices, group_outcomes) in self.run_parallel(requests, &group, flights) {
                for (index, outcome) in indices.into_iter().zip(group_outcomes) {
                    outcomes[index] = Some(outcome);
                }
            }
        }
        outcomes
            .into_iter()
            .map(|outcome| {
                outcome.unwrap_or_else(|| {
                    ChildOutcome::failed(ModelCallError::infrastructure("RuntimeError", "batch scenario lost"))
                })
            })
            .collect()
    }
}

/// Deadline watchdog for one group: records that it fired, then cancels the
/// group's token (which sets the flag of every workbook its core tracks).
struct Watchdog {
    stop: mpsc::Sender<()>,
    handle: std::thread::JoinHandle<()>,
}

impl Watchdog {
    fn start(deadline: Option<Instant>, token: &CancelToken, fired: &Arc<AtomicBool>) -> Option<Self> {
        let deadline = deadline?;
        let (stop, wait) = mpsc::channel::<()>();
        let (token, fired) = (token.clone(), fired.clone());
        let handle = std::thread::Builder::new()
            .name("calculation-deadline".into())
            .spawn(move || {
                let timeout = deadline.saturating_duration_since(Instant::now());
                if matches!(wait.recv_timeout(timeout), Err(RecvTimeoutError::Timeout)) {
                    fired.store(true, Ordering::SeqCst);
                    token.cancel();
                }
            })
            .ok()?;
        Some(Self { stop, handle })
    }

    fn stop(self) {
        let _ = self.stop.send(());
        let _ = self.handle.join();
    }
}

/// What the loaded bytes held in a cell goal seek overwrote.
enum Pinned {
    Formula(String),
    Value(LiteralValue),
}

type PinnedCells = Vec<((String, u32, u32), Pinned)>;

/// Split `len` scenarios into `flights` contiguous chunks whose sizes differ
/// by at most one (the first `len % flights` chunks take the extra one).
fn chunk_bounds(len: usize, flights: usize) -> Vec<std::ops::Range<usize>> {
    let flights = flights.clamp(1, len.max(1));
    let (base, extra) = (len / flights, len % flights);
    let mut start = 0;
    (0..flights)
        .map(|chunk| {
            let end = start + base + usize::from(chunk < extra);
            let range = start..end;
            start = end;
            range
        })
        .filter(|range| !range.is_empty())
        .collect()
}

impl BatchChildEvaluator {
    /// Run one group's scenarios as up to `flights` parallel chunks, each
    /// chunk on its own loaded child (the Python path's concurrent flights);
    /// a chunk holds more than one scenario only when the group exceeds
    /// `flights`. Returns each chunk's request indices with its outcomes.
    fn run_parallel(
        &self,
        requests: &[ChildRequest],
        group: &[usize],
        flights: usize,
    ) -> Vec<(Vec<usize>, Vec<ChildOutcome>)> {
        let chunks: Vec<Vec<usize>> =
            chunk_bounds(group.len(), flights).into_iter().map(|range| group[range].to_vec()).collect();
        let run = |indices: &[usize]| {
            let members: Vec<&ChildRequest> = indices.iter().map(|&index| &requests[index]).collect();
            self.run_group(&members)
        };
        let parallel = chunks.len() > 1;
        std::thread::scope(|scope| {
            let handles: Vec<_> = chunks
                .iter()
                .map(|indices| {
                    parallel.then(|| {
                        std::thread::Builder::new()
                            .name("model-call-prefetch-batch".into())
                            .spawn_scoped(scope, || run(indices))
                            .ok()
                    })?
                })
                .collect();
            chunks
                .iter()
                .zip(handles)
                .map(|(indices, handle)| {
                    // A single chunk, or one whose thread could not start,
                    // runs here; a chunk that panicked yields no outcomes
                    // (reported as lost scenarios).
                    let outcomes = match handle {
                        Some(handle) => handle.join().unwrap_or_default(),
                        None => run(indices),
                    };
                    (indices.clone(), outcomes)
                })
                .collect()
        })
    }

    fn run_group(&self, requests: &[&ChildRequest]) -> Vec<ChildOutcome> {
        let Some(first) = requests.first() else { return Vec::new() };
        let Some(spec) = first.spec() else {
            return requests
                .iter()
                .map(|_| ChildOutcome::failed(ModelCallError::routing("pinned child package is unavailable")))
                .collect();
        };
        let mut context = first.context.clone();
        context.flags.prefetch = false;
        let compiled = if context.flags.compiled { self.compiled.clone() } else { None };
        // The group's own token: the dispatch's shared token cancels it, and
        // so does the deadline watchdog.
        let token = CancelToken::new();
        let inner = token.clone();
        first.cancel.on_cancel(move || inner.cancel());
        let fired = Arc::new(AtomicBool::new(false));
        let core = Arc::new(RunCore::new(
            first.package.clone(),
            context.clone(),
            self.source.clone(),
            compiled.clone(),
            None,
            token.clone(),
        ));
        let fresh_timings = core.state().timings.clone();
        let watchdog = Watchdog::start(context.deadline, &token, &fired);
        let mut loaded: Option<Loaded> = None;
        let mut outcomes = Vec::with_capacity(requests.len());
        for request in requests {
            *core.state() = RunState {
                timings: fresh_timings.clone(),
                memo: context.flags.call_memo.then(ModelCallMemo::new),
                ..RunState::default()
            };
            let result = if token.is_cancelled() {
                Err(cancelled_error(&core, &fired))
            } else {
                let scenario = Scenario { core: &core, compiled: compiled.as_ref(), fired: &fired, spec, request };
                scenario.run(&mut loaded)
            };
            let state = core.state();
            outcomes.push(ChildOutcome {
                result,
                invocations: state.invocations.clone(),
                faults: state.faults(),
                timings: state.timings.clone(),
                solvers: state.solvers.clone(),
                diagnostics: state.diagnostics.clone(),
            });
        }
        if let Some(watchdog) = watchdog {
            watchdog.stop();
        }
        if let Some(loaded) = loaded.take() {
            core.release_loaded(loaded);
        }
        core.closed.store(true, Ordering::SeqCst);
        outcomes
    }
}

fn cancelled_error(core: &RunCore, fired: &AtomicBool) -> ModelCallError {
    if fired.load(Ordering::SeqCst) || core.check_deadline().is_err() {
        return ModelCallError::deadline();
    }
    ModelCallError::infrastructure("CancelledError", "prefetch flight cancelled")
}

struct Scenario<'a> {
    core: &'a Arc<RunCore>,
    compiled: Option<&'a Arc<dyn CompiledChildHook>>,
    fired: &'a AtomicBool,
    spec: &'a ModelSpec,
    request: &'a ChildRequest,
}

impl Scenario<'_> {
    fn add_seconds(&self, key: &str, started: Instant) {
        self.core.state().timings.add_seconds(key, started.elapsed().as_secs_f64());
    }

    /// `RunCore::calculate_child`, reusing the group's loaded child.
    fn run(&self, loaded: &mut Option<Loaded>) -> Result<ChildMatrix, ModelCallError> {
        let started = Instant::now();
        let (spec, request) = (self.spec, self.request);
        let location = spec.resolve_output(&request.output).ok_or_else(|| {
            ModelCallError::routing(format!(
                "child output selector is not declared: Undeclared output {}",
                py_repr(&request.output)
            ))
        })?;
        if let Some(hook) = self.compiled {
            let attempt_started = Instant::now();
            let faults_before = self.core.state().fault_indices.len();
            let nested = ModelCallRouter::nested(Arc::clone(self.core), request.stack.clone());
            let attempt = with_nested_router(nested, || hook.attempt(spec, &request.inputs, location, &request.stack));
            self.add_seconds(timing_keys::COMPILED_SECONDS, attempt_started);
            let attempt = attempt?;
            // As `RunCore::calculate_child`: a nested fault during the compiled
            // run fails this scenario instead of re-firing on the engine.
            if let Some(error) = self.core.state().fault_error_since(faults_before) {
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
        let mut current = match loaded.take() {
            Some(current) => {
                if let Err(error) = self.core.check_deadline() {
                    self.core.release_loaded(current);
                    return Err(error);
                }
                current
            }
            None => self.core.load(spec, &request.stack)?,
        };
        let mut pinned = PinnedCells::new();
        let result = self.evaluate(&mut current, &mut pinned, &location.range);
        if result.is_ok() && restore(&mut write(&current.workbook), &pinned).is_ok() {
            *loaded = Some(current);
        } else {
            self.core.release_loaded(current);
        }
        if result.is_ok() {
            self.add_seconds("child_seconds", started);
        }
        result
    }

    fn evaluate(&self, current: &mut Loaded, pinned: &mut PinnedCells, range: &CellRange) -> Result<ChildMatrix, ModelCallError> {
        let (spec, request) = (self.spec, self.request);
        let admission_started = Instant::now();
        let wire: Vec<(String, WireValue)> =
            request.inputs.iter().map(|(name, value)| (name.clone(), WireValue::from_literal(value))).collect();
        let mut guard = write(&current.workbook);
        let workbook: &mut Workbook = &mut guard;
        let admitted = current.ports.write_scenario(workbook, spec, &wire, false);
        self.add_seconds("admission_seconds", admission_started);
        if let Some(stats) = current.ports.write_stats {
            let mut state = self.core.state();
            for (key, value) in stats.entries() {
                state.timings.add_count(key, value);
            }
        }
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
        self.core.evaluate(workbook, &current.flag).map_err(|error| self.deadline_or(error))?;
        let caller = request.stack.last().map(String::as_str).unwrap_or_default();
        self.solve(workbook, &current.flag, pinned, caller)?;
        let matrix = read_typed_matrix(workbook, range).map_err(PortError::into_error)?;
        if matrix.iter().flatten().any(|value| matches!(value, LiteralValue::Pending)) {
            return Err(ModelCallError::infrastructure("RuntimeError", "child evaluation returned Pending"));
        }
        Ok(matrix)
    }

    /// A cancelled evaluation the group's watchdog caused is the deadline
    /// error the per-flight watchdog reports.
    fn deadline_or(&self, error: ModelCallError) -> ModelCallError {
        if self.fired.load(Ordering::SeqCst) { ModelCallError::deadline() } else { error }
    }

    /// `RunCore::raise_for_cancellation` for this group.
    fn raise_for_cancellation(&self, error: &ExcelError) -> Result<(), ModelCallError> {
        if error.kind != ExcelErrorKind::Cancelled {
            return Ok(());
        }
        if self.fired.load(Ordering::SeqCst) {
            return Err(ModelCallError::deadline());
        }
        self.fault_after(None)
    }

    /// `RunCore::fault_after`.
    fn fault_after(&self, suffix: Option<&str>) -> Result<(), ModelCallError> {
        let state = self.core.state();
        let first = state.fault_indices.first().and_then(|index| state.invocations.get(*index));
        if let Some(event) = first {
            let message = match suffix {
                Some(suffix) => suffix.to_owned(),
                None => format!("child callback infrastructure fault: {}", event.error.clone().unwrap_or_default()),
            };
            return Err(ModelCallError::infrastructure("CallbackInfrastructureError", message));
        }
        Ok(())
    }

    /// `RunCore::solve`, pinning every cell goal seek writes.
    fn solve(
        &self,
        workbook: &mut Workbook,
        flag: &Arc<AtomicBool>,
        pinned: &mut PinnedCells,
        identity: &str,
    ) -> Result<(), ModelCallError> {
        let started = Instant::now();
        let mut model = RecordingSolveModel { workbook, flag: flag.clone(), cancelled: None, pinned };
        let has_blocks = match model.defined_ranges() {
            Ok(ranges) => ranges
                .iter()
                .any(|range| range.scope == NameScope::Workbook && range.name.starts_with(GOAL_SEEK_BLOCK_PREFIX)),
            Err(error) => {
                self.add_seconds("solver_seconds", started);
                return Err(error);
            }
        };
        let (notes, records) = if has_blocks {
            match run_goal_seeks(&mut model) {
                Ok(run) => (run.notes, run.records),
                Err(failure) => {
                    {
                        let mut state = self.core.state();
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
        self.core.check_deadline()?;
        self.fault_after(Some("child callback infrastructure fault during solve"))?;
        {
            let mut state = self.core.state();
            state.solvers.extend(records.into_iter().map(|record| with_workbook(record, identity)));
            state.diagnostics.extend(notes);
        }
        self.add_seconds("solver_seconds", started);
        Ok(())
    }
}

fn with_workbook(mut record: Map<String, Value>, identity: &str) -> Map<String, Value> {
    record.insert("workbook".into(), Value::String(identity.to_owned()));
    record
}

fn write(workbook: &SharedWorkbook) -> std::sync::RwLockWriteGuard<'_, Workbook> {
    workbook.write().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Put back what the loaded bytes held in every cell goal seek wrote.
fn restore(workbook: &mut Workbook, pinned: &PinnedCells) -> Result<(), IoError> {
    for ((sheet, row, col), original) in pinned {
        match original {
            Pinned::Formula(formula) => workbook.set_formula(sheet, *row, *col, formula)?,
            Pinned::Value(value) => workbook.set_value(sheet, *row, *col, value.clone())?,
        }
    }
    Ok(())
}

/// The goal-seek surface over the loaded child (the session's
/// `WorkbookSolveModel` plus `get_formula`), pinning each cell before its
/// first write.
struct RecordingSolveModel<'a> {
    workbook: &'a mut Workbook,
    flag: Arc<AtomicBool>,
    cancelled: Option<ExcelError>,
    pinned: &'a mut PinnedCells,
}

impl SolveModel for RecordingSolveModel<'_> {
    fn get_value(&self, sheet: &str, row: u32, col: u32) -> Result<LiteralValue, ModelCallError> {
        Ok(self.workbook.get_value(sheet, row, col).unwrap_or(LiteralValue::Empty))
    }

    fn set_value(&mut self, sheet: &str, row: u32, col: u32, value: LiteralValue) -> Result<(), ModelCallError> {
        if !self.pinned.iter().any(|((pinned_sheet, pinned_row, pinned_col), _)| {
            pinned_sheet == sheet && *pinned_row == row && *pinned_col == col
        }) {
            let original = match self.workbook.get_formula(sheet, row, col) {
                Some(formula) => Pinned::Formula(formula),
                None => Pinned::Value(self.workbook.get_value(sheet, row, col).unwrap_or(LiteralValue::Empty)),
            };
            self.pinned.push(((sheet.to_owned(), row, col), original));
        }
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
        Ok(defined_ranges(self.workbook))
    }

    fn get_formula(&self, sheet: &str, row: u32, col: u32) -> Result<Option<String>, ModelCallError> {
        Ok(self.workbook.get_formula(sheet, row, col))
    }
}

/// `_named_range_rows` over a loaded workbook.
fn defined_ranges(workbook: &Workbook) -> Vec<DefinedRange> {
    let engine = workbook.engine();
    engine
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
        .collect()
}
