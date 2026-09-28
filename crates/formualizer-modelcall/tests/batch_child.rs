//! Lane P: the one-load batch evaluator against the per-flight path.
//!
//! A synthetic parent/child/grandchild package over in-memory workbooks. The
//! child calls the grandchild (so every scenario carries nested events and a
//! memo hit), and a second child runs a goal seek. For sibling scenarios the
//! batch evaluator must return, per scenario, the outcome the per-flight
//! `SubRequestEvaluator` returns (result, events, faults, goal-seek records,
//! diagnostics, timing keys) while loading the child once instead of once
//! per scenario. Load counts are printed (`--nocapture`) as the measurement.

use chrono::DateTime;
use formualizer_common::{LiteralValue, RangeAddress};
use formualizer_modelcall::batch_child::BatchChildEvaluator;
use formualizer_modelcall::evaluator::{CancelToken, ChildOutcome, ChildRequest};
use formualizer_modelcall::memo::ModelCallMemo;
use formualizer_modelcall::prefetch::{
    ChildBatchEvaluator, ObservedCall, PlanValue, Position, PositionKind, Prefetcher, SiblingGroup, SiblingPlan,
};
use formualizer_modelcall::receipt::Timings;
use formualizer_modelcall::session::{SubRequestEvaluator, WorkbookSource, runtime_workbook_config};
use formualizer_modelcall::{
    CalculationContext, CalculationFlags, ChildEvaluator, ModelCallError, ModelCallEvent, ModelPackage, ModelSpec,
    Operation,
};
use formualizer_workbook::{NamedRangeScope, Workbook};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const SHEET: &str = "Calc";
const PARENT: &str = "parent:sha-parent";

/// One in-memory model: literal/formula cells and an output rectangle.
#[derive(Clone)]
struct Model {
    identity: &'static str,
    cells: Vec<(u32, u32, &'static str)>,
    output: (u32, u32),
    /// `(name, start_row, start_col, end_row, end_col)` workbook-scope names.
    names: Vec<(&'static str, u32, u32, u32, u32)>,
}

fn a1(row: u32, col: u32) -> String {
    format!("'{SHEET}'!{}{row}", (b'A' + u8::try_from(col - 1).unwrap()) as char)
}

fn location(row: u32, col: u32, name: &str, key: &str, port_id: &str) -> Value {
    json!({"sheet": SHEET, "start_row": row, "start_col": col, "end_row": row, "end_col": col,
           "name": name, "key": key, "port_id": port_id, "shape": "scalar", "date_system": 1900, "date_fields": []})
}

fn spec_json(model: &Model) -> Value {
    let (row, col) = model.output;
    json!({
        "identity": model.identity,
        "workbook_path": format!("memory://{}", model.identity),
        "workbook_sha256": format!("sha-{}", model.identity),
        "manifest": {
            "spec": "fio", "spec_version": "0.3.0",
            "manifest": {"id": format!("model-{}", model.identity), "name": model.identity,
                         "workbook": {"uri": "memory://model.xlsx", "locale": "en-US", "date_system": 1900}},
            "ports": [
                {"id": "amount", "dir": "in", "shape": "scalar", "location": {"a1": a1(1, 1)},
                 "schema": {"type": "any"}, "constraints": {"nullable": true}},
                {"id": "output_result", "dir": "out", "shape": "scalar", "location": {"a1": a1(row, col)},
                 "schema": {"type": "any"}, "constraints": {"nullable": true}}
            ]
        },
        "inputs": {"amount": location(1, 1, "XINPUT_amount", "amount", "amount")},
        "outputs": {"result": location(row, col, "XOUTPUT_result", "result", "output_result")},
        "defaults": {"amount": 1},
        "goal_seek": [],
        "descriptor": {}
    })
}

/// Builds each model's workbook and counts loads per identity.
struct MemorySource {
    models: HashMap<String, Model>,
    loads: Mutex<HashMap<String, usize>>,
}

impl MemorySource {
    fn new(models: &[&Model]) -> Arc<Self> {
        Arc::new(Self {
            models: models.iter().map(|model| (model.identity.to_owned(), (*model).clone())).collect(),
            loads: Mutex::new(HashMap::new()),
        })
    }

    fn loads(&self, identity: &str) -> usize {
        self.loads.lock().unwrap().get(identity).copied().unwrap_or(0)
    }
}

impl WorkbookSource for MemorySource {
    fn load(&self, spec: &ModelSpec, context: &CalculationContext) -> Result<Workbook, ModelCallError> {
        *self.loads.lock().unwrap().entry(spec.identity.clone()).or_default() += 1;
        let model = self
            .models
            .get(&spec.identity)
            .ok_or_else(|| ModelCallError::infrastructure("FileNotFoundError", spec.identity.clone()))?;
        let mut workbook = Workbook::new_with_config(runtime_workbook_config(context.random_seed));
        workbook.add_sheet(SHEET).unwrap();
        workbook.set_value(SHEET, 1, 1, LiteralValue::Number(1.0)).unwrap();
        for (row, col, cell) in &model.cells {
            if cell.starts_with('=') {
                workbook.set_formula(SHEET, *row, *col, cell).unwrap();
            } else if let Ok(number) = cell.parse::<f64>() {
                workbook.set_value(SHEET, *row, *col, LiteralValue::Number(number)).unwrap();
            } else if *cell == "TRUE" {
                workbook.set_value(SHEET, *row, *col, LiteralValue::Boolean(true)).unwrap();
            } else {
                workbook.set_value(SHEET, *row, *col, LiteralValue::Text((*cell).to_owned())).unwrap();
            }
        }
        for (name, start_row, start_col, end_row, end_col) in &model.names {
            let address = RangeAddress::new(SHEET, *start_row, *start_col, *end_row, *end_col).unwrap();
            workbook.define_named_range(name, &address, NamedRangeScope::Workbook).unwrap();
        }
        Ok(workbook)
    }
}

/// Child: B1 = 3 * amount, B2 = grandchild(amount) twice (the repeat is a
/// memo hit), B3 = B1 + B2.
fn child() -> Model {
    Model {
        identity: "child",
        cells: vec![
            (1, 2, "=A1*3"),
            (
                2,
                2,
                "=MDL.CALLMODEL(\"rates/grand\",0,\"result\",\"amount\",A1)+MDL.CALLMODEL(\"rates/grand\",0,\"result\",\"amount\",A1)",
            ),
            (3, 2, "=B1+B2"),
        ],
        output: (3, 2),
        names: Vec::new(),
    }
}

/// Grandchild: B1 = amount + 100.
fn grand() -> Model {
    Model { identity: "grand", cells: vec![(1, 2, "=A1+100")], output: (1, 2), names: Vec::new() }
}

/// Goal-seek child: B2 is the changing cell, B3 = B2^3 - 2*B2 - amount, the
/// block `Xsolve_Root` at H1:I12; the output B4 = B2 * 10 reads the root.
fn solver() -> Model {
    Model {
        identity: "solver",
        cells: vec![
            (2, 2, "1"),
            (3, 2, "=B2*B2*B2-2*B2-A1"),
            (4, 2, "=B2*10"),
            (1, 8, "Run if"),
            (1, 9, "TRUE"),
            (2, 8, "Target cell"),
            (2, 9, "='Calc'!$B$3"),
            (3, 8, "Target value"),
            (3, 9, "0"),
            (4, 8, "By changing"),
            (4, 9, "= 'Calc'!$B$2 "),
            (5, 8, "Solve algorithm"),
            (5, 9, " Brent "),
            (6, 8, "Max change"),
            (6, 9, "1e-12"),
            (7, 8, "Max iterations"),
            (7, 9, "100"),
            (8, 8, "Lower bound"),
            (8, 9, "0"),
            (9, 8, "Upper bound"),
            (9, 9, "300"),
            (10, 8, "Solve result"),
            (11, 8, "Solve  Iteration"),
            (12, 8, "Solve target"),
        ],
        output: (4, 2),
        names: vec![("Xsolve_Root", 1, 8, 12, 9)],
    }
}

fn package(models: &[(&str, &Model)]) -> Arc<ModelPackage> {
    let parent = Model { identity: "parent", cells: Vec::new(), output: (2, 1), names: Vec::new() };
    let mut children = serde_json::Map::new();
    let mut routes = serde_json::Map::new();
    for (route, model) in models {
        children.insert(model.identity.to_owned(), spec_json(model));
        routes.insert((*route).to_owned(), Value::String(model.identity.to_owned()));
    }
    Arc::new(
        serde_json::from_value(json!({
            "package_id": "synthetic", "parent": spec_json(&parent),
            "children": children, "child_routes": routes,
        }))
        .unwrap(),
    )
}

fn context(prefetch_max: u32) -> CalculationContext {
    let flags = CalculationFlags { prefetch: true, prefetch_max, ..CalculationFlags::default() };
    let now = DateTime::parse_from_rfc3339("2026-09-28T00:00:00+00:00").unwrap();
    CalculationContext::new(now, Operation::Client, 147, None, 32, flags).unwrap()
}

fn request(package: &Arc<ModelPackage>, version: &str, inputs: Vec<(String, LiteralValue)>) -> ChildRequest {
    let mut context = context(1);
    context.flags.prefetch = false;
    ChildRequest {
        package: package.clone(),
        child_version: version.to_owned(),
        inputs,
        output: "result".into(),
        stack: vec![PARENT.to_owned(), format!("{version}:sha-{version}")],
        context,
        cancel: CancelToken::new(),
    }
}

fn amount(value: LiteralValue) -> Vec<(String, LiteralValue)> {
    vec![("amount".to_owned(), value)]
}

/// The per-flight path as the prefetcher runs it: one thread per flight.
fn per_flight(evaluator: SubRequestEvaluator, requests: &[ChildRequest]) -> Vec<ChildOutcome> {
    std::thread::scope(|scope| {
        let handles: Vec<_> =
            requests.iter().map(|request| scope.spawn(|| evaluator.evaluate_child(request))).collect();
        handles.into_iter().map(|handle| handle.join().unwrap()).collect()
    })
}

fn timing_keys(timings: &Timings) -> Vec<String> {
    let mut keys: Vec<String> = timings.0.keys().map(str::to_owned).collect();
    keys.sort();
    keys
}

fn assert_same_outcomes(label: &str, expected: &[ChildOutcome], actual: &[ChildOutcome]) {
    assert_eq!(expected.len(), actual.len(), "{label}: outcome count");
    for (index, (expected, actual)) in expected.iter().zip(actual).enumerate() {
        assert_eq!(expected.result, actual.result, "{label}[{index}] result");
        assert_eq!(expected.invocations, actual.invocations, "{label}[{index}] invocations");
        assert_eq!(expected.faults, actual.faults, "{label}[{index}] faults");
        assert_eq!(expected.solvers, actual.solvers, "{label}[{index}] solvers");
        assert_eq!(expected.diagnostics, actual.diagnostics, "{label}[{index}] diagnostics");
        assert_eq!(timing_keys(&expected.timings), timing_keys(&actual.timings), "{label}[{index}] timing keys");
    }
}

fn number(outcome: &ChildOutcome) -> f64 {
    match outcome.result.as_ref().expect("result")[0][0] {
        LiteralValue::Number(value) => value,
        LiteralValue::Int(value) => value as f64,
        ref other => panic!("not a number: {other:?}"),
    }
}

#[test]
fn five_siblings_match_the_per_flight_path_on_one_load() {
    let (child, grand) = (child(), grand());
    let package = package(&[("rates/child", &child), ("rates/grand", &grand)]);
    let requests: Vec<ChildRequest> =
        (1..=5).map(|value| request(&package, "child", amount(LiteralValue::Number(f64::from(value))))).collect();

    let flights = MemorySource::new(&[&child, &grand]);
    let expected = per_flight(SubRequestEvaluator::new(flights.clone(), None), &requests);
    let batched = MemorySource::new(&[&child, &grand]);
    let actual = BatchChildEvaluator::new(batched.clone(), None).evaluate_children(&requests);

    assert_same_outcomes("siblings", &expected, &actual);
    for (value, outcome) in (1..=5).zip(&actual) {
        let value = f64::from(value);
        assert_eq!(number(outcome), value * 3.0 + 2.0 * (value + 100.0));
        assert_eq!(outcome.invocations.len(), 2, "grandchild call and its memo hit");
    }
    println!(
        "LOADS five_siblings child per_flight={} batch={} grandchild per_flight={} batch={}",
        flights.loads("child"),
        batched.loads("child"),
        flights.loads("grand"),
        batched.loads("grand")
    );
    assert_eq!(flights.loads("child"), 5);
    assert_eq!(batched.loads("child"), 1);
    assert_eq!(flights.loads("grand"), batched.loads("grand"));
}

#[test]
fn failed_scenarios_match_and_drop_the_loaded_child() {
    let (child, grand) = (child(), grand());
    let package = package(&[("rates/child", &child), ("rates/grand", &grand)]);
    let requests = vec![
        request(&package, "child", amount(LiteralValue::Number(1.0))),
        // Undeclared input: an admission (routing) failure; the child is dropped.
        request(&package, "child", vec![("bogus".to_owned(), LiteralValue::Number(2.0))]),
        request(&package, "child", amount(LiteralValue::Number(3.0))),
        // Text input: evaluates to error values (a completed matrix).
        request(&package, "child", amount(LiteralValue::Text("x".into()))),
        request(&package, "child", amount(LiteralValue::Number(5.0))),
        // Undeclared output selector: refused before any load.
        ChildRequest { output: "missing".into(), ..request(&package, "child", amount(LiteralValue::Number(6.0))) },
    ];
    let flights = MemorySource::new(&[&child, &grand]);
    let expected = per_flight(SubRequestEvaluator::new(flights.clone(), None), &requests);
    let batched = MemorySource::new(&[&child, &grand]);
    let actual = BatchChildEvaluator::new(batched.clone(), None).evaluate_children(&requests);

    assert_same_outcomes("mixed", &expected, &actual);
    assert!(matches!(actual[1].result, Err(ModelCallError::Routing(_))));
    assert!(matches!(actual[5].result, Err(ModelCallError::Routing(_))));
    assert!(matches!(actual[3].result.as_ref().unwrap()[0][0], LiteralValue::Error(_)));
    println!(
        "LOADS mixed child per_flight={} batch={}",
        flights.loads("child"),
        batched.loads("child")
    );
    assert_eq!(flights.loads("child"), 5);
    assert_eq!(batched.loads("child"), 2, "one load, one reload after the failed admission");
}

#[test]
fn goal_seek_scenarios_on_one_load_match_fresh_loads() {
    let solver = solver();
    let package = package(&[("rates/solver", &solver)]);
    let requests: Vec<ChildRequest> = [5.0, 30.0, 100.0, 5.0]
        .into_iter()
        .map(|value| request(&package, "solver", amount(LiteralValue::Number(value))))
        .collect();
    // Reference: every scenario on a freshly loaded child (a batch of one).
    let fresh = MemorySource::new(&[&solver]);
    let single = BatchChildEvaluator::new(fresh.clone(), None);
    let expected: Vec<ChildOutcome> =
        requests.iter().flat_map(|request| single.evaluate_children(std::slice::from_ref(request))).collect();
    let batched = MemorySource::new(&[&solver]);
    let actual = BatchChildEvaluator::new(batched.clone(), None).evaluate_children(&requests);

    assert_same_outcomes("goal seek", &expected, &actual);
    for outcome in &actual {
        assert_eq!(outcome.solvers.len(), 1, "one converged block per scenario: {:?}", outcome.result);
    }
    assert_eq!(number(&actual[0]).to_bits(), number(&actual[3]).to_bits(), "repeat after other roots is bit-identical");
    println!("LOADS goal_seek child fresh={} batch={}", fresh.loads("solver"), batched.loads("solver"));
    assert_eq!(fresh.loads("solver"), 4);
    assert_eq!(batched.loads("solver"), 1);
}

fn plan() -> SiblingPlan {
    let constant = |value: i64| PlanValue::Constant(LiteralValue::Int(value));
    SiblingPlan {
        groups: vec![SiblingGroup {
            target: "rates/child".into(),
            output: "result".into(),
            cells: (1..=4).map(|row| format!("Calc!A{row}")).collect(),
            positions: vec![Position {
                name: "amount".into(),
                kind: PositionKind::Override,
                values: (1..=4).map(constant).collect(),
            }],
        }],
        call_cells: 4,
        unresolved_calls: 0,
        rejected_groups: 0,
    }
}

/// Dispatch and join one parent-level miss; returns the merged events, the
/// memo and the load count.
fn prefetch_round(batch: bool) -> (Vec<ModelCallEvent>, ModelCallMemo, usize) {
    let (child, grand) = (child(), grand());
    let package = package(&[("rates/child", &child), ("rates/grand", &grand)]);
    let source = MemorySource::new(&[&child, &grand]);
    let evaluator: Arc<dyn ChildEvaluator> = Arc::new(SubRequestEvaluator::new(source.clone(), None));
    let mut prefetcher = Prefetcher::new(plan(), package.clone(), context(5), evaluator);
    if batch {
        prefetcher = prefetcher.with_batch_evaluator(Arc::new(BatchChildEvaluator::new(source.clone(), None)));
    }
    let mut memo = ModelCallMemo::new();
    let stack = vec![PARENT.to_owned()];
    let output = LiteralValue::Text("result".into());
    let inputs = amount(LiteralValue::Number(1.0));
    let spec = package.children.get("child").unwrap();
    let key = memo.key(&stack, "child:sha-child", &output, &inputs, Some(spec)).unwrap();
    let mut timings = Timings::base();
    let call = ObservedCall {
        stack: &stack,
        target: "rates/child",
        output: &output,
        identity: "child:sha-child",
        child_version: "child",
        child: spec,
        inputs: &inputs,
        observed_key: &key,
    };
    assert!(prefetcher.dispatch(&memo, call, &mut timings));
    let mut invocations = vec![ModelCallEvent::started(0, &stack, LiteralValue::Text("rates/child".into()), output)];
    prefetcher.join_into(&mut invocations, &mut memo, &mut timings);
    let report = prefetcher.report().unwrap();
    assert_eq!((report.dispatched, report.stored, report.not_stored), (3, 3, 0));
    (invocations, memo, source.loads("child"))
}

#[test]
fn prefetch_receipt_is_the_same_with_the_batch_evaluator() {
    let (threaded, threaded_memo, threaded_loads) = prefetch_round(false);
    let (batched, batched_memo, batched_loads) = prefetch_round(true);
    assert_eq!(threaded, batched, "merged invocations");
    assert_eq!(threaded.len(), 1 + 3 * 3, "observed event + 3 flights x (flight + 2 nested)");
    let stack = vec![PARENT.to_owned()];
    let output = LiteralValue::Text("result".into());
    for value in [2.0, 3.0, 4.0] {
        let key = threaded_memo
            .peek_key(&stack, "child:sha-child", &output, &amount(LiteralValue::Number(value)), None)
            .unwrap();
        assert!(threaded_memo.contains(&key) && batched_memo.contains(&key), "sibling {value} adopted");
    }
    println!("LOADS prefetch child threaded={threaded_loads} batch={batched_loads}");
    assert_eq!((threaded_loads, batched_loads), (3, 1));
}
