//! Lane A: the `MDL.CALLMODEL` handler (`callbacks.ChildRouter`, renamed
//! `ModelCallRouter`): deadline check, child resolution, cycle and depth
//! refusal, input parsing, memo, prefetch dispatch, child evaluation on a
//! separate `Workbook`, error mapping (`#REF!` / `#CALC!`), fault recording
//! and parent `cancel()`.
//!
//! Reentrancy: the handler holds only the request core and the cancel flag of
//! the workbook it is registered on. It never reads, writes or evaluates that
//! workbook; after a fault it sets the flag, the one permitted action.
//!
//! Run-state seam (CP1 finding 2): an in-line child is evaluated on the SAME
//! request core (`RunCore`: invocations, memo, faults, timings shared through
//! `Mutex<RunState>`), with the caller stack extended by the child. Rule: no
//! run-state lock (state, prefetch, pool) is held across `evaluate`,
//! `calculate_child` or a hook call; each lock is taken for one bookkeeping
//! step and dropped. The only lock held across an evaluation is the evaluated
//! workbook's own `RwLock`, and a handler never touches that workbook.

use formualizer_common::{ExcelError, ExcelErrorKind, LiteralValue};
use formualizer_workbook::{CustomFnHandler, CustomFnOptions, Workbook};
use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{INFRASTRUCTURE_ERROR_MESSAGE, UNBOUND_ERROR_MESSAGE};
use crate::event::{CallStatus, ModelCallEvent};
use crate::import_boundary::call_function_names;
use crate::key::{MemoKey, callback_inputs};
use crate::ports::native_matrix;
use crate::prefetch::ObservedCall;
use crate::session::RunCore;
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

/// One caller's router: its stack is fixed when the workbook is loaded.
#[derive(Clone)]
pub struct ModelCallRouter {
    pub stack: Vec<String>,
    core: Arc<RunCore>,
    /// The cancel flag of the workbook this router is registered on; `None`
    /// for a nested router a compiled child calls through (Python's router
    /// with `workbook=None`).
    workbook_cancel: Option<Arc<AtomicBool>>,
}

impl std::fmt::Debug for ModelCallRouter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ModelCallRouter").field("stack", &self.stack).finish()
    }
}

fn error_value(kind: ExcelErrorKind, message: &str) -> LiteralValue {
    LiteralValue::Error(ExcelError::new(kind).with_message(message.to_owned()))
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "panic".to_owned()
    }
}

impl ModelCallRouter {
    pub(crate) fn bound(core: Arc<RunCore>, stack: Vec<String>, workbook_cancel: Arc<AtomicBool>) -> Self {
        Self { stack, core, workbook_cancel: Some(workbook_cancel) }
    }

    pub(crate) fn nested(core: Arc<RunCore>, stack: Vec<String>) -> Self {
        Self { stack, core, workbook_cancel: None }
    }

    /// How many infrastructure faults this request has recorded so far
    /// (`len(session.faults)`): a compiled child compares it before and after
    /// its run, as `CompiledRoute.attempt` does.
    pub fn fault_count(&self) -> usize {
        self.core.state().fault_indices.len()
    }

    /// The request context of the run this router belongs to (clock,
    /// deadline, flags): what a native compiled child needs (package B S1).
    pub fn context(&self) -> &crate::CalculationContext {
        &self.core.context
    }

    /// The error of the first fault recorded after the first `before` faults.
    pub(crate) fn fault_error_since(&self, before: usize) -> Option<String> {
        self.core.state().fault_error_since(before)
    }

    fn update(&self, index: usize, edit: impl FnOnce(&mut ModelCallEvent)) {
        if let Some(event) = self.core.state().invocations.get_mut(index) {
            edit(event);
        }
    }

    /// Route one call. The value is the child's output matrix (an array),
    /// `#REF!` with the refusal for a routing error, or `#CALC!` for a fault
    /// (which is recorded, cancels the calling workbook and fails the run).
    pub fn call(&self, args: &[LiteralValue]) -> LiteralValue {
        if self.core.closed.load(Ordering::SeqCst) {
            return error_value(ExcelErrorKind::Calc, UNBOUND_ERROR_MESSAGE);
        }
        let arguments = match CallArguments::split(args) {
            Ok(arguments) => arguments,
            Err(error) => return LiteralValue::Error(error.to_excel_error()),
        };
        let index = {
            let mut state = self.core.state();
            let index = state.invocations.len();
            state.invocations.push(ModelCallEvent::started(
                index,
                &self.stack,
                arguments.target.clone(),
                arguments.output.clone(),
            ));
            index
        };
        let mut missed: Option<MemoKey> = None;
        let mut dispatched = false;
        let outcome = catch_unwind(AssertUnwindSafe(|| self.attempt(index, arguments, &mut missed, &mut dispatched)))
            .unwrap_or_else(|payload| Err(ModelCallError::infrastructure("PanicException", panic_text(&*payload))));
        let value = match outcome {
            Ok(value) => value,
            Err(ModelCallError::Routing(message)) => {
                let returned = ExcelError::new(ExcelErrorKind::Ref).with_message(message.clone());
                self.update(index, |event| {
                    event.status = CallStatus::RoutingError;
                    event.error = Some(message);
                    event.returned_error = Some(returned.clone());
                });
                LiteralValue::Error(returned)
            }
            Err(error) => {
                let returned = ExcelError::new(ExcelErrorKind::Calc).with_message(INFRASTRUCTURE_ERROR_MESSAGE);
                {
                    let mut state = self.core.state();
                    if let Some(event) = state.invocations.get_mut(index) {
                        event.status = CallStatus::InfrastructureError;
                        event.error = Some(error.event_error());
                        event.returned_error = Some(returned.clone());
                    }
                    state.fault_indices.push(index);
                }
                // The run is doomed; stop the caller at its next layer boundary.
                if let Some(flag) = &self.workbook_cancel {
                    flag.store(true, Ordering::SeqCst);
                }
                LiteralValue::Error(returned)
            }
        };
        if missed.is_some()
            && let Some(memo) = self.core.state().memo.as_mut()
        {
            memo.not_stored();
        }
        if dispatched {
            let mut prefetch = self.core.prefetch.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(prefetch) = prefetch.as_mut() {
                let mut state = self.core.state();
                let failed = state.invocations.get(index).is_some_and(|event| event.status == CallStatus::InfrastructureError);
                if failed {
                    prefetch.abandon(&mut state.timings);
                } else {
                    let state = &mut *state;
                    if let Some(memo) = state.memo.as_mut() {
                        prefetch.join_into(&mut state.invocations, memo, &mut state.timings);
                    }
                }
            }
        }
        value
    }

    fn attempt(
        &self,
        index: usize,
        arguments: CallArguments<'_>,
        missed: &mut Option<MemoKey>,
        dispatched: &mut bool,
    ) -> Result<LiteralValue, ModelCallError> {
        let core = &self.core;
        core.check_deadline()?;
        let (version, child) = core.resolve_child(arguments.target)?;
        let identity = child.model_identity();
        self.update(index, |event| event.child = Some(identity.clone()));
        if self.stack.contains(&identity) {
            return Err(ModelCallError::routing("active workbook child cycle rejected"));
        }
        if self.stack.len() >= core.context.max_depth as usize {
            return Err(ModelCallError::routing("maximum child depth exceeded"));
        }
        let inputs = callback_inputs(arguments.block, arguments.tail)?;
        self.update(index, |event| event.inputs = Some(inputs.clone()));
        let memo_on = core.state().memo.is_some();
        if memo_on {
            let (key, found) = {
                let mut state = core.state();
                let state = &mut *state;
                let memo = state.memo.as_mut().ok_or_else(|| ModelCallError::infrastructure("RuntimeError", "memo"))?;
                let key = memo.key(&self.stack, &identity, arguments.output, &inputs, Some(child));
                let found = key.as_ref().and_then(|key| memo.lookup(key));
                if let Some(entry) = &found
                    && let Some(event) = state.invocations.get_mut(index)
                {
                    event.status = CallStatus::Memoized;
                    event.memo_of = Some(entry.source_index);
                    event.matrix = Some(entry.matrix.clone());
                }
                (key, found)
            };
            if let Some(entry) = found {
                if let Some(key) = &key {
                    let mut prefetch = core.prefetch.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Some(prefetch) = prefetch.as_mut() {
                        prefetch.note_hit(key, &mut core.state().timings);
                    }
                }
                return native_matrix(&entry.matrix);
            }
            missed.clone_from(&key);
            if let Some(key) = &key {
                let mut prefetch = core.prefetch.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(prefetch) = prefetch.as_mut() {
                    let target = match arguments.target {
                        LiteralValue::Text(target) => target.as_str(),
                        _ => "",
                    };
                    let call = ObservedCall {
                        stack: &self.stack,
                        target,
                        output: arguments.output,
                        identity: &identity,
                        child_version: &version,
                        child,
                        inputs: &inputs,
                        observed_key: key,
                    };
                    let mut state = core.state();
                    let state = &mut *state;
                    if let Some(memo) = state.memo.as_ref() {
                        *dispatched = prefetch.dispatch(memo, call, &mut state.timings);
                    }
                }
            }
        }
        let mut stack = self.stack.clone();
        stack.push(identity);
        let matrix = core.calculate_child(child, &inputs, arguments.output, &stack, Some(index))?;
        self.update(index, |event| {
            event.status = CallStatus::Completed;
            event.matrix = Some(matrix.clone());
        });
        let native = native_matrix(&matrix)?;
        if let Some(key) = missed.take()
            && let Some(memo) = core.state().memo.as_mut()
        {
            memo.store(key, index, &matrix);
        }
        Ok(native)
    }
}

/// The call function's binding on one loaded workbook (`RouterBinding`): the
/// registration outlives every run; a run only swaps the router behind it.
/// Unbound, a call answers `#CALC!` (`child router unbound`).
#[derive(Default)]
pub struct RouterSlot(Mutex<Option<ModelCallRouter>>);

impl std::fmt::Debug for RouterSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("RouterSlot").field("bound", &self.is_bound()).finish()
    }
}

impl RouterSlot {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, Option<ModelCallRouter>> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `RouterBinding.bind(runtime, stack)`.
    pub(crate) fn bind(&self, router: ModelCallRouter) {
        *self.guard() = Some(router);
    }

    /// `RouterBinding.unbind()`: also breaks the workbook -> router -> run cycle.
    pub fn unbind(&self) {
        *self.guard() = None;
    }

    pub fn is_bound(&self) -> bool {
        self.guard().is_some()
    }

    /// The bound router, cloned out so no lock is held across the call.
    fn router(&self) -> Option<ModelCallRouter> {
        self.guard().clone()
    }
}

/// The registered handler: one binding per loaded workbook.
struct CallHandler {
    slot: Arc<RouterSlot>,
}

impl CustomFnHandler for CallHandler {
    fn call(&self, args: &[LiteralValue]) -> Result<LiteralValue, ExcelError> {
        Ok(match self.slot.router() {
            Some(router) => router.call(args),
            None => error_value(ExcelErrorKind::Calc, UNBOUND_ERROR_MESSAGE),
        })
    }
}

/// Register the call function under `MDL.CALLMODEL` and every imported alias
/// (`RouterBinding.register`): `min_args = 3`, no maximum, not volatile,
/// deterministic, not thread-safe. An occupied name is unregistered first.
pub(crate) fn register_call_handler(workbook: &mut Workbook, slot: Arc<RouterSlot>) -> Result<(), ModelCallError> {
    let handler: Arc<dyn CustomFnHandler> = Arc::new(CallHandler { slot });
    for name in call_function_names() {
        let _ = workbook.unregister_custom_function(name);
        let options = CustomFnOptions {
            min_args: MIN_ARGS,
            max_args: None,
            volatile: false,
            thread_safe: false,
            deterministic: true,
            allow_override_builtin: false,
        };
        workbook
            .register_custom_function(name, options, handler.clone())
            .map_err(|error| ModelCallError::infrastructure("RuntimeError", error.to_string()))?;
    }
    Ok(())
}

thread_local! {
    static NESTED: RefCell<Vec<ModelCallRouter>> = const { RefCell::new(Vec::new()) };
}

/// Run `f` with `router` as the router a compiled child's own calls go
/// through (`CompiledRoute.attempt`'s `ChildRouter(session, stack)`).
pub(crate) fn with_nested_router<T>(router: ModelCallRouter, f: impl FnOnce() -> T) -> T {
    NESTED.with(|stack| stack.borrow_mut().push(router));
    struct Pop;
    impl Drop for Pop {
        fn drop(&mut self) {
            NESTED.with(|stack| {
                stack.borrow_mut().pop();
            });
        }
    }
    let _pop = Pop;
    f()
}

/// The router of the compiled child attempt running on this thread, if any.
/// The Python binding hands it to the hook as the child's `xcall`.
pub fn current_nested_router() -> Option<ModelCallRouter> {
    NESTED.with(|stack| stack.borrow().last().cloned())
}
