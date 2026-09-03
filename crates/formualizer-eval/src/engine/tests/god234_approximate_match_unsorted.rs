//! GOD-234: Excel's approximate MATCH runs an unguarded binary search.
//!
//! Oracle: desktop Microsoft Excel 16.105.3, driven by AppleScript against a
//! fresh workbook, every result read back twice (AppleScript and the cached
//! sheet XML) and cross-checked; all controls green. Measured 2026-09-03.
//! Receipt (in the bakeoff checkout, not this fork):
//!   artifacts/private/god234/round/receipts/god234_match_oracle_receipt.json
//!
//! THIS FILE MUST NEVER BE EDITED TO MAKE AN ENGINE RESULT AGREE. The values on
//! the right-hand side are what Microsoft Excel returned. If the engine
//! disagrees, the engine is wrong -- fix the engine, or (if Excel is genuinely
//! being inconsistent) record the divergence explicitly as the
//! `MATCH(2, {3,1,5,2,4}, -1)` residual below does.
//!
//! The law the 27 discriminating rows below pin down, over the lookup vector
//! AFTER the existing projection of blanks and out-of-class entries:
//!
//!   lo, hi = 0, n - 1
//!   while lo <= hi:
//!       mid = (lo + hi) / 2
//!       c = cmp_for_lookup(a[mid], key)
//!       if c == 0:                                  # equality early exit,
//!           end = mid                               # then walk to the far end
//!           step = +1 if match_type == 1 else -1    # of the CONTIGUOUS run of
//!           while 0 <= end + step < n and \
//!                 cmp_for_lookup(a[end + step], key) == 0:
//!               end += step
//!           return end
//!       if (c < 0 if match_type == 1 else c > 0): lo = mid + 1
//!       else: hi = mid - 1
//!   return hi if hi >= 0 else #N/A
//!
//! The equal-run walk is pinned by the addendum rows `a-asc-run-mid-mt1-*`
//! (`MATCH(7, {1,5,7,7,7,9}, 1)` = 5, not 3) and `a-desc-run-mid-mtneg1-*`
//! (`MATCH(7, {9,7,7,7,5}, -1)` = 2, not 3).
//!
//! There is NO sortedness check. `MATCH("DGS10", B2:E2, 1)` is #N/A even though
//! DGS10 sits at position 4, and `MATCH("DGS7", B2:E2, 1)` is 3 rather than the
//! 4 a plain upper-bound bisection would give -- both are consequences of the
//! probe sequence, and together they rule out every guarded or scanning model.

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
        // The receipts carry two readings per row: the cached sheet XML (where a
        // boolean is "0"/"1" with type "b") and AppleScript's `string value`
        // (where it is "FALSE"/"TRUE"). This renderer targets the AppleScript
        // surface, which is the one the boolean control row is compared against.
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
        "  {:<24} excel={:<8} fz={:<8} {}",
        id,
        excel,
        got,
        if ok { "ok" } else { "** DIVERGES **" }
    );
}

/// Build the measured fixture sheet exactly as the oracle workbook had it.
fn fixture() -> Engine<TestWorkbook> {
    let mut e = mk();
    txt(&mut e, 2, 2, "DGS3");
    txt(&mut e, 3, 2, "DGS5");
    txt(&mut e, 4, 2, "DGS7");
    txt(&mut e, 5, 2, "DGS10");
    txt(&mut e, 2, 3, "DGS10");
    txt(&mut e, 3, 3, "DGS3");
    txt(&mut e, 4, 3, "DGS5");
    txt(&mut e, 5, 3, "DGS7");
    txt(&mut e, 2, 4, "DGS3");
    txt(&mut e, 4, 4, "DGS7");
    txt(&mut e, 5, 4, "DGS10");
    num(&mut e, 2, 5, 3.0);
    num(&mut e, 3, 5, 1.0);
    num(&mut e, 4, 5, 5.0);
    num(&mut e, 5, 5, 2.0);
    num(&mut e, 6, 5, 4.0);
    num(&mut e, 2, 6, 1.0);
    num(&mut e, 3, 6, 2.0);
    num(&mut e, 4, 6, 3.0);
    num(&mut e, 5, 6, 4.0);
    num(&mut e, 6, 6, 5.0);
    num(&mut e, 2, 7, 1.0);
    txt(&mut e, 3, 7, "DGS5");
    num(&mut e, 4, 7, 3.0);
    txt(&mut e, 5, 7, "DGS10");
    txt(&mut e, 2, 8, "DGS7");
    txt(&mut e, 3, 8, "DGS5");
    txt(&mut e, 4, 8, "DGS3");
    txt(&mut e, 5, 8, "AAA");
    txt(&mut e, 8, 1, "DGS3");
    txt(&mut e, 8, 2, "DGS5");
    txt(&mut e, 8, 3, "DGS7");
    txt(&mut e, 8, 4, "DGS10");
    num(&mut e, 10, 1, 1.0);
    num(&mut e, 10, 2, 2.0);
    num(&mut e, 10, 3, 3.0);
    num(&mut e, 10, 4, 4.0);
    num(&mut e, 11, 1, 100.0);
    num(&mut e, 12, 1, 200.0);
    num(&mut e, 13, 1, 300.0);
    num(&mut e, 14, 1, 400.0);
    num(&mut e, 11, 2, 500.0);
    num(&mut e, 12, 2, 600.0);
    num(&mut e, 13, 2, 700.0);
    num(&mut e, 14, 2, 800.0);
    num(&mut e, 11, 3, 900.0);
    num(&mut e, 12, 3, 1000.0);
    num(&mut e, 13, 3, 1100.0);
    num(&mut e, 14, 3, 1200.0);
    num(&mut e, 11, 4, 1300.0);
    num(&mut e, 12, 4, 1400.0);
    num(&mut e, 13, 4, 1500.0);
    num(&mut e, 14, 4, 1600.0);
    e
}

#[test]
fn god234_excel_oracle_approximate_match_on_unsorted_data() {
    let mut e = fixture();
    let mut fails = 0u32;
    println!("=== GOD-234 oracle (Excel 16.105.3) vs formualizer ===");
    row(
        "prod-omitted",
        "4",
        ev(&mut e, "=MATCH(\"DSG7\",B2:E2)"),
        &mut fails,
    );
    row(
        "prod-mt1",
        "4",
        ev(&mut e, "=MATCH(\"DSG7\",B2:E2,1)"),
        &mut fails,
    );
    row(
        "prod-mt0",
        "#N/A",
        ev(&mut e, "=MATCH(\"DSG7\",B2:E2,0)"),
        &mut fails,
    );
    row(
        "prod-mtneg1",
        "#N/A",
        ev(&mut e, "=MATCH(\"DSG7\",B2:E2,-1)"),
        &mut fails,
    );
    row(
        "present-DGS3-omitted",
        "1",
        ev(&mut e, "=MATCH(\"DGS3\",B2:E2)"),
        &mut fails,
    );
    row(
        "present-DGS3-mt1",
        "1",
        ev(&mut e, "=MATCH(\"DGS3\",B2:E2,1)"),
        &mut fails,
    );
    row(
        "present-DGS3-mt0",
        "1",
        ev(&mut e, "=MATCH(\"DGS3\",B2:E2,0)"),
        &mut fails,
    );
    row(
        "present-DGS5-omitted",
        "2",
        ev(&mut e, "=MATCH(\"DGS5\",B2:E2)"),
        &mut fails,
    );
    row(
        "present-DGS5-mt1",
        "2",
        ev(&mut e, "=MATCH(\"DGS5\",B2:E2,1)"),
        &mut fails,
    );
    row(
        "present-DGS5-mt0",
        "2",
        ev(&mut e, "=MATCH(\"DGS5\",B2:E2,0)"),
        &mut fails,
    );
    row(
        "present-DGS7-omitted",
        "3",
        ev(&mut e, "=MATCH(\"DGS7\",B2:E2)"),
        &mut fails,
    );
    row(
        "present-DGS7-mt1",
        "3",
        ev(&mut e, "=MATCH(\"DGS7\",B2:E2,1)"),
        &mut fails,
    );
    row(
        "present-DGS7-mt0",
        "3",
        ev(&mut e, "=MATCH(\"DGS7\",B2:E2,0)"),
        &mut fails,
    );
    row(
        "present-DGS10-omitted",
        "#N/A",
        ev(&mut e, "=MATCH(\"DGS10\",B2:E2)"),
        &mut fails,
    );
    row(
        "present-DGS10-mt1",
        "#N/A",
        ev(&mut e, "=MATCH(\"DGS10\",B2:E2,1)"),
        &mut fails,
    );
    row(
        "present-DGS10-mt0",
        "4",
        ev(&mut e, "=MATCH(\"DGS10\",B2:E2,0)"),
        &mut fails,
    );
    row(
        "below-AAA-mt1",
        "#N/A",
        ev(&mut e, "=MATCH(\"AAA\",B2:E2,1)"),
        &mut fails,
    );
    row(
        "below-AAA-mt0",
        "#N/A",
        ev(&mut e, "=MATCH(\"AAA\",B2:E2,0)"),
        &mut fails,
    );
    row(
        "between-DGS6-mt1",
        "2",
        ev(&mut e, "=MATCH(\"DGS6\",B2:E2,1)"),
        &mut fails,
    );
    row(
        "sorted-DSG7-mt1",
        "4",
        ev(&mut e, "=MATCH(\"DSG7\",B3:E3,1)"),
        &mut fails,
    );
    row(
        "sorted-DGS3-mt1",
        "2",
        ev(&mut e, "=MATCH(\"DGS3\",B3:E3,1)"),
        &mut fails,
    );
    row(
        "sorted-DGS10-mt1",
        "1",
        ev(&mut e, "=MATCH(\"DGS10\",B3:E3,1)"),
        &mut fails,
    );
    row(
        "sorted-AAA-mt1",
        "#N/A",
        ev(&mut e, "=MATCH(\"AAA\",B3:E3,1)"),
        &mut fails,
    );
    row(
        "sorted-DGS6-mt1",
        "3",
        ev(&mut e, "=MATCH(\"DGS6\",B3:E3,1)"),
        &mut fails,
    );
    row(
        "arrlit-DSG7-mt1",
        "4",
        ev(
            &mut e,
            "=MATCH(\"DSG7\",{\"DGS3\",\"DGS5\",\"DGS7\",\"DGS10\"},1)",
        ),
        &mut fails,
    );
    row(
        "arrlit-DGS3-mt1",
        "1",
        ev(
            &mut e,
            "=MATCH(\"DGS3\",{\"DGS3\",\"DGS5\",\"DGS7\",\"DGS10\"},1)",
        ),
        &mut fails,
    );
    row(
        "arrlit-DGS10-mt1",
        "#N/A",
        ev(
            &mut e,
            "=MATCH(\"DGS10\",{\"DGS3\",\"DGS5\",\"DGS7\",\"DGS10\"},1)",
        ),
        &mut fails,
    );
    row(
        "column-DSG7-mt1",
        "4",
        ev(&mut e, "=MATCH(\"DSG7\",H1:H4,1)"),
        &mut fails,
    );
    row(
        "column-DGS3-mt1",
        "1",
        ev(&mut e, "=MATCH(\"DGS3\",H1:H4,1)"),
        &mut fails,
    );
    row(
        "column-DGS10-mt1",
        "#N/A",
        ev(&mut e, "=MATCH(\"DGS10\",H1:H4,1)"),
        &mut fails,
    );
    row(
        "blanks-DSG7-mt1",
        "4",
        ev(&mut e, "=MATCH(\"DSG7\",B4:E4,1)"),
        &mut fails,
    );
    row(
        "blanks-DGS3-mt1",
        "1",
        ev(&mut e, "=MATCH(\"DGS3\",B4:E4,1)"),
        &mut fails,
    );
    row(
        "num-unsorted-2-mt1",
        "#N/A",
        ev(&mut e, "=MATCH(2,B5:F5,1)"),
        &mut fails,
    );
    row(
        "num-unsorted-6-mt1",
        "5",
        ev(&mut e, "=MATCH(6,B5:F5,1)"),
        &mut fails,
    );
    row(
        "num-unsorted-0-mt1",
        "#N/A",
        ev(&mut e, "=MATCH(0,B5:F5,1)"),
        &mut fails,
    );
    row(
        "num-unsorted-2-mt0",
        "4",
        ev(&mut e, "=MATCH(2,B5:F5,0)"),
        &mut fails,
    );
    row(
        "num-sorted-6-mt1",
        "5",
        ev(&mut e, "=MATCH(6,B6:F6,1)"),
        &mut fails,
    );
    row(
        "num-sorted-0-mt1",
        "#N/A",
        ev(&mut e, "=MATCH(0,B6:F6,1)"),
        &mut fails,
    );
    row(
        "num-sorted-3-mt1",
        "3",
        ev(&mut e, "=MATCH(3,B6:F6,1)"),
        &mut fails,
    );
    row(
        "mixed-text-mt1",
        "4",
        ev(&mut e, "=MATCH(\"DSG7\",B7:E7,1)"),
        &mut fails,
    );
    row(
        "mixed-num-mt1",
        "1",
        ev(&mut e, "=MATCH(2,B7:E7,1)"),
        &mut fails,
    );
    row(
        "desc-DGS6-mtneg1",
        "1",
        ev(&mut e, "=MATCH(\"DGS6\",B8:E8,-1)"),
        &mut fails,
    );
    row(
        "desc-DGS6-mt1",
        "4",
        ev(&mut e, "=MATCH(\"DGS6\",B8:E8,1)"),
        &mut fails,
    );
    row(
        "desc-DSG7-mtneg1",
        "#N/A",
        ev(&mut e, "=MATCH(\"DSG7\",B8:E8,-1)"),
        &mut fails,
    );
    row(
        "desc-DSG7-mt1",
        "4",
        ev(&mut e, "=MATCH(\"DSG7\",B8:E8,1)"),
        &mut fails,
    );
    row(
        "case-lower-mt0",
        "3",
        ev(&mut e, "=MATCH(\"dgs7\",B2:E2,0)"),
        &mut fails,
    );
    row(
        "case-lower-mt1",
        "3",
        ev(&mut e, "=MATCH(\"dgs7\",B2:E2,1)"),
        &mut fails,
    );
    row(
        "wild-star-mt0",
        "1",
        ev(&mut e, "=MATCH(\"DGS*\",B2:E2,0)"),
        &mut fails,
    );
    row(
        "wild-qmark-mt0",
        "1",
        ev(&mut e, "=MATCH(\"DGS?\",B2:E2,0)"),
        &mut fails,
    );
    row(
        "wild-suffix-mt0",
        "3",
        ev(&mut e, "=MATCH(\"*7\",B2:E2,0)"),
        &mut fails,
    );
    row(
        "wild-star-mt1",
        "#N/A",
        ev(&mut e, "=MATCH(\"DGS*\",B2:E2,1)"),
        &mut fails,
    );
    row(
        "sib-hlookup-DSG7",
        "DGS10",
        ev(&mut e, "=HLOOKUP(\"DSG7\",B2:E2,1,TRUE)"),
        &mut fails,
    );
    row(
        "sib-hlookup-DGS3",
        "DGS3",
        ev(&mut e, "=HLOOKUP(\"DGS3\",B2:E2,1,TRUE)"),
        &mut fails,
    );
    row(
        "sib-lookup-DSG7",
        "DGS10",
        ev(&mut e, "=LOOKUP(\"DSG7\",B2:E2)"),
        &mut fails,
    );
    row(
        "sib-vlookup-DSG7",
        "DGS10",
        ev(&mut e, "=VLOOKUP(\"DSG7\",H1:H4,1,TRUE)"),
        &mut fails,
    );
    // GUARD: XLOOKUP / XMATCH must NOT have absorbed the new approximate-MATCH
    // law. Excel's XLOOKUP with match_mode 1 ("exact or next larger") answers
    // "NF" here and XMATCH answers #N/A, where legacy MATCH answers 4.
    row(
        "sib-xlookup-DSG7",
        "NF",
        ev(&mut e, "=XLOOKUP(\"DSG7\",B2:E2,B2:E2,\"NF\",1)"),
        &mut fails,
    );
    row(
        "sib-xmatch-DSG7",
        "#N/A",
        ev(&mut e, "=XMATCH(\"DSG7\",B2:E2,1)"),
        &mut fails,
    );
    row(
        "prodshape-match-col",
        "12",
        ev(
            &mut e,
            "=INDEX(K1:N4,MATCH(3,J1:J4,1),MATCH(\"DSG7\",B2:E2))/100",
        ),
        &mut fails,
    );
    row(
        "prodshape-literal-col",
        "12",
        ev(&mut e, "=INDEX(K1:N4,MATCH(3,J1:J4,1),4)/100"),
        &mut fails,
    );
    row(
        "ctl-match-exact",
        "2",
        ev(&mut e, "=MATCH(\"DGS5\",B2:E2,0)"),
        &mut fails,
    );
    row(
        "ctl-match-sorted-num",
        "3",
        ev(&mut e, "=MATCH(3,B6:F6,1)"),
        &mut fails,
    );
    row(
        "ctl-match-arrlit",
        "2",
        ev(&mut e, "=MATCH(\"b\",{\"a\",\"b\",\"c\"},1)"),
        &mut fails,
    );
    assert_eq!(fails, 0, "{fails} row(s) diverge from measured Excel");
}

/// MEASURED RESIDUAL, not a passing expectation.
///
/// `=MATCH(2,B5:F5,-1)` over the unsorted numeric row [3, 1, 5, 2, 4]: Excel
/// returns **1**. The unguarded-bisection law that reproduces the other 27
/// discriminating rows returns 4. The probe sequence, 0-based over
/// [3, 1, 5, 2, 4]: mid = (0+4)/2 = 2, a[2] = 5 > 2, so match_type -1 goes right
/// and lo = 3; mid = (3+4)/2 = 3, a[3] = 2, equality early exit at 0-based 3,
/// i.e. the 1-based position 4. (An earlier version of this comment mis-stated
/// the first probe as `B5:F5[3]`; the first probe is the 0-based mid 2.)
///
/// Recorded here rather than papered over. It is NOT asserted as correct in
/// either direction, and the implementation was deliberately not contorted to
/// hit it. The addendum has since widened this residual: `match_type = -1` over
/// data that is not descending is an undefined corner across eight further
/// measured rows -- see
/// `god234_addendum_known_divergence_match_type_minus_one` -- and 24 candidate
/// bisection variants brute-forced against all 23 measured `match_type = -1`
/// rows fit at most 17 of them. GOD-234 pins `match_type` omitted or 1 and
/// deliberately implements no law for -1 on non-descending data.
#[test]
#[ignore = "GOD-234 known divergence: Excel says 1, the measured-law implementation says 4"]
fn god234_known_divergence_match_type_minus_one_on_unsorted_numbers() {
    let mut e = fixture();
    let excel = "1";
    let got = ev(&mut e, "=MATCH(2,B5:F5,-1)");
    println!("GOD-234 residual: excel={excel} formualizer={got}");
    assert_eq!(
        got, excel,
        "known divergence -- see the doc comment; do not 'fix' this by editing the expected value"
    );
}

/// Build the ADDENDUM oracle fixture sheet exactly as its workbook had it.
///
/// Receipt (in the bakeoff checkout, not this fork):
///   artifacts/private/god234/round/receipts/god234_match_oracle_addendum_receipt.json
fn addendum_fixture() -> Engine<TestWorkbook> {
    let mut e = mk();
    num(&mut e, 2, 2, 1.0);
    num(&mut e, 3, 2, 5.0);
    num(&mut e, 4, 2, 7.0);
    num(&mut e, 5, 2, 7.0);
    num(&mut e, 6, 2, 7.0);
    num(&mut e, 7, 2, 9.0);
    num(&mut e, 2, 3, 9.0);
    num(&mut e, 3, 3, 7.0);
    num(&mut e, 4, 3, 7.0);
    num(&mut e, 5, 3, 7.0);
    num(&mut e, 6, 3, 5.0);
    num(&mut e, 2, 4, 1.0);
    num(&mut e, 3, 4, 7.0);
    num(&mut e, 4, 4, 5.0);
    num(&mut e, 5, 4, 7.0);
    num(&mut e, 6, 4, 9.0);
    num(&mut e, 2, 5, 7.0);
    num(&mut e, 3, 5, 1.0);
    num(&mut e, 4, 5, 7.0);
    num(&mut e, 2, 6, 7.0);
    num(&mut e, 3, 6, 7.0);
    num(&mut e, 4, 6, 7.0);
    num(&mut e, 5, 6, 9.0);
    num(&mut e, 6, 6, 11.0);
    num(&mut e, 2, 7, 1.0);
    num(&mut e, 3, 7, 3.0);
    num(&mut e, 4, 7, 7.0);
    num(&mut e, 5, 7, 7.0);
    num(&mut e, 6, 7, 7.0);
    num(&mut e, 2, 8, 1.0);
    num(&mut e, 3, 8, 3.0);
    num(&mut e, 4, 8, 7.0);
    num(&mut e, 5, 8, 9.0);
    num(&mut e, 6, 8, 11.0);
    num(&mut e, 2, 9, 7.0);
    num(&mut e, 3, 9, 7.0);
    num(&mut e, 4, 9, 7.0);
    num(&mut e, 5, 9, 5.0);
    num(&mut e, 6, 9, 3.0);
    num(&mut e, 2, 10, 11.0);
    num(&mut e, 3, 10, 9.0);
    num(&mut e, 4, 10, 7.0);
    num(&mut e, 5, 10, 7.0);
    num(&mut e, 6, 10, 7.0);
    num(&mut e, 2, 11, 9.0);
    num(&mut e, 3, 11, 7.0);
    num(&mut e, 4, 11, 5.0);
    num(&mut e, 5, 11, 7.0);
    num(&mut e, 6, 11, 3.0);
    num(&mut e, 2, 12, 7.0);
    num(&mut e, 3, 12, 7.0);
    num(&mut e, 4, 12, 1.0);
    num(&mut e, 5, 12, 7.0);
    num(&mut e, 6, 12, 9.0);
    num(&mut e, 7, 12, 7.0);
    num(&mut e, 2, 13, 1.0);
    num(&mut e, 3, 13, 2.0);
    num(&mut e, 4, 13, 3.0);
    num(&mut e, 5, 13, 7.0);
    num(&mut e, 6, 13, 7.0);
    num(&mut e, 7, 13, 7.0);
    num(&mut e, 8, 13, 7.0);
    num(&mut e, 9, 13, 7.0);
    num(&mut e, 10, 13, 9.0);
    num(&mut e, 2, 14, 10.0);
    num(&mut e, 3, 14, 30.0);
    num(&mut e, 4, 14, 20.0);
    num(&mut e, 5, 14, 40.0);
    num(&mut e, 6, 14, 50.0);
    num(&mut e, 2, 15, 50.0);
    num(&mut e, 3, 15, 30.0);
    num(&mut e, 4, 15, 40.0);
    num(&mut e, 5, 15, 20.0);
    num(&mut e, 6, 15, 10.0);
    num(&mut e, 2, 16, 50.0);
    num(&mut e, 3, 16, 40.0);
    num(&mut e, 4, 16, 30.0);
    num(&mut e, 5, 16, 20.0);
    num(&mut e, 6, 16, 10.0);
    txt(&mut e, 2, 17, "foo");
    txt(&mut e, 3, 17, "fob");
    txt(&mut e, 4, 17, "bar");
    txt(&mut e, 5, 17, "baz");
    txt(&mut e, 2, 20, "a");
    txt(&mut e, 3, 20, "b");
    txt(&mut e, 4, 20, "c");
    txt(&mut e, 5, 20, "d");
    num(&mut e, 2, 21, 1.0);
    num(&mut e, 3, 21, 2.0);
    num(&mut e, 4, 21, 3.0);
    num(&mut e, 5, 21, 4.0);
    num(&mut e, 2, 22, 5.0);
    num(&mut e, 2, 23, 1.0);
    num(&mut e, 3, 23, 3.0);
    // D23: an ERROR CELL, seeded as a formula so the approximate
    // search can be measured over a vector that contains an error.
    e.set_cell_formula("S", 23, 4, parse("=1/0").unwrap())
        .unwrap();
    num(&mut e, 5, 23, 7.0);
    num(&mut e, 6, 23, 9.0);
    num(&mut e, 2, 26, 1.0);
    num(&mut e, 3, 26, 3.0);
    num(&mut e, 4, 26, 5.0);
    num(&mut e, 5, 26, 5.0);
    num(&mut e, 6, 26, 5.0);
    num(&mut e, 7, 26, 9.0);
    num(&mut e, 2, 27, 101.0);
    num(&mut e, 3, 27, 102.0);
    num(&mut e, 4, 27, 103.0);
    num(&mut e, 5, 27, 104.0);
    num(&mut e, 6, 27, 105.0);
    num(&mut e, 7, 27, 106.0);
    num(&mut e, 2, 28, 5.0);
    num(&mut e, 3, 28, 1.0);
    num(&mut e, 4, 28, 5.0);
    num(&mut e, 5, 28, 9.0);
    num(&mut e, 6, 28, 5.0);
    num(&mut e, 2, 29, 201.0);
    num(&mut e, 3, 29, 202.0);
    num(&mut e, 4, 29, 203.0);
    num(&mut e, 5, 29, 204.0);
    num(&mut e, 6, 29, 205.0);
    txt(&mut e, 2, 30, "DGS3");
    txt(&mut e, 3, 30, "DGS5");
    txt(&mut e, 4, 30, "DGS7");
    txt(&mut e, 5, 30, "DGS10");
    num(&mut e, 12, 1, 1.0);
    num(&mut e, 13, 1, 101.0);
    num(&mut e, 12, 2, 3.0);
    num(&mut e, 13, 2, 102.0);
    num(&mut e, 12, 3, 5.0);
    num(&mut e, 13, 3, 103.0);
    num(&mut e, 12, 4, 5.0);
    num(&mut e, 13, 4, 104.0);
    num(&mut e, 12, 5, 5.0);
    num(&mut e, 13, 5, 105.0);
    num(&mut e, 12, 6, 9.0);
    num(&mut e, 13, 6, 106.0);
    num(&mut e, 14, 1, 5.0);
    num(&mut e, 15, 1, 201.0);
    num(&mut e, 14, 2, 1.0);
    num(&mut e, 15, 2, 202.0);
    num(&mut e, 14, 3, 5.0);
    num(&mut e, 15, 3, 203.0);
    num(&mut e, 14, 4, 9.0);
    num(&mut e, 15, 4, 204.0);
    num(&mut e, 14, 5, 5.0);
    num(&mut e, 15, 5, 205.0);
    e
}

/// The GOD-234 ADDENDUM oracle: 135 further rows measured on the SAME desktop
/// Microsoft Excel 16.105.3, after the fix landed, to settle the questions the
/// first 67-row oracle left open -- equality runs, the VLOOKUP/HLOOKUP/LOOKUP
/// siblings on duplicate-bearing vectors, match_type coercion, error cells in
/// the lookup vector, and degenerate vectors.
///
/// Receipt: artifacts/private/god234/round/receipts/god234_match_oracle_addendum_receipt.json
/// (all controls green, AppleScript and cached-XML readings agree, no refusals).
///
/// THIS FILE MUST NEVER BE EDITED TO MAKE AN ENGINE RESULT AGREE. The 13 rows
/// the engine does NOT reproduce are excluded from this test and asserted, at
/// Excel's value, in the `#[ignore]`d known-divergence tests below.
#[test]
fn god234_addendum_oracle_matches_the_engine() {
    let mut e = addendum_fixture();
    let mut fails = 0u32;
    println!("=== GOD-234 addendum oracle (Excel 16.105.3) vs formualizer ===");
    row(
        "a-asc-run-mid-mt1-arr",
        "5",
        ev(&mut e, "=MATCH(7,{1,5,7,7,7,9},1)"),
        &mut fails,
    );
    row(
        "a-asc-run-mid-mt1-ref",
        "5",
        ev(&mut e, "=MATCH(7,B2:G2,1)"),
        &mut fails,
    );
    row(
        "a-desc-run-mid-mtneg1-arr",
        "2",
        ev(&mut e, "=MATCH(7,{9,7,7,7,5},-1)"),
        &mut fails,
    );
    row(
        "a-desc-run-mid-mtneg1-ref",
        "2",
        ev(&mut e, "=MATCH(7,B3:F3,-1)"),
        &mut fails,
    );
    row(
        "a-asc-run-mid-mt0-ref",
        "3",
        ev(&mut e, "=MATCH(7,B2:G2,0)"),
        &mut fails,
    );
    row(
        "a-desc-run-mid-mt1-ref",
        "4",
        ev(&mut e, "=MATCH(7,B3:F3,1)"),
        &mut fails,
    );
    row(
        "a-desc-run-mid-mt0-ref",
        "2",
        ev(&mut e, "=MATCH(7,B3:F3,0)"),
        &mut fails,
    );
    row(
        "a-noncontig5-mt1-arr",
        "4",
        ev(&mut e, "=MATCH(7,{1,7,5,7,9},1)"),
        &mut fails,
    );
    row(
        "a-noncontig5-mt1-ref",
        "4",
        ev(&mut e, "=MATCH(7,B4:F4,1)"),
        &mut fails,
    );
    row(
        "a-noncontig5-mtneg1-ref",
        "#N/A",
        ev(&mut e, "=MATCH(7,B4:F4,-1)"),
        &mut fails,
    );
    row(
        "a-noncontig5-mt0-ref",
        "2",
        ev(&mut e, "=MATCH(7,B4:F4,0)"),
        &mut fails,
    );
    row(
        "a-noncontig3-mt1-arr",
        "3",
        ev(&mut e, "=MATCH(7,{7,1,7},1)"),
        &mut fails,
    );
    row(
        "a-noncontig3-mt1-ref",
        "3",
        ev(&mut e, "=MATCH(7,B5:D5,1)"),
        &mut fails,
    );
    row(
        "a-noncontig3-mtneg1-ref",
        "1",
        ev(&mut e, "=MATCH(7,B5:D5,-1)"),
        &mut fails,
    );
    row(
        "a-run-start-mt1",
        "3",
        ev(&mut e, "=MATCH(7,B6:F6,1)"),
        &mut fails,
    );
    row(
        "a-run-start-mtneg1",
        "1",
        ev(&mut e, "=MATCH(7,B6:F6,-1)"),
        &mut fails,
    );
    row(
        "a-run-end-mt1",
        "5",
        ev(&mut e, "=MATCH(7,B7:F7,1)"),
        &mut fails,
    );
    row(
        "a-run-len1-mt1",
        "3",
        ev(&mut e, "=MATCH(7,B8:F8,1)"),
        &mut fails,
    );
    row(
        "a-desc-run-start-mtneg1",
        "1",
        ev(&mut e, "=MATCH(7,B9:F9,-1)"),
        &mut fails,
    );
    row(
        "a-desc-run-start-mt1",
        "3",
        ev(&mut e, "=MATCH(7,B9:F9,1)"),
        &mut fails,
    );
    row(
        "a-desc-run-end-mtneg1",
        "3",
        ev(&mut e, "=MATCH(7,B10:F10,-1)"),
        &mut fails,
    );
    row(
        "a-desc-run-end-mt1",
        "5",
        ev(&mut e, "=MATCH(7,B10:F10,1)"),
        &mut fails,
    );
    row(
        "a-desc-noncontig-mtneg1",
        "2",
        ev(&mut e, "=MATCH(7,B11:F11,-1)"),
        &mut fails,
    );
    row(
        "a-desc-noncontig-mt1",
        "4",
        ev(&mut e, "=MATCH(7,B11:F11,1)"),
        &mut fails,
    );
    row(
        "a-window-mt1",
        "4",
        ev(&mut e, "=MATCH(7,B12:G12,1)"),
        &mut fails,
    );
    row(
        "a-window-mtneg1",
        "1",
        ev(&mut e, "=MATCH(7,B12:G12,-1)"),
        &mut fails,
    );
    row(
        "a-window-mt0",
        "1",
        ev(&mut e, "=MATCH(7,B12:G12,0)"),
        &mut fails,
    );
    row(
        "a-window-mt1-arr",
        "4",
        ev(&mut e, "=MATCH(7,{7,7,1,7,9,7},1)"),
        &mut fails,
    );
    row(
        "a-window-mtneg1-arr",
        "1",
        ev(&mut e, "=MATCH(7,{7,7,1,7,9,7},-1)"),
        &mut fails,
    );
    row(
        "a-longrun-mt1",
        "8",
        ev(&mut e, "=MATCH(7,B13:J13,1)"),
        &mut fails,
    );
    row(
        "a-longrun-mt0",
        "4",
        ev(&mut e, "=MATCH(7,B13:J13,0)"),
        &mut fails,
    );
    row(
        "b-wild-star-o-ref",
        "1",
        ev(&mut e, "=MATCH(\"*o*\",B17:E17,0)"),
        &mut fails,
    );
    row(
        "b-wild-bqz-ref",
        "4",
        ev(&mut e, "=MATCH(\"b?z\",B17:E17,0)"),
        &mut fails,
    );
    row(
        "b-wild-zstar-ref",
        "#N/A",
        ev(&mut e, "=MATCH(\"z*\",B17:E17,0)"),
        &mut fails,
    );
    row(
        "b-wild-star-o-arr",
        "1",
        ev(
            &mut e,
            "=MATCH(\"*o*\",{\"foo\",\"fob\",\"bar\",\"baz\"},0)",
        ),
        &mut fails,
    );
    row(
        "b-wild-bqz-arr",
        "4",
        ev(
            &mut e,
            "=MATCH(\"b?z\",{\"foo\",\"fob\",\"bar\",\"baz\"},0)",
        ),
        &mut fails,
    );
    row(
        "b-wild-zstar-arr",
        "#N/A",
        ev(&mut e, "=MATCH(\"z*\",{\"foo\",\"fob\",\"bar\",\"baz\"},0)"),
        &mut fails,
    );
    row(
        "b-desc-30-mtneg1-ref",
        "3",
        ev(&mut e, "=MATCH(30,B16:F16,-1)"),
        &mut fails,
    );
    row(
        "b-desc-60-mtneg1-ref",
        "#N/A",
        ev(&mut e, "=MATCH(60,B16:F16,-1)"),
        &mut fails,
    );
    row(
        "b-desc-30-mtneg1-arr",
        "3",
        ev(&mut e, "=MATCH(30,{50,40,30,20,10},-1)"),
        &mut fails,
    );
    row(
        "b-desc-60-mtneg1-arr",
        "#N/A",
        ev(&mut e, "=MATCH(60,{50,40,30,20,10},-1)"),
        &mut fails,
    );
    row(
        "b-unsorted-30-omitted-ref",
        "3",
        ev(&mut e, "=MATCH(30,B14:F14)"),
        &mut fails,
    );
    row(
        "b-unsorted-30-mt1-ref",
        "3",
        ev(&mut e, "=MATCH(30,B14:F14,1)"),
        &mut fails,
    );
    row(
        "b-unsorted-30-omitted-arr",
        "3",
        ev(&mut e, "=MATCH(30,{10,30,20,40,50})"),
        &mut fails,
    );
    row(
        "b-unsorted-30-mt1-arr",
        "3",
        ev(&mut e, "=MATCH(30,{10,30,20,40,50},1)"),
        &mut fails,
    );
    row(
        "c-sorted-match-5",
        "5",
        ev(&mut e, "=MATCH(5,B26:G26,1)"),
        &mut fails,
    );
    row(
        "c-sorted-match-6",
        "5",
        ev(&mut e, "=MATCH(6,B26:G26,1)"),
        &mut fails,
    );
    row(
        "c-sorted-match-10",
        "6",
        ev(&mut e, "=MATCH(10,B26:G26,1)"),
        &mut fails,
    );
    row(
        "c-sorted-match-0",
        "#N/A",
        ev(&mut e, "=MATCH(0,B26:G26,1)"),
        &mut fails,
    );
    row(
        "c-sorted-matchcol-5",
        "5",
        ev(&mut e, "=MATCH(5,L1:L6,1)"),
        &mut fails,
    );
    row(
        "c-sorted-hlookup-5",
        "105",
        ev(&mut e, "=HLOOKUP(5,B26:G27,2,TRUE)"),
        &mut fails,
    );
    row(
        "c-sorted-hlookup-6",
        "105",
        ev(&mut e, "=HLOOKUP(6,B26:G27,2,TRUE)"),
        &mut fails,
    );
    row(
        "c-sorted-hlookup-10",
        "106",
        ev(&mut e, "=HLOOKUP(10,B26:G27,2,TRUE)"),
        &mut fails,
    );
    row(
        "c-sorted-hlookup-0",
        "#N/A",
        ev(&mut e, "=HLOOKUP(0,B26:G27,2,TRUE)"),
        &mut fails,
    );
    row(
        "c-sorted-vlookup-5",
        "105",
        ev(&mut e, "=VLOOKUP(5,L1:M6,2,TRUE)"),
        &mut fails,
    );
    row(
        "c-sorted-vlookup-6",
        "105",
        ev(&mut e, "=VLOOKUP(6,L1:M6,2,TRUE)"),
        &mut fails,
    );
    row(
        "c-sorted-vlookup-10",
        "106",
        ev(&mut e, "=VLOOKUP(10,L1:M6,2,TRUE)"),
        &mut fails,
    );
    row(
        "c-sorted-vlookup-0",
        "#N/A",
        ev(&mut e, "=VLOOKUP(0,L1:M6,2,TRUE)"),
        &mut fails,
    );
    row(
        "c-sorted-lookup-5",
        "105",
        ev(&mut e, "=LOOKUP(5,B26:G26,B27:G27)"),
        &mut fails,
    );
    row(
        "c-sorted-lookup-6",
        "105",
        ev(&mut e, "=LOOKUP(6,B26:G26,B27:G27)"),
        &mut fails,
    );
    row(
        "c-sorted-lookup-10",
        "106",
        ev(&mut e, "=LOOKUP(10,B26:G26,B27:G27)"),
        &mut fails,
    );
    row(
        "c-sorted-lookup-0",
        "#N/A",
        ev(&mut e, "=LOOKUP(0,B26:G26,B27:G27)"),
        &mut fails,
    );
    row(
        "c-sorted-lookupcol-5",
        "105",
        ev(&mut e, "=LOOKUP(5,L1:L6,M1:M6)"),
        &mut fails,
    );
    row(
        "c-sorted-index-match-5",
        "105",
        ev(&mut e, "=INDEX(B27:G27,MATCH(5,B26:G26,1))"),
        &mut fails,
    );
    row(
        "c-unsorted-match-5",
        "3",
        ev(&mut e, "=MATCH(5,B28:F28,1)"),
        &mut fails,
    );
    row(
        "c-unsorted-match-6",
        "3",
        ev(&mut e, "=MATCH(6,B28:F28,1)"),
        &mut fails,
    );
    row(
        "c-unsorted-match-10",
        "5",
        ev(&mut e, "=MATCH(10,B28:F28,1)"),
        &mut fails,
    );
    row(
        "c-unsorted-match-0",
        "#N/A",
        ev(&mut e, "=MATCH(0,B28:F28,1)"),
        &mut fails,
    );
    row(
        "c-unsorted-matchcol-5",
        "3",
        ev(&mut e, "=MATCH(5,N1:N5,1)"),
        &mut fails,
    );
    row(
        "c-unsorted-hlookup-5",
        "203",
        ev(&mut e, "=HLOOKUP(5,B28:F29,2,TRUE)"),
        &mut fails,
    );
    row(
        "c-unsorted-hlookup-6",
        "203",
        ev(&mut e, "=HLOOKUP(6,B28:F29,2,TRUE)"),
        &mut fails,
    );
    row(
        "c-unsorted-hlookup-10",
        "205",
        ev(&mut e, "=HLOOKUP(10,B28:F29,2,TRUE)"),
        &mut fails,
    );
    row(
        "c-unsorted-hlookup-0",
        "#N/A",
        ev(&mut e, "=HLOOKUP(0,B28:F29,2,TRUE)"),
        &mut fails,
    );
    row(
        "c-unsorted-vlookup-5",
        "203",
        ev(&mut e, "=VLOOKUP(5,N1:O5,2,TRUE)"),
        &mut fails,
    );
    row(
        "c-unsorted-vlookup-6",
        "203",
        ev(&mut e, "=VLOOKUP(6,N1:O5,2,TRUE)"),
        &mut fails,
    );
    row(
        "c-unsorted-vlookup-10",
        "205",
        ev(&mut e, "=VLOOKUP(10,N1:O5,2,TRUE)"),
        &mut fails,
    );
    row(
        "c-unsorted-vlookup-0",
        "#N/A",
        ev(&mut e, "=VLOOKUP(0,N1:O5,2,TRUE)"),
        &mut fails,
    );
    row(
        "c-unsorted-lookup-10",
        "205",
        ev(&mut e, "=LOOKUP(10,B28:F28,B29:F29)"),
        &mut fails,
    );
    row(
        "c-unsorted-lookup-0",
        "#N/A",
        ev(&mut e, "=LOOKUP(0,B28:F28,B29:F29)"),
        &mut fails,
    );
    row(
        "c-unsorted-index-match-5",
        "203",
        ev(&mut e, "=INDEX(B29:F29,MATCH(5,B28:F28,1))"),
        &mut fails,
    );
    row(
        "c-runvec-hlookup-7",
        "7",
        ev(&mut e, "=HLOOKUP(7,B2:G2,1,TRUE)"),
        &mut fails,
    );
    row(
        "c-runvec-lookup-7",
        "7",
        ev(&mut e, "=LOOKUP(7,B2:G2)"),
        &mut fails,
    );
    row(
        "x-alltext-numkey-mt1",
        "#N/A",
        ev(&mut e, "=MATCH(3,B20:E20,1)"),
        &mut fails,
    );
    row(
        "x-alltext-numkey-mtneg1",
        "#N/A",
        ev(&mut e, "=MATCH(3,B20:E20,-1)"),
        &mut fails,
    );
    row(
        "x-alltext-numkey-mt0",
        "#N/A",
        ev(&mut e, "=MATCH(3,B20:E20,0)"),
        &mut fails,
    );
    row(
        "x-allnum-textkey-mt1",
        "#N/A",
        ev(&mut e, "=MATCH(\"c\",B21:E21,1)"),
        &mut fails,
    );
    row(
        "x-allnum-textkey-mtneg1",
        "#N/A",
        ev(&mut e, "=MATCH(\"c\",B21:E21,-1)"),
        &mut fails,
    );
    row(
        "x-allnum-textkey-mt0",
        "#N/A",
        ev(&mut e, "=MATCH(\"c\",B21:E21,0)"),
        &mut fails,
    );
    row(
        "x-single-eq-mt1",
        "1",
        ev(&mut e, "=MATCH(5,B22,1)"),
        &mut fails,
    );
    row(
        "x-single-above-mt1",
        "1",
        ev(&mut e, "=MATCH(9,B22,1)"),
        &mut fails,
    );
    row(
        "x-single-below-mt1",
        "#N/A",
        ev(&mut e, "=MATCH(1,B22,1)"),
        &mut fails,
    );
    row(
        "x-single-eq-mtneg1",
        "1",
        ev(&mut e, "=MATCH(5,B22,-1)"),
        &mut fails,
    );
    row(
        "x-single-above-mtneg1",
        "#N/A",
        ev(&mut e, "=MATCH(9,B22,-1)"),
        &mut fails,
    );
    row(
        "x-single-below-mtneg1",
        "1",
        ev(&mut e, "=MATCH(1,B22,-1)"),
        &mut fails,
    );
    row(
        "x-single-eq-mt0",
        "1",
        ev(&mut e, "=MATCH(5,B22,0)"),
        &mut fails,
    );
    row(
        "x-single-above-mt0",
        "#N/A",
        ev(&mut e, "=MATCH(9,B22,0)"),
        &mut fails,
    );
    row(
        "x-err-cell-value",
        "#DIV/0!",
        ev(&mut e, "=D23"),
        &mut fails,
    );
    row(
        "x-err-above-mt1",
        "4",
        ev(&mut e, "=MATCH(8,B23:F23,1)"),
        &mut fails,
    );
    row(
        "x-err-below-mt1",
        "1",
        ev(&mut e, "=MATCH(2,B23:F23,1)"),
        &mut fails,
    );
    row(
        "x-err-at-mt1",
        "2",
        ev(&mut e, "=MATCH(5,B23:F23,1)"),
        &mut fails,
    );
    row(
        "x-err-mt0",
        "4",
        ev(&mut e, "=MATCH(7,B23:F23,0)"),
        &mut fails,
    );
    row(
        "x-err-mtneg1",
        "#N/A",
        ev(&mut e, "=MATCH(8,B23:F23,-1)"),
        &mut fails,
    );
    row(
        "x-err-hlookup",
        "7",
        ev(&mut e, "=HLOOKUP(8,B23:F23,1,TRUE)"),
        &mut fails,
    );
    row(
        "x-blank-mt1",
        "#N/A",
        ev(&mut e, "=MATCH(5,B24:E24,1)"),
        &mut fails,
    );
    row(
        "x-blank-mt0",
        "#N/A",
        ev(&mut e, "=MATCH(5,B24:E24,0)"),
        &mut fails,
    );
    row(
        "x-blank-mtneg1",
        "#N/A",
        ev(&mut e, "=MATCH(5,B24:E24,-1)"),
        &mut fails,
    );
    row(
        "x-blank-count",
        "0",
        ev(&mut e, "=COUNTA(B24:E24)"),
        &mut fails,
    );
    row("x-mt-2", "5", ev(&mut e, "=MATCH(5,B26:G26,2)"), &mut fails);
    row(
        "x-mt-1p5",
        "5",
        ev(&mut e, "=MATCH(5,B26:G26,1.5)"),
        &mut fails,
    );
    row(
        "x-mt-0p5",
        "5",
        ev(&mut e, "=MATCH(5,B26:G26,0.5)"),
        &mut fails,
    );
    row(
        "x-mt-str1",
        "5",
        ev(&mut e, "=MATCH(5,B26:G26,\"1\")"),
        &mut fails,
    );
    row(
        "x-mt-str0",
        "3",
        ev(&mut e, "=MATCH(5,B26:G26,\"0\")"),
        &mut fails,
    );
    row(
        "x-mt-strx",
        "#VALUE!",
        ev(&mut e, "=MATCH(5,B26:G26,\"x\")"),
        &mut fails,
    );
    row(
        "x-mt-true",
        "5",
        ev(&mut e, "=MATCH(5,B26:G26,TRUE)"),
        &mut fails,
    );
    row(
        "x-mt-empty",
        "3",
        ev(&mut e, "=MATCH(5,B26:G26,)"),
        &mut fails,
    );
    row(
        "ctl-match-exact",
        "2",
        ev(&mut e, "=MATCH(\"DGS5\",B30:E30,0)"),
        &mut fails,
    );
    row(
        "ctl-match-sorted-num",
        "2",
        ev(&mut e, "=MATCH(3,B8:F8,1)"),
        &mut fails,
    );
    row(
        "ctl-match-arrlit",
        "2",
        ev(&mut e, "=MATCH(\"b\",{\"a\",\"b\",\"c\"},1)"),
        &mut fails,
    );
    row(
        "ctl-text-locale",
        "1,234.50",
        ev(&mut e, "=TEXT(1234.5,\"#,##0.00\")"),
        &mut fails,
    );
    row(
        // AppleScript reading; the cached-XML reading of the same cell is "0"
        // with type "b".
        "ctl-exact-transpose",
        "FALSE",
        ev(&mut e, "=EXACT(\"DSG7\",\"DGS7\")"),
        &mut fails,
    );
    row(
        "ctl-code-mid",
        "83",
        ev(&mut e, "=CODE(MID(\"DSG7\",2,1))"),
        &mut fails,
    );
    row(
        "ctl-sum-sentinels",
        "621",
        ev(&mut e, "=SUM(B27:G27)"),
        &mut fails,
    );
    assert_eq!(
        fails, 0,
        "{fails} addendum row(s) diverge from measured Excel"
    );
}

/// KNOWN DIVERGENCE (out of scope, measured): `match_type = -1` over data that
/// is not descending.
///
/// All eight rows below are `match_type = -1` (or a value that coerces to it)
/// over a vector that is NOT descending. Excel's answers here fit no single
/// bisection: 24 candidate binary-search variants were brute-forced against all
/// 23 measured `match_type = -1` rows across both oracles and none reproduces
/// more than 17 of them. GOD-234's pinned defect is `match_type` omitted or 1,
/// so this round deliberately does NOT change the `match_type = -1` search; the
/// corner is handed on as an open thread instead.
///
/// Note that Excel's sign coercion itself is NOT the divergence: `-2` and `-1.5`
/// are correctly taken as -1 (rows `x-mt-neg2`, `x-mt-neg1p5`), and the engine
/// agrees on that; they land here only because the -1 search then differs.
///
/// The expectations are Excel's values, so this test is RED the moment it is
/// un-ignored. Never edit them to match the engine.
#[test]
#[ignore = "GOD-234 open thread: match_type = -1 over non-descending data is an undefined corner this round does not implement"]
fn god234_addendum_known_divergence_match_type_minus_one() {
    let mut e = addendum_fixture();
    let mut fails = 0u32;
    println!("=== GOD-234 addendum match_type=-1 residual (Excel 16.105.3) ===");
    // Ascending vector {1,5,7,7,7,9}, key inside the run of 7s.
    row(
        "a-asc-run-mid-mtneg1-ref",
        "#N/A",
        ev(&mut e, "=MATCH(7,B2:G2,-1)"),
        &mut fails,
    );
    // {1,3,7,7,7}: the run ends at the vector's end.
    row(
        "a-run-end-mtneg1",
        "#N/A",
        ev(&mut e, "=MATCH(7,B7:F7,-1)"),
        &mut fails,
    );
    // {1,3,7,9,11}: a run of length 1.
    row(
        "a-run-len1-mtneg1",
        "#N/A",
        ev(&mut e, "=MATCH(7,B8:F8,-1)"),
        &mut fails,
    );
    // {1,2,3,7,7,7,7,7,9}: a long run.
    row(
        "a-longrun-mtneg1",
        "#N/A",
        ev(&mut e, "=MATCH(7,B13:J13,-1)"),
        &mut fails,
    );
    // {50,30,40,20,10}: nearly-descending. This is the pair that proves the
    // old in-file unit test in builtins/lookup/core.rs asserted a DERIVED 3
    // where Excel measures 2; see the ignored test alongside it.
    row(
        "b-unsorteddesc-30-mtneg1-ref",
        "2",
        ev(&mut e, "=MATCH(30,B15:F15,-1)"),
        &mut fails,
    );
    row(
        "b-unsorteddesc-30-mtneg1-arr",
        "2",
        ev(&mut e, "=MATCH(30,{50,30,40,20,10},-1)"),
        &mut fails,
    );
    // Ascending {1,3,5,5,5,9}: match_type given as -2 and -1.5, both of which
    // Excel takes by SIGN as -1.
    row(
        "x-mt-neg2",
        "#N/A",
        ev(&mut e, "=MATCH(5,B26:G26,-2)"),
        &mut fails,
    );
    row(
        "x-mt-neg1p5",
        "#N/A",
        ev(&mut e, "=MATCH(5,B26:G26,-1.5)"),
        &mut fails,
    );
    assert_eq!(
        fails, 0,
        "{fails} match_type = -1 row(s) still diverge -- expected while the open \
         thread is open; do NOT edit the expectations above"
    );
}

/// KNOWN DIVERGENCE (out of scope, measured): `match_type` given as a BOOLEAN
/// or as a reference to a BLANK cell.
///
/// Excel coerces `match_type` by sign, and takes both `FALSE` and a blank
/// reference as **0** (exact). Over the ascending vector `{1,3,5,5,5,9}` with
/// key 5 that is position **3**; this engine answers **5**, because
/// `MatchFn::eval` matches only `Number` / `Int` / `Text` when reading the third
/// argument and silently leaves `match_type` at its default 1 for a `Boolean`
/// or an `Empty`. `TRUE` is right only by accident (it also lands on 1).
///
/// Measured, Excel 16.105.3, addendum rows `x-mt-false` and `x-mt-blankref`.
/// The related rows the engine DOES reproduce -- `x-mt-true`, `x-mt-str0`,
/// `x-mt-str1`, `x-mt-strx` (`#VALUE!`), `x-mt-0p5`, `x-mt-1p5`, `x-mt-2` and
/// `x-mt-empty` -- are asserted in the passing test above; note especially that
/// a third argument that is present but EMPTY (`=MATCH(5,B26:G26,)`) behaves as
/// match_type 0 while a fully ABSENT third argument behaves as 1. Two different
/// defaults, and the engine already gets that pair right.
///
/// This is a two-line fix in `MatchFn::eval` (accept `Boolean` and `Empty`) but
/// it MOVES ENGINE BEHAVIOUR, and GOD-234's blast-radius census and two-engine
/// graded differential were run against a build without it. It is therefore
/// held out of this round and recorded as an open thread.
#[test]
#[ignore = "GOD-234 open thread: match_type coercion of Boolean / blank reference is unimplemented; behaviour change held out of this round"]
fn god234_addendum_known_divergence_match_type_boolean_and_blank() {
    let mut e = addendum_fixture();
    let mut fails = 0u32;
    println!("=== GOD-234 addendum match_type coercion residual (Excel 16.105.3) ===");
    row(
        "x-mt-false",
        "3",
        ev(&mut e, "=MATCH(5,B26:G26,FALSE)"),
        &mut fails,
    );
    row(
        "x-mt-blankref",
        "3",
        ev(&mut e, "=MATCH(5,B26:G26,B24)"),
        &mut fails,
    );
    assert_eq!(
        fails, 0,
        "{fails} match_type-coercion row(s) still diverge -- expected while the \
         open thread is open; do NOT edit the expectations above"
    );
}

/// KNOWN DIVERGENCE (out of scope, measured): `LOOKUP`'s own bisection.
///
/// The addendum measured `MATCH`, `VLOOKUP`, `HLOOKUP` and `LOOKUP` over the
/// SAME two duplicate-bearing vectors at the same four keys, and in Excel all
/// four functions agree with each other on all eight cases -- see
/// `god234_addendum_sibling_functions_agree_with_each_other` below.
///
/// This engine reproduces that agreement everywhere except `LOOKUP` over the
/// UNSORTED vector `{5,1,5,9,5}` at keys 5 and 6, where Excel answers with the
/// third column (203) and `LOOKUP` here answers with the fifth (205). `LOOKUP`
/// keeps its own bisection in `builtins/lookup/legacy.rs`; it did NOT pick up
/// the GOD-234 shared helper, and bringing it onto that helper is a behaviour
/// change outside this round's pinned defect. Recorded as an open thread.
///
/// Measured, Excel 16.105.3, addendum rows `c-unsorted-lookup-5` and
/// `c-unsorted-lookup-6`.
#[test]
#[ignore = "GOD-234 open thread: LOOKUP's own bisection disagrees with MATCH/VLOOKUP/HLOOKUP on an unsorted duplicate-bearing vector"]
fn god234_addendum_known_divergence_lookup_own_bisection() {
    let mut e = addendum_fixture();
    let mut fails = 0u32;
    println!("=== GOD-234 addendum LOOKUP residual (Excel 16.105.3) ===");
    row(
        "c-unsorted-lookup-5",
        "203",
        ev(&mut e, "=LOOKUP(5,B28:F28,B29:F29)"),
        &mut fails,
    );
    row(
        "c-unsorted-lookup-6",
        "203",
        ev(&mut e, "=LOOKUP(6,B28:F28,B29:F29)"),
        &mut fails,
    );
    assert_eq!(
        fails, 0,
        "{fails} LOOKUP row(s) still diverge -- expected while the open thread \
         is open; do NOT edit the expectations above"
    );
}

/// KNOWN GAP, not a MATCH divergence: `INFO` is not implemented here.
///
/// `=INFO("release")` is the addendum's provenance control -- it is how the
/// receipt records which Excel produced it (16.105). This engine has no `INFO`
/// and answers `#NAME?`. Recorded so the addendum's row set is accounted for
/// exhaustively and nobody later reads its absence as a silent skip.
#[test]
#[ignore = "INFO is unimplemented in this engine; the row is an oracle provenance control, not a MATCH claim"]
fn god234_addendum_info_release_control_is_unimplemented() {
    let mut e = addendum_fixture();
    assert_eq!(ev(&mut e, "=INFO(\"release\")"), "16.105");
}

/// The addendum's headline sibling result, stated as its own test because it is
/// the measurement that retires reviewer MAJOR-2 (VLOOKUP/HLOOKUP picking up the
/// equality-run stop across 3,111 corpus sites with no discriminating evidence).
///
/// Excel was measured over a sorted-with-duplicates vector `{1,3,5,5,5,9}` and
/// an unsorted-with-duplicates vector `{5,1,5,9,5}`, at keys 5, 6, 10 and 0, for
/// `MATCH`, `VLOOKUP`, `HLOOKUP` and `LOOKUP`. All four functions agree with
/// each other on all eight cases. This test asserts that the engine's
/// `MATCH`/`VLOOKUP`/`HLOOKUP` reproduce that agreement; `LOOKUP`'s two
/// divergent cells are held in
/// `god234_addendum_known_divergence_lookup_own_bisection`.
#[test]
fn god234_addendum_sibling_functions_agree_with_each_other() {
    let mut e = addendum_fixture();
    let mut fails = 0u32;
    println!("=== GOD-234 addendum sibling agreement (Excel 16.105.3) ===");
    // Sorted with duplicates: {1,3,5,5,5,9}; payload row {101..106}.
    for (key, index, payload) in [("5", "5", "105"), ("6", "5", "105"), ("10", "6", "106")] {
        row(
            &format!("c-sorted-match-{key}"),
            index,
            ev(&mut e, &format!("=MATCH({key},B26:G26,1)")),
            &mut fails,
        );
        row(
            &format!("c-sorted-hlookup-{key}"),
            payload,
            ev(&mut e, &format!("=HLOOKUP({key},B26:G27,2,TRUE)")),
            &mut fails,
        );
        row(
            &format!("c-sorted-vlookup-{key}"),
            payload,
            ev(&mut e, &format!("=VLOOKUP({key},L1:M6,2,TRUE)")),
            &mut fails,
        );
        row(
            &format!("c-sorted-lookup-{key}"),
            payload,
            ev(&mut e, &format!("=LOOKUP({key},B26:G26,B27:G27)")),
            &mut fails,
        );
    }
    // Key below every entry: #N/A from all four.
    row(
        "c-sorted-match-0",
        "#N/A",
        ev(&mut e, "=MATCH(0,B26:G26,1)"),
        &mut fails,
    );
    row(
        "c-sorted-hlookup-0",
        "#N/A",
        ev(&mut e, "=HLOOKUP(0,B26:G27,2,TRUE)"),
        &mut fails,
    );
    row(
        "c-sorted-vlookup-0",
        "#N/A",
        ev(&mut e, "=VLOOKUP(0,L1:M6,2,TRUE)"),
        &mut fails,
    );
    row(
        "c-sorted-lookup-0",
        "#N/A",
        ev(&mut e, "=LOOKUP(0,B26:G26,B27:G27)"),
        &mut fails,
    );
    // Unsorted with duplicates: {5,1,5,9,5}; payload row {201..205}. LOOKUP at
    // keys 5 and 6 is the held-out divergence, so it is absent here by design.
    for (key, index, payload) in [("5", "3", "203"), ("6", "3", "203"), ("10", "5", "205")] {
        row(
            &format!("c-unsorted-match-{key}"),
            index,
            ev(&mut e, &format!("=MATCH({key},B28:F28,1)")),
            &mut fails,
        );
        row(
            &format!("c-unsorted-hlookup-{key}"),
            payload,
            ev(&mut e, &format!("=HLOOKUP({key},B28:F29,2,TRUE)")),
            &mut fails,
        );
        row(
            &format!("c-unsorted-vlookup-{key}"),
            payload,
            ev(&mut e, &format!("=VLOOKUP({key},N1:O5,2,TRUE)")),
            &mut fails,
        );
    }
    row(
        "c-unsorted-lookup-10",
        "205",
        ev(&mut e, "=LOOKUP(10,B28:F28,B29:F29)"),
        &mut fails,
    );
    row(
        "c-unsorted-match-0",
        "#N/A",
        ev(&mut e, "=MATCH(0,B28:F28,1)"),
        &mut fails,
    );
    row(
        "c-unsorted-hlookup-0",
        "#N/A",
        ev(&mut e, "=HLOOKUP(0,B28:F29,2,TRUE)"),
        &mut fails,
    );
    row(
        "c-unsorted-vlookup-0",
        "#N/A",
        ev(&mut e, "=VLOOKUP(0,N1:O5,2,TRUE)"),
        &mut fails,
    );
    row(
        "c-unsorted-lookup-0",
        "#N/A",
        ev(&mut e, "=LOOKUP(0,B28:F28,B29:F29)"),
        &mut fails,
    );
    assert_eq!(
        fails, 0,
        "{fails} sibling row(s) diverge from measured Excel"
    );
}
