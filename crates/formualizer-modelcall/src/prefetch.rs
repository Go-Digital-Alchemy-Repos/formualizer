//! Lane B: sibling prefetch (`workbook_runtime/prefetch.py`): the sibling
//! plan read from the parent's formulas with `formualizer-parse` (call names
//! from `import_boundary::call_function_names`), `sibling_vectors`,
//! `_harmonise`, and flights run through `ChildEvaluator` (one loaded child
//! evaluated for several scenarios where siblings share it). Lane 0
//! placeholder: no plan is ever found, so no flight is dispatched.

use formualizer_common::LiteralValue;
use std::sync::Arc;

use crate::evaluator::ChildEvaluator;
use crate::event::ModelCallEvent;
use crate::key::{InputPairs, MemoKey};
use crate::memo::ModelCallMemo;
use crate::receipt::{PrefetchReport, Timings};
use crate::spec::{ModelPackage, ModelSpec};
use crate::{CalculationContext, ModelCallError};

/// One group of sibling call sites: same target and output, differing inputs.
#[derive(Debug, Clone, PartialEq)]
pub struct SiblingGroup {
    pub target: String,
    pub output: String,
    /// Per call site, the constant input pairs it passes (`_group_plan`).
    pub members: Vec<InputPairs>,
}

/// `SiblingPlan`: every group in the parent workbook.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SiblingPlan {
    pub groups: Vec<SiblingGroup>,
}

/// `sibling_plan(spec)`: `Ok(None)` when the parent has no sibling group.
pub fn sibling_plan(_spec: &ModelSpec) -> Result<Option<SiblingPlan>, ModelCallError> {
    Ok(None)
}

/// `sibling_vectors(plan, target, output, inputs)`: the other members'
/// input vectors, harmonised to the observed call's value styles.
pub fn sibling_vectors(
    _plan: &SiblingPlan,
    _target: &str,
    _output: &str,
    _inputs: &[(String, LiteralValue)],
) -> Vec<InputPairs> {
    Vec::new()
}

/// The in-line call a dispatch is made for.
#[derive(Debug, Clone, Copy)]
pub struct ObservedCall<'a> {
    pub stack: &'a [String],
    pub target: &'a str,
    pub output: &'a LiteralValue,
    /// Child `identity:sha256`.
    pub identity: &'a str,
    /// Key into `package.children`.
    pub child_version: &'a str,
    pub child: &'a ModelSpec,
    pub inputs: &'a [(String, LiteralValue)],
    pub observed_key: &'a MemoKey,
}

pub struct Prefetcher {
    plan: SiblingPlan,
    max_flights: u32,
    store_errors: bool,
    evaluator: Arc<dyn ChildEvaluator>,
    package: Arc<ModelPackage>,
    context: CalculationContext,
}

impl Prefetcher {
    pub fn new(
        plan: SiblingPlan,
        package: Arc<ModelPackage>,
        context: CalculationContext,
        evaluator: Arc<dyn ChildEvaluator>,
    ) -> Self {
        let max_flights = context.flags.prefetch_max.max(1);
        let store_errors = context.flags.prefetch_errors;
        Self { plan, max_flights, store_errors, evaluator, package, context }
    }

    pub fn plan(&self) -> &SiblingPlan {
        &self.plan
    }

    /// Start sibling flights for a parent-level miss; true if any started.
    /// Never fails. Records prefetch timings on first dispatch.
    pub fn dispatch(&mut self, _memo: &ModelCallMemo, _call: ObservedCall<'_>, _timings: &mut Timings) -> bool {
        let _ = (&self.evaluator, &self.package, &self.context, self.max_flights, self.store_errors);
        false
    }

    /// The in-line call hit a key a flight stored.
    pub fn note_hit(&mut self, _key: &MemoKey, _timings: &mut Timings) {}

    /// Join flights in dispatch order, merge their events (offset indices,
    /// `prefetch: true`) and adopt their results into the memo. Never fails.
    pub fn join_into(&mut self, _invocations: &mut Vec<ModelCallEvent>, _memo: &mut ModelCallMemo, _timings: &mut Timings) {}

    /// Cancel and join every flight; nothing is stored. Never fails.
    pub fn abandon(&mut self, _timings: &mut Timings) {}

    /// `None` until something was dispatched.
    pub fn report(&self) -> Option<PrefetchReport> {
        None
    }
}
