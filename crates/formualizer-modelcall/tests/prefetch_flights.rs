//! The request-scoped prefetcher (Lane B): dispatch, join, adopt, abandon,
//! counters and timings, over a fake child evaluator and a fake memo that
//! follows the `ModelCallMemo` contract (`peek_key` / `contains` / `adopt`).

use chrono::DateTime;
use formualizer_common::{ExcelError, ExcelErrorKind, LiteralValue};
use formualizer_modelcall::evaluator::{ChildOutcome, ChildRequest};
use formualizer_modelcall::event::CallStatus;
use formualizer_modelcall::prefetch::{
    ChildBatchEvaluator, ObservedCall, PlanValue, Position, PositionKind, PrefetchMemo, Prefetcher, SequentialBatch,
    SiblingGroup, SiblingPlan,
};
use formualizer_modelcall::receipt::{TimingValue, Timings};
use formualizer_modelcall::{
    CalculationContext, CalculationFlags, ChildEvaluator, ChildMatrix, KeyForm, MemoKey, ModelCallEvent, ModelPackage,
    ModelSpec, Operation,
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PARENT: &str = "parent:psha";
const CHILD: &str = "child:csha";

fn spec(identity: &str) -> serde_json::Value {
    json!({"identity": identity, "workbook_path": "/nonexistent.xlsx", "workbook_sha256": "sha",
           "manifest": {}, "inputs": {}, "outputs": {}})
}

fn package() -> Arc<ModelPackage> {
    Arc::new(
        serde_json::from_value(json!({
            "package_id": "p", "parent": spec("parent"), "children": {"child": spec("child")},
            "child_routes": {"Folder/Svc": "child"}
        }))
        .unwrap(),
    )
}

fn context(prefetch_max: u32, prefetch_errors: bool) -> CalculationContext {
    let flags = CalculationFlags { prefetch: true, prefetch_max, prefetch_errors, ..CalculationFlags::default() };
    let now = DateTime::parse_from_rfc3339("2026-09-28T12:00:00+00:00").unwrap();
    CalculationContext::new(now, Operation::Client, 147, None, 32, flags).unwrap()
}

/// One group: `term` differs by constant (10, 20, 30, 40); `rate` identical.
fn plan() -> SiblingPlan {
    let term = |value: i64| PlanValue::Constant(LiteralValue::Int(value));
    SiblingPlan {
        groups: vec![SiblingGroup {
            target: "Folder/Svc".into(),
            output: "Premium".into(),
            cells: vec!["S!A1".into(), "S!A2".into(), "S!A3".into(), "S!A4".into()],
            positions: vec![Position {
                name: "term".into(),
                kind: PositionKind::Override,
                values: vec![term(10), term(20), term(30), term(40)],
            }],
        }],
        call_cells: 4,
        unresolved_calls: 0,
        rejected_groups: 0,
    }
}

#[derive(Default)]
struct FakeMemo {
    entries: HashMap<MemoKey, (usize, ChildMatrix)>,
}

impl PrefetchMemo for FakeMemo {
    fn peek_key(
        &self,
        stack: &[String],
        identity: &str,
        output: &LiteralValue,
        inputs: &[(String, LiteralValue)],
        _spec: Option<&ModelSpec>,
    ) -> Option<MemoKey> {
        MemoKey::new(stack, identity, output, KeyForm::Inputs, inputs).ok()
    }
    fn contains(&self, key: &MemoKey) -> bool {
        self.entries.contains_key(key)
    }
    fn adopt(&mut self, key: MemoKey, source_index: usize, matrix: &ChildMatrix, allow_errors: bool) -> bool {
        let admissible = if allow_errors {
            formualizer_modelcall::matrix_is_finished(matrix)
        } else {
            formualizer_modelcall::matrix_is_memoisable(matrix)
        };
        if !admissible || self.entries.contains_key(&key) {
            return false;
        }
        self.entries.insert(key, (source_index, matrix.clone()));
        true
    }
}

/// The child's result is `[[term * 2]]`; term 30 returns `#DIV/0!`, term 40
/// fails. Each call records one grandchild event (index 0) and a memoized
/// one (index 1, `memo_of` 0). `gate` holds evaluations until cancelled.
#[derive(Default)]
struct FakeEvaluator {
    calls: AtomicUsize,
    seen: Mutex<Vec<ChildRequest>>,
    wait_for_cancel: bool,
}

fn term_of(request: &ChildRequest) -> f64 {
    match request.inputs.iter().find(|(name, _)| name == "term").map(|(_, value)| value) {
        Some(LiteralValue::Int(value)) => *value as f64,
        Some(LiteralValue::Number(value)) => *value,
        _ => f64::NAN,
    }
}

impl ChildEvaluator for FakeEvaluator {
    fn evaluate_child(&self, request: &ChildRequest) -> ChildOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(request.clone());
        if self.wait_for_cancel {
            while !request.cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            return ChildOutcome::failed(formualizer_modelcall::ModelCallError::infrastructure("Cancelled", "x"));
        }
        let term = term_of(request);
        if term == 40.0 {
            return ChildOutcome::failed(formualizer_modelcall::ModelCallError::infrastructure("RuntimeError", "boom"));
        }
        let cell = if term == 30.0 {
            LiteralValue::Error(ExcelError::new(ExcelErrorKind::Div))
        } else {
            LiteralValue::Number(term * 2.0)
        };
        let stack = request.stack.clone();
        let mut grandchild = ModelCallEvent::started(0, &stack, LiteralValue::Text("G/Svc".into()), LiteralValue::Text("o".into()));
        grandchild.status = CallStatus::Completed;
        let mut memoized = grandchild.clone();
        memoized.index = 1;
        memoized.status = CallStatus::Memoized;
        memoized.memo_of = Some(0);
        ChildOutcome { result: Ok(vec![vec![cell]]), invocations: vec![grandchild, memoized], ..ChildOutcome::failed(formualizer_modelcall::ModelCallError::infrastructure("x", "x")) }
    }
}

struct Call {
    stack: Vec<String>,
    output: LiteralValue,
    inputs: Vec<(String, LiteralValue)>,
    key: MemoKey,
    child: ModelSpec,
}

fn observed(term: f64) -> Call {
    let stack = vec![PARENT.to_owned()];
    let output = LiteralValue::Text("Premium".into());
    let inputs = vec![("term".to_owned(), LiteralValue::Number(term)), ("rate".to_owned(), LiteralValue::Number(0.05))];
    let key = MemoKey::new(&stack, CHILD, &output, KeyForm::Inputs, &inputs).unwrap();
    Call { stack, output, inputs, key, child: serde_json::from_value(spec("child")).unwrap() }
}

impl Call {
    fn view(&self) -> ObservedCall<'_> {
        ObservedCall {
            stack: &self.stack,
            target: "Folder/Svc",
            output: &self.output,
            identity: CHILD,
            child_version: "child",
            child: &self.child,
            inputs: &self.inputs,
            observed_key: &self.key,
        }
    }
}

fn sibling_key(call: &Call, term: f64) -> MemoKey {
    let mut inputs = call.inputs.clone();
    inputs[0].1 = LiteralValue::Number(term);
    MemoKey::new(&call.stack, CHILD, &call.output, KeyForm::Inputs, &inputs).unwrap()
}

fn run(prefetcher: &mut Prefetcher, memo: &mut FakeMemo, call: &Call) -> (bool, Vec<ModelCallEvent>, Timings) {
    let mut timings = Timings::base();
    let dispatched = prefetcher.dispatch_on(memo, call.view(), &mut timings);
    // The in-line call's own event, recorded before the join.
    let mut invocations = vec![ModelCallEvent::started(0, &call.stack, LiteralValue::Text("Folder/Svc".into()), call.output.clone())];
    prefetcher.join_into_on(&mut invocations, memo, &mut timings);
    (dispatched, invocations, timings)
}

#[test]
fn one_flight_by_default_merges_events_and_adopts_its_result() {
    let evaluator = Arc::new(FakeEvaluator::default());
    let mut prefetcher = Prefetcher::new(plan(), package(), context(1, false), evaluator.clone());
    let mut memo = FakeMemo::default();
    let call = observed(10.0);
    let (dispatched, invocations, timings) = run(&mut prefetcher, &mut memo, &call);
    assert!(dispatched);
    assert_eq!(evaluator.calls.load(Ordering::SeqCst), 1);
    // The sibling is harmonised to the observed float spelling of `term`.
    let request = evaluator.seen.lock().unwrap()[0].clone();
    assert_eq!(request.inputs[0], ("term".to_owned(), LiteralValue::Number(20.0)));
    assert_eq!(request.stack, [PARENT, CHILD]);
    assert!(!request.context.flags.prefetch);
    assert_eq!(request.output, "Premium");
    // Flight event at offset 1, sub-request events after it, all `prefetch`.
    assert_eq!(invocations.len(), 4);
    let flight = &invocations[1];
    assert_eq!((flight.index, flight.status, flight.prefetch), (1, CallStatus::Completed, true));
    assert_eq!(flight.child.as_deref(), Some(CHILD));
    assert_eq!(flight.matrix, Some(vec![vec![LiteralValue::Number(40.0)]]));
    assert_eq!(flight.stack, [PARENT]);
    assert_eq!((invocations[2].index, invocations[3].index, invocations[3].memo_of), (2, 3, Some(2)));
    assert!(invocations[2].prefetch && invocations[3].prefetch);
    // Adopted under the sibling's own key, source = the flight event.
    assert_eq!(memo.entries.get(&sibling_key(&call, 20.0)).map(|entry| entry.0), Some(1));
    let report = prefetcher.report().unwrap();
    assert_eq!((report.dispatched, report.stored, report.not_stored, report.hits, report.on_slot), (1, 1, 0, 0, None));
    let keys: Vec<&str> = timings.0.keys().collect();
    assert_eq!(&keys[8..], ["prefetch_count", "prefetch_seconds", "prefetch_wait_seconds", "prefetch_hits"]);
    assert_eq!(timings.get("prefetch_count"), Some(TimingValue::Count(1)));
    // A later hit on that key is counted; a hit on another key is not.
    let mut timings = timings;
    prefetcher.note_hit(&sibling_key(&call, 20.0), &mut timings);
    prefetcher.note_hit(&sibling_key(&call, 99.0), &mut timings);
    assert_eq!(timings.get("prefetch_hits"), Some(TimingValue::Count(1)));
    assert_eq!(prefetcher.report().unwrap().hits, 1);
}

#[test]
fn several_flights_store_clean_results_only_and_never_repeat_a_key() {
    let evaluator = Arc::new(FakeEvaluator::default());
    let mut prefetcher = Prefetcher::new(plan(), package(), context(8, false), evaluator.clone());
    let mut memo = FakeMemo::default();
    let call = observed(10.0);
    let (dispatched, invocations, _) = run(&mut prefetcher, &mut memo, &call);
    assert!(dispatched);
    assert_eq!(evaluator.calls.load(Ordering::SeqCst), 3);
    // 20 stored; 30 (#DIV/0!) and 40 (failed) not stored, their events not merged.
    let report = prefetcher.report().unwrap();
    assert_eq!((report.dispatched, report.stored, report.not_stored), (3, 1, 2));
    assert_eq!(invocations.len(), 4);
    assert!(memo.contains(&sibling_key(&call, 20.0)));
    assert!(!memo.contains(&sibling_key(&call, 30.0)));
    // The next miss of the same group dispatches nothing already attempted.
    let (dispatched, _, _) = run(&mut prefetcher, &mut memo, &observed(20.0));
    assert!(!dispatched);
    assert_eq!(evaluator.calls.load(Ordering::SeqCst), 3);
}

#[test]
fn error_results_are_admitted_only_with_prefetch_errors() {
    let evaluator = Arc::new(FakeEvaluator::default());
    let mut prefetcher = Prefetcher::new(plan(), package(), context(8, true), evaluator);
    let mut memo = FakeMemo::default();
    let call = observed(10.0);
    run(&mut prefetcher, &mut memo, &call);
    let report = prefetcher.report().unwrap();
    assert_eq!((report.stored, report.not_stored), (2, 1));
    assert!(memo.contains(&sibling_key(&call, 30.0)));
}

#[test]
fn no_dispatch_below_the_parent_or_without_a_plan_match() {
    let evaluator = Arc::new(FakeEvaluator::default());
    let mut prefetcher = Prefetcher::new(plan(), package(), context(8, false), evaluator.clone());
    let memo = FakeMemo::default();
    let mut timings = Timings::base();
    let mut nested = observed(10.0);
    nested.stack.push(CHILD.to_owned());
    assert!(!prefetcher.dispatch_on(&memo, nested.view(), &mut timings));
    let mut other = observed(10.0);
    other.output = LiteralValue::Text("Other".into());
    assert!(!prefetcher.dispatch_on(&memo, other.view(), &mut timings));
    let mut unmatched = observed(10.0);
    unmatched.inputs[0].1 = LiteralValue::Number(11.0);
    assert!(!prefetcher.dispatch_on(&memo, unmatched.view(), &mut timings));
    assert_eq!(evaluator.calls.load(Ordering::SeqCst), 0);
    assert!(prefetcher.report().is_none());
    assert_eq!(timings.0.len(), 8);
}

#[test]
fn abandon_cancels_and_joins_every_flight_storing_nothing() {
    let evaluator = Arc::new(FakeEvaluator { wait_for_cancel: true, ..FakeEvaluator::default() });
    let mut prefetcher = Prefetcher::new(plan(), package(), context(8, false), evaluator.clone());
    let memo = FakeMemo::default();
    let mut timings = Timings::base();
    assert!(prefetcher.dispatch_on(&memo, observed(10.0).view(), &mut timings));
    prefetcher.abandon(&mut timings);
    let report = prefetcher.report().unwrap();
    assert_eq!((report.dispatched, report.stored, report.not_stored), (3, 0, 3));
    assert!(memo.entries.is_empty());
}

#[test]
fn batch_variant_runs_all_scenarios_on_one_thread_with_the_same_receipt() {
    struct Counting {
        inner: SequentialBatch,
        batches: AtomicUsize,
    }
    impl ChildBatchEvaluator for Counting {
        fn evaluate_children(&self, requests: &[ChildRequest]) -> Vec<ChildOutcome> {
            self.batches.fetch_add(1, Ordering::SeqCst);
            assert!(requests.windows(2).all(|pair| pair[0].child_version == pair[1].child_version));
            self.inner.evaluate_children(requests)
        }
    }
    let threaded_evaluator = Arc::new(FakeEvaluator::default());
    let mut threaded = Prefetcher::new(plan(), package(), context(8, false), threaded_evaluator);
    let mut threaded_memo = FakeMemo::default();
    let (_, threaded_events, _) = run(&mut threaded, &mut threaded_memo, &observed(10.0));

    let evaluator = Arc::new(FakeEvaluator::default());
    let batch = Arc::new(Counting { inner: SequentialBatch(evaluator.clone()), batches: AtomicUsize::new(0) });
    let mut batched = Prefetcher::new(plan(), package(), context(8, false), evaluator.clone()).with_batch_evaluator(batch.clone());
    let mut memo = FakeMemo::default();
    let (dispatched, events, _) = run(&mut batched, &mut memo, &observed(10.0));
    assert!(dispatched);
    assert_eq!(batch.batches.load(Ordering::SeqCst), 1);
    assert_eq!(evaluator.calls.load(Ordering::SeqCst), 3);
    assert_eq!(events, threaded_events);
    assert_eq!(batched.report(), threaded.report());
    assert_eq!(memo.entries.len(), threaded_memo.entries.len());
}
