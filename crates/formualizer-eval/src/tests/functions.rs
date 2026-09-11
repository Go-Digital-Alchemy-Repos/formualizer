use crate::builtins::math::{Atan2Fn, CosFn, SinFn, TanFn};
use crate::test_workbook::TestWorkbook;
use crate::traits::ArgumentHandle;
use formualizer_common::{ExcelErrorKind, LiteralValue};
use formualizer_parse::parser::{ASTNode, ASTNodeType, ReferenceType};

fn ensure_lifting_builtins() {
    static BUILTINS: std::sync::Once = std::sync::Once::new();
    BUILTINS.call_once(crate::builtins::load_builtins);
}

fn evaluate_lifting_formula(wb: &TestWorkbook, formula: &str) -> LiteralValue {
    use formualizer_parse::parser::Parser;

    ensure_lifting_builtins();
    let mut parser = Parser::new(formula).expect("formula parser");
    let ast = parser.parse().expect("valid formula");
    match wb.interpreter().evaluate_ast(&ast) {
        Ok(value) => value.into_literal(),
        Err(error) => LiteralValue::Error(error),
    }
}

#[test]
fn large_accepts_parenthesized_reference_union() {
    let wb = TestWorkbook::new().with_range(
        "Sheet1",
        1,
        1,
        vec![
            vec![LiteralValue::Int(9)],
            vec![LiteralValue::Int(5)],
            vec![LiteralValue::Int(7)],
        ],
    );

    assert_eq!(
        evaluate_lifting_formula(&wb, "=LARGE((A1,A2,A3),1)"),
        LiteralValue::Number(9.0)
    );
    assert_eq!(
        evaluate_lifting_formula(&wb, "=LARGE(A1:A3,1)"),
        LiteralValue::Number(9.0)
    );
}

#[test]
fn large_union_keeps_reference_cell_coercion_rules() {
    let wb = TestWorkbook::new().with_range(
        "Sheet1",
        1,
        1,
        vec![
            vec![LiteralValue::Boolean(true)],
            vec![LiteralValue::Int(2)],
            vec![LiteralValue::Text("9".into())],
        ],
    );

    assert_eq!(
        evaluate_lifting_formula(&wb, "=LARGE((A1,A2),2)"),
        LiteralValue::Error(formualizer_common::ExcelError::new(ExcelErrorKind::Num))
    );
    assert_eq!(
        evaluate_lifting_formula(&wb, "=LARGE((A2,A3),2)"),
        LiteralValue::Error(formualizer_common::ExcelError::new(ExcelErrorKind::Num))
    );
    assert!(matches!(
        evaluate_lifting_formula(&wb, "=LARGE((1,2),1)"),
        LiteralValue::Error(error) if error.kind == ExcelErrorKind::Value
    ));
    assert!(matches!(
        evaluate_lifting_formula(&wb, "=LARGE((A2,3),1)"),
        LiteralValue::Error(error) if error.kind == ExcelErrorKind::Value
    ));
}

fn concat_match_workbook() -> TestWorkbook {
    TestWorkbook::new().with_range(
        "Sheet1",
        1,
        1,
        vec![
            vec![
                LiteralValue::Text("B".into()),
                LiteralValue::Int(2),
                LiteralValue::Text("A".into()),
                LiteralValue::Text("B".into()),
                LiteralValue::Text("C".into()),
            ],
            vec![
                LiteralValue::Empty,
                LiteralValue::Empty,
                LiteralValue::Int(1),
                LiteralValue::Int(2),
                LiteralValue::Int(3),
            ],
        ],
    )
}

fn match_gate_workbook() -> TestWorkbook {
    TestWorkbook::new().with_range(
        "Sheet1",
        1,
        5,
        vec![
            vec![LiteralValue::Int(10), LiteralValue::Int(20)],
            vec![LiteralValue::Int(30), LiteralValue::Int(40)],
        ],
    )
}

fn assert_match_index(workbook: &TestWorkbook, formula: &str, expected: i64) {
    assert_eq!(
        evaluate_lifting_formula(workbook, formula),
        LiteralValue::Int(expected),
        "{formula}"
    );
}

fn assert_match_na(workbook: &TestWorkbook, formula: &str) {
    assert!(
        matches!(
            evaluate_lifting_formula(workbook, formula),
            LiteralValue::Error(error) if error.kind == ExcelErrorKind::Na
        ),
        "{formula}"
    );
}

#[test]
fn match_gate_lift_1xn_num_first() {
    assert_match_index(
        &TestWorkbook::new(),
        r#"=MATCH("2b",{1,2,3}&{"a","b","c"},0)"#,
        2,
    );
}

#[test]
fn match_gate_lift_1xn_str_first() {
    assert_match_index(
        &TestWorkbook::new(),
        r#"=MATCH("b2",{"a","b","c"}&{1,2,3},0)"#,
        2,
    );
}

#[test]
fn match_gate_lift_1xn_scalar_rhs() {
    assert_match_index(&TestWorkbook::new(), r#"=MATCH("2b",{1,2,3}&"b",0)"#, 2);
}

#[test]
fn match_gate_lift_1xn_scalar_lhs() {
    assert_match_index(&TestWorkbook::new(), r#"=MATCH("b2","b"&{1,2,3},0)"#, 2);
}

#[test]
fn match_gate_lift_nx1_num_first() {
    assert_match_index(
        &TestWorkbook::new(),
        r#"=MATCH("2b",{1;2;3}&{"a";"b";"c"},0)"#,
        2,
    );
}

#[test]
fn match_gate_lift_nx1_str_first() {
    assert_match_index(
        &TestWorkbook::new(),
        r#"=MATCH("b2",{"a";"b";"c"}&{1;2;3},0)"#,
        2,
    );
}

#[test]
fn match_gate_literal_1xn() {
    assert_match_index(&TestWorkbook::new(), "=MATCH(2,{1,2,3},0)", 2);
}

#[test]
fn match_gate_literal_nx1() {
    assert_match_index(&TestWorkbook::new(), "=MATCH(2,{1;2;3},0)", 2);
}

#[test]
fn match_gate_rejects_lifted_2d_matching() {
    assert_match_na(
        &TestWorkbook::new(),
        r#"=MATCH("2b",{1,2;3,4}&{"a","b";"c","d"},0)"#,
    );
}

#[test]
fn match_gate_rejects_lifted_2d_nonmatching() {
    assert_match_na(
        &TestWorkbook::new(),
        r#"=MATCH("zz",{1,2;3,4}&{"a","b";"c","d"},0)"#,
    );
}

#[test]
fn match_gate_rejects_literal_2d() {
    assert_match_na(&TestWorkbook::new(), "=MATCH(3,{1,2;3,4},0)");
}

#[test]
fn match_gate_rejects_reference_2d() {
    assert_match_na(&match_gate_workbook(), "=MATCH(30,E1:F2,0)");
}

#[test]
fn match_gate_reference_1row() {
    assert_match_index(&match_gate_workbook(), "=MATCH(20,E1:F1,0)", 2);
}

#[test]
fn match_gate_reference_1col() {
    assert_match_index(&match_gate_workbook(), "=MATCH(30,E1:E2,0)", 2);
}

#[test]
fn match_gate_rejects_literal_2x3() {
    assert_match_na(&TestWorkbook::new(), "=MATCH(3,{1,2,3;4,5,6},0)");
}

#[test]
fn match_gate_rejects_literal_3x2() {
    assert_match_na(&TestWorkbook::new(), "=MATCH(3,{1,2;3,4;5,6},0)");
}

#[test]
fn match_gate_rejects_lifted_2x3() {
    assert_match_na(
        &TestWorkbook::new(),
        r#"=MATCH("2b",{1,2,3;4,5,6}&{"a","b","c";"d","e","f"},0)"#,
    );
}

#[test]
fn match_gate_rejects_approx1_literal_2d() {
    assert_match_na(&TestWorkbook::new(), "=MATCH(3,{1,2;3,4},1)");
}

#[test]
fn match_gate_rejects_approxm1_literal_2d() {
    assert_match_na(&TestWorkbook::new(), "=MATCH(3,{4,3;2,1},-1)");
}

#[test]
fn match_gate_approx1_1d_exact() {
    assert_match_index(&TestWorkbook::new(), "=MATCH(3,{1,3,4},1)", 2);
}

#[test]
fn match_gate_approxm1_1d_exact() {
    assert_match_index(&TestWorkbook::new(), "=MATCH(3,{4,3,1},-1)", 2);
}

#[test]
fn match_gate_rejects_approx1_reference_2d() {
    assert_match_na(&match_gate_workbook(), "=MATCH(30,E1:F2,1)");
}

#[test]
fn match_gate_rejects_approxm1_reference_2d() {
    assert_match_na(&match_gate_workbook(), "=MATCH(30,E1:F2,-1)");
}

#[test]
fn range_concat_lifts_elementwise_direct_ast() {
    let wb = concat_match_workbook();
    assert_eq!(
        evaluate_lifting_formula(&wb, "=MATCH(A1&B1,C1:E1&C2:E2,0)"),
        LiteralValue::Int(2)
    );
    assert_eq!(
        evaluate_lifting_formula(&wb, "=MATCH(A1&B1,OFFSET(C1:E1,0,0)&C2:E2,0)"),
        LiteralValue::Int(2)
    );

    let ast = formualizer_parse::parser::parse("=OFFSET(C1:E1,0,0)")
        .expect("valid reference-returning operand");
    let interpreter = wb.interpreter();
    assert!(ArgumentHandle::new(&ast, &interpreter).may_return_reference());
}

#[test]
fn range_concat_lifts_elementwise_engine_arena() {
    ensure_lifting_builtins();
    let mut engine =
        crate::engine::Engine::new(TestWorkbook::new(), crate::engine::EvalConfig::default());
    for (row, col, value) in [
        (1, 3, LiteralValue::Text("B".into())),
        (1, 4, LiteralValue::Int(2)),
        (1, 5, LiteralValue::Text("A".into())),
        (1, 6, LiteralValue::Text("B".into())),
        (1, 7, LiteralValue::Text("C".into())),
        (2, 5, LiteralValue::Int(1)),
        (2, 6, LiteralValue::Int(2)),
        (2, 7, LiteralValue::Int(3)),
    ] {
        engine
            .set_cell_value("Sheet1", row, col, value)
            .expect("set arena concat fixture");
    }
    engine
        .set_cell_formula(
            "Sheet1",
            1,
            1,
            formualizer_parse::parser::parse("=MATCH($C$1&$D$1,OFFSET($E$1:$G$1,0,0)&$E$2:$G$2,0)")
                .expect("valid arena concat formula"),
        )
        .expect("set arena concat formula");
    engine
        .evaluate_all()
        .expect("evaluate arena concat formula");
    assert_eq!(
        engine
            .get_cell_value("Sheet1", 1, 1)
            .expect("concat result"),
        LiteralValue::Number(2.0)
    );
}

#[test]
fn range_concat_shape_discovery_does_not_evaluate_untaken_if_arm() {
    ensure_lifting_builtins();
    use crate::engine::{CycleConfig, CycleDetection, CyclePolicy, EvalConfig};
    let mut engine = crate::engine::Engine::new(
        TestWorkbook::new(),
        EvalConfig::default().with_cycle(CycleConfig {
            detection: CycleDetection::Runtime,
            policy: CyclePolicy::Error,
        }),
    );
    for row in 1..=3 {
        engine
            .set_cell_value("Sheet1", row, 1, LiteralValue::Int(1))
            .expect("set IF condition");
    }
    engine
        .set_cell_formula(
            "Sheet1",
            2,
            3,
            formualizer_parse::parser::parse("=$F$4").expect("valid back edge"),
        )
        .expect("set back edge");
    engine
        .set_cell_formula(
            "Sheet1",
            4,
            6,
            formualizer_parse::parser::parse("=SUM(IF(A1:A3=1,10,OFFSET(E1:E3,C2,0)))")
                .expect("valid IF formula"),
        )
        .expect("set IF formula");
    engine.evaluate_all().expect("evaluate lazy IF formula");
    assert_eq!(
        engine.get_cell_value("Sheet1", 4, 6).expect("IF sum"),
        LiteralValue::Number(30.0)
    );
}

#[test]
fn switch_array_lifts_and_preserves_lazy_arms_direct_ast() {
    let wb = TestWorkbook::new().with_range(
        "Sheet1",
        1,
        1,
        vec![
            vec![LiteralValue::Int(1)],
            vec![LiteralValue::Int(2)],
            vec![LiteralValue::Int(1)],
        ],
    );
    assert_eq!(
        evaluate_lifting_formula(&wb, "=SUM(SWITCH(TRUE,A1:A3=1,10,A1:A3=2,20,0))",),
        LiteralValue::Number(40.0)
    );
    assert_eq!(
        evaluate_lifting_formula(&wb, "=SWITCH(TRUE,A1:A3=1,10,A1:A3=2,20)"),
        LiteralValue::Array(vec![
            vec![LiteralValue::Number(10.0)],
            vec![LiteralValue::Number(20.0)],
            vec![LiteralValue::Number(10.0)],
        ])
    );
    assert_eq!(
        evaluate_lifting_formula(&wb, "=SUM(SWITCH(TRUE,A1:A3>0,10,1/0))"),
        LiteralValue::Number(30.0)
    );
    assert_eq!(
        evaluate_lifting_formula(&wb, "=SWITCH(TRUE,A1:A3=1,10)"),
        LiteralValue::Array(vec![
            vec![LiteralValue::Number(10.0)],
            vec![LiteralValue::Error(formualizer_common::ExcelError::new(
                ExcelErrorKind::Na
            ))],
            vec![LiteralValue::Number(10.0)],
        ])
    );

    let circular = LiteralValue::Error(formualizer_common::ExcelError::new(ExcelErrorKind::Circ));
    let wb_with_circular_reference = TestWorkbook::new()
        .with_range(
            "Sheet1",
            1,
            1,
            vec![
                vec![LiteralValue::Int(1)],
                vec![LiteralValue::Int(2)],
                vec![LiteralValue::Int(1)],
            ],
        )
        .with_range(
            "Sheet1",
            1,
            4,
            vec![
                vec![circular.clone()],
                vec![circular.clone()],
                vec![circular],
            ],
        );
    assert_eq!(
        evaluate_lifting_formula(
            &wb_with_circular_reference,
            "=SUM(SWITCH(TRUE,A1:A3>0,10,OFFSET(D1:D3,0,0)))",
        ),
        LiteralValue::Number(30.0)
    );

    let wb = wb
        .with_range(
            "Sheet1",
            1,
            2,
            vec![
                vec![LiteralValue::Int(10)],
                vec![LiteralValue::Int(20)],
                vec![LiteralValue::Int(30)],
            ],
        )
        .with_range(
            "Sheet1",
            1,
            3,
            vec![
                vec![LiteralValue::Int(100)],
                vec![LiteralValue::Int(200)],
                vec![LiteralValue::Int(300)],
            ],
        );
    assert_eq!(
        evaluate_lifting_formula(&wb, "=SWITCH(TRUE,A1:A3=1,B1:B3,A1:A3=2,C1:C3)"),
        LiteralValue::Array(vec![
            vec![LiteralValue::Number(10.0)],
            vec![LiteralValue::Number(200.0)],
            vec![LiteralValue::Number(30.0)],
        ])
    );
}

#[test]
fn switch_array_lift_engine_ignores_untaken_error_and_back_edge() {
    ensure_lifting_builtins();
    use crate::engine::{CycleConfig, CycleDetection, CyclePolicy, EvalConfig};
    let mut engine = crate::engine::Engine::new(
        TestWorkbook::new(),
        EvalConfig::default().with_cycle(CycleConfig {
            detection: CycleDetection::Runtime,
            policy: CyclePolicy::Error,
        }),
    );
    for row in 1..=3 {
        engine
            .set_cell_value("Sheet1", row, 1, LiteralValue::Int(1))
            .expect("set SWITCH condition value");
        engine
            .set_cell_value("Sheet1", row, 4, LiteralValue::Int(row as i64 * 10))
            .expect("set first SWITCH result range");
        engine
            .set_cell_value("Sheet1", row, 5, LiteralValue::Int(row as i64 * 100))
            .expect("set second SWITCH result range");
    }
    engine
        .set_cell_formula(
            "Sheet1",
            2,
            3,
            formualizer_parse::parser::parse("=$F$4").expect("valid back edge"),
        )
        .expect("set back edge");
    engine
        .set_cell_formula(
            "Sheet1",
            4,
            6,
            formualizer_parse::parser::parse(
                "=SUM(SWITCH(TRUE,$A$1:$A$3=1,10,$A$1:$A$3=2,1/0,$C$1:$C$3))",
            )
            .expect("valid arena SWITCH formula"),
        )
        .expect("set arena SWITCH formula");
    engine
        .set_cell_formula(
            "Sheet1",
            4,
            7,
            formualizer_parse::parser::parse(
                "=SWITCH(TRUE,$A$1:$A$3=1,$D$1:$D$3,$A$1:$A$3=2,$E$1:$E$3)",
            )
            .expect("valid arena SWITCH range-result formula"),
        )
        .expect("set arena SWITCH range-result formula");
    engine
        .evaluate_all()
        .expect("untaken SWITCH arms stay lazy");
    assert_eq!(
        engine.get_cell_value("Sheet1", 4, 6).expect("SWITCH sum"),
        LiteralValue::Number(30.0)
    );
    for (row, expected) in [(4, 10), (5, 20), (6, 30)] {
        assert_eq!(
            engine
                .get_cell_value("Sheet1", row, 7)
                .expect("SWITCH projected result"),
            LiteralValue::Number(expected as f64)
        );
    }
}

#[test]
fn ifs_array_lift_audit_records_distinct_value_resolution_path() {
    let wb = TestWorkbook::new().with_range(
        "Sheet1",
        1,
        1,
        vec![
            vec![LiteralValue::Int(1)],
            vec![LiteralValue::Int(2)],
            vec![LiteralValue::Int(1)],
        ],
    );
    assert!(matches!(
        evaluate_lifting_formula(&wb, "=SUM(IFS(A1:A3=1,10,A1:A3=2,20,TRUE,0))"),
        LiteralValue::Error(error) if error.kind == ExcelErrorKind::Value
    ));
}

fn array_lifting_engine() -> crate::engine::Engine<TestWorkbook> {
    ensure_lifting_builtins();
    let mut engine =
        crate::engine::Engine::new(TestWorkbook::new(), crate::engine::EvalConfig::default());
    for (row, a, b) in [(1, 1, 10), (2, 2, 20), (3, 3, 30)] {
        engine
            .set_cell_value("Sheet1", row, 1, LiteralValue::Int(a))
            .expect("set lookup key");
        engine
            .set_cell_value("Sheet1", row, 2, LiteralValue::Int(b))
            .expect("set lookup result");
    }
    for (col, value) in [(1, 3), (2, 1), (3, 2)] {
        engine
            .set_cell_value("Sheet1", 5, col, LiteralValue::Int(value))
            .expect("set lookup needle");
    }
    engine
}

fn evaluate_lifting_engine_formula(
    engine: &mut crate::engine::Engine<TestWorkbook>,
    formula: &str,
) {
    engine
        .set_cell_formula(
            "Sheet1",
            20,
            1,
            formualizer_parse::parser::parse(formula).expect("valid arena formula"),
        )
        .expect("set arena formula");
    engine.evaluate_all().expect("arena formula evaluation");
}

fn array_lifting_workbook() -> TestWorkbook {
    TestWorkbook::new()
        .with_range(
            "Sheet1",
            1,
            1,
            vec![
                vec![
                    LiteralValue::Int(1),
                    LiteralValue::Int(10),
                    LiteralValue::Text("ab".into()),
                    LiteralValue::Int(1),
                ],
                vec![
                    LiteralValue::Int(2),
                    LiteralValue::Int(20),
                    LiteralValue::Text("abab".into()),
                    LiteralValue::Int(4),
                ],
                vec![
                    LiteralValue::Int(3),
                    LiteralValue::Int(30),
                    LiteralValue::Text("ababab".into()),
                    LiteralValue::Int(9),
                ],
            ],
        )
        .with_range(
            "Sheet1",
            5,
            1,
            vec![
                vec![
                    LiteralValue::Int(3),
                    LiteralValue::Int(1),
                    LiteralValue::Int(2),
                    LiteralValue::Int(2),
                    LiteralValue::Int(2),
                    LiteralValue::Int(2),
                ],
                vec![
                    LiteralValue::Int(7),
                    LiteralValue::Int(8),
                    LiteralValue::Int(9),
                ],
            ],
        )
        .with_range(
            "Sheet1",
            10,
            1,
            vec![
                vec![
                    LiteralValue::Int(1),
                    LiteralValue::Int(2),
                    LiteralValue::Int(3),
                ],
                vec![
                    LiteralValue::Int(10),
                    LiteralValue::Int(20),
                    LiteralValue::Int(30),
                ],
            ],
        )
}

#[test]
fn array_lifting_lookup_family() {
    let wb = array_lifting_workbook();
    let number = |formula| evaluate_lifting_formula(&wb, formula);

    assert_eq!(
        number("=SUMPRODUCT(A6:C6,1*(XLOOKUP(A5:C5,A1:A3,B1:B3)<>0))"),
        LiteralValue::Number(24.0)
    );
    assert_eq!(
        number("=SUM(XLOOKUP(A5:C5,A1:A3,B1:B3))"),
        LiteralValue::Number(60.0)
    );
    assert_eq!(
        number("=XLOOKUP(A5:C5,A1:A3,B1:B3)"),
        LiteralValue::Array(vec![vec![
            LiteralValue::Number(30.0),
            LiteralValue::Number(10.0),
            LiteralValue::Number(20.0),
        ]])
    );
    assert_eq!(
        number("=XLOOKUP({1;2},A1:A3,B1:C3)"),
        LiteralValue::Array(vec![
            vec![LiteralValue::Number(10.0), LiteralValue::Text("ab".into())],
            vec![
                LiteralValue::Number(20.0),
                LiteralValue::Text("abab".into()),
            ],
        ])
    );
    assert_eq!(
        number("=_xlfn.XLOOKUP({1;2},A1:A3,B1:C3)"),
        LiteralValue::Array(vec![
            vec![LiteralValue::Number(10.0), LiteralValue::Text("ab".into())],
            vec![
                LiteralValue::Number(20.0),
                LiteralValue::Text("abab".into()),
            ],
        ])
    );
    assert_eq!(
        number("=_xlfn._xlws.XLOOKUP({1;2},A1:A3,B1:C3)"),
        LiteralValue::Array(vec![
            vec![LiteralValue::Number(10.0), LiteralValue::Text("ab".into())],
            vec![
                LiteralValue::Number(20.0),
                LiteralValue::Text("abab".into()),
            ],
        ])
    );
    assert_eq!(
        number("=SUM(XMATCH(A5:C5,A1:A3,0))"),
        LiteralValue::Number(6.0)
    );
    assert_eq!(
        number("=SUMPRODUCT(A6:C6,1*(INDEX(B1:B3,MATCH(A5:C5,A1:A3,0))<>0))"),
        LiteralValue::Number(24.0)
    );
    assert_eq!(
        number("=SUM(MATCH(A5:C5,A1:A3,0))"),
        LiteralValue::Number(6.0)
    );
    assert_eq!(
        number("=SUM(INDEX(B1:B3,A5:C5))"),
        LiteralValue::Number(60.0)
    );
    assert_eq!(
        number("=SUM(INDEX(A11:C11,1,A5:C5))"),
        LiteralValue::Number(60.0)
    );
    assert_eq!(
        number("=SUM(VLOOKUP(A5:C5,A1:B3,D5:F5,FALSE))"),
        LiteralValue::Number(60.0)
    );
    assert_eq!(
        number("=SUM(HLOOKUP(A5:C5,A10:C11,D5:F5,FALSE))"),
        LiteralValue::Number(60.0)
    );
    assert_eq!(
        number("=SUM(LOOKUP(A5:C5,A1:A3,B1:B3))"),
        LiteralValue::Number(60.0)
    );
    let mut engine = array_lifting_engine();
    evaluate_lifting_engine_formula(&mut engine, "=SUM(XLOOKUP(A5:C5,A1:A3,B1:B3))");
    assert_eq!(
        engine.get_cell_value("Sheet1", 20, 1),
        Some(LiteralValue::Number(60.0))
    );
}

#[test]
fn array_lifting_scalar_family() {
    let wb = array_lifting_workbook()
        .with_cell_a1("Sheet1", "G1", LiteralValue::Int(43831))
        .with_range(
            "Sheet1",
            1,
            8,
            vec![vec![
                LiteralValue::Int(0),
                LiteralValue::Int(1),
                LiteralValue::Int(2),
            ]],
        );
    let number = |formula| evaluate_lifting_formula(&wb, formula);

    assert_eq!(number("=SUM(ABS(A1:A3))"), LiteralValue::Number(6.0));
    assert_eq!(number("=SUM(ROUND(A1:A3,0))"), LiteralValue::Number(6.0));
    assert_eq!(number("=SUM(IF(A1:A3>1,1,0))"), LiteralValue::Number(2.0));
    assert_eq!(number("=SUM(LEN(C1:C3))"), LiteralValue::Number(12.0));
    assert_eq!(number("=SUM(ISNUMBER(A1:A3)*1)"), LiteralValue::Number(3.0));
    assert_eq!(number("=SUM(MOD(A1:A3,2))"), LiteralValue::Number(2.0));
    assert_eq!(number("=SUM(SQRT(D1:D3))"), LiteralValue::Number(6.0));
    // The `=SUM(EDATE(G1,H1:J1))` assertion that stood here moved to
    // `cl087_edate_months_slot_stops_lifting` under CL-087. It asserted a
    // NUMBER, it was authored in GOD-185 commit 727a92e5 -- the same commit
    // that created the name allowlist this round retires -- and no Excel
    // oracle ever backed it (ES-008 recorded the whole allowlist as
    // "unverified on an Excel oracle"). See that test for the citation.

    match number("=ROUND(A1:B2,A1:A3)") {
        LiteralValue::Error(error) => assert_eq!(error.kind, ExcelErrorKind::Value),
        other => panic!("expected incompatible broadcast error, got {other:?}"),
    }
}

#[test]
fn array_lifting_if_is_lazy() {
    use crate::function::Function;
    use crate::traits::FunctionContext;
    use formualizer_common::ExcelError;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[derive(Debug)]
    struct CircularReadFn(Arc<AtomicUsize>);

    impl Function for CircularReadFn {
        fn name(&self) -> &'static str {
            "CIRCULAR_READ"
        }

        fn eval<'x, 'b, 'c>(
            &self,
            _args: &'c [ArgumentHandle<'x, 'b>],
            _ctx: &dyn FunctionContext<'b>,
        ) -> Result<crate::traits::CalcValue<'b>, ExcelError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(crate::traits::CalcValue::Scalar(LiteralValue::Error(
                ExcelError::new(ExcelErrorKind::Circ),
            )))
        }
    }

    let circular_reads = Arc::new(AtomicUsize::new(0));
    let wb = array_lifting_workbook()
        .with_range(
            "Sheet1",
            1,
            11,
            vec![
                vec![LiteralValue::Int(10), LiteralValue::Int(100)],
                vec![LiteralValue::Int(20), LiteralValue::Int(200)],
                vec![LiteralValue::Int(30), LiteralValue::Int(300)],
            ],
        )
        .with_function(Arc::new(CircularReadFn(Arc::clone(&circular_reads))));
    assert_eq!(
        evaluate_lifting_formula(&wb, "=IF({TRUE,FALSE},{1,1}/{1,0},9)"),
        LiteralValue::Array(vec![vec![
            LiteralValue::Number(1.0),
            LiteralValue::Number(9.0),
        ]])
    );
    assert_eq!(
        evaluate_lifting_formula(&wb, "=IF({TRUE,TRUE},1,{1,1}/{0,0})"),
        LiteralValue::Array(vec![vec![
            LiteralValue::Number(1.0),
            LiteralValue::Number(1.0),
        ]])
    );
    assert_eq!(
        evaluate_lifting_formula(
            &wb,
            "=IF({TRUE,FALSE},{11,CIRCULAR_READ()},{CIRCULAR_READ(),22})",
        ),
        LiteralValue::Array(vec![vec![
            LiteralValue::Number(11.0),
            LiteralValue::Number(22.0),
        ]])
    );
    assert_eq!(circular_reads.load(Ordering::SeqCst), 0);
    assert_eq!(
        evaluate_lifting_formula(&wb, "=IF({TRUE;TRUE},XLOOKUP({1;2},A1:A3,K1:L3),0)",),
        LiteralValue::Array(vec![
            vec![LiteralValue::Number(10.0), LiteralValue::Number(100.0)],
            vec![LiteralValue::Number(20.0), LiteralValue::Number(200.0)],
        ])
    );
    assert_eq!(
        evaluate_lifting_formula(
            &wb,
            "=IF(TRUE,IF({TRUE;TRUE},XLOOKUP({1;2},A1:A3,K1:L3),0),0)",
        ),
        LiteralValue::Array(vec![
            vec![LiteralValue::Number(10.0), LiteralValue::Number(100.0)],
            vec![LiteralValue::Number(20.0), LiteralValue::Number(200.0)],
        ])
    );
    assert_eq!(
        evaluate_lifting_formula(
            &wb,
            "=IF({TRUE;TRUE},IF({TRUE;TRUE},XLOOKUP({1;2},A1:A3,K1:L3),0),0)",
        ),
        LiteralValue::Array(vec![
            vec![LiteralValue::Number(10.0), LiteralValue::Number(100.0)],
            vec![LiteralValue::Number(20.0), LiteralValue::Number(200.0)],
        ])
    );
    assert_eq!(
        evaluate_lifting_formula(&wb, "=IF({TRUE,NA()},1,2)"),
        LiteralValue::Array(vec![vec![
            LiteralValue::Number(1.0),
            LiteralValue::Error(ExcelError::new(ExcelErrorKind::Na)),
        ]])
    );
    assert_eq!(
        evaluate_lifting_formula(&wb, "=IF({TRUE,FALSE},ABS({11,CIRCULAR_READ()}),9)"),
        LiteralValue::Array(vec![vec![
            LiteralValue::Number(11.0),
            LiteralValue::Number(9.0),
        ]])
    );
    assert_eq!(circular_reads.load(Ordering::SeqCst), 0);
    let mut engine = array_lifting_engine();
    evaluate_lifting_engine_formula(&mut engine, "=IF({TRUE,FALSE},{1,1}/{1,0},9)");
    assert_eq!(
        engine.get_cell_value("Sheet1", 20, 1),
        Some(LiteralValue::Number(1.0))
    );
    assert_eq!(
        engine.get_cell_value("Sheet1", 20, 2),
        Some(LiteralValue::Number(9.0))
    );

    let arena_reads = Arc::new(AtomicUsize::new(0));
    let mut engine = crate::engine::Engine::new(
        TestWorkbook::new().with_function(Arc::new(CircularReadFn(Arc::clone(&arena_reads)))),
        crate::engine::EvalConfig::default(),
    );
    evaluate_lifting_engine_formula(&mut engine, "=IF({TRUE,FALSE},ABS({11,CIRCULAR_READ()}),9)");
    assert_eq!(
        engine.get_cell_value("Sheet1", 20, 1),
        Some(LiteralValue::Number(11.0))
    );
    assert_eq!(
        engine.get_cell_value("Sheet1", 20, 2),
        Some(LiteralValue::Number(9.0))
    );
    assert_eq!(arena_reads.load(Ordering::SeqCst), 0);
    for (row, key, first, second) in [(1, 1, 10, 100), (2, 2, 20, 200), (3, 3, 30, 300)] {
        engine
            .set_cell_value("Sheet1", row, 1, LiteralValue::Int(key))
            .expect("set IF lookup key");
        engine
            .set_cell_value("Sheet1", row, 11, LiteralValue::Int(first))
            .expect("set IF first return");
        engine
            .set_cell_value("Sheet1", row, 12, LiteralValue::Int(second))
            .expect("set IF second return");
    }
    evaluate_lifting_engine_formula(&mut engine, "=IF({TRUE;TRUE},XLOOKUP({1;2},A1:A3,K1:L3),0)");
    for (row, expected) in [(20, [10.0, 100.0]), (21, [20.0, 200.0])] {
        for (offset, value) in expected.into_iter().enumerate() {
            assert_eq!(
                engine.get_cell_value("Sheet1", row, offset as u32 + 1),
                Some(LiteralValue::Number(value))
            );
        }
    }
    evaluate_lifting_engine_formula(
        &mut engine,
        "=IF(TRUE,IF({TRUE;TRUE},XLOOKUP({1;2},A1:A3,K1:L3),0),0)",
    );
    for (row, expected) in [(20, [10.0, 100.0]), (21, [20.0, 200.0])] {
        for (offset, value) in expected.into_iter().enumerate() {
            assert_eq!(
                engine.get_cell_value("Sheet1", row, offset as u32 + 1),
                Some(LiteralValue::Number(value))
            );
        }
    }
    evaluate_lifting_engine_formula(
        &mut engine,
        "=IF({TRUE;TRUE},IF({TRUE;TRUE},XLOOKUP({1;2},A1:A3,K1:L3),0),0)",
    );
    for (row, expected) in [(20, [10.0, 100.0]), (21, [20.0, 200.0])] {
        for (offset, value) in expected.into_iter().enumerate() {
            assert_eq!(
                engine.get_cell_value("Sheet1", row, offset as u32 + 1),
                Some(LiteralValue::Number(value))
            );
        }
    }
}

#[test]
fn array_lifting_preserves_single_intersection() {
    let wb = array_lifting_workbook();
    assert_eq!(
        evaluate_lifting_formula(&wb, "=_xlfn.SINGLE(A1:A3)"),
        LiteralValue::Number(1.0)
    );
}

#[test]
fn array_lifting_preserves_range_reducers() {
    let wb = array_lifting_workbook();
    assert_eq!(
        evaluate_lifting_formula(&wb, "=SUM(A1:A3)"),
        LiteralValue::Number(6.0)
    );
    assert_eq!(
        evaluate_lifting_formula(&wb, "=SUMIF(A1:A3,\">1\",B1:B3)"),
        LiteralValue::Number(50.0)
    );
}

#[test]
fn array_lifting_preserves_by_ref() {
    use crate::args::{ArgSchema, ShapeKind};
    use crate::function::Function;
    use crate::traits::FunctionContext;
    use formualizer_common::{ArgKind, CoercionPolicy, ExcelError};
    use smallvec::smallvec;

    #[derive(Debug)]
    struct ByRefFn;

    impl Function for ByRefFn {
        fn name(&self) -> &'static str {
            "BYREF"
        }

        fn min_args(&self) -> usize {
            1
        }

        fn arg_schema(&self) -> &'static [ArgSchema] {
            static SCHEMA: std::sync::LazyLock<Vec<ArgSchema>> = std::sync::LazyLock::new(|| {
                vec![ArgSchema {
                    kinds: smallvec![ArgKind::Any],
                    required: true,
                    by_ref: true,
                    shape: ShapeKind::Range,
                    coercion: CoercionPolicy::None,
                    max: None,
                    repeating: None,
                    default: None,
                }]
            });
            &SCHEMA
        }

        fn eval<'x, 'b, 'c>(
            &self,
            args: &'c [ArgumentHandle<'x, 'b>],
            _ctx: &dyn FunctionContext<'b>,
        ) -> Result<crate::traits::CalcValue<'b>, ExcelError> {
            args[0].as_reference_or_eval()?;
            Ok(crate::traits::CalcValue::Scalar(LiteralValue::Int(1)))
        }
    }

    let wb = array_lifting_workbook().with_function(std::sync::Arc::new(ByRefFn));
    assert_eq!(
        evaluate_lifting_formula(&wb, "=BYREF(A1:A3)"),
        LiteralValue::Int(1)
    );
}

fn interp(wb: &TestWorkbook) -> crate::interpreter::Interpreter<'_> {
    wb.interpreter()
}

#[test]
fn sin_map_matches_scalar_for_array_input() {
    let wb = TestWorkbook::new().with_function(std::sync::Arc::new(SinFn));
    let ctx = interp(&wb);

    // Input array 2x2
    let arr = LiteralValue::Array(vec![
        vec![
            LiteralValue::Number(0.0),
            LiteralValue::Number(std::f64::consts::PI / 2.0),
        ],
        vec![
            LiteralValue::Number(std::f64::consts::PI),
            LiteralValue::Number(3.0 * std::f64::consts::PI / 2.0),
        ],
    ]);
    let node = ASTNode::new(ASTNodeType::Literal(arr), None);
    let args = vec![ArgumentHandle::new(&node, &ctx)];

    let sin = ctx.context.get_function("", "SIN").unwrap();

    // Scalar path maps via interpreter if we push SIN over each (simulate by map)
    // Here we call dispatch directly, which should use the map path because input is array.
    let out = sin
        .dispatch(&args, &ctx.function_context(None))
        .unwrap()
        .into_literal();
    match out {
        LiteralValue::Array(rows) => {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].len(), 2);
            // Check a few known values
            if let LiteralValue::Number(n) = rows[0][0] {
                assert!((n - 0.0).abs() < 1e-9);
            } else {
                panic!("unexpected");
            }
            if let LiteralValue::Number(n) = rows[0][1] {
                assert!((n - 1.0).abs() < 1e-9);
            } else {
                panic!("unexpected");
            }
        }
        v => panic!("unexpected result {v:?}"),
    }
}

#[test]
fn cos_map_matches_scalar_for_array_input() {
    let wb = TestWorkbook::new().with_function(std::sync::Arc::new(CosFn));
    let ctx = interp(&wb);

    let arr = LiteralValue::Array(vec![vec![
        LiteralValue::Number(0.0),
        LiteralValue::Number(std::f64::consts::PI / 2.0),
    ]]);
    let node = ASTNode::new(ASTNodeType::Literal(arr), None);
    let args = vec![ArgumentHandle::new(&node, &ctx)];

    let cos = ctx.context.get_function("", "COS").unwrap();
    let out = cos
        .dispatch(&args, &ctx.function_context(None))
        .unwrap()
        .into_literal();
    match out {
        LiteralValue::Array(rows) => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].len(), 2);
            if let LiteralValue::Number(n) = rows[0][0] {
                assert!((n - 1.0).abs() < 1e-9);
            } else {
                panic!();
            }
            if let LiteralValue::Number(n) = rows[0][1] {
                assert!(n.abs() < 1e-9);
            } else {
                panic!();
            }
        }
        v => panic!("unexpected result {v:?}"),
    }
}

#[test]
fn tan_map_handles_array_input() {
    let wb = TestWorkbook::new().with_function(std::sync::Arc::new(TanFn));
    let ctx = interp(&wb);

    let arr = LiteralValue::Array(vec![vec![
        LiteralValue::Number(0.0),
        LiteralValue::Number(std::f64::consts::PI / 4.0),
    ]]);
    let node = ASTNode::new(ASTNodeType::Literal(arr), None);
    let args = vec![ArgumentHandle::new(&node, &ctx)];

    let tan = ctx.context.get_function("", "TAN").unwrap();
    let out = tan
        .dispatch(&args, &ctx.function_context(None))
        .unwrap()
        .into_literal();
    match out {
        LiteralValue::Array(rows) => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].len(), 2);
            match rows[0][0] {
                LiteralValue::Number(n) => assert!(n.abs() < 1e-9),
                _ => panic!(),
            }
            match rows[0][1] {
                LiteralValue::Number(n) => assert!((n - 1.0).abs() < 1e-9),
                _ => panic!(),
            }
        }
        v => panic!("unexpected result {v:?}"),
    }
}

#[test]
fn atan2_map_broadcasts_scalar_over_array() {
    let wb = TestWorkbook::new().with_function(std::sync::Arc::new(Atan2Fn));
    let ctx = interp(&wb);

    // x is scalar, y is array -> broadcast x
    let x = ASTNode::new(ASTNodeType::Literal(LiteralValue::Number(1.0)), None);
    let y_arr = LiteralValue::Array(vec![vec![
        LiteralValue::Number(0.0),
        LiteralValue::Number(1.0),
    ]]);
    let y = ASTNode::new(ASTNodeType::Literal(y_arr), None);
    let args = vec![ArgumentHandle::new(&x, &ctx), ArgumentHandle::new(&y, &ctx)];

    let f = ctx.context.get_function("", "ATAN2").unwrap();
    let out = f
        .dispatch(&args, &ctx.function_context(None))
        .unwrap()
        .into_literal();
    match out {
        LiteralValue::Array(rows) => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].len(), 2);
            match rows[0][0] {
                LiteralValue::Number(n) => assert!((n - 0.0).abs() < 1e-9),
                _ => panic!(),
            }
            match rows[0][1] {
                LiteralValue::Number(n) => assert!((n - (1.0f64).atan2(1.0)).abs() < 1e-9),
                _ => panic!(),
            }
        }
        v => panic!("unexpected result {v:?}"),
    }
}

#[test]
fn sin_map_equals_scalar_per_cell() {
    let wb = TestWorkbook::new().with_function(std::sync::Arc::new(SinFn));
    let ctx = interp(&wb);

    let arr = LiteralValue::Array(vec![
        vec![
            LiteralValue::Number(0.0),
            LiteralValue::Number(std::f64::consts::PI / 2.0),
        ],
        vec![
            LiteralValue::Number(std::f64::consts::PI),
            LiteralValue::Number(3.0 * std::f64::consts::PI / 2.0),
        ],
    ]);
    let node_arr = ASTNode::new(ASTNodeType::Literal(arr), None);
    let args_arr = vec![ArgumentHandle::new(&node_arr, &ctx)];

    let sin = ctx.context.get_function("", "SIN").unwrap();
    let fctx = ctx.function_context(None);
    let out = sin.dispatch(&args_arr, &fctx).unwrap().into_literal();
    let rows = match out {
        LiteralValue::Array(r) => r,
        v => panic!("unexpected {v:?}"),
    };

    for (i, row) in rows.iter().enumerate() {
        for (j, cell) in row.iter().enumerate() {
            let input = match (i, j) {
                (0, 0) => 0.0,
                (0, 1) => std::f64::consts::PI / 2.0,
                (1, 0) => std::f64::consts::PI,
                (1, 1) => 3.0 * std::f64::consts::PI / 2.0,
                _ => unreachable!(),
            };
            let node_scalar = ASTNode::new(ASTNodeType::Literal(LiteralValue::Number(input)), None);
            let args_scalar = vec![ArgumentHandle::new(&node_scalar, &ctx)];
            let expected = sin.dispatch(&args_scalar, &fctx).unwrap().into_literal();
            assert_eq!(&expected, cell);
        }
    }
}

#[test]
fn cos_map_equals_scalar_per_cell() {
    let wb = TestWorkbook::new().with_function(std::sync::Arc::new(CosFn));
    let ctx = interp(&wb);

    let arr_vals = [0.0, std::f64::consts::PI / 2.0, std::f64::consts::PI];
    let arr = LiteralValue::Array(vec![
        vec![
            LiteralValue::Number(arr_vals[0]),
            LiteralValue::Number(arr_vals[1]),
        ],
        vec![LiteralValue::Number(arr_vals[2]), LiteralValue::Number(0.0)],
    ]);
    let node_arr = ASTNode::new(ASTNodeType::Literal(arr), None);
    let args_arr = vec![ArgumentHandle::new(&node_arr, &ctx)];

    let cos = ctx.context.get_function("", "COS").unwrap();
    let out = cos
        .dispatch(&args_arr, &ctx.function_context(None))
        .unwrap()
        .into_literal();
    let rows = match out {
        LiteralValue::Array(r) => r,
        v => panic!("unexpected {v:?}"),
    };

    match &rows[0][0] {
        LiteralValue::Number(n) => assert!((n - 1.0).abs() < 1e-9),
        _ => panic!(),
    }
    match &rows[0][1] {
        LiteralValue::Number(n) => assert!(n.abs() < 1e-9),
        _ => panic!(),
    }
    match &rows[1][0] {
        LiteralValue::Number(n) => assert!((n + 1.0).abs() < 1e-9),
        _ => panic!(),
    }
}

#[test]
fn atan2_map_equals_scalar_per_cell_broadcast() {
    let wb = TestWorkbook::new().with_function(std::sync::Arc::new(Atan2Fn));
    let ctx = interp(&wb);

    // x scalar, y array
    let x_node = ASTNode::new(ASTNodeType::Literal(LiteralValue::Number(1.0)), None);
    let y_arr = LiteralValue::Array(vec![vec![
        LiteralValue::Number(0.0),
        LiteralValue::Number(1.0),
        LiteralValue::Number(2.0),
    ]]);
    let y_node = ASTNode::new(ASTNodeType::Literal(y_arr), None);

    let atan2 = ctx.context.get_function("", "ATAN2").unwrap();
    let args_vec = vec![
        ArgumentHandle::new(&x_node, &ctx),
        ArgumentHandle::new(&y_node, &ctx),
    ];
    let fctx = ctx.function_context(None);
    let out = atan2.dispatch(&args_vec, &fctx).unwrap().into_literal();
    let rows = match out {
        LiteralValue::Array(r) => r,
        v => panic!("unexpected {v:?}"),
    };
    let row = &rows[0];

    for (idx, y) in [0.0, 1.0, 2.0].iter().enumerate() {
        let xs = ASTNode::new(ASTNodeType::Literal(LiteralValue::Number(1.0)), None);
        let ys = ASTNode::new(ASTNodeType::Literal(LiteralValue::Number(*y)), None);
        let expected = atan2
            .dispatch(
                &[
                    ArgumentHandle::new(&xs, &ctx),
                    ArgumentHandle::new(&ys, &ctx),
                ],
                &fctx,
            )
            .unwrap()
            .into_literal();
        assert_eq!(&expected, &row[idx]);
    }
}

#[test]
fn interpreter_ref_context_returns_range_reference() {
    let wb = TestWorkbook::new()
        .with_cell_a1("Sheet1", "A1", LiteralValue::Int(1))
        .with_cell_a1("Sheet1", "A2", LiteralValue::Int(2));
    let ctx = interp(&wb);

    let node = ASTNode::new(
        ASTNodeType::Reference {
            original: "A1:A2".into(),
            reference: ReferenceType::Range {
                sheet: None,
                start_row: Some(1),
                start_col: Some(1),
                end_row: Some(2),
                end_col: Some(1),
                start_row_abs: false,
                start_col_abs: false,
                end_row_abs: false,
                end_col_abs: false,
            },
        },
        None,
    );
    let r = ctx.evaluate_ast_as_reference(&node).expect("ref ok");
    match r {
        ReferenceType::Range {
            start_row, end_row, ..
        } => {
            assert_eq!(start_row, Some(1));
            assert_eq!(end_row, Some(2));
        }
        _ => panic!("expected range"),
    }
}

#[test]
fn range_operator_composition_same_sheet() {
    let wb = TestWorkbook::new();
    let ctx = interp(&wb);
    let left = ASTNode::new(
        ASTNodeType::Reference {
            original: "A1".into(),
            reference: ReferenceType::Cell {
                sheet: None,
                row: 1,
                col: 1,
                row_abs: false,
                col_abs: false,
            },
        },
        None,
    );
    let right = ASTNode::new(
        ASTNodeType::Reference {
            original: "B2".into(),
            reference: ReferenceType::Cell {
                sheet: None,
                row: 2,
                col: 2,
                row_abs: false,
                col_abs: false,
            },
        },
        None,
    );
    // cannot call private eval_binary here; skip direct value-context enforcement
    // reference context via helper
    let lref = ctx.evaluate_ast_as_reference(&left).unwrap();
    let rref = ctx.evaluate_ast_as_reference(&right).unwrap();
    let comb = crate::reference::combine_references(&lref, &rref).unwrap();
    match comb {
        ReferenceType::Range {
            start_row,
            start_col,
            end_row,
            end_col,
            ..
        } => {
            assert_eq!(
                (start_row, start_col, end_row, end_col),
                (Some(1), Some(1), Some(2), Some(2))
            );
        }
        _ => panic!("expected range"),
    }
}

#[test]
fn interpreter_evaluate_ast_as_reference_returns_reference_for_ast_reference() {
    let wb = TestWorkbook::new()
        .with_cell_a1("Sheet1", "A1", LiteralValue::Int(7))
        .with_cell_a1("Sheet1", "A2", LiteralValue::Int(8));
    let ctx = interp(&wb);

    let node = ASTNode::new(
        ASTNodeType::Reference {
            original: "A1:A2".to_string(),
            reference: ReferenceType::Range {
                sheet: None,
                start_row: Some(1),
                start_col: Some(1),
                end_row: Some(2),
                end_col: Some(1),
                start_row_abs: false,
                start_col_abs: false,
                end_row_abs: false,
                end_col_abs: false,
            },
        },
        None,
    );
    let r = ctx
        .evaluate_ast_as_reference(&node)
        .expect("expected reference");
    match r {
        ReferenceType::Range {
            start_row, end_row, ..
        } => {
            assert_eq!(start_row, Some(1));
            assert_eq!(end_row, Some(2));
        }
        _ => panic!("expected range reference"),
    }
}

#[test]
fn structured_ref_basic_specifiers() {
    use crate::traits::Resolver;
    type V = LiteralValue;
    // Build a test workbook with a simple table
    let wb = TestWorkbook::new().with_simple_table(
        "Sales",
        vec!["Region".into(), "Amount".into(), "Units".into()],
        vec![
            vec![V::Text("N".into()), V::Number(10.0), V::Int(2)],
            vec![V::Text("S".into()), V::Number(20.0), V::Int(3)],
        ],
        Some(vec![V::Text("".into()), V::Number(30.0), V::Int(5)]),
    );

    // Column reference
    let r = ReferenceType::from_string("Sales[Amount]").unwrap();
    let range = wb.resolve_range_like(&r).unwrap();
    assert_eq!(range.dimensions(), (2, 1));
    assert_eq!(range.get(0, 0).unwrap(), V::Number(10.0));
    assert_eq!(range.get(1, 0).unwrap(), V::Number(20.0));

    // Column range
    let r = ReferenceType::from_string("Sales[Amount:Units]").unwrap();
    let range = wb.resolve_range_like(&r).unwrap();
    assert_eq!(range.dimensions(), (2, 2));
    assert_eq!(range.get(0, 0).unwrap(), V::Number(10.0));
    assert_eq!(range.get(1, 1).unwrap(), V::Int(3));

    // Headers
    let r = ReferenceType::from_string("Sales[#Headers]").unwrap();
    let range = wb.resolve_range_like(&r).unwrap();
    assert_eq!(range.dimensions(), (1, 3));

    // Totals
    let r = ReferenceType::from_string("Sales[#Totals]").unwrap();
    let range = wb.resolve_range_like(&r).unwrap();
    assert_eq!(range.dimensions(), (1, 3));
    assert_eq!(range.get(0, 1).unwrap(), V::Number(30.0));

    // All = headers + data + totals
    let r = ReferenceType::from_string("Sales[#All]").unwrap();
    let range = wb.resolve_range_like(&r).unwrap();
    assert_eq!(range.dimensions(), (1 + 2 + 1, 3));
}

#[test]
fn interpreter_broadcasts_numeric_binary() {
    let wb = TestWorkbook::new();
    let ctx = interp(&wb);

    // {1,2;3,4} + {10;20} => {11,12;23,24}
    let left = LiteralValue::Array(vec![
        vec![LiteralValue::Int(1), LiteralValue::Int(2)],
        vec![LiteralValue::Int(3), LiteralValue::Int(4)],
    ]);
    let right = LiteralValue::Array(vec![
        vec![LiteralValue::Int(10)],
        vec![LiteralValue::Int(20)],
    ]);
    let lnode = ASTNode::new(ASTNodeType::Literal(left), None);
    let rnode = ASTNode::new(ASTNodeType::Literal(right), None);
    let plus = ASTNode::new(
        ASTNodeType::BinaryOp {
            op: "+".into(),
            left: Box::new(lnode),
            right: Box::new(rnode),
        },
        None,
    );
    let out = ctx.evaluate_ast(&plus).unwrap().into_literal();
    match out {
        LiteralValue::Array(rows) => {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].len(), 2);
            assert_eq!(rows[0][0], LiteralValue::Number(11.0));
            assert_eq!(rows[0][1], LiteralValue::Number(12.0));
            assert_eq!(rows[1][0], LiteralValue::Number(23.0));
            assert_eq!(rows[1][1], LiteralValue::Number(24.0));
        }
        v => panic!("unexpected {v:?}"),
    }
}

#[test]
fn interpreter_broadcast_scalar_over_array() {
    let wb = TestWorkbook::new();
    let ctx = interp(&wb);
    // 2 * {1,2,3} => {2,4,6}
    let lnode = ASTNode::new(ASTNodeType::Literal(LiteralValue::Int(2)), None);
    let right = LiteralValue::Array(vec![vec![
        LiteralValue::Int(1),
        LiteralValue::Int(2),
        LiteralValue::Int(3),
    ]]);
    let rnode = ASTNode::new(ASTNodeType::Literal(right), None);
    let node = ASTNode::new(
        ASTNodeType::BinaryOp {
            op: "*".into(),
            left: Box::new(lnode),
            right: Box::new(rnode),
        },
        None,
    );
    let out = ctx.evaluate_ast(&node).unwrap().into_literal();
    match out {
        LiteralValue::Array(rows) => {
            assert_eq!(
                rows[0],
                vec![
                    LiteralValue::Number(2.0),
                    LiteralValue::Number(4.0),
                    LiteralValue::Number(6.0),
                ]
            );
        }
        v => panic!("unexpected {v:?}"),
    }
}

#[test]
fn interpreter_incompatible_broadcast_is_value_error() {
    let wb = TestWorkbook::new();
    let ctx = interp(&wb);

    // {1,2} + {1,2,3} -> #VALUE!
    let l = LiteralValue::Array(vec![vec![LiteralValue::Int(1), LiteralValue::Int(2)]]);
    let r = LiteralValue::Array(vec![vec![
        LiteralValue::Int(1),
        LiteralValue::Int(2),
        LiteralValue::Int(3),
    ]]);
    let lnode = ASTNode::new(ASTNodeType::Literal(l), None);
    let rnode = ASTNode::new(ASTNodeType::Literal(r), None);
    let n = ASTNode::new(
        ASTNodeType::BinaryOp {
            op: "+".into(),
            left: Box::new(lnode),
            right: Box::new(rnode),
        },
        None,
    );
    match ctx.evaluate_ast(&n).unwrap().into_literal() {
        LiteralValue::Error(e) => assert_eq!(e, "#VALUE!"),
        v => panic!("expected value error, got {v:?}"),
    }
}

fn reference_returning_engine(g1: i64) -> crate::engine::Engine<TestWorkbook> {
    use crate::engine::{CycleConfig, CycleDetection, CyclePolicy, EvalConfig};

    let cfg = EvalConfig::default().with_cycle(CycleConfig {
        detection: CycleDetection::Runtime,
        policy: CyclePolicy::Error,
    });
    let mut engine = crate::engine::Engine::new(TestWorkbook::new(), cfg);
    for row in 1..=20 {
        for (col, value) in [
            (1, row as i64),
            (2, 100 + row as i64),
            (3, 200 + row as i64),
            (4, row as i64),
            (5, 400 + row as i64),
            (6, 500 + row as i64),
        ] {
            engine
                .set_cell_value("Sheet1", row, col, LiteralValue::Int(value))
                .expect("set reference fixture value");
        }
    }
    engine
        .set_cell_value("Sheet1", 1, 7, LiteralValue::Int(g1))
        .expect("set selector");
    engine
}

fn evaluate_reference_returning_formula(g1: i64, formula: &str) -> LiteralValue {
    let mut engine = reference_returning_engine(g1);
    engine
        .set_cell_formula(
            "Sheet1",
            1,
            10,
            formualizer_parse::parser::parse(formula).expect("valid reference-returning formula"),
        )
        .expect("set reference-returning formula");
    engine
        .evaluate_all()
        .expect("evaluate reference-returning formula");
    engine
        .get_cell_value("Sheet1", 1, 10)
        .expect("formula result")
}

#[test]
fn reference_returning_if_offset_index() {
    assert_eq!(
        evaluate_reference_returning_formula(1, "=OFFSET(INDEX(IF(G1=1,A1:C20,D1:F20),2,1),0,1)",),
        LiteralValue::Number(102.0)
    );
    assert_eq!(
        evaluate_reference_returning_formula(0, "=OFFSET(INDEX(IF(G1=1,A1:C20,D1:F20),2,1),0,1)",),
        LiteralValue::Number(402.0)
    );
}

#[test]
fn reference_returning_if_offset_direct() {
    assert_eq!(
        evaluate_reference_returning_formula(1, "=OFFSET(IF(G1=1,A1:C20,D1:F20),1,1)"),
        LiteralValue::Number(102.0)
    );
    assert_eq!(
        evaluate_reference_returning_formula(0, "=OFFSET(IF(G1=1,A1:C20,D1:F20),1,1)"),
        LiteralValue::Number(402.0)
    );
}

#[test]
fn reference_returning_ifs_offset_index() {
    assert_eq!(
        evaluate_reference_returning_formula(
            1,
            "=OFFSET(INDEX(IFS(G1=1,A1:C20,TRUE,D1:F20),2,1),0,1)",
        ),
        LiteralValue::Number(102.0)
    );
    assert_eq!(
        evaluate_reference_returning_formula(
            0,
            "=OFFSET(INDEX(IFS(G1=1,A1:C20,TRUE,D1:F20),2,1),0,1)",
        ),
        LiteralValue::Number(402.0)
    );
}

#[test]
fn reference_returning_choose_offset_index() {
    assert_eq!(
        evaluate_reference_returning_formula(1, "=OFFSET(INDEX(CHOOSE(G1,A1:C20,D1:F20),2,1),0,1)",),
        LiteralValue::Number(102.0)
    );
    assert_eq!(
        evaluate_reference_returning_formula(2, "=OFFSET(INDEX(CHOOSE(G1,A1:C20,D1:F20),2,1),0,1)",),
        LiteralValue::Number(402.0)
    );
}

#[test]
fn reference_returning_if_family_value_paths() {
    assert_eq!(
        evaluate_reference_returning_formula(1, "=SUM(IF(G1=1,A1:A20,D1:D20))"),
        LiteralValue::Number(210.0)
    );
    assert_eq!(
        evaluate_reference_returning_formula(1, "=VLOOKUP(5,CHOOSE(1,A1:B20,D1:E20),2,FALSE)",),
        LiteralValue::Number(105.0)
    );
    assert_eq!(
        evaluate_reference_returning_formula(1, "=INDEX(IFERROR(1/0,A1:C20),3,3)"),
        LiteralValue::Number(203.0)
    );
    assert_eq!(
        evaluate_reference_returning_formula(1, "=IF(G1=1,5,A1:A3)"),
        LiteralValue::Number(5.0)
    );
    assert_eq!(
        evaluate_reference_returning_formula(0, "=SUM(IF(G1=1,5,A1:A3))"),
        LiteralValue::Number(6.0)
    );
}

#[test]
fn if_family_selector_evaluation_count() {
    use crate::function::{FnCaps, Function};
    use crate::traits::{FunctionContext, ResolvedArgument};
    use formualizer_common::ExcelError;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    #[derive(Debug)]
    struct CountSelectorFn {
        array: Arc<AtomicBool>,
        calls: Arc<AtomicUsize>,
        selected: Arc<AtomicBool>,
    }

    impl Function for CountSelectorFn {
        fn caps(&self) -> FnCaps {
            FnCaps::empty()
        }

        fn name(&self) -> &'static str {
            "COUNTSELECTOR"
        }

        fn arg_schema(&self) -> &'static [crate::args::ArgSchema] {
            &[]
        }

        fn eval<'a, 'b, 'c>(
            &self,
            _args: &'c [ArgumentHandle<'a, 'b>],
            _ctx: &dyn FunctionContext<'b>,
        ) -> Result<crate::traits::CalcValue<'b>, ExcelError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.array.load(Ordering::SeqCst) {
                return Ok(crate::traits::CalcValue::Scalar(LiteralValue::Array(vec![
                    vec![LiteralValue::Boolean(true), LiteralValue::Boolean(false)],
                ])));
            }
            Ok(crate::traits::CalcValue::Scalar(LiteralValue::Boolean(
                self.selected.load(Ordering::SeqCst),
            )))
        }
    }

    fn workbook(
        array: Arc<AtomicBool>,
        calls: Arc<AtomicUsize>,
        selected: Arc<AtomicBool>,
    ) -> TestWorkbook {
        TestWorkbook::new()
            .with_range(
                "Sheet1",
                1,
                1,
                vec![
                    vec![LiteralValue::Int(1)],
                    vec![LiteralValue::Int(2)],
                    vec![LiteralValue::Int(3)],
                ],
            )
            .with_function(Arc::new(CountSelectorFn {
                array,
                calls,
                selected,
            }))
    }

    crate::builtins::load_builtins();
    let array = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let selected = Arc::new(AtomicBool::new(true));

    let wb = workbook(
        Arc::clone(&array),
        Arc::clone(&calls),
        Arc::clone(&selected),
    );
    let interpreter = wb.interpreter();
    let ast = formualizer_parse::parser::parse("=IF(COUNTSELECTOR(),A1:A3,5)")
        .expect("valid AST selector formula");
    let handle = ArgumentHandle::new(&ast, &interpreter);
    assert!(matches!(
        handle.resolve_once(),
        Ok(ResolvedArgument::Range(_))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "AST reference arm");

    calls.store(0, Ordering::SeqCst);
    selected.store(false, Ordering::SeqCst);
    let ast = formualizer_parse::parser::parse("=IF(COUNTSELECTOR(),A1:A3,5)")
        .expect("valid AST selector formula");
    let handle = ArgumentHandle::new(&ast, &interpreter);
    assert!(matches!(
        handle.resolve_once(),
        Ok(ResolvedArgument::Value(crate::traits::CalcValue::Scalar(
            LiteralValue::Number(5.0)
        )))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "AST value arm");

    calls.store(0, Ordering::SeqCst);
    selected.store(true, Ordering::SeqCst);
    let ast = formualizer_parse::parser::parse("=INDEX(IF(COUNTSELECTOR(),A1:A3,D1:D3),0,1)")
        .expect("valid zero-index fallback formula");
    assert!(matches!(
        interpreter.evaluate_ast(&ast),
        Ok(crate::traits::CalcValue::Range(_))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "AST zero-index fallback");

    for (formula, label) in [
        (
            "=INDEX(IF(COUNTSELECTOR(),5,A1:A3),1)",
            "selected scalar fallback",
        ),
        (
            "=INDEX(IF(COUNTSELECTOR(),A1:A3,D1:D3),99,1)",
            "out-of-bounds fallback",
        ),
        (
            "=INDEX(IF(COUNTSELECTOR(),A1:B3,D1:E3),2)",
            "2-D omitted-column fallback",
        ),
    ] {
        calls.store(0, Ordering::SeqCst);
        let ast = formualizer_parse::parser::parse(formula).expect("valid INDEX fallback formula");
        let _ = interpreter.evaluate_ast(&ast);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "AST {label}");
    }

    for (select_reference, expected) in [(true, 6.0), (false, 5.0)] {
        calls.store(0, Ordering::SeqCst);
        selected.store(select_reference, Ordering::SeqCst);
        let wb = workbook(
            Arc::clone(&array),
            Arc::clone(&calls),
            Arc::clone(&selected),
        );
        let mut engine = crate::engine::Engine::new(
            wb,
            crate::engine::EvalConfig::default().with_cycle(crate::engine::CycleConfig {
                detection: crate::engine::CycleDetection::Runtime,
                policy: crate::engine::CyclePolicy::Error,
            }),
        );
        for (row, value) in [(1, 1), (2, 2), (3, 3)] {
            engine
                .set_cell_value("Sheet1", row, 1, LiteralValue::Int(value))
                .expect("set arena reference value");
        }
        engine
            .set_cell_formula(
                "Sheet1",
                1,
                10,
                formualizer_parse::parser::parse("=SUM(IF(COUNTSELECTOR(),A1:A3,5))")
                    .expect("valid arena selector formula"),
            )
            .expect("set arena selector formula");
        engine
            .evaluate_all()
            .expect("evaluate arena selector formula");
        assert_eq!(
            engine.get_cell_value("Sheet1", 1, 10),
            Some(LiteralValue::Number(expected))
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "Arena {} arm",
            if select_reference {
                "reference"
            } else {
                "value"
            }
        );
    }

    for path in ["AST", "Arena"] {
        calls.store(0, Ordering::SeqCst);
        array.store(true, Ordering::SeqCst);
        let wb = workbook(
            Arc::clone(&array),
            Arc::clone(&calls),
            Arc::clone(&selected),
        );
        if path == "AST" {
            let interpreter = wb.interpreter();
            let ast = formualizer_parse::parser::parse("=IF(COUNTSELECTOR(),{1,1},{2,2})")
                .expect("valid AST array-selector formula");
            let handle = ArgumentHandle::new(&ast, &interpreter);
            let _ = handle.resolve_once();
        } else {
            let mut engine = crate::engine::Engine::new(
                wb,
                crate::engine::EvalConfig::default().with_cycle(crate::engine::CycleConfig {
                    detection: crate::engine::CycleDetection::Runtime,
                    policy: crate::engine::CyclePolicy::Error,
                }),
            );
            engine
                .set_cell_formula(
                    "Sheet1",
                    1,
                    10,
                    formualizer_parse::parser::parse("=SUM(IF(COUNTSELECTOR(),{1,1},{2,2}))")
                        .expect("valid Arena array-selector formula"),
                )
                .expect("set Arena array-selector formula");
            engine.evaluate_all().expect("evaluate array selector");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2, "{path} array selector");
        array.store(false, Ordering::SeqCst);
    }
}

fn assert_reference_formula_number(formula: &str, expected: f64) {
    match evaluate_reference_returning_formula(1, formula) {
        LiteralValue::Int(value) => assert_eq!(value as f64, expected, "{formula}"),
        LiteralValue::Number(value) => assert_eq!(value, expected, "{formula}"),
        other => panic!("expected {expected} from {formula}, got {other:?}"),
    }
}

fn assert_reference_formula_value_error(formula: &str) {
    match evaluate_reference_returning_formula(1, formula) {
        LiteralValue::Error(error) => assert_eq!(
            error.kind,
            formualizer_common::ExcelErrorKind::Value,
            "{formula}"
        ),
        other => panic!("expected #VALUE! from {formula}, got {other:?}"),
    }
}

/// CHOOSE selector bounds on the value path. `CHOOSE(len, ...)` is the last
/// valid index and `CHOOSE(len + 1, ...)` must be #VALUE! rather than an
/// out-of-bounds argument access: a selector bound widened by one panics on
/// the len + 1 case while every wider overflow still errors.
#[test]
fn choose_selector_bounds_value_path() {
    assert_reference_formula_number("=CHOOSE(2,A1,B1)", 101.0);
    assert_reference_formula_value_error("=CHOOSE(0,A1,B1)");
    assert_reference_formula_value_error("=CHOOSE(3,A1,B1)");
}

/// The same bounds through `resolve_choose_reference_or_value`: OFFSET forces
/// CHOOSE down the reference path, which carries its own selector check.
#[test]
fn choose_selector_bounds_reference_path() {
    assert_reference_formula_number("=OFFSET(CHOOSE(2,A1,B1),0,0)", 101.0);
    assert_reference_formula_number("=OFFSET(CHOOSE(2,A1,B1),1,0)", 102.0);
    assert_reference_formula_value_error("=OFFSET(CHOOSE(0,A1,B1),0,0)");
    assert_reference_formula_value_error("=OFFSET(CHOOSE(3,A1,B1),0,0)");
}

/// Pin every syntax arm of `ArgumentHandle::may_return_reference` so the
/// guard is falsifiable: it exists to keep arguments that cannot produce a
/// reference off the reference-resolution path, and the downstream let-chain
/// re-rejects them, so only direct assertions can catch a broken arm.
#[test]
fn may_return_reference_syntax_arms() {
    crate::builtins::load_builtins();
    let wb = TestWorkbook::new();
    let interpreter = wb.interpreter();
    let arm = |formula: &str| {
        let ast = formualizer_parse::parser::parse(formula).expect("parse arm formula");
        ArgumentHandle::new(&ast, &interpreter).may_return_reference()
    };

    assert!(arm("=A1"), "cell reference");
    assert!(arm("=A1:B3"), "range reference");
    assert!(arm("=INDEX(A1:B3,1,1)"), "RETURNS_REFERENCE function");
    assert!(
        arm("=UNBOUNDNAME"),
        "an unbound name may be a workbook named range"
    );
    assert!(!arm("=SUM(A1:B3)"), "value-returning function");
    assert!(!arm("=1+2"), "arithmetic expression");
    assert!(!arm("=\"text\""), "literal");
}

/// The LET/LAMBDA exclusion: a local binding shadows any workbook name of the
/// same spelling and locals resolve only on the value path, so a bound name
/// must report itself as not reference-capable.
#[test]
fn may_return_reference_excludes_let_lambda_locals() {
    use crate::interpreter::{LocalBinding, LocalEnv};

    crate::builtins::load_builtins();
    let wb = TestWorkbook::new();
    let interpreter = wb.interpreter();
    let ast = formualizer_parse::parser::parse("=X").expect("parse local name");

    let unbound = ArgumentHandle::new(&ast, &interpreter);
    assert!(
        unbound.may_return_reference(),
        "without a local binding the name may be a workbook named range"
    );

    let env = LocalEnv::default().with_binding("X", LocalBinding::Value(LiteralValue::Int(7)));
    let scoped = interpreter.with_local_env(env);
    let bound = ArgumentHandle::new(&ast, &scoped);
    assert!(
        !bound.may_return_reference(),
        "a LET/LAMBDA local must not be sent down the named-range route"
    );
}

fn god187ad_reference_returning_engine() -> crate::engine::Engine<TestWorkbook> {
    ensure_lifting_builtins();
    let mut engine =
        crate::engine::Engine::new(TestWorkbook::new(), crate::engine::EvalConfig::default());
    for row in 1..=20 {
        for (col, value) in [
            (1, row as i64),
            (2, 100 + row as i64),
            (3, 200 + row as i64),
        ] {
            engine
                .set_cell_value("Sheet1", row, col, LiteralValue::Int(value))
                .expect("set reference fixture value");
        }
    }
    engine
}

fn god187ad_evaluate_reference_formula(formula: &str) -> LiteralValue {
    let mut engine = god187ad_reference_returning_engine();
    engine
        .set_cell_formula(
            "Sheet1",
            1,
            10,
            formualizer_parse::parser::parse(formula).expect("valid reference formula"),
        )
        .expect("set reference formula");
    engine.evaluate_all().expect("evaluate reference formula");
    engine
        .get_cell_value("Sheet1", 1, 10)
        .expect("formula result")
}

#[test]
fn reference_returning_iferror_offset_index() {
    assert_eq!(
        god187ad_evaluate_reference_formula("=OFFSET(INDEX(IFERROR(1/0,A1:C20),2,1),0,1)"),
        LiteralValue::Number(102.0)
    );
}

#[test]
fn reference_returning_ifna_offset_index() {
    assert_eq!(
        god187ad_evaluate_reference_formula("=OFFSET(INDEX(IFNA(NA(),A1:C20),2,1),0,1)"),
        LiteralValue::Number(102.0)
    );
}

#[test]
fn if_family_reference_live_edges_idempotent() {
    use crate::engine::{CycleConfig, CycleDetection, CyclePolicy, EvalConfig};

    ensure_lifting_builtins();
    let mut engine = crate::engine::Engine::new(
        TestWorkbook::new(),
        EvalConfig::default().with_cycle(CycleConfig {
            detection: CycleDetection::Runtime,
            policy: CyclePolicy::Error,
        }),
    );
    engine.set_cycle_instrumentation_targets(vec![("Sheet1".to_string(), 9, 3)]);
    engine
        .set_cell_value("Sheet1", 1, 7, LiteralValue::Int(1))
        .expect("set selector");
    for row in 1..=100 {
        engine
            .set_cell_value("Sheet1", row, 17, LiteralValue::Int(row as i64))
            .expect("set Q value");
        engine
            .set_cell_value("Sheet1", row, 18, LiteralValue::Int(1000 + row as i64))
            .expect("set R value");
    }
    engine
        .set_cell_formula(
            "Sheet1",
            9,
            3,
            formualizer_parse::parser::parse("=OFFSET(INDEX(IF(G1=1,Q1:Q100,R1:R100),50,1),0,0)")
                .expect("valid live-edge formula"),
        )
        .expect("set live-edge formula");
    engine
        .set_cell_formula(
            "Sheet1",
            50,
            17,
            formualizer_parse::parser::parse("=C9").expect("valid back edge"),
        )
        .expect("set back edge");

    engine.evaluate_all().expect("first recalc");
    let first = engine.cycle_instrumentation_targets()[0].edges.clone();
    assert!(!first.is_empty(), "first recalc must record live edges");
    engine.evaluate_all().expect("second recalc");
    let second = engine.cycle_instrumentation_targets()[0].edges.clone();
    assert_eq!(first, second, "live edges changed across identical recalcs");
}

#[test]
fn lifted_scalar_override_cannot_reenter_reference_resolution() {
    ensure_lifting_builtins();
    let workbook = TestWorkbook::new().with_range(
        "Sheet1",
        1,
        1,
        vec![vec![LiteralValue::Int(11)], vec![LiteralValue::Int(22)]],
    );
    let interpreter = workbook.interpreter();
    let ast = formualizer_parse::parser::parse("=IF(TRUE,A1:A2,A1:A2)")
        .expect("valid reference-returning IF");
    let handle = ArgumentHandle::new(&ast, &interpreter).with_scalar_value(LiteralValue::Int(22));

    assert!(!handle.may_return_reference());
    match handle
        .resolve_reference_or_value()
        .expect("resolve projected scalar")
    {
        crate::function::FunctionResolution::Value(value) => {
            assert_eq!(value.into_literal(), LiteralValue::Int(22));
        }
        _ => panic!("projected scalar re-entered reference resolution"),
    }
}

#[test]
fn binary_lifted_scalar_override_cannot_reenter_reference_resolution() {
    ensure_lifting_builtins();
    let workbook = concat_match_workbook();
    let interpreter = workbook.interpreter();
    let ast = formualizer_parse::parser::parse("=OFFSET(C1:E1,0,0)")
        .expect("valid reference-returning OFFSET");
    let handle =
        ArgumentHandle::new(&ast, &interpreter).with_scalar_value(LiteralValue::Text("B".into()));

    assert!(!handle.may_return_reference());
    assert_eq!(
        handle.value_at(0, 0).unwrap(),
        LiteralValue::Text("B".into())
    );
}

// ───────────────────────── CL-087 / ES-008 element-wise lifting ─────────────
//
// The lifted-argument set used to be a hard-coded function-NAME allowlist in
// the interpreter. It is now owned by the callee via
// `Function::elementwise_lifted_positions`, driven by `FnCaps::ELEMENTWISE`
// and the declared argument schema. These tests pin the behaviour that change
// is supposed to produce.
//
// Invented data only. A1:A3 hold the date serials 45000/45001/45002
// (2023-03-15/16/17), B1 holds 45000 as a scalar control, and C1:C3 hold the
// years 2021/2022/2023.

fn cl087_workbook() -> TestWorkbook {
    TestWorkbook::new().with_range(
        "Sheet1",
        1,
        1,
        vec![
            vec![
                LiteralValue::Int(45000),
                LiteralValue::Int(45000),
                LiteralValue::Int(2021),
            ],
            vec![
                LiteralValue::Int(45001),
                LiteralValue::Empty,
                LiteralValue::Int(2022),
            ],
            vec![
                LiteralValue::Int(45002),
                LiteralValue::Empty,
                LiteralValue::Int(2023),
            ],
        ],
    )
}

fn cl087_number(value: &LiteralValue) -> f64 {
    match value {
        LiteralValue::Int(n) => *n as f64,
        LiteralValue::Number(n) => *n,
        LiteralValue::Boolean(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        other => panic!("expected a number, got {other:?}"),
    }
}

/// Flattens a lifted (spilled) result row-major into numbers.
fn cl087_elementwise(wb: &TestWorkbook, formula: &str) -> Vec<f64> {
    match evaluate_lifting_formula(wb, formula) {
        LiteralValue::Array(rows) => rows
            .iter()
            .flat_map(|row| row.iter().map(cl087_number))
            .collect(),
        other => panic!("{formula}: expected an element-wise array, got {other:?}"),
    }
}

fn cl087_scalar(wb: &TestWorkbook, formula: &str) -> f64 {
    let value = evaluate_lifting_formula(wb, formula);
    assert!(
        !matches!(value, LiteralValue::Array(_)),
        "{formula}: expected a scalar, got a spilled array"
    );
    cl087_number(&value)
}

/// Asserts a scalar error with NO spill.
fn cl087_scalar_error(wb: &TestWorkbook, formula: &str, kind: ExcelErrorKind) {
    match evaluate_lifting_formula(wb, formula) {
        LiteralValue::Error(error) => assert_eq!(error.kind, kind, "{formula}"),
        other => panic!("{formula}: expected a scalar {kind:?} error, got {other:?}"),
    }
}

fn cl087_lifted_positions(name: &str, args_len: usize) -> Option<Vec<usize>> {
    ensure_lifting_builtins();
    crate::function_registry::get("", name)
        .unwrap_or_else(|| panic!("{name} is registered"))
        .elementwise_lifted_positions(args_len)
}

fn cl087_lift_refuses_range_reference(name: &str) -> bool {
    ensure_lifting_builtins();
    crate::function_registry::get("", name)
        .unwrap_or_else(|| panic!("{name} is registered"))
        .elementwise_lift_refuses_range_reference()
}

#[test]
fn cl087_date_parts_lift_over_a_range() {
    // Excel measured, 16.105.3, OT-198 receipt `g4d_excel_cse_probe.json`.
    // The entry mode DIFFERS per assertion and is cited per assertion: only
    // YEAR has a dynamic-array row; MONTH and DAY were measured under
    // Ctrl+Shift+Enter array entry only.
    let wb = cl087_workbook();
    // Rows `Q27_year_range_dynamic` (dynamic entry, Range.Formula2, 3 cells
    // filled) and `Q01_year_range` (array entry, Range.FormulaArray).
    assert_eq!(
        cl087_elementwise(&wb, "=YEAR(A1:A3)"),
        vec![2023.0, 2023.0, 2023.0]
    );
    // Row `Q02_month_range`: ARRAY entry (Ctrl+Shift+Enter) only. No
    // dynamic-entry row exists for MONTH over a range.
    assert_eq!(cl087_elementwise(&wb, "=MONTH(A1:A3)"), vec![3.0, 3.0, 3.0]);
    // Row `Q03_day_range`: ARRAY entry (Ctrl+Shift+Enter) only. No
    // dynamic-entry row exists for DAY over a range.
    assert_eq!(
        cl087_elementwise(&wb, "=DAY(A1:A3)"),
        vec![15.0, 16.0, 17.0]
    );
}

#[test]
fn cl087_date_parts_lift_over_an_if_produced_array() {
    // Excel measured, 16.105.3, OT-198 receipt `g4d_excel_cse_probe.json`.
    // This is the producer shape that CL-087 was raised for. Entry mode is
    // cited per assertion.
    let wb = cl087_workbook();
    // Rows `Q31_year_if_array_dynamic` (dynamic entry) and `Q07_year_if_array`
    // (array entry): both modes agree.
    assert_eq!(
        cl087_elementwise(&wb, "=YEAR(IF(ISNUMBER(A1:A3),A1:A3,0))"),
        vec![2023.0, 2023.0, 2023.0]
    );
    // Row `Q10_month_if_array`: ARRAY entry (Ctrl+Shift+Enter) only.
    assert_eq!(
        cl087_elementwise(&wb, "=MONTH(IF(ISNUMBER(A1:A3),A1:A3,0))"),
        vec![3.0, 3.0, 3.0]
    );
    // Row `Q11_day_if_array`: ARRAY entry (Ctrl+Shift+Enter) only.
    assert_eq!(
        cl087_elementwise(&wb, "=DAY(IF(ISNUMBER(A1:A3),A1:A3,0))"),
        vec![15.0, 16.0, 17.0]
    );
}

#[test]
fn cl087_date_parts_lift_through_a_let_local() {
    // Excel measured, 16.105.3, OT-198 receipt `g4d_excel_cse_probe.json`.
    // Entry mode is cited per assertion, and the LAST assertion is the one
    // where the two entry modes DISAGREE.
    let wb = cl087_workbook();
    // Rows `Q29_let_local_range_year_dynamic` (dynamic entry) and
    // `Q19_let_local_range_year_plain` (array entry): both modes agree.
    assert_eq!(
        cl087_elementwise(&wb, "=LET(r,A1:A3,YEAR(r))"),
        vec![2023.0, 2023.0, 2023.0]
    );
    // Row `Q22_let_local_range_month_plain`: ARRAY entry only.
    assert_eq!(
        cl087_elementwise(&wb, "=LET(r,A1:A3,MONTH(r))"),
        vec![3.0, 3.0, 3.0]
    );
    // Row `Q23_let_local_range_day_plain`: ARRAY entry only.
    assert_eq!(
        cl087_elementwise(&wb, "=LET(r,A1:A3,DAY(r))"),
        vec![15.0, 16.0, 17.0]
    );
    // The Knighthead producer shape. The two Excel entry modes DISAGREE here:
    // row `Q28_producer_shape_dynamic` (dynamic entry, Range.Formula2)
    // measured {2023;2023;2023} spilling three rows, while row
    // `Q20_producer_shape_plain` (Ctrl+Shift+Enter array entry) measured
    // #VALUE! in all three cells. The assertion pins the DYNAMIC-entry row,
    // because dynamic entry is the mode the Knighthead producer itself uses.
    // The array-entry answer is a declared residual, not a refutation.
    assert_eq!(
        cl087_elementwise(&wb, "=LET(r,IF(ISNUMBER(A1:A3),A1:A3,0),YEAR(r))"),
        vec![2023.0, 2023.0, 2023.0]
    );
}

#[test]
fn cl087_rounding_family_lifts_over_a_range() {
    let wb = cl087_workbook();
    let serials = vec![45000.0, 45001.0, 45002.0];
    // Excel measured, 16.105.3, OT-198 receipt `g4d_excel_cse_probe.json`
    // rows `Q35_roundup_range_dynamic` (dynamic entry) and `Q05_roundup_range`
    // (array entry): both modes agree.
    assert_eq!(cl087_elementwise(&wb, "=ROUNDUP(A1:A3,0)"), serials);
    // Excel measured, same receipt, row `Q34_round_range_dynamic`: DYNAMIC
    // entry only. Must not regress (ROUND lifted before CL-087 too).
    assert_eq!(cl087_elementwise(&wb, "=ROUND(A1:A3,0)"), serials);
    // Specification, same rounding family.
    assert_eq!(cl087_elementwise(&wb, "=ROUNDDOWN(A1:A3,0)"), serials);
    assert_eq!(cl087_elementwise(&wb, "=TRUNC(A1:A3)"), serials);
    assert_eq!(cl087_elementwise(&wb, "=INT(A1:A3)"), serials);
    // Specification.
    assert_eq!(
        cl087_elementwise(&wb, "=LET(r,A1:A3,ROUNDUP(r,0))"),
        serials
    );
}

#[test]
fn cl087_date_constructor_lifts_over_a_range() {
    // Specification: DATE function page, 1900 date system.
    let wb = cl087_workbook();
    let expected = vec![44197.0, 44562.0, 44927.0];
    assert_eq!(cl087_elementwise(&wb, "=DATE(C1:C3,1,1)"), expected);
    assert_eq!(
        cl087_elementwise(&wb, "=LET(r,C1:C3,DATE(r,1,1))"),
        expected
    );
}

#[test]
fn cl087_preexisting_scalar_lifting_does_not_regress() {
    let wb = cl087_workbook();
    let serials = vec![45000.0, 45001.0, 45002.0];
    assert_eq!(cl087_elementwise(&wb, "=ABS(A1:A3)"), serials);
    assert_eq!(cl087_elementwise(&wb, "=LEN(A1:A3)"), vec![5.0, 5.0, 5.0]);
    assert_eq!(
        cl087_elementwise(&wb, "=SQRT(A1:A3)"),
        vec![45000f64.sqrt(), 45001f64.sqrt(), 45002f64.sqrt()]
    );
    assert_eq!(cl087_elementwise(&wb, "=MOD(A1:A3,2)"), vec![0.0, 1.0, 0.0]);
    assert_eq!(
        cl087_elementwise(&wb, "=ISNUMBER(A1:A3)*1"),
        vec![1.0, 1.0, 1.0]
    );
    assert_eq!(
        cl087_elementwise(&wb, "=LET(r,IF(ISNUMBER(A1:A3),A1:A3,0),ABS(r))"),
        serials
    );
}

#[test]
fn cl087_aggregation_over_a_lifted_result() {
    let wb = cl087_workbook();
    // ES-008 aggregation shape.
    assert_eq!(cl087_scalar(&wb, "=SUM(YEAR(A1:A3))"), 6069.0);
    // SUM is a reducer and must NOT start lifting.
    assert_eq!(cl087_scalar(&wb, "=SUM(A1:A3)"), 135003.0);
}

#[test]
fn cl087_atp_date_offsets_do_not_lift_over_a_range() {
    // The Analysis-ToolPak lineage answers #VALUE! to a multi-cell range in a
    // scalar-shaped slot, with no spill. EDATE is a BEHAVIOUR CHANGE: it
    // lifted before CL-087. Provenance is cited per assertion; only the first
    // two are Excel-measured.
    let wb = cl087_workbook();
    // Excel measured, 16.105.3, OT-198 receipt `g4d_excel_cse_probe.json`
    // rows `Q32_eomonth_range_dynamic` (dynamic entry, 1 cell filled, so no
    // spill) and `Q04_eomonth_range` (array entry, #VALUE! in all three).
    cl087_scalar_error(&wb, "=EOMONTH(A1:A3,0)", ExcelErrorKind::Value);
    // Excel measured, same receipt, row `Q33_edate_range_dynamic`: DYNAMIC
    // entry only, 1 cell filled, so no spill. No array-entry row exists for
    // EDATE over a range.
    cl087_scalar_error(&wb, "=EDATE(A1:A3,0)", ExcelErrorKind::Value);
    // The LET-local shape is NOT asserted here. It used to be, extending the
    // measured EDATE(A1:A3,0) answer to `=LET(r,A1:A3,EDATE(r,0))` on the
    // reasoning that a LET local binds the range unchanged; on this fork it
    // does not. See `cl087_atp_date_offsets_through_a_let_local_is_cl085`
    // below, which PINS the measured behaviour and states why it diverges.
}

/// PINS a MEASURED divergence from desktop Excel that this change does not
/// cause and must not be read as endorsing.
///
/// MEASURED on this fork after the CL-087 array-value lift (GOD-286, fork
/// `lead/god286-cl087-array-lifting`):
///   `=LET(r,A1:A3,EDATE(r,0))`   -> {45000;45001;45002}  (LIFTS, no error)
///   `=LET(r,A1:A3,EOMONTH(r,0))` -> {45016;45016;45016}  (LIFTS, no error)
///
/// EXCEL EXPECTATION: `#VALUE!` for both, no spill. In desktop Excel a LET
/// range local keeps its reference-ness — `LET(r,A1:A3,YEAR(r))` behaves
/// exactly like `YEAR(A1:A3)` (OT-198 `g4d_excel_cse_probe.json`, row
/// `Q29_let_local_range_year_dynamic` vs row `Q27_year_range_dynamic`) — so
/// the ATP range-reference refusal Excel shows at rows Q32/Q33 would apply
/// through the local too.
///
/// CAUSE OF THE DIVERGENCE: it is NOT the array-value lift added here, and NOT
/// the range-reference refusal, which is enforced at every one of the
/// interpreter's five lifting sites. It is CL-085, the missing
/// reference-preserving LET binding: on this fork a LET local is materialised
/// as a VALUE (measured: `ISREF` on a range-bound local is FALSE), so the
/// refusal cannot see a reference to refuse and the argument presents as an
/// array value, which this change lifts by design. CL-085 is fixed on a
/// different candidate lineage (fork 6da35585) that is NOT in this round's
/// base; when that binding lands, these two formulas should return to
/// `#VALUE!` and this test is the one to update.
#[test]
fn cl087_atp_date_offsets_through_a_let_local_is_cl085() {
    let wb = cl087_workbook();
    assert_eq!(
        cl087_elementwise(&wb, "=LET(r,A1:A3,EDATE(r,0))"),
        vec![45000.0, 45001.0, 45002.0]
    );
    assert_eq!(
        cl087_elementwise(&wb, "=LET(r,A1:A3,EOMONTH(r,0))"),
        vec![45016.0, 45016.0, 45016.0]
    );
}

/// EDATE's SECOND slot (`months`) with a multi-cell range: the shape that
/// `array_lifting_scalar_family` asserted as a NUMBER until CL-087.
///
/// PROVENANCE, stated because this assertion CHANGED:
///  - the number it used to assert was authored in GOD-185 commit 727a92e5,
///    the same commit that created the function-name allowlist CL-087 retires,
///    and ES-008 records that allowlist as "unverified on an Excel oracle".
///    It was an author expectation, never a measurement.
///  - the measurement that exists is for EDATE's FIRST slot: desktop Excel
///    16.105.3 answers `#VALUE!` to `EDATE(A1:A3,0)` under dynamic-array entry
///    (OT-198 receipt `g4d_excel_cse_probe.json` row `Q33_edate_range_dynamic`),
///    the Analysis-ToolPak lineage resist that `EOMONTH` also shows (Q32).
///  - the months-slot shape itself has NO live-Excel measurement. GOD-286 could
///    not open Excel. Held-corpus exposure is zero AS A LOWER BOUND: of 272,791
///    EDATE-bearing cells across the 67 held workbooks, 0 put a multi-cell
///    range in either slot (receipt `g8b_edate_months_slot_census.json`). That
///    receipt states in its own `limits` field that it is a STATIC TEXT census
///    which does NOT count a range reaching EDATE through a LET/LAMBDA local, a
///    defined name or INDIRECT, "so this is a lower bound" -- and the sibling
///    test `cl087_atp_date_offsets_do_not_lift_over_a_range` asserts exactly
///    the LET-local shape, so that uncounted class is known to be reachable.
///    The census therefore bounds, but does not prove, the exposure. It is
///    carried as a declared residual for a later Excel oracle, not as a
///    settled answer.
#[test]
fn cl087_edate_months_slot_stops_lifting() {
    let wb = cl087_workbook();
    cl087_scalar_error(&wb, "=SUM(EDATE(B1,A1:A3))", ExcelErrorKind::Value);
}

#[test]
fn cl087_scalar_controls() {
    let wb = cl087_workbook();
    assert_eq!(cl087_scalar(&wb, "=YEAR(B1)"), 2023.0);
    assert_eq!(cl087_scalar(&wb, "=LET(r,B1,YEAR(r))"), 2023.0);
    cl087_scalar_error(&wb, "=YEAR(-1)", ExcelErrorKind::Num);
    cl087_scalar_error(&wb, "=IF(\"abc\",1,0)", ExcelErrorKind::Value);
    assert_eq!(cl087_scalar(&wb, "=IF(1,1,0)"), 1.0);
}

#[test]
fn cl087_lookup_family_lifted_positions_are_unchanged() {
    // Exactly the positions the retired name allowlist returned.
    assert_eq!(cl087_lifted_positions("XLOOKUP", 6), Some(vec![0]));
    assert_eq!(cl087_lifted_positions("XMATCH", 4), Some(vec![0]));
    assert_eq!(cl087_lifted_positions("MATCH", 3), Some(vec![0]));
    assert_eq!(cl087_lifted_positions("LOOKUP", 3), Some(vec![0]));
    assert_eq!(cl087_lifted_positions("INDEX", 3), Some(vec![1, 2]));
    assert_eq!(cl087_lifted_positions("VLOOKUP", 4), Some(vec![0, 2]));
    assert_eq!(cl087_lifted_positions("HLOOKUP", 4), Some(vec![0, 2]));
    assert_eq!(cl087_lifted_positions("IF", 3), Some(vec![0, 1, 2]));
    assert_eq!(cl087_lifted_positions("SWITCH", 4), Some(vec![0, 1]));
    assert_eq!(cl087_lifted_positions("SWITCH", 6), Some(vec![0, 1, 3]));

    // The scalar-shaped mode/flag slots are NOT lifted, and this round does
    // not change that: MATCH's match_type (index 2), VLOOKUP's range_lookup
    // (index 3), XLOOKUP's if_not_found/match_mode/search_mode (3, 4, 5).
    assert!(!cl087_lifted_positions("MATCH", 3).unwrap().contains(&2));
    assert!(!cl087_lifted_positions("VLOOKUP", 4).unwrap().contains(&3));
    let xlookup = cl087_lifted_positions("XLOOKUP", 6).unwrap();
    for slot in [3usize, 4, 5] {
        assert!(!xlookup.contains(&slot), "XLOOKUP slot {slot}");
    }

    // Reducers do not lift at all.
    assert_eq!(cl087_lifted_positions("SUM", 3), None);

    // The ATP date offsets: this assertion CHANGED in GOD-286's second
    // CL-087 commit. It read `None` for both while EDATE/EOMONTH carried no
    // element-wise declaration at all. They now declare `FnCaps::ELEMENTWISE`
    // (so both scalar slots lift over an ARRAY VALUE) PLUS the separate
    // refusal `elementwise_lift_refuses_range_reference`, which keeps the
    // Excel-measured `#VALUE!` over a live multi-cell RANGE REFERENCE
    // (OT-198 `g4d_excel_cse_probe.json` rows Q32/Q33). The measured refusal
    // is pinned by `cl087_atp_date_offsets_do_not_lift_over_a_range`; the
    // positions below only say which slots the lift covers.
    assert_eq!(cl087_lifted_positions("EDATE", 2), Some(vec![0, 1]));
    assert_eq!(cl087_lifted_positions("EOMONTH", 2), Some(vec![0, 1]));
    assert!(cl087_lift_refuses_range_reference("EDATE"));
    assert!(cl087_lift_refuses_range_reference("EOMONTH"));
    // Nothing else in the registry declares the refusal.
    for name in ["YEAR", "DATE", "ROUNDUP", "ABS", "XLOOKUP", "IF", "SUM"] {
        assert!(
            !cl087_lift_refuses_range_reference(name),
            "{name} must not declare the ATP range-reference refusal"
        );
    }
}

// ─────────── CL-087 / ES-008: the ATP pair lifts over an ARRAY VALUE ────────
//
// EDATE and EOMONTH now declare `FnCaps::ELEMENTWISE` AND
// `Function::elementwise_lift_refuses_range_reference`. That third state is
// "lifts element-wise over an array VALUE, refuses a live multi-cell RANGE
// REFERENCE and keeps its existing scalar-coercion `#VALUE!` there".
//
// Provenance of the two halves:
//  - the RANGE-REFERENCE refusal is Excel-measured (16.105.3, dynamic-array
//    entry): OT-198 receipt `g4d_excel_cse_probe.json` rows
//    `Q32_eomonth_range_dynamic` and `Q33_edate_range_dynamic`. It is pinned
//    by `cl087_atp_date_offsets_do_not_lift_over_a_range` above and MUST NOT
//    REGRESS.
//  - the ARRAY-VALUE lift has NO DIRECT clean-room Excel row. It rests on the
//    HELD-CALLER measurement: at the Knighthead producer cell EOMONTH receives
//    an array value (TYPE 64, 10 elements, `ISREF` FALSE) produced by DATE and
//    desktop Excel computes a NUMBER there (OT-198 g4c and GOD-286 receipt
//    `g3c_producer_bisect.json`).
//
// Invented data only, from `cl087_workbook()`: A1:A3 are the serials
// 45000/45001/45002 (2023-03-15/16/17), whose common month end 2023-03-31 is
// serial 45016. B1 is the scalar control 45000.

#[test]
fn cl087_atp_date_offsets_lift_over_an_array_value() {
    let wb = cl087_workbook();
    // IF(...) produces an array VALUE, not a range reference, so the refusal
    // does not fire and the pair lifts element-wise.
    assert_eq!(
        cl087_elementwise(&wb, "=EOMONTH(IF(ISNUMBER(A1:A3),A1:A3,0),0)"),
        vec![45016.0, 45016.0, 45016.0]
    );
    assert_eq!(
        cl087_elementwise(&wb, "=EDATE(IF(ISNUMBER(A1:A3),A1:A3,0),0)"),
        vec![45000.0, 45001.0, 45002.0]
    );
}

#[test]
fn cl087_atp_date_offsets_lift_over_an_inline_array_literal() {
    // An inline array literal is an array VALUE too: same lift, no refusal.
    let wb = cl087_workbook();
    assert_eq!(
        cl087_elementwise(&wb, "=EOMONTH({45000,45001},0)"),
        vec![45016.0, 45016.0]
    );
    assert_eq!(
        cl087_elementwise(&wb, "=EDATE({45000,45001},0)"),
        vec![45000.0, 45001.0]
    );
}

#[test]
fn cl087_atp_date_offsets_scalar_controls_are_unchanged() {
    // B1 is a SINGLE-CELL reference. A single-cell reference is not a
    // multi-cell range and must NOT trigger the refusal; it dispatches
    // scalar-wise and answers a plain number, exactly as before this change.
    let wb = cl087_workbook();
    assert_eq!(cl087_scalar(&wb, "=EOMONTH(B1,0)"), 45016.0);
    assert_eq!(cl087_scalar(&wb, "=EDATE(B1,0)"), 45000.0);
    // Literal scalars, the pre-existing path.
    assert_eq!(cl087_scalar(&wb, "=EOMONTH(45000,0)"), 45016.0);
    assert_eq!(cl087_scalar(&wb, "=EDATE(45000,0)"), 45000.0);
}

#[test]
fn cl087_single_cell_reference_does_not_trigger_the_range_refusal() {
    // The refusal is a property of a MULTI-CELL range reference only. A
    // single-cell reference resolves to (1,1) and lifts (a no-op lift), so a
    // one-cell range used in an aggregation still computes rather than
    // erroring, while the three-cell range still refuses.
    let wb = cl087_workbook();
    assert_eq!(cl087_scalar(&wb, "=SUM(EOMONTH(A1:A1,0))"), 45016.0);
    assert_eq!(cl087_scalar(&wb, "=SUM(EDATE(A1:A1,0))"), 45000.0);
    cl087_scalar_error(&wb, "=SUM(EOMONTH(A1:A3,0))", ExcelErrorKind::Value);
    cl087_scalar_error(&wb, "=SUM(EDATE(A1:A3,0))", ExcelErrorKind::Value);
}
