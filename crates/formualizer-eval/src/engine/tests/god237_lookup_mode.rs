//! GOD-237: the lookup-mode argument law for MATCH / VLOOKUP / HLOOKUP / LOOKUP.
//!
//! Oracle: desktop Microsoft Excel 16.105.3, driven by AppleScript against a
//! fresh workbook, 222 rows, every result read back twice (AppleScript and the
//! cached sheet XML) and cross-checked; all controls green, no refusals.
//! Receipt (in the bakeoff checkout, not this fork):
//!   artifacts/private/god237/round/receipts/god237_lookup_mode_oracle_receipt.json
//!   sha256 ed7be728cc270ed17db6d6687af1d09575163c748831033eb53e7adf0908571d
//! Engine baseline pinned at fork commit 51c57cde:
//!   artifacts/private/god237/round/receipts/god237_engine_baseline_pin_51c57cde.json
//!   sha256 8aeb83d195efcdf8a23237007ed5d6fa32a9310d316646b483f319aa16c206e5
//!
//! THIS FILE MUST NEVER BE EDITED TO MAKE AN ENGINE RESULT AGREE. The values on
//! the right-hand side are what Microsoft Excel returned. If the engine
//! disagrees, the engine is wrong -- fix the engine, or record the divergence
//! explicitly as the `#[ignore]`d known-divergence tests at the bottom do.
//!
//! THE MEASURED LAW
//! ================
//!
//! MATCH's 3rd argument is coerced as a NUMBER, then taken by SIGN
//! (sign-coercion, NOT truncation: 0.5 -> 1, -0.5 -> -1, 2 -> 1, -2 -> -1):
//!
//!   ABSENT              -> 1 (approximate)     =MATCH(x,r)
//!   PRESENT-BUT-EMPTY   -> 0 (exact)           =MATCH(x,r,)
//!   TRUE                -> 1                   FALSE -> 0
//!   a blank-cell ref    -> 0
//!   "0" / "1"           -> parse as numbers
//!   "TRUE"/"FALSE"/"x"/"" -> #VALUE!
//!   NA()                -> #N/A
//!
//! VLOOKUP's and HLOOKUP's 4th argument is coerced as a BOOLEAN, which is a
//! DIFFERENT law from MATCH's -- the two disagree on -1, on 0.5, and on the
//! text forms:
//!
//!   ABSENT              -> TRUE (approximate)
//!   PRESENT-BUT-EMPTY   -> FALSE (exact)
//!   any non-zero number, INCLUDING -1 and 0.5 -> TRUE
//!   a blank-cell ref    -> FALSE
//!   "TRUE" / "FALSE"    -> accepted
//!   "0" / "1" / ""      -> #VALUE!
//!   NA()                -> #N/A
//!
//! LOOKUP has no mode argument at all.
//!
//! The defect this round fixed (OT-118) was confined to `MatchFn::eval`: its
//! read of the third argument matched only `Number` / `Int` / `Text`, so a
//! `Boolean` or an `Empty` fell through the catch-all arm and silently kept the
//! approximate default of 1. VLOOKUP/HLOOKUP already implemented the boolean
//! law via `range_lookup_is_approximate` + `coercion::to_logical`, and the
//! literal `MATCH(x,r,)` form already worked via the `is_omitted()` branch.

use crate::engine::{Engine, EvalConfig};
use crate::test_workbook::TestWorkbook;
use formualizer_common::LiteralValue;
use formualizer_parse::parse;

fn mk() -> Engine<TestWorkbook> {
    Engine::new(
        TestWorkbook::new(),
        EvalConfig::default().with_parallel(false),
    )
}

fn num(e: &mut Engine<TestWorkbook>, c: u32, r: u32, v: f64) {
    e.set_cell_value("S", r, c, LiteralValue::Number(v))
        .unwrap();
}

fn txt(e: &mut Engine<TestWorkbook>, c: u32, r: u32, v: &str) {
    e.set_cell_value("S", r, c, LiteralValue::Text(v.to_string()))
        .unwrap();
}

fn boolean(e: &mut Engine<TestWorkbook>, c: u32, r: u32, v: bool) {
    e.set_cell_value("S", r, c, LiteralValue::Boolean(v))
        .unwrap();
}

/// Evaluate `f` in a scratch cell and render the result the way the oracle
/// receipt renders Excel's: an integer, an Excel error token, or the text.
fn ev(e: &mut Engine<TestWorkbook>, f: &str) -> String {
    e.set_cell_formula("S", 300, 30, parse(f).unwrap()).unwrap();
    e.evaluate_all().unwrap();
    match e.get_cell_value("S", 300, 30) {
        Some(LiteralValue::Int(i)) => format!("{i}"),
        Some(LiteralValue::Number(n)) => {
            if n.fract() == 0.0 {
                format!("{}", n as i64)
            } else {
                format!("{n}")
            }
        }
        Some(LiteralValue::Text(t)) => t,
        // The receipt carries two readings per row: the cached sheet XML (where
        // a boolean is "0"/"1" with type "b") and AppleScript's `string value`
        // (where it is "FALSE"/"TRUE"). This renderer targets the AppleScript
        // surface, which is the one the boolean rows are compared against.
        Some(LiteralValue::Boolean(b)) => (if b { "TRUE" } else { "FALSE" }).to_string(),
        Some(LiteralValue::Error(x)) => format!("{}", x.kind),
        o => format!("{o:?}"),
    }
}

fn row(id: &str, excel: &str, got: String, fails: &mut u32) {
    let ok = got == excel;
    if !ok {
        *fails += 1;
    }
    println!(
        "  {:<26} excel={:<8} fz={:<8} {}",
        id,
        excel,
        got,
        if ok { "ok" } else { "** DIVERGES **" }
    );
}

/// Build the measured GOD-237 fixture sheet exactly as the oracle workbook had
/// it (`sheet_layout` in the receipt).
///
///   B2:G2  1, 3, 5, 5, 5, 9        ascending, duplicate-bearing
///   B3:G3  101..106                its HLOOKUP result row
///   B4:F4  5, 1, 5, 9, 5           unsorted, duplicate-bearing
///   B5:F5  201..205                its HLOOKUP result row
///   B6:D6  (left blank)            the blank RANGE used as a mode argument
///   B8:E8  1, "b", FALSE, TRUE     the mixed-class vector
///   L1:M6  the VLOOKUP table over the ascending vector
///   N1:O5  the VLOOKUP table over the unsorted vector
///   P1     (left blank)            the blank CELL used as a mode argument
///   P2..P17  the mode-argument reference slots
fn fixture() -> Engine<TestWorkbook> {
    let mut e = mk();

    // B2:G2 ascending with a run of three 5s, and its result row B3:G3.
    num(&mut e, 2, 2, 1.0);
    num(&mut e, 3, 2, 3.0);
    num(&mut e, 4, 2, 5.0);
    num(&mut e, 5, 2, 5.0);
    num(&mut e, 6, 2, 5.0);
    num(&mut e, 7, 2, 9.0);
    num(&mut e, 2, 3, 101.0);
    num(&mut e, 3, 3, 102.0);
    num(&mut e, 4, 3, 103.0);
    num(&mut e, 5, 3, 104.0);
    num(&mut e, 6, 3, 105.0);
    num(&mut e, 7, 3, 106.0);

    // B4:F4 unsorted with duplicates, and its result row B5:F5.
    num(&mut e, 2, 4, 5.0);
    num(&mut e, 3, 4, 1.0);
    num(&mut e, 4, 4, 5.0);
    num(&mut e, 5, 4, 9.0);
    num(&mut e, 6, 4, 5.0);
    num(&mut e, 2, 5, 201.0);
    num(&mut e, 3, 5, 202.0);
    num(&mut e, 4, 5, 203.0);
    num(&mut e, 5, 5, 204.0);
    num(&mut e, 6, 5, 205.0);

    // B6:D6 deliberately left blank.

    // B8:E8: one member of each of Excel's four lookup classes.
    num(&mut e, 2, 8, 1.0);
    txt(&mut e, 3, 8, "b");
    boolean(&mut e, 4, 8, false);
    boolean(&mut e, 5, 8, true);

    // L1:M6 -- VLOOKUP table, ascending key column.
    for (i, (k, v)) in [
        (1.0, 101.0),
        (3.0, 102.0),
        (5.0, 103.0),
        (5.0, 104.0),
        (5.0, 105.0),
        (9.0, 106.0),
    ]
    .iter()
    .enumerate()
    {
        num(&mut e, 12, i as u32 + 1, *k);
        num(&mut e, 13, i as u32 + 1, *v);
    }

    // N1:O5 -- VLOOKUP table, unsorted key column.
    for (i, (k, v)) in [
        (5.0, 201.0),
        (1.0, 202.0),
        (5.0, 203.0),
        (9.0, 204.0),
        (5.0, 205.0),
    ]
    .iter()
    .enumerate()
    {
        num(&mut e, 14, i as u32 + 1, *k);
        num(&mut e, 15, i as u32 + 1, *v);
    }

    // P1 deliberately left blank -- the blank-cell mode reference.
    boolean(&mut e, 16, 2, false); // P2  =FALSE
    boolean(&mut e, 16, 3, true); // P3  =TRUE
    num(&mut e, 16, 4, 0.0); // P4
    num(&mut e, 16, 5, 1.0); // P5
    num(&mut e, 16, 6, -1.0); // P6
    num(&mut e, 16, 7, 0.5); // P7
    num(&mut e, 16, 8, -0.5); // P8
    num(&mut e, 16, 9, 2.0); // P9
    num(&mut e, 16, 10, -2.0); // P10
    // P11..P14 were AUTHORED as text in the oracle script but Excel's
    // `set value` auto-parsed them, so the workbook actually holds a number 0,
    // a number 1, a boolean TRUE and a boolean FALSE. The receipt records this
    // explicitly in `fixture_type_overrides`, and this fixture is built to
    // match what Excel ACTUALLY holds -- which is why the row ids read
    // `refstr0` / `refstrtrue` while the cells are not text at all.
    num(&mut e, 16, 11, 0.0); // P11  authored "0",     held as number 0
    num(&mut e, 16, 12, 1.0); // P12  authored "1",     held as number 1
    boolean(&mut e, 16, 13, true); // P13  authored "TRUE",  held as boolean TRUE
    boolean(&mut e, 16, 14, false); // P14  authored "FALSE", held as boolean FALSE
    txt(&mut e, 16, 15, "x"); // P15  genuinely text
    // P16 is a FORMULA producing the empty string, and P17 a formula producing
    // #N/A -- both are seeded as formulas because their being formula results
    // is exactly what the out-of-scope `#REF!` class turns on.
    e.set_cell_formula("S", 16, 16, parse("=\"\"").unwrap())
        .unwrap();
    e.set_cell_formula("S", 17, 16, parse("=NA()").unwrap())
        .unwrap();

    e
}

// ---------------------------------------------------------------------------
// The defect this round fixed: OT-118.
// ---------------------------------------------------------------------------

/// The 14 rows the OT-118 fix moves, at Excel's measured values.
///
/// Before the fix `MatchFn::eval` matched only `Number` / `Int` / `Text` when
/// reading the third argument, so `FALSE` and a blank-cell reference fell
/// through its catch-all arm and silently kept the approximate default of 1.
/// Every row below therefore answered with the APPROXIMATE result on the
/// baseline build; all 14 are in the baseline pin's `genuine_disagree_ids`.
///
/// Oracle receipt:
///   artifacts/private/god237/round/receipts/god237_lookup_mode_oracle_receipt.json
///   sha256 ed7be728cc270ed17db6d6687af1d09575163c748831033eb53e7adf0908571d
#[test]
fn god237_match_mode_boolean_and_blank_reference_coerce_by_sign() {
    let mut e = fixture();
    let mut fails = 0u32;
    println!("=== GOD-237 OT-118 class-A rows (Excel 16.105.3) ===");

    // Ascending {1,3,5,5,5,9}, key 5. Exact -> 3 (first of the run);
    // approximate -> 5 (far end of the run).
    row(
        "m-s5-false",
        "3",
        ev(&mut e, "=MATCH(5,B2:G2,FALSE)"),
        &mut fails,
    );
    row(
        "m-s5-refblank",
        "3",
        ev(&mut e, "=MATCH(5,B2:G2,P1)"),
        &mut fails,
    );
    row(
        "m-s5-reffalse",
        "3",
        ev(&mut e, "=MATCH(5,B2:G2,P2)"),
        &mut fails,
    );

    // Ascending, key 6: absent from the vector, so exact is #N/A.
    row(
        "m-s6-false",
        "#N/A",
        ev(&mut e, "=MATCH(6,B2:G2,FALSE)"),
        &mut fails,
    );
    row(
        "m-s6-refblank",
        "#N/A",
        ev(&mut e, "=MATCH(6,B2:G2,P1)"),
        &mut fails,
    );
    row(
        "m-s6-reffalse",
        "#N/A",
        ev(&mut e, "=MATCH(6,B2:G2,P2)"),
        &mut fails,
    );

    // Unsorted {5,1,5,9,5}, key 5. Exact -> 1 (first occurrence);
    // approximate -> 3 (where the unguarded bisection lands, GOD-234).
    row(
        "m-u5-false",
        "1",
        ev(&mut e, "=MATCH(5,B4:F4,FALSE)"),
        &mut fails,
    );
    row(
        "m-u5-refblank",
        "1",
        ev(&mut e, "=MATCH(5,B4:F4,P1)"),
        &mut fails,
    );
    row(
        "m-u5-reffalse",
        "1",
        ev(&mut e, "=MATCH(5,B4:F4,P2)"),
        &mut fails,
    );

    // Unsorted, key 6.
    row(
        "m-u6-false",
        "#N/A",
        ev(&mut e, "=MATCH(6,B4:F4,FALSE)"),
        &mut fails,
    );
    row(
        "m-u6-refblank",
        "#N/A",
        ev(&mut e, "=MATCH(6,B4:F4,P1)"),
        &mut fails,
    );
    row(
        "m-u6-reffalse",
        "#N/A",
        ev(&mut e, "=MATCH(6,B4:F4,P2)"),
        &mut fails,
    );

    // The two GOD-234 addendum rows re-measured on this round's fixture; they
    // are the same law and the same two cases the now-un-`#[ignore]`d test
    // `god234_addendum_known_divergence_match_type_boolean_and_blank` asserts.
    row(
        "p-god234-x-mt-false",
        "3",
        ev(&mut e, "=MATCH(5,B2:G2,FALSE)"),
        &mut fails,
    );
    row(
        "p-god234-x-mt-blankref",
        "3",
        ev(&mut e, "=MATCH(5,B2:G2,P1)"),
        &mut fails,
    );

    assert_eq!(
        fails, 0,
        "{fails} OT-118 row(s) diverge from the GOD-237 oracle; do NOT edit the \
         expectations above -- they are what desktop Excel returned"
    );
}

// ---------------------------------------------------------------------------
// The arms the law depends on that were ALREADY correct. Nothing else pins
// these, so a future change to the third-argument read cannot regress them
// silently.
// ---------------------------------------------------------------------------

/// MATCH's third argument: ABSENT vs PRESENT-BUT-EMPTY vs every other form the
/// oracle measured. Two different defaults live here -- an absent third
/// argument is 1 (approximate) while a present-but-empty one is 0 (exact) --
/// and the sign rule means 0.5, 2 and "1" are all approximate while -0.5 and
/// -2 are all descending-mode.
///
/// Oracle receipt:
///   artifacts/private/god237/round/receipts/god237_lookup_mode_oracle_receipt.json
///   sha256 ed7be728cc270ed17db6d6687af1d09575163c748831033eb53e7adf0908571d
#[test]
fn god237_match_mode_already_correct_arms() {
    let mut e = fixture();
    let mut fails = 0u32;
    println!("=== GOD-237 MATCH mode arms already correct on the baseline ===");

    // The two defaults.
    row(
        "m-s5-absent",
        "5",
        ev(&mut e, "=MATCH(5,B2:G2)"),
        &mut fails,
    );
    row(
        "m-s5-empty",
        "3",
        ev(&mut e, "=MATCH(5,B2:G2,)"),
        &mut fails,
    );
    row(
        "m-s6-absent",
        "5",
        ev(&mut e, "=MATCH(6,B2:G2)"),
        &mut fails,
    );
    row(
        "m-s6-empty",
        "#N/A",
        ev(&mut e, "=MATCH(6,B2:G2,)"),
        &mut fails,
    );
    row(
        "m-u5-absent",
        "3",
        ev(&mut e, "=MATCH(5,B4:F4)"),
        &mut fails,
    );
    row(
        "m-u5-empty",
        "1",
        ev(&mut e, "=MATCH(5,B4:F4,)"),
        &mut fails,
    );
    row(
        "m-u6-absent",
        "3",
        ev(&mut e, "=MATCH(6,B4:F4)"),
        &mut fails,
    );
    row(
        "m-u6-empty",
        "#N/A",
        ev(&mut e, "=MATCH(6,B4:F4,)"),
        &mut fails,
    );

    // TRUE is 1. (It was right on the baseline only by accident -- the
    // catch-all arm left the default of 1 in place. Now it is right on purpose,
    // so this row must keep passing.)
    row(
        "m-s5-true",
        "5",
        ev(&mut e, "=MATCH(5,B2:G2,TRUE)"),
        &mut fails,
    );
    row(
        "m-s6-true",
        "5",
        ev(&mut e, "=MATCH(6,B2:G2,TRUE)"),
        &mut fails,
    );
    row(
        "m-u5-true",
        "3",
        ev(&mut e, "=MATCH(5,B4:F4,TRUE)"),
        &mut fails,
    );
    row(
        "m-u6-true",
        "3",
        ev(&mut e, "=MATCH(6,B4:F4,TRUE)"),
        &mut fails,
    );
    row(
        "m-s5-reftrue",
        "5",
        ev(&mut e, "=MATCH(5,B2:G2,P3)"),
        &mut fails,
    );
    row(
        "m-u5-reftrue",
        "3",
        ev(&mut e, "=MATCH(5,B4:F4,P3)"),
        &mut fails,
    );

    // Literal numbers, taken by SIGN and not by truncation.
    row(
        "m-s5-zero",
        "3",
        ev(&mut e, "=MATCH(5,B2:G2,0)"),
        &mut fails,
    );
    row("m-s5-one", "5", ev(&mut e, "=MATCH(5,B2:G2,1)"), &mut fails);
    row(
        "m-s5-half",
        "5",
        ev(&mut e, "=MATCH(5,B2:G2,0.5)"),
        &mut fails,
    );
    row("m-s5-two", "5", ev(&mut e, "=MATCH(5,B2:G2,2)"), &mut fails);
    row(
        "m-s6-zero",
        "#N/A",
        ev(&mut e, "=MATCH(6,B2:G2,0)"),
        &mut fails,
    );
    row(
        "m-s6-half",
        "5",
        ev(&mut e, "=MATCH(6,B2:G2,0.5)"),
        &mut fails,
    );

    // Numeric references.
    row(
        "m-s5-ref0",
        "3",
        ev(&mut e, "=MATCH(5,B2:G2,P4)"),
        &mut fails,
    );
    row(
        "m-s5-ref1",
        "5",
        ev(&mut e, "=MATCH(5,B2:G2,P5)"),
        &mut fails,
    );
    row(
        "m-s5-refhalf",
        "5",
        ev(&mut e, "=MATCH(5,B2:G2,P7)"),
        &mut fails,
    );

    // Text that PARSES as a number is taken as that number; text that does not
    // -- including "TRUE"/"FALSE", which the VLOOKUP law DOES accept -- is
    // #VALUE!. This is the sharpest discriminator between the two laws.
    row(
        "m-s5-str0",
        "3",
        ev(&mut e, "=MATCH(5,B2:G2,\"0\")"),
        &mut fails,
    );
    row(
        "m-s5-str1",
        "5",
        ev(&mut e, "=MATCH(5,B2:G2,\"1\")"),
        &mut fails,
    );
    row(
        "m-s5-strtrue",
        "#VALUE!",
        ev(&mut e, "=MATCH(5,B2:G2,\"TRUE\")"),
        &mut fails,
    );
    row(
        "m-s5-strfalse",
        "#VALUE!",
        ev(&mut e, "=MATCH(5,B2:G2,\"FALSE\")"),
        &mut fails,
    );
    row(
        "m-s5-strx",
        "#VALUE!",
        ev(&mut e, "=MATCH(5,B2:G2,\"x\")"),
        &mut fails,
    );
    row(
        "m-s5-emptystr",
        "#VALUE!",
        ev(&mut e, "=MATCH(5,B2:G2,\"\")"),
        &mut fails,
    );
    // P11 and P13 are the auto-parsed slots -- number 0 and boolean TRUE, per
    // the receipt's `fixture_type_overrides`. `m-s5-refstrtrue` therefore
    // exercises the Boolean arm this round added, not a text arm.
    row(
        "m-s5-refstr0",
        "3",
        ev(&mut e, "=MATCH(5,B2:G2,P11)"),
        &mut fails,
    );
    row(
        "m-s5-refstrtrue",
        "5",
        ev(&mut e, "=MATCH(5,B2:G2,P13)"),
        &mut fails,
    );

    // An error in the mode slot propagates.
    row(
        "m-s5-na",
        "#N/A",
        ev(&mut e, "=MATCH(5,B2:G2,NA())"),
        &mut fails,
    );

    assert_eq!(
        fails, 0,
        "{fails} MATCH mode row(s) diverge from the GOD-237 oracle; do NOT edit \
         the expectations above -- they are what desktop Excel returned"
    );
}

/// VLOOKUP's and HLOOKUP's fourth argument is a BOOLEAN, not a number, and the
/// difference is observable: -1 and 0.5 are TRUE here (approximate) where MATCH
/// takes -1 as descending-mode; "TRUE"/"FALSE" are accepted here where MATCH
/// rejects them; and "0"/"1" are #VALUE! here where MATCH parses them.
///
/// Oracle receipt:
///   artifacts/private/god237/round/receipts/god237_lookup_mode_oracle_receipt.json
///   sha256 ed7be728cc270ed17db6d6687af1d09575163c748831033eb53e7adf0908571d
#[test]
fn god237_vlookup_hlookup_mode_is_a_boolean() {
    let mut e = fixture();
    let mut fails = 0u32;
    println!("=== GOD-237 VLOOKUP/HLOOKUP range_lookup arms ===");

    // --- VLOOKUP over the ascending table L1:M6, key 5 then key 6. ---
    row(
        "v-s5-absent",
        "105",
        ev(&mut e, "=VLOOKUP(5,L1:M6,2)"),
        &mut fails,
    );
    row(
        "v-s5-empty",
        "103",
        ev(&mut e, "=VLOOKUP(5,L1:M6,2,)"),
        &mut fails,
    );
    row(
        "v-s5-false",
        "103",
        ev(&mut e, "=VLOOKUP(5,L1:M6,2,FALSE)"),
        &mut fails,
    );
    row(
        "v-s5-true",
        "105",
        ev(&mut e, "=VLOOKUP(5,L1:M6,2,TRUE)"),
        &mut fails,
    );
    row(
        "v-s5-zero",
        "103",
        ev(&mut e, "=VLOOKUP(5,L1:M6,2,0)"),
        &mut fails,
    );
    row(
        "v-s5-one",
        "105",
        ev(&mut e, "=VLOOKUP(5,L1:M6,2,1)"),
        &mut fails,
    );
    // -1 is TRUE here. MATCH would read -1 as descending-mode.
    row(
        "v-s5-neg1",
        "105",
        ev(&mut e, "=VLOOKUP(5,L1:M6,2,-1)"),
        &mut fails,
    );
    row(
        "v-s5-half",
        "105",
        ev(&mut e, "=VLOOKUP(5,L1:M6,2,0.5)"),
        &mut fails,
    );
    // "0"/"1" are #VALUE! here. MATCH parses them as numbers.
    row(
        "v-s5-str0",
        "#VALUE!",
        ev(&mut e, "=VLOOKUP(5,L1:M6,2,\"0\")"),
        &mut fails,
    );
    row(
        "v-s5-str1",
        "#VALUE!",
        ev(&mut e, "=VLOOKUP(5,L1:M6,2,\"1\")"),
        &mut fails,
    );
    // "TRUE"/"FALSE" are accepted here. MATCH rejects them.
    row(
        "v-s5-strtrue",
        "105",
        ev(&mut e, "=VLOOKUP(5,L1:M6,2,\"TRUE\")"),
        &mut fails,
    );
    row(
        "v-s5-strfalse",
        "103",
        ev(&mut e, "=VLOOKUP(5,L1:M6,2,\"FALSE\")"),
        &mut fails,
    );
    row(
        "v-s5-emptystr",
        "#VALUE!",
        ev(&mut e, "=VLOOKUP(5,L1:M6,2,\"\")"),
        &mut fails,
    );
    row(
        "v-s5-na",
        "#N/A",
        ev(&mut e, "=VLOOKUP(5,L1:M6,2,NA())"),
        &mut fails,
    );
    row(
        "v-s5-refblank",
        "103",
        ev(&mut e, "=VLOOKUP(5,L1:M6,2,P1)"),
        &mut fails,
    );
    row(
        "v-s5-reffalse",
        "103",
        ev(&mut e, "=VLOOKUP(5,L1:M6,2,P2)"),
        &mut fails,
    );
    row(
        "v-s5-reftrue",
        "105",
        ev(&mut e, "=VLOOKUP(5,L1:M6,2,P3)"),
        &mut fails,
    );
    row(
        "v-s5-ref0",
        "103",
        ev(&mut e, "=VLOOKUP(5,L1:M6,2,P4)"),
        &mut fails,
    );
    row(
        "v-s5-ref1",
        "105",
        ev(&mut e, "=VLOOKUP(5,L1:M6,2,P5)"),
        &mut fails,
    );

    row(
        "v-s6-absent",
        "105",
        ev(&mut e, "=VLOOKUP(6,L1:M6,2)"),
        &mut fails,
    );
    row(
        "v-s6-empty",
        "#N/A",
        ev(&mut e, "=VLOOKUP(6,L1:M6,2,)"),
        &mut fails,
    );
    row(
        "v-s6-false",
        "#N/A",
        ev(&mut e, "=VLOOKUP(6,L1:M6,2,FALSE)"),
        &mut fails,
    );
    row(
        "v-s6-true",
        "105",
        ev(&mut e, "=VLOOKUP(6,L1:M6,2,TRUE)"),
        &mut fails,
    );
    row(
        "v-s6-neg1",
        "105",
        ev(&mut e, "=VLOOKUP(6,L1:M6,2,-1)"),
        &mut fails,
    );
    row(
        "v-s6-strfalse",
        "#N/A",
        ev(&mut e, "=VLOOKUP(6,L1:M6,2,\"FALSE\")"),
        &mut fails,
    );
    row(
        "v-s6-refblank",
        "#N/A",
        ev(&mut e, "=VLOOKUP(6,L1:M6,2,P1)"),
        &mut fails,
    );
    row(
        "v-s6-reffalse",
        "#N/A",
        ev(&mut e, "=VLOOKUP(6,L1:M6,2,P2)"),
        &mut fails,
    );
    row(
        "v-s6-reftrue",
        "105",
        ev(&mut e, "=VLOOKUP(6,L1:M6,2,P3)"),
        &mut fails,
    );

    // --- VLOOKUP over the UNSORTED table N1:O5. ---
    row(
        "v-u5-absent",
        "203",
        ev(&mut e, "=VLOOKUP(5,N1:O5,2)"),
        &mut fails,
    );
    row(
        "v-u5-empty",
        "201",
        ev(&mut e, "=VLOOKUP(5,N1:O5,2,)"),
        &mut fails,
    );
    row(
        "v-u5-false",
        "201",
        ev(&mut e, "=VLOOKUP(5,N1:O5,2,FALSE)"),
        &mut fails,
    );
    row(
        "v-u5-refblank",
        "201",
        ev(&mut e, "=VLOOKUP(5,N1:O5,2,P1)"),
        &mut fails,
    );
    row(
        "v-u5-reffalse",
        "201",
        ev(&mut e, "=VLOOKUP(5,N1:O5,2,P2)"),
        &mut fails,
    );
    row(
        "v-u5-reftrue",
        "203",
        ev(&mut e, "=VLOOKUP(5,N1:O5,2,P3)"),
        &mut fails,
    );
    row(
        "v-u6-false",
        "#N/A",
        ev(&mut e, "=VLOOKUP(6,N1:O5,2,FALSE)"),
        &mut fails,
    );
    row(
        "v-u6-refblank",
        "#N/A",
        ev(&mut e, "=VLOOKUP(6,N1:O5,2,P1)"),
        &mut fails,
    );
    row(
        "v-u6-reftrue",
        "203",
        ev(&mut e, "=VLOOKUP(6,N1:O5,2,P3)"),
        &mut fails,
    );

    // --- HLOOKUP over B2:G3 (ascending) and B4:F5 (unsorted). ---
    row(
        "h-s5-absent",
        "105",
        ev(&mut e, "=HLOOKUP(5,B2:G3,2)"),
        &mut fails,
    );
    row(
        "h-s5-empty",
        "103",
        ev(&mut e, "=HLOOKUP(5,B2:G3,2,)"),
        &mut fails,
    );
    row(
        "h-s5-false",
        "103",
        ev(&mut e, "=HLOOKUP(5,B2:G3,2,FALSE)"),
        &mut fails,
    );
    row(
        "h-s5-true",
        "105",
        ev(&mut e, "=HLOOKUP(5,B2:G3,2,TRUE)"),
        &mut fails,
    );
    row(
        "h-s5-neg1",
        "105",
        ev(&mut e, "=HLOOKUP(5,B2:G3,2,-1)"),
        &mut fails,
    );
    row(
        "h-s5-half",
        "105",
        ev(&mut e, "=HLOOKUP(5,B2:G3,2,0.5)"),
        &mut fails,
    );
    row(
        "h-s5-str0",
        "#VALUE!",
        ev(&mut e, "=HLOOKUP(5,B2:G3,2,\"0\")"),
        &mut fails,
    );
    row(
        "h-s5-str1",
        "#VALUE!",
        ev(&mut e, "=HLOOKUP(5,B2:G3,2,\"1\")"),
        &mut fails,
    );
    row(
        "h-s5-strtrue",
        "105",
        ev(&mut e, "=HLOOKUP(5,B2:G3,2,\"TRUE\")"),
        &mut fails,
    );
    row(
        "h-s5-strfalse",
        "103",
        ev(&mut e, "=HLOOKUP(5,B2:G3,2,\"FALSE\")"),
        &mut fails,
    );
    row(
        "h-s5-emptystr",
        "#VALUE!",
        ev(&mut e, "=HLOOKUP(5,B2:G3,2,\"\")"),
        &mut fails,
    );
    row(
        "h-s5-na",
        "#N/A",
        ev(&mut e, "=HLOOKUP(5,B2:G3,2,NA())"),
        &mut fails,
    );
    row(
        "h-s5-refblank",
        "103",
        ev(&mut e, "=HLOOKUP(5,B2:G3,2,P1)"),
        &mut fails,
    );
    row(
        "h-s5-reffalse",
        "103",
        ev(&mut e, "=HLOOKUP(5,B2:G3,2,P2)"),
        &mut fails,
    );
    row(
        "h-s5-reftrue",
        "105",
        ev(&mut e, "=HLOOKUP(5,B2:G3,2,P3)"),
        &mut fails,
    );
    row(
        "h-s6-false",
        "#N/A",
        ev(&mut e, "=HLOOKUP(6,B2:G3,2,FALSE)"),
        &mut fails,
    );
    row(
        "h-s6-refblank",
        "#N/A",
        ev(&mut e, "=HLOOKUP(6,B2:G3,2,P1)"),
        &mut fails,
    );
    row(
        "h-s6-neg1",
        "105",
        ev(&mut e, "=HLOOKUP(6,B2:G3,2,-1)"),
        &mut fails,
    );
    row(
        "h-u5-empty",
        "201",
        ev(&mut e, "=HLOOKUP(5,B4:F5,2,)"),
        &mut fails,
    );
    row(
        "h-u5-false",
        "201",
        ev(&mut e, "=HLOOKUP(5,B4:F5,2,FALSE)"),
        &mut fails,
    );
    row(
        "h-u5-refblank",
        "201",
        ev(&mut e, "=HLOOKUP(5,B4:F5,2,P1)"),
        &mut fails,
    );
    row(
        "h-u5-absent",
        "203",
        ev(&mut e, "=HLOOKUP(5,B4:F5,2)"),
        &mut fails,
    );
    row(
        "h-u6-false",
        "#N/A",
        ev(&mut e, "=HLOOKUP(6,B4:F5,2,FALSE)"),
        &mut fails,
    );
    row(
        "h-u6-reftrue",
        "203",
        ev(&mut e, "=HLOOKUP(6,B4:F5,2,P3)"),
        &mut fails,
    );

    assert_eq!(
        fails, 0,
        "{fails} VLOOKUP/HLOOKUP mode row(s) diverge from the GOD-237 oracle; do \
         NOT edit the expectations above -- they are what desktop Excel returned"
    );
}

/// LOOKUP has NO mode argument, so the ordinary rows are pinned here to make
/// that explicit and to keep the family's shape honest. The two unsorted-tie
/// rows and the present-but-empty-result-vector row are out of scope and are
/// asserted, at Excel's values, in the `#[ignore]`d tests below.
///
/// Oracle receipt:
///   artifacts/private/god237/round/receipts/god237_lookup_mode_oracle_receipt.json
///   sha256 ed7be728cc270ed17db6d6687af1d09575163c748831033eb53e7adf0908571d
#[test]
fn god237_lookup_has_no_mode_argument() {
    let mut e = fixture();
    let mut fails = 0u32;
    println!("=== GOD-237 LOOKUP rows ===");
    row(
        "l-sorted-5",
        "105",
        ev(&mut e, "=LOOKUP(5,B2:G2,B3:G3)"),
        &mut fails,
    );
    row(
        "l-sorted-6",
        "105",
        ev(&mut e, "=LOOKUP(6,B2:G2,B3:G3)"),
        &mut fails,
    );
    row(
        "l-novector-5",
        "5",
        ev(&mut e, "=LOOKUP(5,B2:G2)"),
        &mut fails,
    );
    row(
        "l-below-all",
        "#N/A",
        ev(&mut e, "=LOOKUP(0,B2:G2,B3:G3)"),
        &mut fails,
    );
    row(
        "l-above-all",
        "106",
        ev(&mut e, "=LOOKUP(99,B2:G2,B3:G3)"),
        &mut fails,
    );
    row(
        "l-arrayform-5",
        "5",
        ev(&mut e, "=LOOKUP(5,{1,3,5,5,5,9})"),
        &mut fails,
    );
    assert_eq!(
        fails, 0,
        "{fails} LOOKUP row(s) diverge from the GOD-237 oracle; do NOT edit the \
         expectations above -- they are what desktop Excel returned"
    );
}

// ---------------------------------------------------------------------------
// KNOWN DIVERGENCES -- out of scope for GOD-237, each an open thread.
//
// These are `#[ignore]`d and assert EXCEL's answer, so each one is RED today.
// They exist so that the 33 genuine disagreements in the baseline pin are
// accounted for EXHAUSTIVELY and none of them can later be read as a silent
// skip. Do NOT edit their expectations to make the engine agree; delete the
// `#[ignore]` when the corresponding thread is closed.
// ---------------------------------------------------------------------------

/// KNOWN DIVERGENCE (out of scope, measured), NEW in GOD-237: when MATCH's third
/// argument is a REFERENCE that Excel cannot reduce to a single value in the
/// formula's own row/column, Excel answers `#REF!` -- not `#VALUE!`, and not the
/// value the referenced cell holds.
///
/// Three shapes were measured, all `#REF!`: a cell holding `=""` (P16), a cell
/// holding `=NA()` (P17), and a multi-cell blank range (B6:D6). Note that the
/// literal forms of the same values are NOT `#REF!` -- `MATCH(5,B2:G2,"")` is
/// `#VALUE!` and `MATCH(5,B2:G2,NA())` is `#N/A`, both of which this engine
/// already reproduces (pinned in `god237_match_mode_already_correct_arms`).
/// So this is a property of the REFERENCE SLOT, not of the values.
///
/// This engine reduces the reference and answers with the underlying value.
/// Reproducing `#REF!` would require the argument evaluator to distinguish an
/// unreducible reference from a reduced one, which is an evaluator-wide change
/// far outside this round's pinned defect.
///
/// Baseline-pin rows: `m-s5-refemptystr`, `m-s5-refna`, `m-s5-refblankrange`,
/// `m-s6-refemptystr`, `m-s6-refna`, `m-s6-refblankrange`.
/// Oracle receipt:
///   artifacts/private/god237/round/receipts/god237_lookup_mode_oracle_receipt.json
///   sha256 ed7be728cc270ed17db6d6687af1d09575163c748831033eb53e7adf0908571d
#[test]
#[ignore = "GOD-237 open thread OT-120: an unreducible REFERENCE in MATCH's mode slot is #REF! in Excel; this engine reduces it"]
fn god237_known_divergence_unreducible_reference_in_match_mode_slot_is_ref() {
    let mut e = fixture();
    let mut fails = 0u32;
    println!("=== GOD-237 residual: #REF! from MATCH's mode reference slot ===");
    row(
        "m-s5-refemptystr",
        "#REF!",
        ev(&mut e, "=MATCH(5,B2:G2,P16)"),
        &mut fails,
    );
    row(
        "m-s5-refna",
        "#REF!",
        ev(&mut e, "=MATCH(5,B2:G2,P17)"),
        &mut fails,
    );
    row(
        "m-s5-refblankrange",
        "#REF!",
        ev(&mut e, "=MATCH(5,B2:G2,B6:D6)"),
        &mut fails,
    );
    row(
        "m-s6-refemptystr",
        "#REF!",
        ev(&mut e, "=MATCH(6,B2:G2,P16)"),
        &mut fails,
    );
    row(
        "m-s6-refna",
        "#REF!",
        ev(&mut e, "=MATCH(6,B2:G2,P17)"),
        &mut fails,
    );
    row(
        "m-s6-refblankrange",
        "#REF!",
        ev(&mut e, "=MATCH(6,B2:G2,B6:D6)"),
        &mut fails,
    );
    assert_eq!(
        fails, 0,
        "{fails} #REF!-slot row(s) still diverge -- expected while the open \
         thread is open; do NOT edit the expectations above"
    );
}

/// KNOWN DIVERGENCE (out of scope, measured), NEW in GOD-237: a MULTI-CELL RANGE
/// in VLOOKUP's or HLOOKUP's fourth argument is ARRAY-BROADCAST by Excel and the
/// result SPILLS -- one result per cell of the range.
///
/// `=VLOOKUP(5,L1:M6,2,B6:D6)` over the three blank cells B6:D6 spills three
/// copies of the exact-mode answer, and the receipt reads the anchor cell, so
/// the measured value is 103 (key 5) and `#N/A` (key 6). This engine has no
/// broadcast over that argument position; it reduces the range instead. The
/// MATCH sibling of the same shape is `#REF!` (previous test) -- the two
/// functions do NOT share this behaviour, which is a second reason it is its
/// own thread.
///
/// Baseline-pin rows: `v-s5-refblankrange`, `v-s6-refblankrange`,
/// `h-s5-refblankrange`, `h-s6-refblankrange`.
/// Oracle receipt:
///   artifacts/private/god237/round/receipts/god237_lookup_mode_oracle_receipt.json
///   sha256 ed7be728cc270ed17db6d6687af1d09575163c748831033eb53e7adf0908571d
#[test]
#[ignore = "GOD-237 open thread OT-121: Excel array-broadcasts a multi-cell range in VLOOKUP/HLOOKUP's 4th argument and spills; this engine does not"]
fn god237_known_divergence_vlookup_hlookup_mode_range_broadcasts() {
    let mut e = fixture();
    let mut fails = 0u32;
    println!("=== GOD-237 residual: VLOOKUP/HLOOKUP mode-range broadcast ===");
    row(
        "v-s5-refblankrange",
        "103",
        ev(&mut e, "=VLOOKUP(5,L1:M6,2,B6:D6)"),
        &mut fails,
    );
    row(
        "v-s6-refblankrange",
        "#N/A",
        ev(&mut e, "=VLOOKUP(6,L1:M6,2,B6:D6)"),
        &mut fails,
    );
    row(
        "h-s5-refblankrange",
        "103",
        ev(&mut e, "=HLOOKUP(5,B2:G3,2,B6:D6)"),
        &mut fails,
    );
    row(
        "h-s6-refblankrange",
        "#N/A",
        ev(&mut e, "=HLOOKUP(6,B2:G3,2,B6:D6)"),
        &mut fails,
    );
    assert_eq!(
        fails, 0,
        "{fails} mode-range broadcast row(s) still diverge -- expected while the \
         open thread is open; do NOT edit the expectations above"
    );
}

/// KNOWN DIVERGENCE (out of scope, measured), NEW in GOD-237: `LOOKUP` with a
/// PRESENT-BUT-EMPTY result vector.
///
/// `=LOOKUP(5,B2:G2,)` is `#VALUE!` in Excel. LOOKUP has no mode argument, so
/// the empty third slot is an empty RESULT VECTOR, and Excel refuses it rather
/// than treating it as absent or as a scalar. This engine answers `0` -- it
/// materialises the omission as a zero result vector.
///
/// Baseline-pin row: `l-emptyvector-5`.
/// Oracle receipt:
///   artifacts/private/god237/round/receipts/god237_lookup_mode_oracle_receipt.json
///   sha256 ed7be728cc270ed17db6d6687af1d09575163c748831033eb53e7adf0908571d
#[test]
#[ignore = "GOD-237 open thread OT-122: LOOKUP with a present-but-empty result vector is #VALUE! in Excel; this engine answers 0"]
fn god237_known_divergence_lookup_present_but_empty_result_vector() {
    let mut e = fixture();
    let mut fails = 0u32;
    println!("=== GOD-237 residual: LOOKUP empty result vector ===");
    row(
        "l-emptyvector-5",
        "#VALUE!",
        ev(&mut e, "=LOOKUP(5,B2:G2,)"),
        &mut fails,
    );
    assert_eq!(
        fails, 0,
        "{fails} LOOKUP empty-result-vector row(s) still diverge -- expected \
         while the open thread is open; do NOT edit the expectation above"
    );
}

/// KNOWN DIVERGENCE (out of scope, measured), NEW in GOD-237: CROSS-CLASS
/// collation on MATCH's approximate arm.
///
/// Over the mixed vector B8:E8 = `{1, "b", FALSE, TRUE}` Excel's approximate
/// search orders the four lookup classes numbers < text < FALSE < TRUE and
/// treats the vector as ascending in that order, so `MATCH(1,B8:E8,1)` is 1 and
/// `MATCH(2,B8:E8,1)` is also 1 (the largest value not exceeding the key, with
/// text and booleans all sorting ABOVE any number). This engine answers 4 for
/// both -- it projects out-of-class entries rather than collating them, so the
/// bisection walks past them to the far end.
///
/// The EXACT arm over the same vector agrees everywhere (`x-exact-1`,
/// `x-exact-b`, `x-exact-true`, `x-exact-false` are all green on the baseline),
/// so this is specifically the approximate arm's cross-class ordering, and it is
/// a change to the shared comparison helper GOD-234 landed -- squarely outside
/// this round's pinned defect.
///
/// Baseline-pin rows: `x-approx-1`, `x-approx-2`. (`x-approx-b` and
/// `x-approx-true` already agree and are pinned as controls below.)
/// Oracle receipt:
///   artifacts/private/god237/round/receipts/god237_lookup_mode_oracle_receipt.json
///   sha256 ed7be728cc270ed17db6d6687af1d09575163c748831033eb53e7adf0908571d
#[test]
#[ignore = "GOD-237 open thread OT-123: Excel collates numbers<text<FALSE<TRUE on MATCH's approximate arm; this engine projects out-of-class entries"]
fn god237_known_divergence_cross_class_collation_on_the_approximate_arm() {
    let mut e = fixture();
    let mut fails = 0u32;
    println!("=== GOD-237 residual: cross-class collation, approximate arm ===");
    row(
        "x-approx-1",
        "1",
        ev(&mut e, "=MATCH(1,B8:E8,1)"),
        &mut fails,
    );
    row(
        "x-approx-2",
        "1",
        ev(&mut e, "=MATCH(2,B8:E8,1)"),
        &mut fails,
    );
    assert_eq!(
        fails, 0,
        "{fails} cross-class collation row(s) still diverge -- expected while \
         the open thread is open; do NOT edit the expectations above"
    );
}

/// KNOWN DIVERGENCE (out of scope, measured): `match_type = -1` over a vector
/// that is NOT descending. This is OT-117, already recorded by GOD-234; the
/// GOD-237 oracle re-measured it on its own fixture and reproduces it, so the
/// four rows are pinned here too rather than left unaccounted for.
///
/// Over the ASCENDING vector `{1,3,5,5,5,9}` Excel answers `#N/A` for every
/// negative match_type; this engine finds a position.
///
/// Baseline-pin rows: `m-s5-neg1`, `m-s5-neghalf`, `m-s5-negtwo`,
/// `m-s5-refneg1`.
/// Oracle receipt:
///   artifacts/private/god237/round/receipts/god237_lookup_mode_oracle_receipt.json
///   sha256 ed7be728cc270ed17db6d6687af1d09575163c748831033eb53e7adf0908571d
#[test]
#[ignore = "GOD-234/GOD-237 open thread OT-117: match_type = -1 over non-descending data"]
fn god237_known_divergence_match_type_negative_over_ascending() {
    let mut e = fixture();
    let mut fails = 0u32;
    println!("=== GOD-237 residual: negative match_type over ascending data ===");
    row(
        "m-s5-neg1",
        "#N/A",
        ev(&mut e, "=MATCH(5,B2:G2,-1)"),
        &mut fails,
    );
    row(
        "m-s5-neghalf",
        "#N/A",
        ev(&mut e, "=MATCH(5,B2:G2,-0.5)"),
        &mut fails,
    );
    row(
        "m-s5-negtwo",
        "#N/A",
        ev(&mut e, "=MATCH(5,B2:G2,-2)"),
        &mut fails,
    );
    row(
        "m-s5-refneg1",
        "#N/A",
        ev(&mut e, "=MATCH(5,B2:G2,P6)"),
        &mut fails,
    );
    assert_eq!(
        fails, 0,
        "{fails} negative-match_type row(s) still diverge -- expected while the \
         open thread is open; do NOT edit the expectations above"
    );
}

/// KNOWN DIVERGENCE (out of scope, measured): `LOOKUP`'s own bisection over the
/// unsorted duplicate-bearing vector. This is OT-119, already recorded by
/// GOD-234; the GOD-237 oracle re-measured it and reproduces it.
///
/// Excel answers 203 (the third column) where this engine's `LOOKUP` answers
/// 205 (the fifth). `LOOKUP` keeps its own bisection in
/// `builtins/lookup/legacy.rs` and did not pick up the GOD-234 shared helper.
///
/// Baseline-pin rows: `l-unsorted-5`, `l-unsorted-6`.
/// Oracle receipt:
///   artifacts/private/god237/round/receipts/god237_lookup_mode_oracle_receipt.json
///   sha256 ed7be728cc270ed17db6d6687af1d09575163c748831033eb53e7adf0908571d
#[test]
#[ignore = "GOD-234/GOD-237 open thread OT-119: LOOKUP's own bisection disagrees with MATCH/VLOOKUP/HLOOKUP on an unsorted duplicate-bearing vector"]
fn god237_known_divergence_lookup_own_bisection_unsorted() {
    let mut e = fixture();
    let mut fails = 0u32;
    println!("=== GOD-237 residual: LOOKUP over the unsorted vector ===");
    row(
        "l-unsorted-5",
        "203",
        ev(&mut e, "=LOOKUP(5,B4:F4,B5:F5)"),
        &mut fails,
    );
    row(
        "l-unsorted-6",
        "203",
        ev(&mut e, "=LOOKUP(6,B4:F4,B5:F5)"),
        &mut fails,
    );
    assert_eq!(
        fails, 0,
        "{fails} LOOKUP row(s) still diverge -- expected while the open thread \
         is open; do NOT edit the expectations above"
    );
}

/// The mixed-class controls that DO agree, pinned so the cross-class thread
/// above is scoped honestly: the divergence is the approximate arm's collation,
/// not the exact arm's, and not every approximate row.
///
/// Oracle receipt:
///   artifacts/private/god237/round/receipts/god237_lookup_mode_oracle_receipt.json
///   sha256 ed7be728cc270ed17db6d6687af1d09575163c748831033eb53e7adf0908571d
#[test]
fn god237_mixed_class_vector_exact_arm_agrees() {
    let mut e = fixture();
    let mut fails = 0u32;
    println!("=== GOD-237 mixed-class vector {{1,\"b\",FALSE,TRUE}} ===");
    row(
        "x-exact-1",
        "1",
        ev(&mut e, "=MATCH(1,B8:E8,0)"),
        &mut fails,
    );
    row(
        "x-exact-b",
        "2",
        ev(&mut e, "=MATCH(\"b\",B8:E8,0)"),
        &mut fails,
    );
    row(
        "x-exact-true",
        "4",
        ev(&mut e, "=MATCH(TRUE,B8:E8,0)"),
        &mut fails,
    );
    row(
        "x-exact-false",
        "3",
        ev(&mut e, "=MATCH(FALSE,B8:E8,0)"),
        &mut fails,
    );
    row(
        "x-approx-b",
        "2",
        ev(&mut e, "=MATCH(\"b\",B8:E8,1)"),
        &mut fails,
    );
    row(
        "x-approx-true",
        "4",
        ev(&mut e, "=MATCH(TRUE,B8:E8,1)"),
        &mut fails,
    );
    assert_eq!(
        fails, 0,
        "{fails} mixed-class row(s) diverge from the GOD-237 oracle; do NOT edit \
         the expectations above -- they are what desktop Excel returned"
    );
}

// The oracle's two `INFO()` rows (`p-info-release` = "16.105",
// `p-info-system` = "mac") are the receipt's provenance controls. `INFO` is
// unimplemented in this engine and answers `#NAME?`; that permanent divergence
// is already recorded by
// `god234_addendum_info_release_control_is_unimplemented` in
// `god234_approximate_match_unsorted.rs` and is not duplicated here.
