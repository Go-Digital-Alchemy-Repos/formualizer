#[cfg(test)]
mod tests {
    use crate::test_workbook::TestWorkbook;
    use formualizer_common::error::{ExcelError, ExcelErrorKind};
    use formualizer_parse::{LiteralValue, parser::Parser};
    use std::sync::Arc;

    fn workbook() -> TestWorkbook {
        TestWorkbook::new()
            .with_function(Arc::new(crate::builtins::lambda::LetFn))
            .with_function(Arc::new(crate::builtins::logical::AndFn))
            .with_function(Arc::new(crate::builtins::logical::IfFn))
            .with_function(Arc::new(crate::builtins::logical::OrFn))
            .with_function(Arc::new(crate::builtins::logical_ext::IfsFn))
            .with_function(Arc::new(crate::builtins::logical_ext::NotFn))
            .with_function(Arc::new(crate::builtins::logical_ext::XorFn))
            .with_function(Arc::new(crate::builtins::info::IsNumberFn))
            .with_function(Arc::new(crate::builtins::info::NaFn))
            .with_function(Arc::new(crate::builtins::lookup::SequenceFn))
            .with_function(Arc::new(crate::builtins::lookup::TransposeFn))
            .with_function(Arc::new(crate::builtins::math::numeric::MultinomialFn))
            .with_cell("Sheet1", 1, 1, LiteralValue::Int(1))
            .with_cell("Sheet1", 3, 1, LiteralValue::Int(3))
            .with_cell("Sheet1", 1, 4, LiteralValue::Text("x".into()))
            .with_cell("Sheet1", 2, 4, LiteralValue::Text("y".into()))
            .with_cell("Sheet1", 3, 4, LiteralValue::Text("z".into()))
            .with_cell("Sheet1", 4, 3, LiteralValue::Empty)
            .with_cell("Sheet1", 1, 5, LiteralValue::Int(3))
            .with_cell(
                "Sheet1",
                1,
                6,
                LiteralValue::Error(ExcelError::new(ExcelErrorKind::Na)),
            )
            .with_cell("Sheet1", 2, 6, LiteralValue::Int(1))
    }

    fn evaluate(formula: &str) -> Result<LiteralValue, ExcelError> {
        let mut parser = Parser::new(formula).expect("parser");
        let ast = parser
            .parse()
            .map_err(|error| ExcelError::new(ExcelErrorKind::Error).with_message(error.message))?;
        workbook()
            .interpreter()
            .evaluate_ast(&ast)
            .map(|value| value.into_literal())
    }

    fn assert_error(formula: &str, expected: ExcelErrorKind) {
        let error = match evaluate(formula) {
            Ok(LiteralValue::Error(error)) | Err(error) => error,
            Ok(other) => panic!("{formula}: expected {expected:?}, got {other:?}"),
        };
        assert_eq!(error.kind, expected, "{formula}");
    }

    macro_rules! value_case {
        ($name:ident, $formula:literal, $expected:expr) => {
            #[test]
            fn $name() {
                assert_eq!(evaluate($formula).unwrap(), $expected, "{}", $formula);
            }
        };
    }

    macro_rules! error_case {
        ($name:ident, $formula:literal, $kind:ident) => {
            #[test]
            fn $name() {
                assert_error($formula, ExcelErrorKind::$kind);
            }
        };
    }

    value_case!(
        god187ah_let_or_false,
        "=LET(p,FALSE,OR(p))",
        LiteralValue::Boolean(false)
    );
    value_case!(
        god187ah_let_and_true,
        "=LET(p,TRUE,AND(p))",
        LiteralValue::Boolean(true)
    );
    value_case!(
        god187ah_let_and_false,
        "=LET(p,FALSE,AND(p))",
        LiteralValue::Boolean(false)
    );
    value_case!(
        god187ah_let_or_num,
        "=LET(p,1,OR(p))",
        LiteralValue::Boolean(true)
    );
    value_case!(
        god187ah_let_or_after_falsy,
        "=LET(p,FALSE,OR(FALSE,p))",
        LiteralValue::Boolean(false)
    );
    value_case!(
        god187ah_let_multi_local,
        "=LET(a,1,b,2,AND(a,b))",
        LiteralValue::Boolean(true)
    );
    value_case!(
        god187ah_let_range_local_cmp,
        "=LET(p,A1:A3,OR(p=3))",
        LiteralValue::Boolean(true)
    );
    error_case!(god187ah_let_blank_local, "=LET(p,C4,OR(p))", Value);
    value_case!(
        god187ah_let_not_control,
        "=LET(p,FALSE,NOT(p))",
        LiteralValue::Boolean(true)
    );

    value_case!(
        god187ah_or_cell_vs_arrlit,
        "=OR(E1={3,8,12})",
        LiteralValue::Boolean(true)
    );
    value_case!(
        god187ah_or_cell_vs_arrlit_neg,
        "=OR(E1={4,8,12})",
        LiteralValue::Boolean(false)
    );
    value_case!(
        god187ah_and_cell_vs_arrlit,
        "=AND(E1={3,8,12})",
        LiteralValue::Boolean(false)
    );
    value_case!(
        god187ah_and_cell_vs_arrlit_all,
        "=AND(E1={3,3,3})",
        LiteralValue::Boolean(true)
    );
    value_case!(
        god187ah_or_range_cmp,
        "=OR(A1:A3=3)",
        LiteralValue::Boolean(true)
    );
    value_case!(
        god187ah_or_range_cmp_neg,
        "=OR(A1:A3>5)",
        LiteralValue::Boolean(false)
    );
    value_case!(
        god187ah_and_range_cmp,
        "=AND(A1:A3>0)",
        LiteralValue::Boolean(false)
    );
    value_case!(
        god187ah_or_sequence,
        "=OR(SEQUENCE(3))",
        LiteralValue::Boolean(true)
    );
    value_case!(
        god187ah_and_sequence,
        "=AND(SEQUENCE(3))",
        LiteralValue::Boolean(true)
    );
    value_case!(
        god187ah_or_if_computed,
        "=OR(IF(A1:A3=3,TRUE,FALSE))",
        LiteralValue::Boolean(true)
    );
    value_case!(
        god187ah_or_isnumber,
        "=OR(ISNUMBER(A1:A3))",
        LiteralValue::Boolean(true)
    );
    value_case!(
        god187ah_or_transpose,
        "=OR(TRANSPOSE(E1={3,8,12}))",
        LiteralValue::Boolean(true)
    );
    value_case!(
        god187ah_or_arith_arr,
        "=OR(E1+{0,1,2}=3)",
        LiteralValue::Boolean(true)
    );
    value_case!(
        god187ah_or_doubleneg,
        "=OR(--(E1={3,8,12}))",
        LiteralValue::Boolean(true)
    );
    value_case!(
        god187ah_multinomial_sequence,
        "=MULTINOMIAL(SEQUENCE(3))",
        LiteralValue::Number(60.0)
    );
    value_case!(
        god187ah_multinomial_range_blank,
        "=MULTINOMIAL(A1:A3)",
        LiteralValue::Number(4.0)
    );
    value_case!(
        god187ah_multinomial_arrlit,
        "=MULTINOMIAL({1,2,3})",
        LiteralValue::Number(60.0)
    );
    error_case!(
        god187ah_multinomial_bool_reject,
        "=MULTINOMIAL(A1:A3=3)",
        Value
    );
    value_case!(
        god187ah_ifs_or_shape_ot059,
        "=IFS(OR(E1={3,8,12}),1,TRUE,0)",
        LiteralValue::Number(1.0)
    );
    value_case!(
        god187ah_xor_control,
        "=XOR(E1={3,8,12})",
        LiteralValue::Boolean(true)
    );

    value_case!(
        god187ah_and_range_blank_ignored,
        "=AND(A1:A3)",
        LiteralValue::Boolean(true)
    );
    value_case!(
        god187ah_or_range_plain,
        "=OR(A1:A3)",
        LiteralValue::Boolean(true)
    );
    error_case!(god187ah_or_range_blank_only, "=OR(A2:A2)", Value);
    error_case!(god187ah_and_range_blank_only, "=AND(A2:A2)", Value);
    error_case!(god187ah_or_text_range_only, "=OR(D1:D3)", Value);
    error_case!(god187ah_and_text_range_only, "=AND(D1:D3)", Value);
    value_case!(
        god187ah_or_mixed_text_range,
        "=OR(A1:A3,D1:D3)",
        LiteralValue::Boolean(true)
    );
    value_case!(
        god187ah_or_arrlit_text_ignored,
        "=OR({0,\"x\"})",
        LiteralValue::Boolean(false)
    );
    error_case!(god187ah_or_direct_text, "=OR(\"x\")", Value);
    value_case!(
        god187ah_or_direct_text_with_true,
        "=OR(TRUE,\"x\")",
        LiteralValue::Boolean(true)
    );
    value_case!(
        god187ah_or_arrlit_01,
        "=OR(FALSE,{0,0,1})",
        LiteralValue::Boolean(true)
    );
    error_case!(god187ah_or_error_propagates, "=OR(TRUE,NA())", Na);
    error_case!(god187ah_and_error_direct, "=AND(FALSE,NA())", Na);
    error_case!(god187ah_or_error_range, "=OR(F1:F2)", Na);
    error_case!(god187ah_and_error_range, "=AND(F1:F2)", Na);
    error_case!(god187ah_or_error_computed, "=OR((F1:F2)=1)", Na);
    error_case!(god187ah_and_error_computed, "=AND((F1:F2)=1)", Na);

    value_case!(
        god187ah_let_array_local_or,
        "=LET(p,A1:A3=3,OR(p))",
        LiteralValue::Boolean(true)
    );
    value_case!(
        god187ah_let_arrlit_local_or,
        "=LET(p,E1={3,8,12},OR(p))",
        LiteralValue::Boolean(true)
    );
    error_case!(god187ah_and_direct_text, "=AND(\"x\")", Value);
    value_case!(
        god187ah_and_direct_text_with_true,
        "=AND(TRUE,\"x\")",
        LiteralValue::Boolean(true)
    );
    value_case!(
        god187ah_and_direct_text_with_false,
        "=AND(FALSE,\"x\")",
        LiteralValue::Boolean(false)
    );
    value_case!(
        god187ah_nested_let,
        "=LET(a,1,LET(b,2,OR(a,b)))",
        LiteralValue::Boolean(true)
    );
    value_case!(
        god187ah_or_two_computed,
        "=OR(A1:A3=3,A1:A3>5)",
        LiteralValue::Boolean(true)
    );
    value_case!(
        god187ah_or_1x1_computed,
        "=OR(A1:A1=1)",
        LiteralValue::Boolean(true)
    );
    error_case!(
        god187ah_multinomial_neg_computed,
        "=MULTINOMIAL(SEQUENCE(3)-2)",
        Num
    );
    error_case!(god187ah_or_empty_args, "=OR()", Value);
    error_case!(god187ah_and_empty_args, "=AND()", Value);

    error_case!(god187ah_concat_na_right, "=NA()&\"x\"", Na);
    error_case!(god187ah_concat_na_left, "=\"a\"&NA()", Na);
    error_case!(god187ah_concat_div0, "=1/0&\"x\"", Div);
    error_case!(god187ah_concat_error_left_precedence, "=NA()&1/0", Na);
    error_case!(god187ah_div0_control, "=1/0", Div);
    value_case!(
        god187ah_concat_control,
        "=\"a\"&\"b\"",
        LiteralValue::Text("ab".into())
    );
}
