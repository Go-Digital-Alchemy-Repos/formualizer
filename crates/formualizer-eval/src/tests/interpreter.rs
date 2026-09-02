#[cfg(test)]
mod tests {
    use crate::test_workbook::TestWorkbook;
    use formualizer_common::error::{ExcelError, ExcelErrorKind};
    use formualizer_parse::LiteralValue;
    use formualizer_parse::parser::Parser;
    use std::sync::Arc;

    /// Helper function to parse and evaluate a formula.
    fn evaluate_formula(formula: &str, wb: &TestWorkbook) -> Result<LiteralValue, ExcelError> {
        let mut parser = Parser::new(formula).unwrap();
        let ast = parser
            .parse()
            .map_err(|e| ExcelError::new(ExcelErrorKind::Error).with_message(e.message.clone()))?;

        let interpreter = wb.interpreter();

        let cv = interpreter.evaluate_ast(&ast)?;
        if formula.contains('{') {
            Ok(match cv {
                crate::traits::CalcValue::Scalar(v)
                | crate::traits::CalcValue::AnnotatedScalar(v, _) => v,
                crate::traits::CalcValue::Range(rv) => {
                    let (rows, _cols) = rv.dims();
                    let mut data = Vec::with_capacity(rows);
                    let _ = rv.for_each_row(&mut |row| {
                        data.push(row.to_vec());
                        Ok(())
                    });
                    LiteralValue::Array(data)
                }
                crate::traits::CalcValue::Callable(_) => {
                    LiteralValue::Error(ExcelError::new(ExcelErrorKind::Calc))
                }
            })
        } else {
            Ok(cv.into_literal())
        }
    }

    fn create_workbook() -> TestWorkbook {
        use std::sync::Arc;
        TestWorkbook::new()
            .with_function(Arc::new(crate::builtins::math::SumFn))
            .with_function(Arc::new(crate::builtins::logical::IfFn))
            .with_function(Arc::new(crate::builtins::logical::AndFn))
            .with_function(Arc::new(crate::builtins::logical::TrueFn))
            .with_function(Arc::new(crate::builtins::logical::FalseFn))
    }

    #[test]
    fn range_duplicate_sum_is_correct() {
        // Prepare a small range and sum it twice; internal caching is an engine concern.
        let wb = TestWorkbook::new()
            .with_function(Arc::new(crate::builtins::math::SumFn))
            .with_cell("Sheet1", 1, 1, LiteralValue::Int(1))
            .with_cell("Sheet1", 1, 2, LiteralValue::Int(2))
            .with_cell("Sheet1", 2, 1, LiteralValue::Int(3))
            .with_cell("Sheet1", 2, 2, LiteralValue::Int(4));
        let mut parser = Parser::new("=SUM(A1:B2, A1:B2)").unwrap();
        let ast = parser.parse().unwrap();
        let interp = wb.interpreter();
        let res = interp.evaluate_ast(&ast).unwrap().into_literal();
        assert_eq!(res, LiteralValue::Number(20.0));
    }

    #[test]
    fn test_basic_arithmetic() {
        let wb = create_workbook();
        // Basic arithmetic
        assert_eq!(
            evaluate_formula("=1+2", &wb).unwrap(),
            LiteralValue::Number(3.0)
        );
        assert_eq!(
            evaluate_formula("=3-1", &wb).unwrap(),
            LiteralValue::Number(2.0)
        );
        assert_eq!(
            evaluate_formula("=2*3", &wb).unwrap(),
            LiteralValue::Number(6.0)
        );
        assert_eq!(
            evaluate_formula("=6/2", &wb).unwrap(),
            LiteralValue::Number(3.0)
        );
        assert_eq!(
            evaluate_formula("=2^3", &wb).unwrap(),
            LiteralValue::Number(8.0)
        );

        // Order of operations
        assert_eq!(
            evaluate_formula("=1+2*3", &wb).unwrap(),
            LiteralValue::Number(7.0)
        );
        assert_eq!(
            evaluate_formula("=(1+2)*3", &wb).unwrap(),
            LiteralValue::Number(9.0)
        );
        assert_eq!(
            evaluate_formula("=2^3+1", &wb).unwrap(),
            LiteralValue::Number(9.0)
        );
        assert_eq!(
            evaluate_formula("=2^(3+1)", &wb).unwrap(),
            LiteralValue::Number(16.0)
        );
    }

    #[test]
    fn test_unary_operators() {
        let wb = create_workbook();
        // Unary operators
        assert_eq!(
            evaluate_formula("=-5", &wb).unwrap(),
            LiteralValue::Number(-5.0)
        );
        assert_eq!(
            evaluate_formula("=+5", &wb).unwrap(),
            LiteralValue::Number(5.0)
        );
        assert_eq!(
            evaluate_formula("=--5", &wb).unwrap(),
            LiteralValue::Number(5.0)
        );
        assert_eq!(
            evaluate_formula("=-(-5)", &wb).unwrap(),
            LiteralValue::Number(5.0)
        );

        // Percentage operator
        assert_eq!(
            evaluate_formula("=50%", &wb).unwrap(),
            LiteralValue::Number(0.5)
        );
        assert_eq!(
            evaluate_formula("=100%+20%", &wb).unwrap(),
            LiteralValue::Number(1.2)
        );
    }

    /// Unary `+` is a pass-through (identity) operator in Excel/LibreOffice,
    /// not a numeric coercion. The `=+A1` idiom is a Lotus 1-2-3 carry-over
    /// that is common in finance models, and must not turn text labels into
    /// `#VALUE!`. Unary `-` and `%` retain their numeric-coercion semantics.
    ///
    /// Ground truth was captured by writing the same formulas to an .xlsx,
    /// having LibreOffice recalculate them, and reading back cached values.
    #[test]
    fn test_unary_plus_is_passthrough_excel_parity() {
        let wb = create_workbook()
            .with_cell("Sheet1", 1, 1, LiteralValue::Text("2014F".to_string()))
            .with_cell("Sheet1", 2, 1, LiteralValue::Text("hello".to_string()))
            .with_cell("Sheet1", 3, 1, LiteralValue::Text("5".to_string()))
            .with_cell("Sheet1", 4, 1, LiteralValue::Number(42.0))
            .with_cell("Sheet1", 5, 1, LiteralValue::Number(-3.5))
            .with_cell("Sheet1", 6, 1, LiteralValue::Boolean(true))
            .with_cell("Sheet1", 7, 1, LiteralValue::Boolean(false))
            .with_cell("Sheet1", 8, 1, LiteralValue::Empty)
            .with_cell(
                "Sheet1",
                9,
                1,
                LiteralValue::Error(ExcelError::new(ExcelErrorKind::Div)),
            );

        // Reference operand: text passes through unchanged. Pre-fix this returned #VALUE!.
        assert_eq!(
            evaluate_formula("=+A1", &wb).unwrap(),
            LiteralValue::Text("2014F".to_string()),
        );
        assert_eq!(
            evaluate_formula("=+A2", &wb).unwrap(),
            LiteralValue::Text("hello".to_string()),
        );
        // Numeric-looking text stays text (Excel: `=+"5"` cached as "5", not 5).
        assert_eq!(
            evaluate_formula("=+A3", &wb).unwrap(),
            LiteralValue::Text("5".to_string()),
        );
        // Numbers pass through.
        assert_eq!(
            evaluate_formula("=+A4", &wb).unwrap(),
            LiteralValue::Number(42.0),
        );
        assert_eq!(
            evaluate_formula("=+A5", &wb).unwrap(),
            LiteralValue::Number(-3.5),
        );
        // Booleans stay booleans (LibreOffice cached value confirms this).
        assert_eq!(
            evaluate_formula("=+A6", &wb).unwrap(),
            LiteralValue::Boolean(true),
        );
        assert_eq!(
            evaluate_formula("=+A7", &wb).unwrap(),
            LiteralValue::Boolean(false),
        );
        // Empty operand: identity preserves Empty (matches `=A8`).
        assert_eq!(evaluate_formula("=+A8", &wb).unwrap(), LiteralValue::Empty);
        // Errors propagate unchanged.
        match evaluate_formula("=+A9", &wb).unwrap() {
            LiteralValue::Error(e) => assert_eq!(e.kind, ExcelErrorKind::Div),
            other => panic!("expected #DIV/0! got {other:?}"),
        }

        // String literals.
        assert_eq!(
            evaluate_formula("=+\"hello\"", &wb).unwrap(),
            LiteralValue::Text("hello".to_string()),
        );
        assert_eq!(
            evaluate_formula("=+\"5\"", &wb).unwrap(),
            LiteralValue::Text("5".to_string()),
        );

        // Numeric literals (regression guard: must remain numeric).
        assert_eq!(
            evaluate_formula("=+5", &wb).unwrap(),
            LiteralValue::Number(5.0),
        );
        assert_eq!(
            evaluate_formula("=+5.5", &wb).unwrap(),
            LiteralValue::Number(5.5),
        );
        // Double unary plus on a number stays numeric.
        assert_eq!(
            evaluate_formula("=++5", &wb).unwrap(),
            LiteralValue::Number(5.0),
        );
    }

    /// Unary `-` must continue to coerce. `=-A1` on a non-numeric string
    /// must still return #VALUE! to stay Excel-compatible. Guards against an
    /// overzealous fix that pass-throughs both `+` and `-`.
    #[test]
    fn test_unary_minus_still_coerces_strings() {
        let wb = create_workbook()
            .with_cell("Sheet1", 1, 1, LiteralValue::Text("2014F".to_string()))
            .with_cell("Sheet1", 2, 1, LiteralValue::Text("5".to_string()))
            .with_cell("Sheet1", 3, 1, LiteralValue::Boolean(true))
            .with_cell("Sheet1", 4, 1, LiteralValue::Empty);

        match evaluate_formula("=-A1", &wb).unwrap() {
            LiteralValue::Error(e) => assert_eq!(e.kind, ExcelErrorKind::Value),
            other => panic!("expected #VALUE! got {other:?}"),
        }
        assert_eq!(
            evaluate_formula("=-A2", &wb).unwrap(),
            LiteralValue::Number(-5.0),
        );
        assert_eq!(
            evaluate_formula("=-A3", &wb).unwrap(),
            LiteralValue::Number(-1.0),
        );
        assert_eq!(
            evaluate_formula("=-A4", &wb).unwrap(),
            LiteralValue::Number(0.0),
        );
        match evaluate_formula("=-\"hello\"", &wb).unwrap() {
            LiteralValue::Error(e) => assert_eq!(e.kind, ExcelErrorKind::Value),
            other => panic!("expected #VALUE! got {other:?}"),
        }
    }

    /// Percent operator must continue to coerce just like unary `-`.
    #[test]
    fn test_unary_percent_still_coerces_strings() {
        let wb = create_workbook()
            .with_cell("Sheet1", 1, 1, LiteralValue::Text("2014F".to_string()))
            .with_cell("Sheet1", 2, 1, LiteralValue::Text("50".to_string()));

        match evaluate_formula("=A1%", &wb).unwrap() {
            LiteralValue::Error(e) => assert_eq!(e.kind, ExcelErrorKind::Value),
            other => panic!("expected #VALUE! got {other:?}"),
        }
        assert_eq!(
            evaluate_formula("=A2%", &wb).unwrap(),
            LiteralValue::Number(0.5),
        );
    }

    /// Unary `+` inside a larger expression: the surrounding arithmetic
    /// still coerces its operands, so `=1++"hello"` and `=1++A_text` remain
    /// #VALUE!. Pass-through only changes the value produced by the `+` node
    /// itself. Matches Excel: `=1++"hello"` -> #VALUE!, `=1++"5"` -> 6.
    #[test]
    fn test_inner_unary_plus_on_text_still_errors_in_arithmetic() {
        let wb =
            create_workbook().with_cell("Sheet1", 1, 1, LiteralValue::Text("2014F".to_string()));

        match evaluate_formula("=1++\"hello\"", &wb).unwrap() {
            LiteralValue::Error(e) => assert_eq!(e.kind, ExcelErrorKind::Value),
            other => panic!("expected #VALUE! got {other:?}"),
        }
        match evaluate_formula("=1++A1", &wb).unwrap() {
            LiteralValue::Error(e) => assert_eq!(e.kind, ExcelErrorKind::Value),
            other => panic!("expected #VALUE! got {other:?}"),
        }
        assert_eq!(
            evaluate_formula("=1++\"5\"", &wb).unwrap(),
            LiteralValue::Number(6.0),
        );
    }

    /// Unary `+` applied to an array operates element-wise as pass-through.
    /// `=+{"a","b","c"}` returns an array of text, not an error.
    #[test]
    fn test_unary_plus_array_passthrough() {
        let wb = create_workbook();

        match evaluate_formula("=+{\"a\",\"b\",\"c\"}", &wb).unwrap() {
            LiteralValue::Array(rows) => {
                assert_eq!(rows.len(), 1);
                assert_eq!(
                    rows[0],
                    vec![
                        LiteralValue::Text("a".to_string()),
                        LiteralValue::Text("b".to_string()),
                        LiteralValue::Text("c".to_string()),
                    ]
                );
            }
            other => panic!("expected array, got {other:?}"),
        }

        match evaluate_formula("=+{1,2,3}", &wb).unwrap() {
            LiteralValue::Array(rows) => {
                assert_eq!(rows.len(), 1);
                assert_eq!(
                    rows[0],
                    vec![
                        LiteralValue::Number(1.0),
                        LiteralValue::Number(2.0),
                        LiteralValue::Number(3.0),
                    ]
                );
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    /// Original bug-report scenario: leading `=+SheetRef!Cell` on a text
    /// label must pass through, not become #VALUE!.
    #[test]
    fn test_unary_plus_on_cross_sheet_text_reference() {
        let wb = TestWorkbook::new()
            .with_function(Arc::new(crate::builtins::math::SumFn))
            .with_cell("SheetA", 1, 1, LiteralValue::Text("2014F".to_string()));

        let mut parser = Parser::new("=+SheetA!A1").unwrap();
        let ast = parser.parse().unwrap();
        let interp = wb.interpreter();
        let v = interp.evaluate_ast(&ast).unwrap().into_literal();
        assert_eq!(v, LiteralValue::Text("2014F".to_string()));
    }

    #[test]
    fn test_value_coercion() {
        let wb = create_workbook();
        // Boolean to number coercion
        assert_eq!(
            evaluate_formula("=TRUE+1", &wb).unwrap(),
            LiteralValue::Number(2.0)
        );
        assert_eq!(
            evaluate_formula("=FALSE+1", &wb).unwrap(),
            LiteralValue::Number(1.0)
        );

        // Text to number coercion
        assert_eq!(
            evaluate_formula("=\"5\"+2", &wb).unwrap(),
            LiteralValue::Number(7.0)
        );

        // Number to boolean coercion in logical contexts
        assert_eq!(
            evaluate_formula("=IF(1, \"Yes\", \"No\")", &wb).unwrap(),
            LiteralValue::Text("Yes".to_string())
        );
        assert_eq!(
            evaluate_formula("=IF(0, \"Yes\", \"No\")", &wb).unwrap(),
            LiteralValue::Text("No".to_string())
        );
    }

    #[test]
    fn test_string_concatenation() {
        let wb = create_workbook();
        // String concatenation
        assert_eq!(
            evaluate_formula("=\"Hello\"&\" \"&\"World\"", &wb).unwrap(),
            LiteralValue::Text("Hello World".to_string())
        );

        // Number to string coercion in concatenation
        assert_eq!(
            evaluate_formula("=\"LiteralValue: \"&123", &wb).unwrap(),
            LiteralValue::Text("LiteralValue: 123".to_string())
        );

        // Boolean to string coercion in concatenation
        assert_eq!(
            evaluate_formula("=\"Is true: \"&TRUE", &wb).unwrap(),
            LiteralValue::Text("Is true: TRUE".to_string())
        );
    }

    #[test]
    fn test_comparisons() {
        let wb = create_workbook();
        // Equal and not equal
        assert_eq!(
            evaluate_formula("=1=1", &wb).unwrap(),
            LiteralValue::Boolean(true)
        );
        assert_eq!(
            evaluate_formula("=1<>1", &wb).unwrap(),
            LiteralValue::Boolean(false)
        );
        assert_eq!(
            evaluate_formula("=1=2", &wb).unwrap(),
            LiteralValue::Boolean(false)
        );
        assert_eq!(
            evaluate_formula("=1<>2", &wb).unwrap(),
            LiteralValue::Boolean(true)
        );

        // Greater than, less than
        assert_eq!(
            evaluate_formula("=2>1", &wb).unwrap(),
            LiteralValue::Boolean(true)
        );
        assert_eq!(
            evaluate_formula("=1<2", &wb).unwrap(),
            LiteralValue::Boolean(true)
        );
        assert_eq!(
            evaluate_formula("=1>2", &wb).unwrap(),
            LiteralValue::Boolean(false)
        );
        assert_eq!(
            evaluate_formula("=2<1", &wb).unwrap(),
            LiteralValue::Boolean(false)
        );

        // Greater than or equal, less than or equal
        assert_eq!(
            evaluate_formula("=2>=1", &wb).unwrap(),
            LiteralValue::Boolean(true)
        );
        assert_eq!(
            evaluate_formula("=1<=2", &wb).unwrap(),
            LiteralValue::Boolean(true)
        );
        assert_eq!(
            evaluate_formula("=1>=1", &wb).unwrap(),
            LiteralValue::Boolean(true)
        );
        assert_eq!(
            evaluate_formula("=1<=1", &wb).unwrap(),
            LiteralValue::Boolean(true)
        );

        // Text comparisons
        assert_eq!(
            evaluate_formula("=\"a\"=\"a\"", &wb).unwrap(),
            LiteralValue::Boolean(true)
        );
        assert_eq!(
            evaluate_formula("=\"a\"=\"A\"", &wb).unwrap(),
            LiteralValue::Boolean(true)
        ); // Case-insensitive
        assert_eq!(
            evaluate_formula("=\"a\"<\"b\"", &wb).unwrap(),
            LiteralValue::Boolean(true)
        );
        assert_eq!(
            evaluate_formula("=\"b\">\"a\"", &wb).unwrap(),
            LiteralValue::Boolean(true)
        );

        // Mixed type comparisons. Excel ranks types (number < text < boolean)
        // and never coerces across a rank boundary; see the god228 fixture
        // below for the full measured table.
        assert_eq!(
            evaluate_formula("=\"5\"=5", &wb).unwrap(),
            LiteralValue::Boolean(false)
        ); // GOD-228 receipt row `n14`: text never equals a number
        assert_eq!(
            evaluate_formula("=TRUE=1", &wb).unwrap(),
            LiteralValue::Boolean(false)
        ); // GOD-228 receipt row `n04`: boolean outranks number
    }

    /// GOD-228 / ES-044 / CL-065: Excel's relational type rank,
    /// `number < text < boolean`, applied identically by all six operators.
    ///
    /// All 34 rows are copied verbatim from the Excel oracle receipt
    /// `artifacts/private/god228/probe/excel-boolean-ordering-probe.json`
    /// (sha256
    /// `76e3fbb0dd34158332425be795a0cd553459cc6e263b4c5c59a886d89360c2da`),
    /// measured in Microsoft Excel for Mac 16.105.3, `en_US`, 2026-09-02. The
    /// tuple is `(receipt row id, formula, Excel's value)`; no expected value
    /// here was inferred, derived or copied from the engine.
    ///
    /// Rows `n17`-`n20` reference `$Z$1`, a blank cell. `TestWorkbook`'s
    /// resolver returns `#REF!` for a key that is absent from its map, so the
    /// cell is written explicitly as `LiteralValue::Empty` — that is the same
    /// value the real engine's resolver hands `Interpreter::compare` for a
    /// never-written in-bounds cell.
    ///
    /// SCOPE NOTE (GOD-228 review cycle 1, finding M2). The diagnosis
    /// `research/reports/god228_boolean_ordering_diagnosis_2026-09-02.md` §3.1
    /// recommended fixing the boolean half only and filing the numeric-text
    /// half (`"5"=5`, `"5">4`, `"5"<4`) as separate work, because at the time
    /// §3.1 was written there was no Excel oracle for numeric text. §3.3's
    /// later measurement supplied exactly that oracle — receipt rows `n14`,
    /// `n15` and `n16` above — so the round deliberately widened its boundary
    /// and implemented both halves in one change rather than leaving the
    /// engine coercing numeric text for a further round. The widening is
    /// recorded here because the commit implements more than §3.1 asked for;
    /// nothing about it is inferred, the three anchoring rows are measured.
    #[test]
    fn test_god228_boolean_ordering_type_rank() {
        let wb = create_workbook()
            // Z1: blank. Column 26, row 1.
            .with_cell("Sheet1", 1, 26, LiteralValue::Empty);

        let cases: [(&str, &str, bool); 34] = [
            ("n01", "=TRUE<=1", false),
            ("n02", "=TRUE>=1", true),
            ("n03", "=TRUE<>1", true),
            ("n04", "=TRUE=1", false),
            ("n05", "=1<TRUE", true),
            ("n06", "=1<=TRUE", true),
            ("n07", "=TRUE>FALSE", true),
            ("n08", "=FALSE>TRUE", false),
            ("n09", "=TRUE>=FALSE", true),
            ("n10", "=\"a\"<TRUE", true),
            ("n11", "=\"Z\"<FALSE", true),
            ("n12", "=TRUE>\"z\"", true),
            ("n13", "=\"TRUE\"=TRUE", false),
            ("n14", "=\"5\"=5", false),
            ("n15", "=\"5\">4", true),
            ("n16", "=\"5\"<4", false),
            ("n17", "=$Z$1=FALSE", true),
            ("n18", "=$Z$1<TRUE", true),
            ("n19", "=$Z$1=0", true),
            ("n20", "=$Z$1=\"\"", true),
            ("n21", "=FALSE=0", false),
            ("n22", "=0<>FALSE", true),
            ("n23", "=FALSE<\"a\"", false),
            ("n24", "=TRUE<=\"a\"", false),
            ("f01", "=FALSE>0", true),
            ("f02", "=TRUE>0", true),
            ("f03", "=FALSE>1", true),
            ("f04", "=TRUE>1", true),
            ("f05", "=FALSE=0", false),
            ("f06", "=FALSE<0", false),
            ("f07", "=TRUE<1", false),
            ("f08", "=TRUE<0", false),
            ("f09", "=\"a\">1", true),
            ("f10", "=FALSE>\"a\"", true),
        ];

        for (id, formula, expected) in cases {
            assert_eq!(
                evaluate_formula(formula, &wb).unwrap(),
                LiteralValue::Boolean(expected),
                "receipt row {id}: {formula}"
            );
        }
    }

    /// GOD-228 review cycle 1, finding m1. PROVENANCE: these expectations are
    /// *derived* from the ES-044 type rank (`number < text < boolean`) and the
    /// blank-operand polymorphism rule measured in receipt rows `n17`-`n20`;
    /// they are NOT themselves measured in desktop Excel, because the 34-row
    /// receipt
    /// `artifacts/private/god228/probe/excel-boolean-ordering-probe.json`
    /// contains no row for any of them. They are pinned so a later change
    /// cannot move them silently. A later Excel probe should confirm them.
    #[test]
    fn test_god228_rank_corollaries_documented_not_excel_measured() {
        let wb = create_workbook()
            .with_cell("Sheet1", 1, 26, LiteralValue::Empty)
            .with_cell("Sheet1", 2, 26, LiteralValue::Empty);

        let cases: [(&str, bool); 10] = [
            // Blank versus blank: equal, on every operator.
            ("=$Z$1=$Z$2", true),
            ("=$Z$1<>$Z$2", false),
            ("=$Z$1<=$Z$2", true),
            ("=$Z$1>=$Z$2", true),
            ("=$Z$1<$Z$2", false),
            // Blank adopts the other operand's type, so it is not ranked.
            ("=$Z$1<\"a\"", true),
            ("=$Z$1>TRUE", false),
            // Numeric text stays text on the remaining operators.
            ("=\"5\"<>5", true),
            ("=\"5\">=4", true),
            // Int/Int still compares numerically (no fast-path arm covers it).
            ("=2>1", true),
        ];

        for (formula, expected) in cases {
            assert_eq!(
                evaluate_formula(formula, &wb).unwrap(),
                LiteralValue::Boolean(expected),
                "{formula}"
            );
        }
    }

    /// GOD-228 review cycle 1, finding M1. PROVENANCE: these expectations are
    /// *derived* from the ES-044 type rank (`number < text < boolean`) plus
    /// the decision, taken in this round, that a serial-bearing temporal value
    /// belongs to the number class — on a sheet a date cell *is* a number. They
    /// are NOT measured in desktop Excel: the 34-row receipt
    /// `artifacts/private/god228/probe/excel-boolean-ordering-probe.json`
    /// (sha256
    /// `76e3fbb0dd34158332425be795a0cd553459cc6e263b4c5c59a886d89360c2da`)
    /// has no temporal row at all. A later Excel probe should confirm them.
    ///
    /// Four of these moved between the parent commit `c9abf377` and this
    /// round's `79a3e990` (`DATE(2003,1,1)<TRUE` FALSE -> TRUE,
    /// `DATE(2003,1,1)<=TRUE` FALSE -> TRUE, `DATE(2003,1,1)>TRUE` TRUE ->
    /// FALSE, `TIME(12,0,0)<FALSE` FALSE -> TRUE), so the derived rank moves
    /// observable behaviour and must not move again unnoticed. The
    /// temporal-versus-number rows below were unchanged by the round and are
    /// kept as regression guards.
    ///
    /// Two shapes are pinned deliberately. `DATE(...)`/`TIME(...)` return
    /// `LiteralValue::Number` in this engine, so those rows exercise the
    /// number-versus-boolean rank as a user would write it but do NOT reach
    /// `excel_type_rank`'s temporal arms. The `A1`-`A4` rows hold
    /// `LiteralValue::Date`, `Time`, `DateTime` and `Duration` cells directly,
    /// which is the only way to exercise the `_ => 0` catch-all that puts the
    /// temporal variants in the number class.
    #[test]
    fn test_god228_temporal_rank_documented_not_excel_measured() {
        crate::builtins::load_builtins();

        // Written as a user would: DATE()/TIME() yield plain numbers here.
        let wb = create_workbook();
        let formula_cases: [(&str, bool); 9] = [
            // Moved by this round (parent c9abf377 gave the opposite value).
            ("=DATE(2003,1,1)<TRUE", true),
            ("=DATE(2003,1,1)<=TRUE", true),
            ("=DATE(2003,1,1)>TRUE", false),
            ("=TIME(12,0,0)<FALSE", true),
            // Unchanged by this round; regression guards for the number class.
            ("=DATE(2003,1,1)=37622", true),
            ("=DATE(2003,1,1)<37623", true),
            ("=DATE(2003,1,1)>37621", true),
            ("=TIME(12,0,0)=0.5", true),
            // Derived temporal-versus-text: rank 0 < rank 1, so a date serial
            // is below any text on every operator.
            ("=DATE(2003,1,1)<\"a\"", true),
        ];
        for (formula, expected) in formula_cases {
            assert_eq!(
                evaluate_formula(formula, &wb).unwrap(),
                LiteralValue::Boolean(expected),
                "{formula}"
            );
        }

        // Genuine temporal `LiteralValue` variants, which is what
        // `excel_type_rank`'s `_ => 0` catch-all actually classifies.
        let wb_temporal = create_workbook()
            .with_cell(
                "Sheet1",
                1,
                1,
                LiteralValue::Date(chrono::NaiveDate::from_ymd_opt(2003, 1, 1).unwrap()),
            )
            .with_cell(
                "Sheet1",
                2,
                1,
                LiteralValue::Time(chrono::NaiveTime::from_hms_opt(12, 0, 0).unwrap()),
            )
            .with_cell(
                "Sheet1",
                3,
                1,
                LiteralValue::DateTime(
                    chrono::NaiveDate::from_ymd_opt(2003, 1, 1)
                        .unwrap()
                        .and_hms_opt(12, 0, 0)
                        .unwrap(),
                ),
            )
            .with_cell(
                "Sheet1",
                4,
                1,
                LiteralValue::Duration(chrono::Duration::hours(12)),
            );
        let cell_cases: [(&str, bool); 17] = [
            // A1 = Date, A2 = Time, A3 = DateTime, A4 = Duration.
            // Temporal versus boolean: rank 0 < rank 2.
            ("=A1<TRUE", true),
            ("=A1<=TRUE", true),
            ("=A1>TRUE", false),
            ("=A1=TRUE", false),
            ("=A2<FALSE", true),
            ("=A3<TRUE", true),
            ("=A4<TRUE", true),
            // Temporal versus text: rank 0 < rank 1.
            ("=A1<\"a\"", true),
            ("=A1>\"a\"", false),
            ("=A2<\"a\"", true),
            ("=A3<\"a\"", true),
            ("=A4<\"a\"", true),
            // Same rank as a number: compared on the serial, as before.
            ("=A1=37622", true),
            ("=A1<37623", true),
            ("=A1>37621", true),
            ("=A2=0.5", true),
            ("=A4=0.5", true),
        ];
        for (formula, expected) in cell_cases {
            assert_eq!(
                evaluate_formula(formula, &wb_temporal).unwrap(),
                LiteralValue::Boolean(expected),
                "{formula}"
            );
        }
    }

    /// GOD-228 review cycle 1, finding M2. PROVENANCE: these expectations are
    /// *derived* from the measured type rank; the measured anchors for the
    /// numeric-text half are receipt rows `n14` (`"5"=5` FALSE), `n15`
    /// (`"5">4` TRUE) and `n16` (`"5"<4` FALSE) in
    /// `artifacts/private/god228/probe/excel-boolean-ordering-probe.json`. The
    /// specific literal forms below — percent text, exponent text, signed text,
    /// zero text, whitespace-padded text, decimal text — are NOT individually
    /// Excel-measured. A later Excel probe should confirm them.
    ///
    /// Every one of these returned TRUE before this round (the old fallback ran
    /// `to_number_lenient_with_locale` on both sides) and returns FALSE now, so
    /// the numeric-text half of the change is far wider than its three measured
    /// rows and is pinned here in full.
    ///
    /// The last row is the one place where `Empty` polymorphism and the
    /// numeric-text change interact: a never-written cell adopts the other
    /// operand's TEXT type and becomes `""`, so `blank="0"` is FALSE (and
    /// `blank<"0"` is TRUE) rather than the numeric `0 = 0` TRUE the old
    /// lenient fallback produced.
    #[test]
    fn test_god228_numeric_text_documented_not_excel_measured() {
        let wb = create_workbook().with_cell("Sheet1", 1, 26, LiteralValue::Empty);

        let cases: [(&str, bool); 9] = [
            ("=\"90%\"=0.9", false),
            ("=\"90%\"<1", false),
            ("=\"1e3\"=1000", false),
            ("=\"-5\"<0", false),
            ("=\"0\"=0", false),
            ("=\" 5 \"=5", false),
            ("=\"1.5\"=1.5", false),
            // Blank versus numeric text: `Empty` becomes `""`, not `0`.
            ("=$Z$1=\"0\"", false),
            ("=$Z$1<\"0\"", true),
        ];

        for (formula, expected) in cases {
            assert_eq!(
                evaluate_formula(formula, &wb).unwrap(),
                LiteralValue::Boolean(expected),
                "{formula}"
            );
        }
    }

    #[test]
    fn test_function_calls() {
        let wb = create_workbook();
        assert_eq!(
            evaluate_formula("=SUM(1,2,3)", &wb).unwrap(),
            LiteralValue::Number(6.0)
        );

        // Function with array argument
        assert_eq!(
            evaluate_formula("=SUM({1,2,3;4,5,6})", &wb).unwrap(),
            LiteralValue::Number(21.0)
        );

        // Nested function calls
        assert_eq!(
            evaluate_formula("=IF(SUM(1,2)>0, \"Positive\", \"Negative\")", &wb).unwrap(),
            LiteralValue::Text("Positive".to_string())
        );

        // Function with boolean logic
        assert_eq!(
            evaluate_formula("=AND(TRUE, TRUE)", &wb).unwrap(),
            LiteralValue::Boolean(true)
        );
        assert_eq!(
            evaluate_formula("=AND(TRUE, FALSE)", &wb).unwrap(),
            LiteralValue::Boolean(false)
        );
    }

    #[test]
    fn test_cell_references() {
        let wb = create_workbook()
            .with_cell("Sheet1", 1, 1, LiteralValue::Number(5.0))
            .with_cell("Sheet1", 1, 2, LiteralValue::Number(10.0))
            .with_cell("Sheet1", 1, 3, LiteralValue::Text("Hello".to_string()));

        // Basic cell references
        assert_eq!(
            evaluate_formula("=A1", &wb).unwrap(),
            LiteralValue::Number(5.0)
        );
        assert_eq!(
            evaluate_formula("=A1+B1", &wb).unwrap(),
            LiteralValue::Number(15.0)
        );
        assert_eq!(
            evaluate_formula("=C1&\" World\"", &wb).unwrap(),
            LiteralValue::Text("Hello World".to_string())
        );

        // Reference in function
        assert_eq!(
            evaluate_formula("=SUM(A1,B1)", &wb).unwrap(),
            LiteralValue::Number(15.0)
        );
    }

    #[test]
    fn test_range_references() {
        let wb = create_workbook()
            .with_cell("Sheet1", 1, 1, LiteralValue::Number(1.0))
            .with_cell("Sheet1", 1, 2, LiteralValue::Number(2.0))
            .with_cell("Sheet1", 2, 1, LiteralValue::Number(3.0))
            .with_cell("Sheet1", 2, 2, LiteralValue::Number(4.0));

        // Sum of range
        assert_eq!(
            evaluate_formula("=SUM(A1:B2)", &wb).unwrap(),
            LiteralValue::Number(10.0)
        );
    }

    #[test]
    fn test_named_ranges() {
        let wb = create_workbook()
            .with_named_range(
                "MyRange",
                vec![
                    vec![LiteralValue::Number(10.0), LiteralValue::Number(20.0)],
                    vec![LiteralValue::Number(30.0), LiteralValue::Number(40.0)],
                ],
            )
            .with_cell("MyRange", 1, 1, LiteralValue::Number(100.0));

        // Use named range
        assert_eq!(
            evaluate_formula("=SUM(MyRange)", &wb).unwrap(),
            LiteralValue::Number(100.0)
        );
    }

    #[test]
    fn test_array_operations() {
        let wb = create_workbook();
        // Create an array
        let result = evaluate_formula("={1,2,3;4,5,6}", &wb).unwrap();
        if let LiteralValue::Array(arr) = result {
            assert_eq!(arr.len(), 2);
            assert_eq!(arr[0].len(), 3);
            assert_eq!(arr[0][0], LiteralValue::Number(1.0));
            assert_eq!(arr[1][2], LiteralValue::Number(6.0));
        } else {
            panic!("Expected array result");
        }

        // Array arithmetic
        let result = evaluate_formula("={1,2,3}+{4,5,6}", &wb).unwrap();
        if let LiteralValue::Array(arr) = result {
            assert_eq!(arr[0][0], LiteralValue::Number(5.0));
            assert_eq!(arr[0][1], LiteralValue::Number(7.0));
            assert_eq!(arr[0][2], LiteralValue::Number(9.0));
        } else {
            panic!("Expected array result");
        }
    }

    #[test]
    fn test_complex_formulas() {
        let wb = create_workbook()
            .with_cell_a1("Sheet1", "A1", LiteralValue::Number(10.0))
            .with_cell_a1("Sheet1", "B1", LiteralValue::Number(5.0))
            .with_cell_a1("Sheet1", "C1", LiteralValue::Boolean(true));
        // Complex formula with multiple operations and functions
        assert_eq!(
            evaluate_formula("=IF(A1>B1, SUM(A1, B1)/(A1-B1), \"A1 <= B1\")", &wb).unwrap(),
            LiteralValue::Number(3.0)
        );

        // Formula with nested IF and boolean logic
        assert_eq!(
            evaluate_formula(
                "=IF(AND(A1>0, B1>0, C1), \"All positive\", \"Not all positive\")",
                &wb
            )
            .unwrap(),
            LiteralValue::Text("All positive".to_string())
        );
    }

    #[test]
    fn test_array_mismatched_dimensions() {
        let wb = create_workbook();
        // {1,2} is a 1x2 array and {3} is a 1x1 array.
        // Expected: broadcasting {3} across both columns => [[1+3, 2+3]] = [[4, 5]]
        let result = evaluate_formula("={1,2}+{3}", &wb).unwrap();
        let expected = LiteralValue::Array(vec![vec![
            LiteralValue::Number(4.0),
            LiteralValue::Number(5.0),
        ]]);
        assert_eq!(result, expected);
    }

    #[test]
    fn interpreter_broadcasts_comparisons() {
        let wb = create_workbook();
        // {1,2} = {1;2} => 2x2 booleans
        match evaluate_formula("={1,2}={1;2}", &wb).unwrap() {
            LiteralValue::Array(rows) => {
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0].len(), 2);
                assert_eq!(rows[0][0], LiteralValue::Boolean(true));
                assert_eq!(rows[0][1], LiteralValue::Boolean(false));
                assert_eq!(rows[1][0], LiteralValue::Boolean(false));
                assert_eq!(rows[1][1], LiteralValue::Boolean(true));
            }
            v => panic!("unexpected {v:?}"),
        }
    }

    #[test]
    fn interpreter_broadcasts_per_cell_errors() {
        let wb = create_workbook();
        // {1,0} ^ {-1;0.5} => per-cell #DIV/0! where 0^-1; others numeric
        match evaluate_formula("={1,0}^{-1;0.5}", &wb).unwrap() {
            LiteralValue::Array(rows) => {
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0].len(), 2);
                // 1^-1 = 1; 0^-1 is treated as #NUM! by current semantics
                assert_eq!(rows[0][0], LiteralValue::Number(1.0));
                match &rows[0][1] {
                    LiteralValue::Error(e) => assert_eq!(e, "#NUM!"),
                    v => panic!("expected num error, got {v:?}"),
                }
                // 1^0.5 = 1; 0^0.5 = 0
                assert_eq!(rows[1][0], LiteralValue::Number(1.0));
                assert_eq!(rows[1][1], LiteralValue::Number(0.0));
            }
            v => panic!("unexpected {v:?}"),
        }
    }

    #[test]
    fn test_unary_operator_on_array() {
        let wb = create_workbook();
        let result = evaluate_formula("=-({1,-2,3})", &wb).unwrap();
        let expected = LiteralValue::Array(vec![vec![
            LiteralValue::Number(-1.0),
            LiteralValue::Number(2.0),
            LiteralValue::Number(-3.0),
        ]]);
        assert_eq!(result, expected);
    }

    #[test]
    fn test_percentage_operator_on_array() {
        let wb = create_workbook();
        let result = evaluate_formula("=({50,100}%)", &wb).unwrap();
        let expected = LiteralValue::Array(vec![vec![
            LiteralValue::Number(0.5),
            LiteralValue::Number(1.0),
        ]]);
        assert_eq!(result, expected);
    }

    #[test]
    fn test_exponentiation_error() {
        let wb = create_workbook();
        // Negative base with fractional exponent should yield a #NUM! error.
        if let LiteralValue::Error(ref e) = evaluate_formula("=(-4)^(0.5)", &wb).unwrap() {
            assert_eq!(e, "#NUM!");
        } else {
            panic!("Expected error result");
        }
    }

    #[test]
    fn test_zero_power_zero() {
        let wb = create_workbook();
        let result = evaluate_formula("=0^0", &wb).unwrap();
        assert_eq!(result, LiteralValue::Number(1.0));
    }

    #[test]
    fn test_division_array_scalar() {
        let wb = create_workbook();
        let result = evaluate_formula("={10,20}/10", &wb).unwrap();
        let expected = LiteralValue::Array(vec![vec![
            LiteralValue::Number(1.0),
            LiteralValue::Number(2.0),
        ]]);
        assert_eq!(result, expected);
    }

    #[test]
    fn test_division_scalar_array() {
        let wb = create_workbook();
        let result = evaluate_formula("=10/{2,0}", &wb).unwrap();
        if let LiteralValue::Array(arr) = result {
            assert_eq!(arr.len(), 1);
            assert_eq!(arr[0].len(), 2);
            assert_eq!(arr[0][0], LiteralValue::Number(5.0));
            if let LiteralValue::Error(ref e) = arr[0][1] {
                assert_eq!(e, "#DIV/0!");
            } else {
                panic!("Expected #DIV/0! error");
            }
        } else {
            panic!("Expected an array result");
        }
    }

    #[test]
    fn test_error_propagation_in_array() {
        let wb = create_workbook();
        let result = evaluate_formula("={\"abc\",5}+1", &wb).unwrap();
        if let LiteralValue::Array(arr) = result {
            assert_eq!(arr.len(), 1);
            assert_eq!(arr[0].len(), 2);
            if let LiteralValue::Error(ref e) = arr[0][0] {
                assert_eq!(e, "#VALUE!");
            } else {
                panic!("Expected error for non-coercible text");
            }
            assert_eq!(arr[0][1], LiteralValue::Number(6.0));
        } else {
            panic!("Expected an array result");
        }
    }

    #[test]
    fn test_invalid_reference() {
        let wb = create_workbook();
        let result = evaluate_formula("=Z999", &wb).unwrap();
        if let LiteralValue::Error(ref e) = result {
            assert_eq!(e, "#REF!");
        } else {
            panic!("Expected error for invalid cell reference");
        }
    }

    #[test]
    fn test_sum_function_argument_count() {
        let wb = create_workbook();
        // SUM() with no arguments returns 0 (Excel behavior)
        let result = evaluate_formula("=SUM()", &wb).unwrap();
        assert_eq!(result, LiteralValue::Number(0.0));
    }

    #[test]
    fn test_if_function_argument_count() {
        let wb = create_workbook();
        // IF expects at most 3 arguments.
        let result = evaluate_formula("=IF(TRUE,1,2,3,4)", &wb).unwrap();
        if let LiteralValue::Error(ref e) = result {
            // expected should mention "at most 3"
            assert!(
                e.message
                    .clone()
                    .unwrap()
                    .contains("expects 2 or 3 arguments, got 5")
            );
        } else {
            panic!("Expected wrong argument count error for IF");
        }
    }

    #[test]
    #[ignore]
    fn test_named_range_not_found() {
        let wb = create_workbook();
        let result = evaluate_formula("=SUM(NonExistentNamedRange)", &wb).unwrap();
        if let LiteralValue::Error(ref e) = result {
            assert_eq!(e, "#NAME?");
        } else {
            panic!("Expected error for non-existent named range");
        }
    }

    #[test]
    fn test_incompatible_types() {
        let wb = create_workbook();
        // Subtracting a number from a text string (not using concatenation) should yield #VALUE!
        let result = evaluate_formula("=\"text\"-1", &wb).unwrap();
        if let LiteralValue::Error(ref e) = result {
            assert_eq!(e, "#VALUE!");
        } else {
            panic!("Expected #VALUE! error for incompatible types");
        }
    }

    #[test]
    fn test_mixed_precedence_concatenation() {
        let wb = create_workbook();
        // Concatenation (&) has lower precedence than addition.
        // So "=\"A\"&1+2" should evaluate as "A" & (1+2) => "A3"
        let result = evaluate_formula("=\"A\"&1+2", &wb).unwrap();
        assert_eq!(result, LiteralValue::Text("A3".to_string()));
    }

    #[test]
    fn test_binary_ops_with_int_and_number() {
        fn unwrap_1x1(value: LiteralValue) -> LiteralValue {
            match value {
                LiteralValue::Array(arr) => {
                    if arr.len() == 1 && arr[0].len() == 1 {
                        arr[0][0].clone()
                    } else {
                        panic!("Expected 1x1 array result");
                    }
                }
                other => other,
            }
        }

        let wb = create_workbook();

        let v = unwrap_1x1(evaluate_formula("={1}+{2.5}", &wb).unwrap());
        assert_eq!(v, LiteralValue::Number(3.5));

        // Test Number + Int
        let v = unwrap_1x1(evaluate_formula("={2.5}+{1}", &wb).unwrap());
        assert_eq!(v, LiteralValue::Number(3.5));

        // Test Int - Number
        let v = unwrap_1x1(evaluate_formula("={5}-{2.5}", &wb).unwrap());
        assert_eq!(v, LiteralValue::Number(2.5));

        // Test Int * Number
        let v = unwrap_1x1(evaluate_formula("={3}*{1.5}", &wb).unwrap());
        assert_eq!(v, LiteralValue::Number(4.5));

        // Test Int / Number
        let v = unwrap_1x1(evaluate_formula("={6}/{2.5}", &wb).unwrap());
        assert_eq!(v, LiteralValue::Number(2.4));

        // Test Int ^ Number
        let v = unwrap_1x1(evaluate_formula("={2}^{2.5}", &wb).unwrap());
        if let LiteralValue::Number(n) = v {
            // Due to floating point precision issues, we compare with an epsilon
            assert!((n - 5.65685424949238).abs() < 0.000000001);
        } else {
            panic!("Expected numeric result");
        }
    }
}
