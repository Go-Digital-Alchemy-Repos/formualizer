//! Comprehensive tests for text functions

#[cfg(test)]
mod tests {
    use crate::builtins::text::*;
    use crate::test_workbook::TestWorkbook;
    use crate::traits::ArgumentHandle;
    use formualizer_common::LiteralValue;
    use formualizer_parse::parser::{ASTNode, ASTNodeType};
    use std::sync::Arc;

    fn lit(v: LiteralValue) -> ASTNode {
        ASTNode::new(ASTNodeType::Literal(v), None)
    }

    #[test]
    fn test_len_edge_cases() {
        let wb = TestWorkbook::new().with_function(Arc::new(LenFn));
        let ctx = wb.interpreter();
        let f = ctx.context.get_function("", "LEN").unwrap();

        // Empty string
        let empty = lit(LiteralValue::Text("".into()));
        assert_eq!(
            f.dispatch(
                &[ArgumentHandle::new(&empty, &ctx)],
                &ctx.function_context(None)
            )
            .unwrap(),
            LiteralValue::Int(0)
        );

        // Number converted to text
        let num = lit(LiteralValue::Number(123.45));
        assert_eq!(
            f.dispatch(
                &[ArgumentHandle::new(&num, &ctx)],
                &ctx.function_context(None)
            )
            .unwrap(),
            LiteralValue::Int(6) // "123.45"
        );

        // Boolean
        let bool_val = lit(LiteralValue::Boolean(true));
        assert_eq!(
            f.dispatch(
                &[ArgumentHandle::new(&bool_val, &ctx)],
                &ctx.function_context(None)
            )
            .unwrap(),
            LiteralValue::Int(4) // "TRUE"
        );
    }

    #[test]
    fn test_left_right_edge_cases() {
        let wb = TestWorkbook::new()
            .with_function(Arc::new(LeftFn))
            .with_function(Arc::new(RightFn));
        let ctx = wb.interpreter();
        let left = ctx.context.get_function("", "LEFT").unwrap();
        let right = ctx.context.get_function("", "RIGHT").unwrap();

        let text = lit(LiteralValue::Text("hello".into()));

        // LEFT with 0 characters
        let zero = lit(LiteralValue::Int(0));
        assert_eq!(
            left.dispatch(
                &[
                    ArgumentHandle::new(&text, &ctx),
                    ArgumentHandle::new(&zero, &ctx)
                ],
                &ctx.function_context(None)
            )
            .unwrap(),
            LiteralValue::Text("".into())
        );

        // LEFT with more than string length
        let large = lit(LiteralValue::Int(100));
        assert_eq!(
            left.dispatch(
                &[
                    ArgumentHandle::new(&text, &ctx),
                    ArgumentHandle::new(&large, &ctx)
                ],
                &ctx.function_context(None)
            )
            .unwrap(),
            LiteralValue::Text("hello".into())
        );

        // RIGHT with negative should return #VALUE!
        let neg = lit(LiteralValue::Int(-1));
        match right
            .dispatch(
                &[
                    ArgumentHandle::new(&text, &ctx),
                    ArgumentHandle::new(&neg, &ctx),
                ],
                &ctx.function_context(None),
            )
            .unwrap()
            .into_literal()
        {
            LiteralValue::Error(e) => assert_eq!(e.to_string(), "#VALUE!"),
            _ => panic!("Expected #VALUE! error"),
        }
    }

    #[test]
    fn test_mid_boundaries() {
        let wb = TestWorkbook::new().with_function(Arc::new(MidFn));
        let ctx = wb.interpreter();
        let f = ctx.context.get_function("", "MID").unwrap();

        let text = lit(LiteralValue::Text("abcdef".into()));

        // MID with start < 1 should return #VALUE!
        let start_zero = lit(LiteralValue::Int(0));
        let count = lit(LiteralValue::Int(3));
        match f
            .dispatch(
                &[
                    ArgumentHandle::new(&text, &ctx),
                    ArgumentHandle::new(&start_zero, &ctx),
                    ArgumentHandle::new(&count, &ctx),
                ],
                &ctx.function_context(None),
            )
            .unwrap()
            .into_literal()
        {
            LiteralValue::Error(e) => assert_eq!(e.to_string(), "#VALUE!"),
            _ => panic!("Expected #VALUE! error"),
        }

        // MID with start > length should return empty
        let start_large = lit(LiteralValue::Int(100));
        assert_eq!(
            f.dispatch(
                &[
                    ArgumentHandle::new(&text, &ctx),
                    ArgumentHandle::new(&start_large, &ctx),
                    ArgumentHandle::new(&count, &ctx)
                ],
                &ctx.function_context(None)
            )
            .unwrap(),
            LiteralValue::Text("".into())
        );

        // MID with large count clips at end
        let start = lit(LiteralValue::Int(4));
        let large_count = lit(LiteralValue::Int(100));
        assert_eq!(
            f.dispatch(
                &[
                    ArgumentHandle::new(&text, &ctx),
                    ArgumentHandle::new(&start, &ctx),
                    ArgumentHandle::new(&large_count, &ctx)
                ],
                &ctx.function_context(None)
            )
            .unwrap(),
            LiteralValue::Text("def".into())
        );
    }

    #[test]
    fn test_find_search_differences() {
        let wb = TestWorkbook::new()
            .with_function(Arc::new(FindFn))
            .with_function(Arc::new(SearchFn));
        let ctx = wb.interpreter();
        let find = ctx.context.get_function("", "FIND").unwrap();
        let search = ctx.context.get_function("", "SEARCH").unwrap();

        // FIND is case-sensitive, SEARCH is not
        let needle = lit(LiteralValue::Text("B".into()));
        let haystack = lit(LiteralValue::Text("abc".into()));

        // FIND should not find "B" in "abc"
        match find
            .dispatch(
                &[
                    ArgumentHandle::new(&needle, &ctx),
                    ArgumentHandle::new(&haystack, &ctx),
                ],
                &ctx.function_context(None),
            )
            .unwrap()
            .into_literal()
        {
            LiteralValue::Error(e) => assert_eq!(e.to_string(), "#VALUE!"),
            _ => panic!("Expected #VALUE! error"),
        }

        // SEARCH should find "B" in "abc" (case-insensitive)
        assert_eq!(
            search
                .dispatch(
                    &[
                        ArgumentHandle::new(&needle, &ctx),
                        ArgumentHandle::new(&haystack, &ctx)
                    ],
                    &ctx.function_context(None)
                )
                .unwrap(),
            LiteralValue::Int(2) // 1-based index
        );
    }

    #[test]
    fn test_substitute_occurrences() {
        let wb = TestWorkbook::new().with_function(Arc::new(SubstituteFn));
        let ctx = wb.interpreter();
        let f = ctx.context.get_function("", "SUBSTITUTE").unwrap();

        let text = lit(LiteralValue::Text("aaa bbb aaa".into()));
        let old = lit(LiteralValue::Text("aaa".into()));
        let new = lit(LiteralValue::Text("ccc".into()));

        // Replace all occurrences (no instance_num)
        assert_eq!(
            f.dispatch(
                &[
                    ArgumentHandle::new(&text, &ctx),
                    ArgumentHandle::new(&old, &ctx),
                    ArgumentHandle::new(&new, &ctx)
                ],
                &ctx.function_context(None)
            )
            .unwrap(),
            LiteralValue::Text("ccc bbb ccc".into())
        );

        // Replace only first occurrence
        let instance = lit(LiteralValue::Int(1));
        assert_eq!(
            f.dispatch(
                &[
                    ArgumentHandle::new(&text, &ctx),
                    ArgumentHandle::new(&old, &ctx),
                    ArgumentHandle::new(&new, &ctx),
                    ArgumentHandle::new(&instance, &ctx)
                ],
                &ctx.function_context(None)
            )
            .unwrap(),
            LiteralValue::Text("ccc bbb aaa".into())
        );

        // Instance number > occurrences returns original
        let large_instance = lit(LiteralValue::Int(10));
        assert_eq!(
            f.dispatch(
                &[
                    ArgumentHandle::new(&text, &ctx),
                    ArgumentHandle::new(&old, &ctx),
                    ArgumentHandle::new(&new, &ctx),
                    ArgumentHandle::new(&large_instance, &ctx)
                ],
                &ctx.function_context(None)
            )
            .unwrap(),
            LiteralValue::Text("aaa bbb aaa".into())
        );
    }

    #[test]
    fn test_trim_edge_cases() {
        let wb = TestWorkbook::new().with_function(Arc::new(TrimFn));
        let ctx = wb.interpreter();
        let f = ctx.context.get_function("", "TRIM").unwrap();

        // Multiple spaces between words
        let text = lit(LiteralValue::Text("  hello    world  ".into()));
        assert_eq!(
            f.dispatch(
                &[ArgumentHandle::new(&text, &ctx)],
                &ctx.function_context(None)
            )
            .unwrap(),
            LiteralValue::Text("hello world".into())
        );

        // Only spaces
        let spaces = lit(LiteralValue::Text("     ".into()));
        assert_eq!(
            f.dispatch(
                &[ArgumentHandle::new(&spaces, &ctx)],
                &ctx.function_context(None)
            )
            .unwrap(),
            LiteralValue::Text("".into())
        );

        // Empty string
        let empty = lit(LiteralValue::Text("".into()));
        assert_eq!(
            f.dispatch(
                &[ArgumentHandle::new(&empty, &ctx)],
                &ctx.function_context(None)
            )
            .unwrap(),
            LiteralValue::Text("".into())
        );
    }

    #[test]
    fn test_proper_case() {
        let wb = TestWorkbook::new().with_function(Arc::new(ProperFn));
        let ctx = wb.interpreter();
        let f = ctx.context.get_function("", "PROPER").unwrap();

        // Basic proper case
        let text = lit(LiteralValue::Text("hello world".into()));
        assert_eq!(
            f.dispatch(
                &[ArgumentHandle::new(&text, &ctx)],
                &ctx.function_context(None)
            )
            .unwrap(),
            LiteralValue::Text("Hello World".into())
        );

        // Mixed case input
        let mixed = lit(LiteralValue::Text("hELLo WoRLd".into()));
        assert_eq!(
            f.dispatch(
                &[ArgumentHandle::new(&mixed, &ctx)],
                &ctx.function_context(None)
            )
            .unwrap(),
            LiteralValue::Text("Hello World".into())
        );

        // Numbers and punctuation
        let punct = lit(LiteralValue::Text("it's 123-test".into()));
        assert_eq!(
            f.dispatch(
                &[ArgumentHandle::new(&punct, &ctx)],
                &ctx.function_context(None)
            )
            .unwrap(),
            LiteralValue::Text("It'S 123-Test".into())
        );
    }

    #[test]
    fn test_exact_comparison() {
        let wb = TestWorkbook::new().with_function(Arc::new(ExactFn));
        let ctx = wb.interpreter();
        let f = ctx.context.get_function("", "EXACT").unwrap();

        // Case sensitive match
        let a = lit(LiteralValue::Text("Hello".into()));
        let b = lit(LiteralValue::Text("Hello".into()));
        assert_eq!(
            f.dispatch(
                &[ArgumentHandle::new(&a, &ctx), ArgumentHandle::new(&b, &ctx)],
                &ctx.function_context(None)
            )
            .unwrap(),
            LiteralValue::Boolean(true)
        );

        // Case sensitive mismatch
        let c = lit(LiteralValue::Text("hello".into()));
        assert_eq!(
            f.dispatch(
                &[ArgumentHandle::new(&a, &ctx), ArgumentHandle::new(&c, &ctx)],
                &ctx.function_context(None)
            )
            .unwrap(),
            LiteralValue::Boolean(false)
        );
    }

    #[test]
    fn test_value_parsing() {
        let wb = TestWorkbook::new().with_function(Arc::new(ValueFn));
        let ctx = wb.interpreter();
        let f = ctx.context.get_function("", "VALUE").unwrap();

        // Scientific notation
        let sci = lit(LiteralValue::Text("1.23E+2".into()));
        assert_eq!(
            f.dispatch(
                &[ArgumentHandle::new(&sci, &ctx)],
                &ctx.function_context(None)
            )
            .unwrap(),
            LiteralValue::Number(123.0)
        );

        // Leading/trailing spaces
        let spaces = lit(LiteralValue::Text("  42.5  ".into()));
        assert_eq!(
            f.dispatch(
                &[ArgumentHandle::new(&spaces, &ctx)],
                &ctx.function_context(None)
            )
            .unwrap(),
            LiteralValue::Number(42.5)
        );

        // Invalid text returns #VALUE!
        let invalid = lit(LiteralValue::Text("not a number".into()));
        match f
            .dispatch(
                &[ArgumentHandle::new(&invalid, &ctx)],
                &ctx.function_context(None),
            )
            .unwrap()
            .into_literal()
        {
            LiteralValue::Error(e) => assert_eq!(e.to_string(), "#VALUE!"),
            _ => panic!("Expected #VALUE! error"),
        }

        // Locale-dependent numeric text should not be silently misparsed
        let comma_decimal = lit(LiteralValue::Text("1.234,56".into()));
        match f
            .dispatch(
                &[ArgumentHandle::new(&comma_decimal, &ctx)],
                &ctx.function_context(None),
            )
            .unwrap()
            .into_literal()
        {
            LiteralValue::Error(e) => assert_eq!(e.to_string(), "#VALUE!"),
            other => panic!("Expected #VALUE! error, got {other:?}"),
        }
    }

    fn eval_text_formula(system: crate::engine::DateSystem, formula: &str) -> LiteralValue {
        use crate::engine::{Engine, EvalConfig};
        use crate::interpreter::Interpreter;
        use formualizer_parse::parser::parse;

        let wb = TestWorkbook::new().with_function(Arc::new(TextFn));
        let engine = Engine::new(wb, EvalConfig::default().with_date_system(system));
        let interpreter = Interpreter::new(&engine, "Sheet1");
        interpreter
            .evaluate_ast(&parse(formula).expect("formula should parse"))
            .expect("formula should evaluate")
            .into_literal()
    }

    fn assert_value_error(value: LiteralValue) {
        match value {
            LiteralValue::Error(error) => assert_eq!(error.to_string(), "#VALUE!"),
            other => panic!("expected #VALUE!, got {other:?}"),
        }
    }

    #[test]
    fn text_ot076_excel_numeric_format_oracle() {
        use crate::engine::DateSystem;

        let cases = [
            (r##"=TEXT(0.09,"0.00%")"##, "9.00%"),
            (r##"=TEXT(0.095,"0.00%")"##, "9.50%"),
            (r##"=TEXT(0.0975,"0.00%")"##, "9.75%"),
            (r##"=TEXT(0.13,"0.00%")"##, "13.00%"),
            (r##"=TEXT(0.7,"0.00%")"##, "70.00%"),
            (r##"=TEXT(1,"0.00%")"##, "100.00%"),
            (r##"=TEXT(0.09,"0.00%"&" ")"##, "9.00% "),
            (
                r##"="S&P 500 with "&TEXT(0.09,"0.00%"&" ")&"Cap""##,
                "S&P 500 with 9.00% Cap",
            ),
            (r##"=TEXT(0.09754,"0.00%")"##, "9.75%"),
            (r##"=TEXT(0.09756,"0.00%")"##, "9.76%"),
            (r##"=TEXT(-0.0975,"0.00%")"##, "-9.75%"),
            (r##"=TEXT(0,"0.00%")"##, "0.00%"),
            (r##"=TEXT(0.09,"0.000")"##, "0.090"),
            (r##"=TEXT(1.23456,"0.000")"##, "1.235"),
            (r##"=TEXT(7,"0000")"##, "0007"),
            (r##"=TEXT(12345.6,"#,##0.00")"##, "12,345.60"),
            (
                r##"=TEXT(0.09,"0.00% "&CHAR(34)&"Cap"&CHAR(34))"##,
                "9.00% Cap",
            ),
            (
                r##"=TEXT(0.09,CHAR(34)&"Cap "&CHAR(34)&"0.00%")"##,
                "Cap 9.00%",
            ),
            (r##"=TEXT(0.09,"0.00\%")"##, "0.09%"),
            (
                r##"=TEXT(-1234.5,"#,##0.00;[Red](#,##0.00)")"##,
                "(1,234.50)",
            ),
            (
                r##"=TEXT(0,"0.00;[Red]-0.00;"&CHAR(34)&"zero"&CHAR(34))"##,
                "zero",
            ),
            (r##"=TEXT(DATE(2024,2,29),"yyyy-mm-dd")"##, "2024-02-29"),
            (r##"=TEXT(1.225,"0.00")"##, "1.23"),
            (r##"=TEXT(0,"#")"##, ""),
            (
                r##"=TEXT(0.004,"0.00;[Red]-0.00;"&CHAR(34)&"zero"&CHAR(34))"##,
                "0.00",
            ),
            (r##"=TEXT(7,"0"&CHAR(34)&";units"&CHAR(34))"##, "7;units"),
            (r##"=TEXT(12200000,"0.0,,")"##, "12.2"),
            (r##"=TEXT(999.999,"#,##0.00")"##, "1,000.00"),
            (r##"=TEXT(0.09,"0.00"&CHAR(34)&"%"&CHAR(34))"##, "0.09%"),
            (r##"=TEXT(0.09,"0.00% Cap")"##, "9.00% Cap"),
        ];

        for (formula, expected) in cases {
            assert_eq!(
                eval_text_formula(DateSystem::Excel1900, formula),
                LiteralValue::Text(expected.into()),
                "{formula}"
            );
        }
    }

    #[test]
    fn text_ot076_unsupported_formats_hold_legacy_results() {
        use crate::engine::DateSystem;

        let cases = [
            // GOD-227 oracle row `b_ot076_A23`
            // (artifacts/private/god227/probe/excel-text-date-probe.json,
            // Excel for Mac 16.105.3, en_US, 2026-09-02): `h:mm AM/PM` is now
            // inside the measured date surface and renders `1:05 PM`. The
            // string pinned here before was the pre-fix echo of the format.
            (r##"=TEXT(TIME(13,5,0),"h:mm AM/PM")"##, "1:05 PM"),
            (r##"=TEXT(12345,"0.00E+00")"##, "12345.00"),
            (r##"=TEXT(1.25,"# ?/?")"##, "1.25"),
        ];
        for (formula, expected) in cases {
            assert_eq!(
                eval_text_formula(DateSystem::Excel1900, formula),
                LiteralValue::Text(expected.into()),
                "{formula}"
            );
        }
    }

    /// OT-080 Excel oracle, tier 1 (Excel for Mac 16.105.3 build
    /// 16.105.26020123, `en_US`, saved-XML readback, 2026-09-01): every
    /// huge-magnitude probe is `#VALUE!`. The accepted OT-076 wheel returned
    /// `inf`, `-inf`, `inf%`, or a 300-digit expansion for these.
    #[test]
    fn text_ot080_huge_magnitudes_are_value_errors() {
        use crate::engine::DateSystem;

        for formula in [
            r##"=TEXT(1E+307,"0.00")"##,
            r##"=TEXT(-1E+307,"0.00")"##,
            r##"=TEXT(1E+307,"#,##0.00")"##,
            r##"=TEXT(1E+307,"0")"##,
            r##"=TEXT(9.9E+306,"0.00")"##,
            r##"=TEXT(1.7E+306,"0.00")"##,
            r##"=TEXT(1E+307,"0.00%")"##,
        ] {
            assert_value_error(eval_text_formula(DateSystem::Excel1900, formula));
        }
    }

    /// OT-080 Excel oracle, tier 2: an unquoted `m` beside numeric placeholders
    /// is a temporal code and yields `#VALUE!`; a bare `m` is the unpadded
    /// month; quoted and backslash-escaped `m` stay literal; `mm` pads.
    #[test]
    fn text_ot080_month_code_oracle() {
        use crate::engine::DateSystem;

        for formula in [
            r##"=TEXT(1,"0m")"##,
            r##"=TEXT(45000,"0.00m")"##,
            r##"=TEXT(5,"0 mm")"##,
        ] {
            assert_value_error(eval_text_formula(DateSystem::Excel1900, formula));
        }
        let cases = [
            (r##"=TEXT(45000,"m")"##, "3"),
            (r##"=TEXT(1,"0""m""")"##, "1m"),
            (r##"=TEXT(1,"0\m")"##, "1m"),
            (r##"=TEXT(45000,"mm")"##, "03"),
        ];
        for (formula, expected) in cases {
            assert_eq!(
                eval_text_formula(DateSystem::Excel1900, formula),
                LiteralValue::Text(expected.into()),
                "{formula}"
            );
        }
    }

    /// OT-080 Excel oracle, tier 3: `TEXT` rounds the 15-significant-digit
    /// decimal view half away from zero. `1.005` and `0.145` sit below the
    /// binary64 midpoint yet round up in Excel; the accepted OT-076 wheel
    /// returned `1.00` and `0.14`.
    #[test]
    fn text_ot080_midpoint_display_rounding_oracle() {
        use crate::engine::DateSystem;

        let cases = [
            (r##"=TEXT(1.005,"0.00")"##, "1.01"),
            (r##"=TEXT(0.145,"0.00")"##, "0.15"),
            (r##"=TEXT(2.675,"0.00")"##, "2.68"),
            (r##"=TEXT(44821.875,"0.00")"##, "44821.88"),
            (r##"=TEXT(8.835,"0.00")"##, "8.84"),
            (r##"=TEXT(1.5,"0")"##, "2"),
            (r##"=TEXT(2.5,"0")"##, "3"),
        ];
        for (formula, expected) in cases {
            assert_eq!(
                eval_text_formula(DateSystem::Excel1900, formula),
                LiteralValue::Text(expected.into()),
                "{formula}"
            );
        }
    }

    #[test]
    fn text_formats_typed_date_with_uppercase_unpadded_tokens() {
        use chrono::NaiveDate;

        let wb = TestWorkbook::new().with_function(Arc::new(TextFn));
        let ctx = wb.interpreter();
        let function = ctx.context.get_function("", "TEXT").unwrap();
        let date = lit(LiteralValue::Date(
            NaiveDate::from_ymd_opt(2031, 7, 4).unwrap(),
        ));
        let format = lit(LiteralValue::Text("M/D/YYYY".into()));
        let result = function
            .dispatch(
                &[
                    ArgumentHandle::new(&date, &ctx),
                    ArgumentHandle::new(&format, &ctx),
                ],
                &ctx.function_context(None),
            )
            .unwrap()
            .into_literal();

        match result {
            LiteralValue::Text(text) => assert_eq!(text.as_bytes(), b"7/4/2031"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn test_text_date_serial_boundaries_1900_formula_level() {
        use crate::engine::DateSystem;

        let cases = [
            (0.0, "1900-01-00"),
            (59.0, "1900-02-28"),
            (60.0, "1900-02-29"),
            (61.0, "1900-03-01"),
            (45306.0, "2024-01-15"),
            (2958465.0, "9999-12-31"),
        ];
        for (serial, expected) in cases {
            assert_eq!(
                eval_text_formula(
                    DateSystem::Excel1900,
                    &format!("=TEXT({serial},\"yyyy-mm-dd\")")
                ),
                LiteralValue::Text(expected.into()),
                "serial {serial}"
            );
        }

        for serial in [-1.0, -0.25, 2958466.0, 1.0e20] {
            assert_value_error(eval_text_formula(
                DateSystem::Excel1900,
                &format!("=TEXT({serial},\"yyyy-mm-dd\")"),
            ));
        }
        assert_value_error(eval_text_formula(
            DateSystem::Excel1900,
            "=TEXT(1E309,\"yyyy-mm-dd\")",
        ));
    }

    #[test]
    fn test_text_date_serial_boundaries_1904_formula_level() {
        use crate::engine::DateSystem;

        let cases = [
            (0.0, "1904-01-01"),
            (59.0, "1904-02-29"),
            (60.0, "1904-03-01"),
            (61.0, "1904-03-02"),
            (43844.0, "2024-01-15"),
            (2957003.0, "9999-12-31"),
        ];
        for (serial, expected) in cases {
            assert_eq!(
                eval_text_formula(
                    DateSystem::Excel1904,
                    &format!("=TEXT({serial},\"yyyy-mm-dd\")")
                ),
                LiteralValue::Text(expected.into()),
                "serial {serial}"
            );
        }

        for serial in [-1.0, -0.25, 2957004.0, 1.0e20] {
            assert_value_error(eval_text_formula(
                DateSystem::Excel1904,
                &format!("=TEXT({serial},\"yyyy-mm-dd\")"),
            ));
        }
        assert_value_error(eval_text_formula(
            DateSystem::Excel1904,
            "=TEXT(1E309,\"yyyy-mm-dd\")",
        ));
    }

    #[test]
    fn test_text_date_fraction_for_direct_modern_serials() {
        use crate::engine::DateSystem;

        for (system, serial) in [
            (DateSystem::Excel1900, 45306.5),
            (DateSystem::Excel1904, 43844.5),
        ] {
            assert_eq!(
                eval_text_formula(system, &format!("=TEXT({serial},\"yyyy-mm-dd hh:mm\")")),
                LiteralValue::Text("2024-01-15 12:00".into())
            );
        }
    }

    #[test]
    fn test_text_hh_mm_rounding_carries_the_displayed_day() {
        use crate::engine::DateSystem;

        let cases_1900 = [
            (59.9997, "1900-02-29 00:00"),
            (60.9997, "1900-03-01 00:00"),
            (61.9997, "1900-03-02 00:00"),
        ];
        for (serial, expected) in cases_1900 {
            assert_eq!(
                eval_text_formula(
                    DateSystem::Excel1900,
                    &format!("=TEXT({serial},\"yyyy-mm-dd hh:mm\")")
                ),
                LiteralValue::Text(expected.into()),
                "serial {serial}"
            );
        }

        let cases_1904 = [
            (59.9997, "1904-03-01 00:00"),
            (60.9997, "1904-03-02 00:00"),
            (61.9997, "1904-03-03 00:00"),
        ];
        for (serial, expected) in cases_1904 {
            assert_eq!(
                eval_text_formula(
                    DateSystem::Excel1904,
                    &format!("=TEXT({serial},\"yyyy-mm-dd hh:mm\")")
                ),
                LiteralValue::Text(expected.into()),
                "serial {serial}"
            );
        }

        assert_value_error(eval_text_formula(
            DateSystem::Excel1900,
            "=TEXT(2958465.9997,\"yyyy-mm-dd hh:mm\")",
        ));
        assert_value_error(eval_text_formula(
            DateSystem::Excel1904,
            "=TEXT(2957003.9997,\"yyyy-mm-dd hh:mm\")",
        ));
    }

    #[test]
    fn test_text_formatting() {
        let wb = TestWorkbook::new().with_function(Arc::new(TextFn));
        let ctx = wb.interpreter();
        let f = ctx.context.get_function("", "TEXT").unwrap();

        // Percent format
        let num = lit(LiteralValue::Number(0.125));
        let fmt = lit(LiteralValue::Text("%".into()));
        assert_eq!(
            f.dispatch(
                &[
                    ArgumentHandle::new(&num, &ctx),
                    ArgumentHandle::new(&fmt, &ctx)
                ],
                &ctx.function_context(None)
            )
            .unwrap(),
            LiteralValue::Text("12%".into()) // 0.125 * 100 = 12.5, rounds to 12
        );

        // Two decimal places
        let pi = lit(LiteralValue::Number(std::f64::consts::PI));
        let dec_fmt = lit(LiteralValue::Text("0.00".into()));
        assert_eq!(
            f.dispatch(
                &[
                    ArgumentHandle::new(&pi, &ctx),
                    ArgumentHandle::new(&dec_fmt, &ctx)
                ],
                &ctx.function_context(None)
            )
            .unwrap(),
            LiteralValue::Text("3.14".into())
        );

        // Locale-dependent numeric text should error (not silently become 0.00)
        let comma_decimal = lit(LiteralValue::Text("1.234,56".into()));
        match f
            .dispatch(
                &[
                    ArgumentHandle::new(&comma_decimal, &ctx),
                    ArgumentHandle::new(&dec_fmt, &ctx),
                ],
                &ctx.function_context(None),
            )
            .unwrap()
            .into_literal()
        {
            LiteralValue::Error(e) => assert_eq!(e.to_string(), "#VALUE!"),
            other => panic!("Expected #VALUE! error, got {other:?}"),
        }
    }

    #[test]
    fn test_textjoin_empty_delimiter() {
        let wb = TestWorkbook::new().with_function(Arc::new(TextJoinFn));
        let ctx = wb.interpreter();
        let f = ctx.context.get_function("", "TEXTJOIN").unwrap();

        // Empty delimiter
        let delim = lit(LiteralValue::Text("".into()));
        let ignore = lit(LiteralValue::Boolean(true));
        let a = lit(LiteralValue::Text("a".into()));
        let b = lit(LiteralValue::Text("b".into()));
        let c = lit(LiteralValue::Text("c".into()));

        assert_eq!(
            f.dispatch(
                &[
                    ArgumentHandle::new(&delim, &ctx),
                    ArgumentHandle::new(&ignore, &ctx),
                    ArgumentHandle::new(&a, &ctx),
                    ArgumentHandle::new(&b, &ctx),
                    ArgumentHandle::new(&c, &ctx),
                ],
                &ctx.function_context(None)
            )
            .unwrap(),
            LiteralValue::Text("abc".into())
        );
    }

    #[test]
    fn test_replace_bounds() {
        let wb = TestWorkbook::new().with_function(Arc::new(ReplaceFn));
        let ctx = wb.interpreter();
        let f = ctx.context.get_function("", "REPLACE").unwrap();

        let text = lit(LiteralValue::Text("hello".into()));

        // Replace at start
        let start = lit(LiteralValue::Int(1));
        let count = lit(LiteralValue::Int(2));
        let new = lit(LiteralValue::Text("HE".into()));
        assert_eq!(
            f.dispatch(
                &[
                    ArgumentHandle::new(&text, &ctx),
                    ArgumentHandle::new(&start, &ctx),
                    ArgumentHandle::new(&count, &ctx),
                    ArgumentHandle::new(&new, &ctx)
                ],
                &ctx.function_context(None)
            )
            .unwrap(),
            LiteralValue::Text("HEllo".into())
        );

        // Replace beyond end
        let start_end = lit(LiteralValue::Int(4));
        let count_large = lit(LiteralValue::Int(10));
        let suffix = lit(LiteralValue::Text("LO!".into()));
        assert_eq!(
            f.dispatch(
                &[
                    ArgumentHandle::new(&text, &ctx),
                    ArgumentHandle::new(&start_end, &ctx),
                    ArgumentHandle::new(&count_large, &ctx),
                    ArgumentHandle::new(&suffix, &ctx)
                ],
                &ctx.function_context(None)
            )
            .unwrap(),
            LiteralValue::Text("helLO!".into())
        );
    }
}
