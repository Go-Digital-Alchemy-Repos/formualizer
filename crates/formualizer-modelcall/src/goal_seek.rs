//! Lane B: goal seek (`engine_adapter._execute_xsolve` / `_finalize_xsolve`
//! and `solve.run_solves`): blocks rediscovered from the workbook's
//! workbook-scope defined names with the import prefix, sorted by name;
//! settings read by label; Brent-Dekker as `_brent_solve`; the same skip and
//! failure categories and notes; audit write-backs on success; a required
//! failure fails the request with its records.
//!
//! Port notes (behaviour source: parity `replayer/engine_adapter.py`,
//! `workbook_runtime/solve.py`):
//!
//! * This is the request solver (`solve._RequestSolver`): a failed block
//!   writes nothing back (its `_finalize_xsolve` returns early on failure);
//!   only a converged block runs the audit write-backs.
//! * Every skip (`xsolve_skipped:<name>:<reason>`) and failure
//!   (`xsolve_failed:<name>:<reason>`) fails the request, as `run_solves`
//!   does; a block whose `Run if` is not on is passed over silently.
//! * [`brent_solve`] is operation-for-operation the Python loop, so roots are
//!   bit-identical: same evaluation order of the objective, same float
//!   expressions in the same association, Python `min`/`max` semantics, and a
//!   zero divisor in the inverse-quadratic step raises (`ZeroDivisionError`,
//!   an engine error) exactly where Python would.
//! * Note strings keep today's `xsolve_*` spelling (receipt content).

use chrono::Timelike;
use formualizer_common::{ExcelError, LiteralValue};
use serde_json::{Map, Value, json};

use crate::evaluator::{DefinedRange, NameScope, SolveModel};
use crate::import_boundary::GOAL_SEEK_BLOCK_PREFIX;
use crate::key::casefold;
use crate::spec::{CellRange, OrderedMap};
use crate::ModelCallError;

/// A converged block's summary (`{"TargetValue", "ByChangingCellValue"}` in
/// Python; ours per NAMING_STANDARD: `target_value`, `result`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GoalSeekResult {
    pub target_value: f64,
    pub result: f64,
}

/// `run_solves` success: results by block suffix, notes, records.
#[derive(Debug, Clone, Default)]
pub struct GoalSeekRun {
    pub results: OrderedMap<GoalSeekResult>,
    /// `xsolve_ran:<name> iterations=<n>` etc. (strings kept byte-compatible).
    pub notes: Vec<String>,
    /// Converged records (`asdict(_SolveRecord)` + `status: converged`).
    pub records: Vec<Map<String, Value>>,
}

/// `RequiredSolverFailure`: the notes and records so far (the last record is
/// the `solver_failure`), and the underlying cause when there was one.
#[derive(Debug, Clone)]
pub struct GoalSeekFailure {
    pub notes: Vec<String>,
    pub records: Vec<Map<String, Value>>,
    pub cause: Option<ModelCallError>,
}

/// `engine_adapter.XSOLVE_LABELS`.
const LABELS: [&str; 15] = [
    "run if",
    "target cell",
    "target value",
    "by changing",
    "solve algorithm",
    "max change",
    "max iterations",
    "initial guess",
    "lower bound",
    "upper bound",
    "solve started",
    "solve successful",
    "solve result",
    "solve iteration",
    "solve target",
];

/// The Python exception type name a cause is recorded under
/// (`record['exception_type']`).
pub fn exception_type(error: &ModelCallError) -> String {
    match error {
        ModelCallError::Routing(_) => "ChildRoutingError".to_owned(),
        ModelCallError::Infrastructure { kind, .. } => kind.clone(),
        ModelCallError::NotImplemented(_) => "NotImplementedError".to_owned(),
    }
}

fn value_error(message: &str) -> ModelCallError {
    ModelCallError::infrastructure("ValueError", message)
}

/// `_normalized_xsolve_label`: whitespace runs collapsed, casefolded, known.
fn normalized_label(value: &LiteralValue) -> Option<&'static str> {
    let LiteralValue::Text(text) = value else { return None };
    let label = casefold(&text.split_whitespace().collect::<Vec<_>>().join(" "));
    LABELS.iter().copied().find(|known| *known == label)
}

/// `_finite_number`: an int or float (never a bool), finite, as f64.
fn finite_number(value: &LiteralValue) -> Option<f64> {
    let converted = match value {
        LiteralValue::Int(number) => *number as f64,
        LiteralValue::Number(number) => *number,
        _ => return None,
    };
    converted.is_finite().then_some(converted)
}

/// One resolved single cell, and the dict the Python record carries for it.
#[derive(Debug, Clone)]
struct Location {
    sheet: String,
    row: u32,
    col: u32,
    record: Map<String, Value>,
}

impl Location {
    /// A label row's value cell, or a reference resolved by address.
    fn cell(sheet: &str, row: u32, col: u32) -> Self {
        let mut record = Map::new();
        record.insert("kind".into(), json!("cell"));
        record.insert("scope".into(), json!("sheet"));
        record.insert("sheet".into(), json!(sheet));
        record.insert("start_row".into(), json!(row));
        record.insert("end_row".into(), json!(row));
        record.insert("start_col".into(), json!(col));
        record.insert("end_col".into(), json!(col));
        Self { sheet: sheet.to_owned(), row, col, record }
    }

    /// A single-cell defined name (`dict(row)` of `get_named_ranges`).
    fn named(defined: &DefinedRange, range: &CellRange) -> Self {
        let mut record = Map::new();
        record.insert("name".into(), json!(defined.name));
        match &defined.scope {
            NameScope::Workbook => {
                record.insert("scope".into(), json!("workbook"));
                record.insert("scope_sheet".into(), Value::Null);
            }
            NameScope::Sheet(sheet) => {
                record.insert("scope".into(), json!("sheet"));
                record.insert("scope_sheet".into(), json!(sheet));
            }
        }
        // `DefinedRange` does not say whether the engine holds the name as a
        // cell or a one-cell range; a one-cell name is recorded as `cell`.
        record.insert("kind".into(), json!("cell"));
        record.insert("sheet".into(), json!(range.sheet));
        record.insert("start_row".into(), json!(range.start_row));
        record.insert("start_col".into(), json!(range.start_col));
        record.insert("end_row".into(), json!(range.end_row));
        record.insert("end_col".into(), json!(range.end_col));
        Self { sheet: range.sheet.clone(), row: range.start_row, col: range.start_col, record }
    }

    fn get(&self, model: &dyn SolveModel) -> Result<LiteralValue, ModelCallError> {
        model.get_value(&self.sheet, self.row, self.col)
    }

    fn set(&self, model: &mut dyn SolveModel, value: LiteralValue) -> Result<(), ModelCallError> {
        model.set_value(&self.sheet, self.row, self.col, value)
    }
}

/// `_set_engine_value`'s literal (`_typed_literal(value,
/// parse_temporal_strings=False)` of the Python value the engine returned, or
/// an error of the same kind with message `xsolve write-back`).
fn write_back_literal(value: &LiteralValue) -> Result<LiteralValue, ModelCallError> {
    Ok(match value {
        LiteralValue::Error(error) => {
            LiteralValue::Error(ExcelError::new(error.kind).with_message("xsolve write-back"))
        }
        // `LiteralValue.datetime(y, m, d, h, mi, s)`: sub-second part dropped.
        LiteralValue::DateTime(stamp) => {
            LiteralValue::DateTime(stamp.with_nanosecond(0).unwrap_or(*stamp))
        }
        LiteralValue::Date(_) | LiteralValue::Empty | LiteralValue::Boolean(_) | LiteralValue::Int(_) => {
            value.clone()
        }
        LiteralValue::Text(_) => value.clone(),
        LiteralValue::Number(number) if number.is_finite() => value.clone(),
        _ => return Err(value_error("binding")),
    })
}

/// `_typed_literal(float)`: a finite number, else `ValueError('binding')`.
fn number_literal(value: f64) -> Result<LiteralValue, ModelCallError> {
    if value.is_finite() { Ok(LiteralValue::Number(value)) } else { Err(value_error("binding")) }
}

/// Why a parameter reference did not resolve.
enum Unresolved {
    /// `ValueError("reference")`: the block is skipped.
    Invalid,
    /// Anything the model raised: an engine error.
    Model(ModelCallError),
}

fn range_is_single(range: &CellRange) -> bool {
    range.start_row == range.end_row && range.start_col == range.end_col
}

/// `CELL_RE.fullmatch`: `$?[A-Za-z]{1,3}$?[1-9][0-9]*` -> (row, col).
fn parse_cell_address(address: &str) -> Option<(u32, u32)> {
    let rest = address.strip_prefix('$').unwrap_or(address);
    let letters = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
    if !(1..=3).contains(&letters) {
        return None;
    }
    let (column, rest) = rest.split_at(letters);
    let rest = rest.strip_prefix('$').unwrap_or(rest);
    let bytes = rest.as_bytes();
    if bytes.is_empty() || bytes[0] == b'0' || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let row = rest.parse::<u32>().ok()?;
    let col = column.bytes().fold(0u32, |total, byte| total * 26 + u32::from(byte.to_ascii_uppercase() - b'A' + 1));
    Some((row, col))
}

/// `EngineAdapter._resolve_solve_reference`.
fn resolve_reference(
    model: &dyn SolveModel,
    ranges: &[DefinedRange],
    parameter: &Location,
    block_sheet: &str,
) -> Result<Location, Unresolved> {
    let formula = model.get_formula(&parameter.sheet, parameter.row, parameter.col).map_err(Unresolved::Model)?;
    let Some(formula) = formula else { return Err(Unresolved::Invalid) };
    let mut token = formula.trim();
    if let Some(rest) = token.strip_prefix('=') {
        token = rest.trim();
    }
    if token.is_empty() {
        return Err(Unresolved::Invalid);
    }
    let folded = casefold(token);
    let matches: Vec<(&DefinedRange, &CellRange)> = ranges
        .iter()
        .filter(|defined| casefold(&defined.name) == folded)
        .filter_map(|defined| defined.range.as_ref().map(|range| (defined, range)))
        .collect();
    let workbook: Vec<_> = matches.iter().copied().filter(|(defined, _)| defined.scope == NameScope::Workbook).collect();
    let chosen = if workbook.is_empty() { matches } else { workbook };
    if chosen.len() == 1 && range_is_single(chosen[0].1) {
        return Ok(Location::named(chosen[0].0, chosen[0].1));
    }
    if !chosen.is_empty() {
        return Err(Unresolved::Invalid);
    }
    let pieces: Vec<&str> = token.split('!').collect();
    let (sheet, address) = match pieces.as_slice() {
        [address] => (block_sheet.to_owned(), *address),
        [sheet, address] => {
            let sheet = if sheet.len() >= 2 && sheet.starts_with('\'') && sheet.ends_with('\'') {
                sheet[1..sheet.len() - 1].replace("''", "'")
            } else {
                (*sheet).to_owned()
            };
            if sheet != block_sheet {
                return Err(Unresolved::Invalid);
            }
            (sheet, *address)
        }
        _ => return Err(Unresolved::Invalid),
    };
    let (row, col) = parse_cell_address(address.trim()).ok_or(Unresolved::Invalid)?;
    Ok(Location::cell(&sheet, row, col))
}

/// What `_execute_xsolve` returned: summary, note, record.
#[derive(Default)]
struct Executed {
    summary: Option<GoalSeekResult>,
    note: Option<String>,
    record: Option<Map<String, Value>>,
}

impl Executed {
    fn note(note: String) -> Self {
        Self { note: Some(note), ..Self::default() }
    }
}

/// `_finalize_xsolve(success=True)`: the audit write-backs.
fn finalize_success(
    model: &mut dyn SolveModel,
    cells: &[(&'static str, Location)],
    change_cell: &Location,
    target_cell: &Location,
    final_value: f64,
    iterations: u32,
) -> Result<(), ModelCallError> {
    let cell = |label: &str| cells.iter().find(|(known, _)| *known == label).map(|(_, location)| location);
    change_cell.set(model, number_literal(final_value)?)?;
    model.evaluate_all()?;
    let solve_target = target_cell.get(model)?;
    for label in ["solve started", "solve successful"] {
        if let Some(location) = cell(label) {
            location.set(model, LiteralValue::Int(1))?;
        }
    }
    if let Some(location) = cell("solve result") {
        location.set(model, number_literal(final_value)?)?;
    }
    if let Some(location) = cell("solve iteration") {
        location.set(model, LiteralValue::Int(i64::from(iterations)))?;
    }
    if let Some(location) = cell("solve target") {
        location.set(model, write_back_literal(&solve_target)?)?;
    }
    model.evaluate_all()
}

/// `_execute_xsolve` under the request solver. `Err` is an exception
/// (`engine_error`); skips and failures are notes.
fn execute_block(
    model: &mut dyn SolveModel,
    ranges: &[DefinedRange],
    block: &DefinedRange,
) -> Result<Executed, ModelCallError> {
    let name = block.name.strip_prefix(GOAL_SEEK_BLOCK_PREFIX).unwrap_or(&block.name).to_owned();
    let skipped = |reason: &str| Ok(Executed::note(format!("xsolve_skipped:{name}:{reason}")));
    let Some(range) = block.range.as_ref() else { return skipped("bad_range") };
    if range.start_row > range.end_row || i64::from(range.end_col) - i64::from(range.start_col) != 1 {
        return skipped("bad_range");
    }

    let mut cells: Vec<(&'static str, Location)> = Vec::new();
    for row in range.start_row..=range.end_row {
        let Some(label) = normalized_label(&model.get_value(&range.sheet, row, range.start_col)?) else {
            continue;
        };
        if cells.iter().any(|(known, _)| *known == label) {
            return skipped("bad_range");
        }
        cells.push((label, Location::cell(&range.sheet, row, range.start_col + 1)));
    }
    let cell = |label: &str| cells.iter().find(|(known, _)| *known == label).map(|(_, location)| location.clone());

    let Some(run_if) = cell("run if") else { return skipped("bad_parameter") };
    let run = match run_if.get(model)? {
        LiteralValue::Boolean(flag) => flag,
        LiteralValue::Int(number) => number == 1,
        LiteralValue::Number(number) => number == 1.0,
        _ => false,
    };
    if !run {
        return Ok(Executed::default());
    }

    if let Some(algorithm_cell) = cell("solve algorithm") {
        let brent = match algorithm_cell.get(model)? {
            LiteralValue::Text(text) => casefold(text.trim()) == "brent",
            _ => false,
        };
        if !brent {
            return skipped("unsupported_algorithm");
        }
    }

    let Some(target_parameter) = cell("target cell") else { return skipped("bad_target_cell") };
    let Some(change_parameter) = cell("by changing") else { return skipped("bad_change_cell") };
    let target_cell = match resolve_reference(model, ranges, &target_parameter, &range.sheet) {
        Ok(location) => location,
        Err(Unresolved::Invalid) => return skipped("bad_target_cell"),
        Err(Unresolved::Model(error)) => return Err(error),
    };
    let change_cell = match resolve_reference(model, ranges, &change_parameter, &range.sheet) {
        Ok(location) => location,
        Err(Unresolved::Invalid) => return skipped("bad_change_cell"),
        Err(Unresolved::Model(error)) => return Err(error),
    };

    // The restore snapshot: read (it can fail) though the request solver
    // never restores.
    let _snapshot = change_cell.get(model)?;
    let Some(target_value_cell) = cell("target value") else { return skipped("bad_parameter") };
    let target_value = finite_number(&target_value_cell.get(model)?);
    let max_change = match cell("max change") {
        Some(location) => finite_number(&location.get(model)?),
        None => Some(1.0),
    };
    let max_iterations = match cell("max iterations") {
        Some(location) => finite_number(&location.get(model)?),
        None => Some(25.0),
    };
    let guess_cell = cell("initial guess");
    let guess = match &guess_cell {
        Some(location) => finite_number(&location.get(model)?),
        None => None,
    };
    let guess_valid = guess_cell.is_none() || guess.is_some();

    let lower_cell = cell("lower bound");
    let upper_cell = cell("upper bound");
    if lower_cell.is_some() != upper_cell.is_some() {
        return skipped("bad_parameter");
    }
    let (lower, upper, bounds_valid) = match (&lower_cell, &upper_cell) {
        (Some(lower_cell), Some(upper_cell)) => {
            let lower = finite_number(&lower_cell.get(model)?);
            let upper = finite_number(&upper_cell.get(model)?);
            let valid = matches!((lower, upper), (Some(low), Some(high)) if low < high);
            (lower, upper, valid)
        }
        _ => {
            let upper = guess.map(|guess| guess * 100.0);
            let valid = matches!(upper, Some(high) if high.is_finite() && 0.0 < high);
            (Some(0.0), upper, valid)
        }
    };

    let numerics = match (target_value, max_change, max_iterations, lower, upper) {
        (Some(target_value), Some(max_change), Some(max_iterations), Some(lower), Some(upper))
            if max_change > 0.0
                && max_iterations > 0.0
                && max_iterations.fract() == 0.0
                && guess_valid
                && bounds_valid =>
        {
            Some((target_value, max_change, max_iterations, lower, upper))
        }
        _ => None,
    };
    let Some((target_value, max_change, max_iterations, lower, upper)) = numerics else {
        return Ok(Executed::note(format!("xsolve_failed:{name}:bad_numeric_parameter")));
    };
    // `int(max_iterations_value)`; beyond u32 the loop could never finish in
    // a request anyway.
    let max_iterations = max_iterations.min(f64::from(u32::MAX)) as u32;

    let solved = {
        let mut objective = |candidate: f64| -> Result<f64, BrentStop> {
            let literal = number_literal(candidate).map_err(BrentStop::Model)?;
            change_cell.set(model, literal).map_err(BrentStop::Model)?;
            model.evaluate_all().map_err(BrentStop::Model)?;
            let target = target_cell.get(model).map_err(BrentStop::Model)?;
            match finite_number(&target) {
                Some(numeric) => Ok(numeric - target_value),
                None => Err(BrentStop::TargetNotNumeric(0)),
            }
        };
        brent_solve(&mut objective, lower, upper, max_change, max_iterations)
    };
    match solved {
        Ok(solved) => {
            finalize_success(model, &cells, &change_cell, &target_cell, solved.value, solved.iterations)?;
            let mut record = Map::new();
            record.insert("suffix".into(), json!(name));
            let mut cell_records = Map::new();
            for (label, location) in &cells {
                cell_records.insert((*label).to_owned(), Value::Object(location.record.clone()));
            }
            record.insert("cells".into(), Value::Object(cell_records));
            record.insert("change_cell".into(), Value::Object(change_cell.record.clone()));
            record.insert("target_cell".into(), Value::Object(target_cell.record.clone()));
            record.insert("target_value".into(), json!(target_value));
            record.insert("root".into(), json!(solved.value));
            record.insert("iterations".into(), json!(solved.iterations));
            Ok(Executed {
                summary: Some(GoalSeekResult { target_value, result: solved.value }),
                note: Some(format!("xsolve_ran:{name} iterations={}", solved.iterations)),
                record: Some(record),
            })
        }
        Err(BrentStop::Model(error)) => Err(error),
        Err(stop) => Ok(Executed::note(format!("xsolve_failed:{name}:{}", stop.reason()))),
    }
}

/// `run_solves`' `fail`: the failure record, with the block's observed cells.
fn failure(
    model: &dyn SolveModel,
    block: &DefinedRange,
    reason: &str,
    category: &str,
    cause: Option<ModelCallError>,
    mut notes: Vec<String>,
    mut records: Vec<Map<String, Value>>,
) -> GoalSeekFailure {
    let name = block.name.strip_prefix(GOAL_SEEK_BLOCK_PREFIX).unwrap_or(&block.name);
    notes.push(format!("{category}:{name}:{reason}"));
    let mut record = Map::new();
    record.insert("type".into(), json!("solver_failure"));
    record.insert("status".into(), json!("failed"));
    record.insert("name".into(), json!(name));
    record.insert("reason".into(), json!(reason));
    record.insert("category".into(), json!(category));
    if let Some(cause) = &cause {
        record.insert("exception_type".into(), json!(exception_type(cause)));
    }
    if let Some(range) = &block.range {
        let observed: Result<Vec<Value>, ModelCallError> = (range.start_row..=range.end_row)
            .map(|row| {
                (range.start_col..=range.end_col)
                    .map(|col| model.get_value(&range.sheet, row, col).map(|value| literal_json(&value)))
                    .collect::<Result<Vec<_>, _>>()
                    .map(Value::Array)
            })
            .collect();
        match observed {
            Ok(rows) => {
                record.insert("diagnostic_observed_cells".into(), Value::Array(rows));
            }
            Err(error) => {
                record.insert("diagnostic_read_error".into(), json!(exception_type(&error)));
            }
        }
    }
    records.push(record);
    GoalSeekFailure { notes, records, cause }
}

fn literal_json(value: &LiteralValue) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// The goal-seek blocks: workbook-scope names with the import prefix
/// (case-sensitive, as `str.startswith`), in name order.
pub fn goal_seek_blocks(ranges: &[DefinedRange]) -> Vec<DefinedRange> {
    let mut blocks: Vec<DefinedRange> = ranges
        .iter()
        .filter(|defined| defined.scope == NameScope::Workbook && defined.name.starts_with(GOAL_SEEK_BLOCK_PREFIX))
        .cloned()
        .collect();
    blocks.sort_by(|left, right| left.name.cmp(&right.name));
    blocks
}

/// Run every goal-seek block of an evaluated workbook, in name order.
pub fn run_goal_seeks(model: &mut dyn SolveModel) -> Result<GoalSeekRun, GoalSeekFailure> {
    let ranges = model
        .defined_ranges()
        .map_err(|cause| GoalSeekFailure { notes: Vec::new(), records: Vec::new(), cause: Some(cause) })?;
    let mut run = GoalSeekRun::default();
    let mut keys: Vec<String> = Vec::new();
    for block in goal_seek_blocks(&ranges) {
        let suffix = block.name[GOAL_SEEK_BLOCK_PREFIX.len()..].to_owned();
        if keys.contains(&casefold(&suffix)) {
            return Err(failure(model, &block, "output_collision", "xsolve_failed", None, run.notes, run.records));
        }
        let executed = match execute_block(model, &ranges, &block) {
            Ok(executed) => executed,
            Err(cause) => {
                return Err(failure(model, &block, "engine_error", "xsolve_failed", Some(cause), run.notes, run.records));
            }
        };
        if let Some(note) = &executed.note {
            if note.starts_with("xsolve_failed:") || note.starts_with("xsolve_skipped:") {
                let reason = note.rsplit_once(':').map_or(note.as_str(), |(_, reason)| reason);
                let category = note.split_once(':').map_or(note.as_str(), |(category, _)| category);
                return Err(failure(model, &block, reason, category, None, run.notes, run.records));
            }
            run.notes.push(note.clone());
        }
        if let Some(summary) = executed.summary {
            keys.push(casefold(&suffix));
            run.results.0.push((suffix, summary));
            if let Some(mut record) = executed.record {
                record.insert("status".into(), json!("converged"));
                run.records.push(record);
            }
        }
    }
    Ok(run)
}

/// `_BrentResult`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrentResult {
    pub value: f64,
    pub iterations: u32,
}

/// Why Brent stopped without a root (`_NoBracket`, `_ZeroSlope`,
/// `_MaxIterations`, `_TargetNotNumeric`), or a model failure.
#[derive(Debug, Clone, PartialEq)]
pub enum BrentStop {
    NoBracket,
    ZeroSlope,
    MaxIterations(u32),
    TargetNotNumeric(u32),
    Model(ModelCallError),
}

impl BrentStop {
    /// The failure reason in notes and records.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::NoBracket => "no_bracket",
            Self::ZeroSlope => "zero_slope",
            Self::MaxIterations(_) => "max_iterations",
            Self::TargetNotNumeric(_) => "target_not_numeric",
            Self::Model(_) => "engine_error",
        }
    }
}

/// Python `min(x, y)`: the first argument unless the second is smaller.
fn py_min(x: f64, y: f64) -> f64 {
    if y < x { y } else { x }
}

/// Python `max(x, y)`: the first argument unless the second is larger.
fn py_max(x: f64, y: f64) -> f64 {
    if y > x { y } else { x }
}

/// Python float division: a zero divisor raises `ZeroDivisionError`.
fn py_div(numerator: f64, denominator: f64) -> Result<f64, BrentStop> {
    if denominator == 0.0 {
        return Err(BrentStop::Model(ModelCallError::infrastructure("ZeroDivisionError", "float division by zero")));
    }
    Ok(numerator / denominator)
}

/// `_brent_solve(objective, lower, upper, max_change, max_iterations)`.
///
/// The objective returns `TargetNotNumeric(_)` for a non-numeric target; the
/// iteration count it carries is set here as Python sets `exc.iterations`
/// (0 for the two bracket evaluations).
pub fn brent_solve(
    objective: &mut dyn FnMut(f64) -> Result<f64, BrentStop>,
    lower: f64,
    upper: f64,
    max_change: f64,
    max_iterations: u32,
) -> Result<BrentResult, BrentStop> {
    let initial = |stop: BrentStop| match stop {
        BrentStop::TargetNotNumeric(_) => BrentStop::TargetNotNumeric(0),
        other => other,
    };
    let (mut a, mut b) = (lower, upper);
    let mut fa = objective(a).map_err(initial)?;
    let mut fb = objective(b).map_err(initial)?;
    if fa == 0.0 {
        return Ok(BrentResult { value: a, iterations: 0 });
    }
    if fb == 0.0 {
        return Ok(BrentResult { value: b, iterations: 0 });
    }
    if fa == fb {
        return Err(BrentStop::ZeroSlope);
    }
    if fa * fb > 0.0 {
        return Err(BrentStop::NoBracket);
    }
    if fa.abs() < fb.abs() {
        std::mem::swap(&mut a, &mut b);
        std::mem::swap(&mut fa, &mut fb);
    }

    let (mut c, mut fc) = (a, fa);
    let mut d = c;
    let mut used_bisection = true;
    let mut previous_probe = b;
    for iteration in 1..=max_iterations {
        let mut candidate = if fa != fc && fb != fc {
            py_div(a * fb * fc, (fa - fb) * (fa - fc))?
                + py_div(b * fa * fc, (fb - fa) * (fb - fc))?
                + py_div(c * fa * fb, (fc - fa) * (fc - fb))?
        } else {
            let denominator = fb - fa;
            if denominator == 0.0 {
                // ES-039: a flat interpolation step is #NUM!, never bisection.
                return Err(BrentStop::ZeroSlope);
            }
            b - fb * (b - a) / denominator
        };

        let boundary = (3.0 * a + b) / 4.0;
        let outside = !(py_min(boundary, b) < candidate && candidate < py_max(boundary, b));
        let too_large_after_bisection = used_bisection && (candidate - b).abs() >= (b - c).abs() / 2.0;
        let too_large_after_interpolation = !used_bisection && (candidate - b).abs() >= (c - d).abs() / 2.0;
        let stalled_after_bisection = used_bisection && (b - c).abs() < max_change;
        let stalled_after_interpolation = !used_bisection && (c - d).abs() < max_change;
        if outside
            || too_large_after_bisection
            || too_large_after_interpolation
            || stalled_after_bisection
            || stalled_after_interpolation
        {
            candidate = (a + b) / 2.0;
            used_bisection = true;
        } else {
            used_bisection = false;
        }

        let value = match objective(candidate) {
            Ok(value) => value,
            Err(BrentStop::TargetNotNumeric(_)) => return Err(BrentStop::TargetNotNumeric(iteration)),
            Err(other) => return Err(other),
        };
        if value == 0.0 {
            return Ok(BrentResult { value: candidate, iterations: iteration });
        }
        let probe_change = (candidate - previous_probe).abs();
        previous_probe = candidate;
        d = c;
        c = b;
        fc = fb;
        if fa * value < 0.0 {
            b = candidate;
            fb = value;
        } else {
            a = candidate;
            fa = value;
        }
        if fa.abs() < fb.abs() {
            std::mem::swap(&mut a, &mut b);
            std::mem::swap(&mut fa, &mut fb);
        }
        if probe_change <= max_change && (b - a).abs() <= max_change {
            return Ok(BrentResult { value: candidate, iterations: iteration });
        }
    }
    Err(BrentStop::MaxIterations(max_iterations))
}
