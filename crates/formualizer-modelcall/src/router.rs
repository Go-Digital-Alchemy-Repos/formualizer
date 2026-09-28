//! Lane A: the `MDL.CALLMODEL` handler (`callbacks.ChildRouter`, renamed
//! `ModelCallRouter`): deadline check, child resolution, cycle and depth
//! refusal, input parsing, memo, prefetch dispatch, child evaluation on a
//! separate `Workbook`, error mapping (`#REF!` / `#CALC!`), fault recording
//! and parent `cancel()`. Lane 0 placeholder: argument splitting is real; the
//! call itself returns `ModelCallError::NotImplemented`.

use formualizer_common::LiteralValue;

use crate::evaluator::ChildMatrix;
use crate::ModelCallError;

/// Minimum argument count the function is registered with (`min_args=3`).
pub const MIN_ARGS: usize = 3;

/// The call's arguments: `(target, block, output, *tail)`.
#[derive(Debug, Clone, Copy)]
pub struct CallArguments<'a> {
    pub target: &'a LiteralValue,
    pub block: &'a LiteralValue,
    pub output: &'a LiteralValue,
    pub tail: &'a [LiteralValue],
}

impl<'a> CallArguments<'a> {
    pub fn split(args: &'a [LiteralValue]) -> Result<Self, ModelCallError> {
        match args {
            [target, block, output, tail @ ..] => Ok(Self { target, block, output, tail }),
            _ => Err(ModelCallError::routing("child call needs a target, an input block and an output")),
        }
    }
}

/// One caller's router: its stack is fixed when the workbook is bound to a run.
#[derive(Debug, Clone)]
pub struct ModelCallRouter {
    pub stack: Vec<String>,
}

impl ModelCallRouter {
    pub fn new(stack: Vec<String>) -> Self {
        Self { stack }
    }

    /// Route one call to its child's output matrix. The handler turns an
    /// `Err` into `err.to_excel_error()` for the cell.
    pub fn call(&self, args: &[LiteralValue]) -> Result<ChildMatrix, ModelCallError> {
        CallArguments::split(args)?;
        Err(ModelCallError::NotImplemented("router::ModelCallRouter::call"))
    }
}
