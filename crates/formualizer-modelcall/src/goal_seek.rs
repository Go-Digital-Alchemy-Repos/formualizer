//! Lane B: goal seek (`engine_adapter._execute_xsolve` / `_finalize_xsolve`
//! and `solve.run_solves`): blocks rediscovered from the workbook's
//! workbook-scope defined names with the import prefix, sorted by name;
//! settings read by label; Brent-Dekker as `_brent_solve`; the same skip and
//! failure categories and notes; audit write-backs on success; a required
//! failure fails the request with its records. Lane 0 placeholder: every
//! entry point returns `ModelCallError::NotImplemented`.

use serde_json::{Map, Value};

use crate::evaluator::SolveModel;
use crate::spec::OrderedMap;
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

/// Run every goal-seek block of an evaluated workbook, in name order.
pub fn run_goal_seeks(_model: &mut dyn SolveModel) -> Result<GoalSeekRun, GoalSeekFailure> {
    Err(GoalSeekFailure {
        notes: Vec::new(),
        records: Vec::new(),
        cause: Some(ModelCallError::NotImplemented("goal_seek::run_goal_seeks")),
    })
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

/// `_brent_solve(objective, lower, upper, max_change, max_iterations)`.
pub fn brent_solve(
    _objective: &mut dyn FnMut(f64) -> Result<f64, BrentStop>,
    _lower: f64,
    _upper: f64,
    _max_change: f64,
    _max_iterations: u32,
) -> Result<BrentResult, BrentStop> {
    Err(BrentStop::Model(ModelCallError::NotImplemented("goal_seek::brent_solve")))
}
