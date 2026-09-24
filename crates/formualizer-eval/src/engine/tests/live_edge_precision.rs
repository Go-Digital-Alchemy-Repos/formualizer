use crate::engine::{CycleConfig, CycleDetection, CyclePolicy, Engine, EvalConfig};
use crate::test_workbook::TestWorkbook;
use chrono::NaiveDate;
use formualizer_common::{ExcelErrorKind, LiteralValue};
use formualizer_parse::parser::parse;

fn runtime_engine() -> Engine<TestWorkbook> {
    let cycle = CycleConfig {
        detection: CycleDetection::Runtime,
        policy: CyclePolicy::Error,
    };
    Engine::new(
        TestWorkbook::new(),
        EvalConfig::default()
            .with_cycle(cycle)
            .with_virtual_dep_telemetry(true),
    )
}

fn set_formula(engine: &mut Engine<TestWorkbook>, row: u32, col: u32, formula: &str) {
    engine
        .set_cell_formula("Sheet1", row, col, parse(formula).expect("parse formula"))
        .expect("set formula");
}

fn is_circ(engine: &Engine<TestWorkbook>, row: u32, col: u32) -> bool {
    matches!(
        engine.get_cell_value("Sheet1", row, col),
        Some(LiteralValue::Error(error)) if error.kind == ExcelErrorKind::Circ
    )
}

fn build_guarded_chain(consumer_formula: &str) -> Engine<TestWorkbook> {
    let mut engine = runtime_engine();
    for row in 1..=100 {
        engine
            .set_cell_value("Sheet1", row, 2, LiteralValue::Int(i64::from(row)))
            .expect("set row number");
        set_formula(&mut engine, row, 5, &format!("=IF(B{row}>=29,$C$9,0)"));
        if row == 1 {
            set_formula(&mut engine, row, 17, "=E1");
        } else {
            set_formula(&mut engine, row, 17, &format!("=Q{}+E{row}", row - 1));
        }
    }
    set_formula(&mut engine, 9, 3, consumer_formula);
    engine
}

#[test]
fn index_rect_edge_precision_acyclic_chain() {
    let mut engine = build_guarded_chain("=INDEX(Q1:Q100,24)");
    engine.evaluate_all().expect("evaluate");

    let c9 = engine.get_cell_value("Sheet1", 9, 3);
    let c9_is_zero = matches!(c9, Some(LiteralValue::Number(0.0) | LiteralValue::Int(0)));
    let circ_count = (1..=100)
        .flat_map(|row| (1..=17).map(move |col| (row, col)))
        .filter(|&(row, col)| is_circ(&engine, row, col))
        .count();
    assert!(
        c9_is_zero && circ_count == 0,
        "C9 expected numeric zero, got {c9:?}; expected zero Circ cells, got {circ_count}"
    );
}

#[test]
fn index_rect_edge_precision_if_selected_base() {
    let mut engine = build_guarded_chain("=INDEX(IF(B1=1,Q1:Q100,A1:A100),24,1)");
    engine.evaluate_all().expect("evaluate");

    let c9 = engine.get_cell_value("Sheet1", 9, 3);
    let c9_is_zero = matches!(c9, Some(LiteralValue::Number(0.0) | LiteralValue::Int(0)));
    let circ_count = (1..=100)
        .flat_map(|row| (1..=17).map(move |col| (row, col)))
        .filter(|&(row, col)| is_circ(&engine, row, col))
        .count();
    assert!(
        c9_is_zero && circ_count == 0,
        "C9 through IF-selected reference expected numeric zero, got {c9:?}; expected zero Circ cells, got {circ_count}"
    );
}

#[test]
fn index_rect_edge_precision_omitted_column_single_vector() {
    let mut engine = build_guarded_chain("=INDEX(Q1:Q100,24,)");
    engine.evaluate_all().expect("evaluate");
    assert!(
        matches!(
            engine.get_cell_value("Sheet1", 9, 3),
            Some(LiteralValue::Number(0.0) | LiteralValue::Int(0))
        ),
        "vertical vector with omitted column must resolve one cell"
    );
    assert_eq!(
        (1..=100)
            .flat_map(|row| (1..=17).map(move |col| (row, col)))
            .filter(|&(row, col)| is_circ(&engine, row, col))
            .count(),
        0
    );
}

#[test]
fn index_rect_edge_precision_omitted_row_single_vector() {
    let mut engine = runtime_engine();
    engine
        .set_cell_value("Sheet1", 1, 17, LiteralValue::Int(0))
        .expect("set Q1");
    engine
        .set_cell_value("Sheet1", 1, 18, LiteralValue::Int(0))
        .expect("set R1");
    set_formula(&mut engine, 1, 19, "=$C$9");
    set_formula(&mut engine, 9, 3, "=INDEX(Q1:S1,,2)");
    engine.evaluate_all().expect("evaluate");
    assert!(
        matches!(
            engine.get_cell_value("Sheet1", 9, 3),
            Some(LiteralValue::Number(0.0) | LiteralValue::Int(0))
        ),
        "horizontal vector with omitted row must resolve one cell"
    );
    assert!(!is_circ(&engine, 1, 19));
}

#[test]
fn index_rect_edge_genuine_cycle_still_detected() {
    let mut engine = build_guarded_chain("=INDEX(Q1:Q100,40)");
    engine.evaluate_all().expect("evaluate");
    assert!(is_circ(&engine, 9, 3), "C9 must remain Circ");
}

#[test]
fn index_selected_error_propagates() {
    let mut engine = runtime_engine();
    set_formula(&mut engine, 1, 17, "=1/0");
    engine
        .set_cell_value("Sheet1", 3, 17, LiteralValue::Int(9))
        .expect("set Q3");
    set_formula(&mut engine, 9, 3, "=INDEX(Q1:Q3,1)");
    engine.evaluate_all().expect("evaluate");
    assert!(
        matches!(
            engine.get_cell_value("Sheet1", 9, 3),
            Some(LiteralValue::Error(error)) if error.kind == ExcelErrorKind::Div
        ),
        "INDEX must propagate the selected cell's DIV/0 error"
    );
}

#[test]
fn index_unselected_error_is_ignored() {
    let mut engine = runtime_engine();
    engine
        .set_cell_value("Sheet1", 1, 17, LiteralValue::Int(7))
        .expect("set Q1");
    set_formula(&mut engine, 3, 17, "=NA()");
    set_formula(&mut engine, 9, 3, "=INDEX(Q1:Q3,1)+1");
    engine.evaluate_all().expect("evaluate");
    assert!(
        matches!(
            engine.get_cell_value("Sheet1", 9, 3),
            Some(LiteralValue::Number(8.0) | LiteralValue::Int(8))
        ),
        "an unselected error must not affect the INDEX result"
    );
}

/// Inverted by F7 (session-runtime-f7-diagnosis-2026-09-24). This test used
/// to assert the GOD-187ad parity gate: an errored selected cell kept eager
/// whole-rect edges and so stamped C9 Circ. That gate is the F7 defect: the
/// selected cell does not depend on Q29:Q100, so an error there must
/// propagate exactly as a non-error selection does in
/// `index_rect_edge_precision_acyclic_chain`.
#[test]
fn index_rect_edge_error_selection_records_selected_cell_only() {
    let mut engine = build_guarded_chain("=INDEX(Q1:Q100,24)");
    set_formula(&mut engine, 24, 17, "=1/0");
    engine.evaluate_all().expect("evaluate");
    assert!(
        matches!(
            engine.get_cell_value("Sheet1", 9, 3),
            Some(LiteralValue::Error(error)) if error.kind == ExcelErrorKind::Div
        ),
        "errored selected cell must propagate DIV/0, got {:?}",
        engine.get_cell_value("Sheet1", 9, 3)
    );
    assert_eq!(
        (1..=100)
            .flat_map(|row| (1..=17).map(move |col| (row, col)))
            .filter(|&(row, col)| is_circ(&engine, row, col))
            .count(),
        0,
        "an errored selection must not close a false cycle"
    );
}

/// Test-only INDEX stand-in with a non-None format policy. The precise path
/// must route its result through `apply_format_propagation`; with the real
/// IndexFn the policy is `None`, which makes the application unobservable, so
/// this marker function is what makes deleting that call falsifiable.
#[derive(Debug)]
struct MarkedIndexFn;

impl crate::function::Function for MarkedIndexFn {
    fn name(&self) -> &'static str {
        "INDEX.MARKED"
    }

    fn min_args(&self) -> usize {
        2
    }

    fn arg_schema(&self) -> &'static [crate::args::ArgSchema] {
        &[]
    }

    fn propagate_format(
        &self,
        _result: &crate::traits::CalcValue<'_>,
    ) -> Option<crate::format::FormatId> {
        Some(MARKER_FORMAT)
    }

    fn eval<'a, 'b, 'c>(
        &self,
        _args: &'c [crate::traits::ArgumentHandle<'a, 'b>],
        _ctx: &dyn crate::traits::FunctionContext<'b>,
    ) -> Result<crate::traits::CalcValue<'b>, formualizer_common::ExcelError> {
        Ok(crate::traits::CalcValue::Scalar(LiteralValue::Error(
            formualizer_common::ExcelError::new(ExcelErrorKind::NImpl),
        )))
    }
}

const MARKER_FORMAT: crate::format::FormatId = crate::format::FormatId(30);

type PreciseDispatchResult = Option<(Option<crate::format::FormatId>, LiteralValue)>;

fn precise_dispatch_on(
    engine: &Engine<TestWorkbook>,
    function: &dyn crate::function::Function,
    formula: &str,
) -> PreciseDispatchResult {
    use formualizer_parse::parser::ASTNodeType;

    let interpreter = crate::interpreter::Interpreter::new(engine, "Sheet1");
    let ast = parse(formula).expect("valid INDEX formula");
    let ASTNodeType::Function { args, .. } = &ast.node_type else {
        panic!("expected a function call: {formula}");
    };
    let handles: Vec<crate::traits::ArgumentHandle<'_, '_>> = args
        .iter()
        .map(|arg| crate::traits::ArgumentHandle::new(arg, &interpreter))
        .collect();
    let ctx = interpreter.function_context(None);
    crate::builtins::reference_fns::IndexFn::precise_dispatch(function, &handles, &ctx)
        .map(|value| (value.format_id(), value.into_literal()))
}

#[test]
fn index_precise_path_applies_format_policy() {
    let mut engine = runtime_engine();
    engine
        .set_cell_value("Sheet1", 1, 17, LiteralValue::Int(7))
        .expect("set Q1");

    // Marker policy: the precise path must apply the dispatching function's
    // format policy to the materialized value.
    let (marked_format, _) = precise_dispatch_on(&engine, &MarkedIndexFn, "=INDEX(Q1:Q3,1)")
        .expect("precise path taken");
    assert_eq!(
        marked_format,
        Some(MARKER_FORMAT),
        "the precise path must route through apply_format_propagation"
    );

    // Control: INDEX itself declares no policy, so the same dispatch clears
    // any annotation.
    engine
        .set_cell_value(
            "Sheet1",
            2,
            17,
            LiteralValue::Date(NaiveDate::from_ymd_opt(2024, 12, 1).expect("valid date")),
        )
        .expect("set Q2");
    let (unmarked_format, _) = precise_dispatch_on(
        &engine,
        &crate::builtins::reference_fns::IndexFn,
        "=INDEX(Q1:Q3,2)",
    )
    .expect("precise path taken");
    assert_eq!(unmarked_format, None, "INDEX drops source annotations");
}

/// Path-taken matrix for `precise_single_cell_selection`: the filter is a
/// perf gate whose rejections are re-rejected downstream, so only direct
/// taken/not-taken assertions can catch an off-by-one in its bounds.
#[test]
fn index_precise_path_taken_matrix() {
    let mut engine = runtime_engine();
    for (row, col, value) in [
        (1, 1, 1),
        (2, 1, 2),
        (3, 1, 3),
        (1, 2, 10),
        (2, 2, 20),
        (3, 2, 30),
    ] {
        engine
            .set_cell_value("Sheet1", row, col, LiteralValue::Int(value))
            .expect("set fixture cell");
    }

    let cases: &[(&str, Option<f64>)] = &[
        ("=INDEX(A1:B3,2,1)", Some(2.0)),
        ("=INDEX(A1:B3,3,2)", Some(30.0)), // both upper bounds inclusive
        ("=INDEX(A1:B3,1,1)", Some(1.0)),
        ("=INDEX(A1:B3,4,1)", None),    // row just past the rect
        ("=INDEX(A1:B3,2,3)", None),    // column just past the rect
        ("=INDEX(A1:B3,0,1)", None),    // zero selects a whole row/column
        ("=INDEX(A1:B3,2)", None),      // 2-arg over a 2D rect is not single-cell
        ("=INDEX(A1:A3,3)", Some(3.0)), // single-column vector boundary
        ("=INDEX(A1:A3,4)", None),
        ("=INDEX(A1:B1,2)", Some(10.0)), // single-row vector boundary
        ("=INDEX(A1:B1,3)", None),
    ];

    for (formula, expected) in cases {
        let result =
            precise_dispatch_on(&engine, &crate::builtins::reference_fns::IndexFn, formula);
        match (result, expected) {
            (Some((_, literal)), Some(expected)) => {
                let number = match literal {
                    LiteralValue::Int(int) => int as f64,
                    LiteralValue::Number(number) => number,
                    other => panic!("{formula}: expected a number, got {other:?}"),
                };
                assert_eq!(number, *expected, "{formula}");
            }
            (None, None) => {}
            (taken, _) => panic!(
                "{formula}: precise path taken = {}, expected {}",
                taken.is_some(),
                expected.is_some()
            ),
        }
    }
}

#[test]
fn sum_over_rect_keeps_whole_rect_edges() {
    let mut engine = build_guarded_chain("=SUM(Q1:Q100)");
    engine.evaluate_all().expect("evaluate");
    assert!(is_circ(&engine, 9, 3), "C9 must remain Circ");
}

#[test]
fn index_unbounded_column_selection_measured() {
    let mut engine = build_guarded_chain("=INDEX(Q:Q,24)");
    engine.evaluate_all().expect("evaluate");
    assert!(
        is_circ(&engine, 9, 3),
        "unbounded INDEX retains whole-column live edges while resolving bounds"
    );
}

/// Direct bound assertions on `precise_single_cell_selection`. The dispatch
/// path re-rejects out-of-range selections in `reference_from_base`, so a
/// widened bound in this filter is invisible to the taken/not-taken matrix
/// above; only predicate-level assertions catch such an off-by-one.
#[test]
fn index_precise_selection_predicate_bounds() {
    use formualizer_parse::parser::ASTNodeType;

    let engine = runtime_engine();
    let checks: &[(&str, u32, u32, bool)] = &[
        ("=INDEX(A1:B3,2,1)", 3, 2, true),
        ("=INDEX(A1:B3,3,2)", 3, 2, true), // inclusive upper corner
        ("=INDEX(A1:B3,4,1)", 3, 2, false), // row one past the rect
        ("=INDEX(A1:B3,2,3)", 3, 2, false), // column one past the rect
        ("=INDEX(A1:B3,0,1)", 3, 2, false), // zero row selects a whole column
        ("=INDEX(A1:B3,1,0)", 3, 2, false), // zero column selects a whole row
        ("=INDEX(A1:B3,2)", 3, 2, false),  // 2-arg over a 2D rect
        ("=INDEX(A1:A3,3)", 3, 1, true),   // column-vector boundary
        ("=INDEX(A1:A3,4)", 3, 1, false),
        ("=INDEX(A1:B1,2)", 1, 2, true), // row-vector boundary
        ("=INDEX(A1:B1,3)", 1, 2, false),
    ];

    for (formula, rows, cols, expected) in checks {
        let interpreter = crate::interpreter::Interpreter::new(&engine, "Sheet1");
        let ast = parse(formula).expect("valid INDEX formula");
        let ASTNodeType::Function { args, .. } = &ast.node_type else {
            panic!("expected a function call: {formula}");
        };
        let handles: Vec<crate::traits::ArgumentHandle<'_, '_>> = args
            .iter()
            .map(|arg| crate::traits::ArgumentHandle::new(arg, &interpreter))
            .collect();
        assert_eq!(
            crate::builtins::reference_fns::IndexFn::precise_single_cell_selection(
                &handles, *rows, *cols
            ),
            *expected,
            "{formula} with dims {rows}x{cols}"
        );
    }
}

/* ───── F7 (session-runtime-f7-diagnosis-2026-09-24): errored INDEX selection ───── */

fn is_error_kind(engine: &Engine<TestWorkbook>, row: u32, col: u32, kind: ExcelErrorKind) -> bool {
    matches!(
        engine.get_cell_value("Sheet1", row, col),
        Some(LiteralValue::Error(error)) if error.kind == kind
    )
}

/// Static SCC {B1, B3, D1}: B1 statically reads D1 through an untaken IF arm,
/// B3 reads D1, D1 = INDEX over B1:B3. Live edges: D1 -> selected cell only,
/// B3 -> D1; acyclic.
fn build_f7_block(index_formula: &str) -> Engine<TestWorkbook> {
    let mut engine = runtime_engine();
    for row in 1..=3 {
        engine
            .set_cell_value("Sheet1", row, 1, LiteralValue::Int(i64::from(row)))
            .expect("set A column");
    }
    set_formula(&mut engine, 1, 2, "=IF($F$1,D1,NA())");
    engine
        .set_cell_value("Sheet1", 2, 2, LiteralValue::Int(0))
        .expect("set B2");
    set_formula(&mut engine, 3, 2, "=D1+1");
    engine
        .set_cell_value("Sheet1", 1, 6, LiteralValue::Boolean(false))
        .expect("set switch F1");
    set_formula(&mut engine, 1, 4, index_formula);
    engine
}

fn f7_snapshot(engine: &Engine<TestWorkbook>) -> Vec<Option<LiteralValue>> {
    [(1, 2), (2, 2), (3, 2), (1, 4)]
        .iter()
        .map(|&(row, col)| engine.get_cell_value("Sheet1", row, col))
        .collect()
}

fn f7_circ_count(engine: &Engine<TestWorkbook>) -> usize {
    [(1, 2), (2, 2), (3, 2), (1, 4)]
        .iter()
        .filter(|&&(row, col)| is_circ(engine, row, col))
        .count()
}

/// T1: the selected cell holds #N/A; INDEX must propagate it without Circ.
#[test]
fn f7_index_selected_error_no_false_cycle() {
    let mut engine = build_f7_block("=INDEX($B$1:$B$3,1)");
    engine.evaluate_all().expect("evaluate");
    assert_eq!(
        f7_circ_count(&engine),
        0,
        "values: {:?}",
        f7_snapshot(&engine)
    );
    assert!(
        is_error_kind(&engine, 1, 4, ExcelErrorKind::Na),
        "D1 must be #N/A"
    );
    assert!(
        is_error_kind(&engine, 3, 2, ExcelErrorKind::Na),
        "B3 must be #N/A"
    );
}

/// T1b: the row index is a MATCH miss; INDEX returns #N/A without Circ.
#[test]
fn f7_index_error_row_index_no_false_cycle() {
    let mut engine = build_f7_block("=INDEX($B$1:$B$3,MATCH(99,$A$1:$A$3,0))");
    engine.evaluate_all().expect("evaluate");
    assert_eq!(
        f7_circ_count(&engine),
        0,
        "values: {:?}",
        f7_snapshot(&engine)
    );
    assert!(
        is_error_kind(&engine, 1, 4, ExcelErrorKind::Na),
        "D1 must be #N/A"
    );
    assert!(
        is_error_kind(&engine, 3, 2, ExcelErrorKind::Na),
        "B3 must be #N/A"
    );
}

/// T2: persisted start state. A switch turns on a genuine live cycle
/// (B1 -> D1 -> B1), members are stamped Circ; the switch is turned off and
/// the block re-evaluated once. The persisted #CIRC! in the selected cell must
/// not make INDEX record the whole block; the result must equal a fresh
/// evaluation of the switched-off block.
#[test]
fn f7_index_persisted_circ_selection_clears_after_switch_off() {
    for index_formula in [
        "=INDEX($B$1:$B$3,1)",
        "=INDEX($B$1:$B$3,MATCH(1,$A$1:$A$3,0))",
    ] {
        let mut engine = build_f7_block(index_formula);
        engine
            .set_cell_value("Sheet1", 1, 6, LiteralValue::Boolean(true))
            .expect("switch on");
        engine.evaluate_all().expect("evaluate with live cycle");
        assert!(
            is_circ(&engine, 1, 4) && is_circ(&engine, 1, 2),
            "{index_formula}: switch-on must stamp the genuine cycle, got {:?}",
            f7_snapshot(&engine)
        );

        engine
            .set_cell_value("Sheet1", 1, 6, LiteralValue::Boolean(false))
            .expect("switch off");
        engine.evaluate_all().expect("evaluate after switch off");

        let mut fresh = build_f7_block(index_formula);
        fresh.evaluate_all().expect("fresh evaluate");

        assert_eq!(
            f7_circ_count(&engine),
            0,
            "{index_formula}: persisted state must not produce Circ, got {:?}",
            f7_snapshot(&engine)
        );
        assert_eq!(
            f7_snapshot(&engine),
            f7_snapshot(&fresh),
            "{index_formula}: persisted evaluation must equal a fresh evaluation"
        );
    }
}

/* ───── OT-285 qualification: precise path against the validated fallback ───── */

/// Upstream's oracle-shaped acyclic test (`index_selected_error_phantom_cycle_stays_acyclic`),
/// restored. Excel oracle: research/upstream/formualizer/excel-oracle-2026-08-22.md,
/// probe 8 row N (N1=`=1/0`, N2=`=INDEX(N1:N3,1)`, N3=`=N2` gives #DIV/0! in
/// N2 and N3 with no circular reference). An errored selected cell must not
/// make INDEX depend on the unselected member that reads it back.
#[test]
fn index_selected_error_phantom_cycle_stays_acyclic() {
    let mut engine = runtime_engine();
    set_formula(&mut engine, 1, 17, "=1/0"); // Q1
    set_formula(&mut engine, 3, 17, "=C9"); // Q3
    set_formula(&mut engine, 9, 3, "=INDEX(Q1:Q3,1)"); // C9
    engine.evaluate_all().expect("evaluate");

    for (row, col, label) in [(9, 3, "C9"), (3, 17, "Q3")] {
        assert!(
            matches!(
                engine.get_cell_value("Sheet1", row, col),
                Some(LiteralValue::Error(error)) if error.kind == ExcelErrorKind::Div
            ),
            "{label} must be #DIV/0! (Excel probe 8 row N), got {:?}",
            engine.get_cell_value("Sheet1", row, col)
        );
    }
    for (row, col) in [(1, 17), (2, 17), (3, 17), (9, 3)] {
        assert!(
            !is_circ(&engine, row, col),
            "no member may be Circ; ({row},{col}) is"
        );
    }
}

/// Test-only INDEX with INDEX's real schema and `eval`, and a non-None
/// format policy, so format propagation differences between the precise
/// path and the validated fallback (its default `dispatch`) are observable.
#[derive(Debug)]
struct MarkedSchemaIndexFn;

impl crate::function::Function for MarkedSchemaIndexFn {
    fn name(&self) -> &'static str {
        "INDEX.MARKED.SCHEMA"
    }

    fn min_args(&self) -> usize {
        crate::function::Function::min_args(&crate::builtins::reference_fns::IndexFn)
    }

    fn arg_schema(&self) -> &'static [crate::args::ArgSchema] {
        crate::function::Function::arg_schema(&crate::builtins::reference_fns::IndexFn)
    }

    fn propagate_format(
        &self,
        _result: &crate::traits::CalcValue<'_>,
    ) -> Option<crate::format::FormatId> {
        Some(MARKER_FORMAT)
    }

    fn eval<'a, 'b, 'c>(
        &self,
        args: &'c [crate::traits::ArgumentHandle<'a, 'b>],
        ctx: &dyn crate::traits::FunctionContext<'b>,
    ) -> Result<crate::traits::CalcValue<'b>, formualizer_common::ExcelError> {
        crate::function::Function::eval(&crate::builtins::reference_fns::IndexFn, args, ctx)
    }
}

#[derive(Clone, Copy)]
enum DispatchPath {
    /// `IndexFn::precise_dispatch` for INDEX itself.
    PreciseIndex,
    /// `IndexFn::validated_dispatch`: INDEX's fallback.
    FallbackIndex,
    /// `IndexFn::precise_dispatch` dispatched as `MarkedSchemaIndexFn`.
    PreciseMarked,
    /// `MarkedSchemaIndexFn`'s default `dispatch`: validation, `eval`,
    /// format policy.
    FallbackMarked,
}

fn dispatch_path_on(
    engine: &Engine<TestWorkbook>,
    path: DispatchPath,
    formula: &str,
) -> PreciseDispatchResult {
    use crate::function::Function;
    use formualizer_parse::parser::ASTNodeType;

    let interpreter = crate::interpreter::Interpreter::new(engine, "Sheet1");
    let ast = parse(formula).expect("valid INDEX formula");
    let ASTNodeType::Function { args, .. } = &ast.node_type else {
        panic!("expected a function call: {formula}");
    };
    let handles: Vec<crate::traits::ArgumentHandle<'_, '_>> = args
        .iter()
        .map(|arg| crate::traits::ArgumentHandle::new(arg, &interpreter))
        .collect();
    let ctx = interpreter.function_context(None);
    let index = crate::builtins::reference_fns::IndexFn;
    let value = match path {
        DispatchPath::PreciseIndex => {
            crate::builtins::reference_fns::IndexFn::precise_dispatch(&index, &handles, &ctx)
        }
        DispatchPath::PreciseMarked => crate::builtins::reference_fns::IndexFn::precise_dispatch(
            &MarkedSchemaIndexFn,
            &handles,
            &ctx,
        ),
        DispatchPath::FallbackIndex => Some(
            index
                .validated_dispatch(&handles, &ctx)
                .unwrap_or_else(|error| {
                    crate::traits::CalcValue::Scalar(LiteralValue::Error(error))
                }),
        ),
        DispatchPath::FallbackMarked => Some(
            MarkedSchemaIndexFn
                .dispatch(&handles, &ctx)
                .unwrap_or_else(|error| {
                    crate::traits::CalcValue::Scalar(LiteralValue::Error(error))
                }),
        ),
    };
    value.map(|value| (value.format_id(), value.into_literal()))
}

fn index_fixture_engine() -> Engine<TestWorkbook> {
    let mut engine = runtime_engine();
    for (row, col, value) in [(1, 1, 1), (3, 1, 3), (1, 2, 10), (3, 2, 30)] {
        engine
            .set_cell_value("Sheet1", row, col, LiteralValue::Int(value))
            .expect("set fixture cell");
    }
    set_formula(&mut engine, 2, 1, "=1/0"); // A2: errored member
    engine
        .set_cell_value(
            "Sheet1",
            2,
            2,
            LiteralValue::Date(NaiveDate::from_ymd_opt(2024, 12, 1).expect("valid date")),
        )
        .expect("set B2"); // B2: a member carrying a date number format
    engine.evaluate_all().expect("evaluate fixture");
    engine
}

/// Review finding 2 (and 5): the precise path must return exactly what the
/// validated fallback returns, value and format id, for index-error
/// precedence, errors in the column argument, 3-argument 2D selections, an
/// errored selected cell, and index arguments whose evaluation returns `Err`
/// (`LAMBDA(x,x)(1)`: immediate invocation is `Err(#N/IMPL)` in the AST
/// interpreter). Each row is run as INDEX (no format policy) and as a
/// format-policy INDEX with INDEX's schema, so validation-time errors
/// (unformatted) and `eval`-time results (formatted) are distinguishable.
#[test]
fn index_precise_path_matches_validated_fallback_table() {
    let engine = index_fixture_engine();
    // (formula, precise path taken, expected error kind or value check)
    let cases: &[(&str, bool, Option<ExcelErrorKind>)] = &[
        ("=INDEX(A1:B3,2,NA())", true, Some(ExcelErrorKind::Na)), // column-argument error
        ("=INDEX(A1:B3,NA(),1/0)", true, Some(ExcelErrorKind::Na)), // first index error wins
        ("=INDEX(A1:B3,2,1)", true, Some(ExcelErrorKind::Div)),   // errored selected cell
        (
            "=INDEX(A1:B3,\"x\",NA())",
            true,
            Some(ExcelErrorKind::Value),
        ),
        ("=INDEX(A1:B3,3,2)", true, None), // 3-argument 2D selection
        ("=INDEX(A1:B3,2,2)", true, None), // selected cell carries a date format
        ("=INDEX(A1:B3,0,1)", false, None), // whole column: declines
        ("=INDEX(A1:B3,2)", false, None),  // 2-arg over 2D: declines
        // Index arguments whose evaluation returns Err.
        (
            "=INDEX(A1:B3,LAMBDA(x,x)(1),1)",
            true,
            Some(ExcelErrorKind::NImpl),
        ),
        (
            "=INDEX(A1:B3,2,LAMBDA(x,x)(1))",
            true,
            Some(ExcelErrorKind::NImpl),
        ),
        // A later coercion error beats an earlier evaluation Err (validation
        // stores the Err and continues).
        (
            "=INDEX(A1:B3,LAMBDA(x,x)(1),NA())",
            true,
            Some(ExcelErrorKind::Na),
        ),
        (
            "=INDEX(A1:B3,LAMBDA(x,x)(1),\"x\")",
            true,
            Some(ExcelErrorKind::Value),
        ),
        (
            "=INDEX(A1:B3,\"x\",LAMBDA(x,x)(1))",
            true,
            Some(ExcelErrorKind::Value),
        ),
        (
            "=INDEX(A1:A3,LAMBDA(x,x)(1))",
            true,
            Some(ExcelErrorKind::NImpl),
        ),
    ];

    for (formula, taken, expected_error) in cases {
        for (precise_path, fallback_path, marked) in [
            (
                DispatchPath::PreciseIndex,
                DispatchPath::FallbackIndex,
                false,
            ),
            (
                DispatchPath::PreciseMarked,
                DispatchPath::FallbackMarked,
                true,
            ),
        ] {
            let precise = dispatch_path_on(&engine, precise_path, formula);
            let fallback = dispatch_path_on(&engine, fallback_path, formula)
                .expect("the fallback always returns");
            assert_eq!(
                precise.is_some(),
                *taken,
                "{formula} (marked={marked}): precise path taken"
            );
            if let Some(precise) = precise {
                assert_eq!(
                    precise, fallback,
                    "{formula} (marked={marked}): precise (format, value) must equal the fallback's"
                );
            }
            if let Some(kind) = expected_error {
                assert!(
                    matches!(&fallback.1, LiteralValue::Error(error) if error.kind == *kind),
                    "{formula} (marked={marked}): expected {kind:?}, got {:?}",
                    fallback.1
                );
            }
        }
    }

    // Non-vacuity of the format comparison: with the format policy, a
    // validation-time error is unformatted, while an errored selected cell
    // and an eval-time Err are formatted, in both paths.
    let marked = |formula| dispatch_path_on(&engine, DispatchPath::PreciseMarked, formula);
    assert_eq!(
        marked("=INDEX(A1:B3,2,NA())").map(|(format, _)| format),
        Some(None)
    );
    let selected_error = marked("=INDEX(A1:B3,2,1)").expect("precise path taken");
    assert_eq!(selected_error.0, Some(MARKER_FORMAT));
    assert!(
        matches!(&selected_error.1, LiteralValue::Error(e) if e.kind == ExcelErrorKind::Div),
        "got {:?}",
        selected_error.1
    );
    assert_eq!(
        marked("=INDEX(A1:B3,LAMBDA(x,x)(1),1)").map(|(format, _)| format),
        Some(Some(MARKER_FORMAT))
    );
    // The Err case really is an evaluation Err, not an error value.
    {
        use formualizer_parse::parser::ASTNodeType;
        let interpreter = crate::interpreter::Interpreter::new(&engine, "Sheet1");
        let ast = parse("=INDEX(A1:B3,LAMBDA(x,x)(1),1)").expect("parse");
        let ASTNodeType::Function { args, .. } = &ast.node_type else {
            panic!("expected a function call");
        };
        let handle = crate::traits::ArgumentHandle::new(&args[1], &interpreter);
        assert!(
            handle.value().is_err(),
            "LAMBDA(x,x)(1) must evaluate to Err for this table to cover finding 5"
        );
    }
}

/// Review finding 4: the precise path reads the index slots' coercion from
/// the dispatching function's schema. A schema with no coercion (the empty
/// schema of `MarkedIndexFn`) makes validation accept a text index, so the
/// precise path must not return the strict-coercion #VALUE! early; `eval`
/// still reads the index strictly, which the precise path reports as the
/// `eval`-time error with the function's format policy.
#[test]
fn index_precise_path_reads_coercion_from_schema() {
    let engine = index_fixture_engine();
    let strict = dispatch_path_on(&engine, DispatchPath::PreciseIndex, "=INDEX(A1:B3,\"x\",1)")
        .expect("precise path taken");
    assert_eq!(strict.0, None, "INDEX validation error is unformatted");
    assert!(matches!(&strict.1, LiteralValue::Error(e) if e.kind == ExcelErrorKind::Value));

    let (format, value) = precise_dispatch_on(&engine, &MarkedIndexFn, "=INDEX(A1:B3,\"x\",1)")
        .expect("precise path taken");
    assert!(matches!(&value, LiteralValue::Error(e) if e.kind == ExcelErrorKind::Value));
    assert_eq!(
        format,
        Some(MARKER_FORMAT),
        "with no schema coercion the #VALUE! comes from eval and is formatted"
    );
}
