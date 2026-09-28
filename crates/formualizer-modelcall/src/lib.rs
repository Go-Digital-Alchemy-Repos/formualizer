//! Child-model call calculation path (GOD-383).
//!
//! A parent workbook calls a pinned child model through the `MDL.CALLMODEL`
//! custom function. This crate is the Rust port of the parity repository's
//! `workbook_runtime` request core (`runtime.py`, `callbacks.py`,
//! `ports.py`, `prefetch.py`) and of the goal seek in
//! `replayer/engine_adapter.py` (`_execute_xsolve` / `_finalize_xsolve`).
//!
//! Lane 0 fixes the contract: the public types in [`spec`], [`context`],
//! [`event`], [`error`], [`key`], [`evaluator`], [`receipt`] and
//! [`import_boundary`]. The behaviour modules are placeholders owned by later
//! lanes (see `docs/modelcall_contract.md`):
//!
//! * Lane A: [`session`], [`router`], [`memo`], [`ports`] and the Python
//!   binding (`bindings/python/src/modelcall.rs`, `lib.rs`).
//! * Lane B: [`prefetch`], [`goal_seek`].
//!
//! Placeholders never panic: each fallible entry point returns
//! [`ModelCallError::NotImplemented`], which maps to `#CALC!`.
//!
//! Reentrancy rule (binding): a call handler never touches the parent
//! `Workbook` it was invoked from. A child is evaluated on a separate
//! `Workbook` instance; the only parent call allowed from inside a handler (or
//! from another thread) is `cancel()`.

pub mod context;
pub mod error;
pub mod evaluator;
pub mod event;
pub mod import_boundary;
pub mod key;
pub mod receipt;
pub mod spec;

// Lane A.
pub mod memo;
pub mod ports;
pub mod router;
pub mod session;

// Lane B.
pub mod goal_seek;
pub mod prefetch;

// Lane I: retained workbooks (pool, warm, sealing).
pub mod retained;

pub use context::{CalculationContext, CalculationFlags, Operation};
pub use error::ModelCallError;
pub use evaluator::{ChildEvaluator, ChildMatrix, ChildRequest, CompiledChildHook, SolveModel};
pub use event::{CallStatus, ModelCallEvent};
pub use import_boundary::{CALL_MODEL_FUNCTION, IMPORTED_CALL_NAMES, call_function_names};
pub use key::{
    InputPairs, KeyForm, MemoKey, MemoToken, Unmemoisable, callback_inputs, casefold,
    matrix_is_finished, matrix_is_memoisable, memo_token,
};
pub use receipt::{CalculationResult, plain_port_value, plain_value};
pub use retained::{RetainedModel, RetainedPool, WarmReport};
pub use session::ModelSession;
pub use spec::{CellRange, GoalSeekSpec, ModelPackage, ModelSpec, PortLocation, UnknownInputPolicy};
