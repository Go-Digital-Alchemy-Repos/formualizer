//! A 3-D span site must give the explicit member sum on the FIRST
//! `evaluate_all` after value writes on a loaded workbook (GOD-242 / CL-051).
//!
//! Fixture shape (invented values, no client data), shortened from the real
//! caller's shape:
//!
//! * thirteen tabs -- `Input`, `Calc`, `Acct1..Acct11` -- so the span
//!   `Acct1:Acct11!L18` covers eleven members in registration order;
//! * a defined name (`SeedInput`, written into `xl/workbook.xml`) covering the
//!   input cell the test writes before evaluating;
//! * a ten-hop formula chain from that input cell to every member cell
//!   (`Input!B2 -> Calc!C2 -> D2 -> E2 -> F2 -> G2 -> Acctk!M18 -> N18 -> O18
//!   -> P18 -> Acctk!L18`);
//! * a STATIC cycle that is live-acyclic: `Calc!C2` adds an `IF` whose
//!   never-taken arm reads the span site `Calc!W18`, so the static graph has a
//!   cycle through the site, the members and the whole chain (one static SCC),
//!   while no live read ever closes it.  That is the caller's "phantom" SCC
//!   shape: the site is INSIDE the SCC, not downstream of it;
//! * the span-site family authored as a real shared formula
//!   (`<f t="shared" si=.. ref="W18:W20">` with text-less followers);
//! * `<calcPr iterate="1">` and saved formula caches, as the caller's file has.
//!
//! On the pin the site is evaluated exactly once, in the SCC unit's initial
//! sweep, while its members still hold pre-write values; having recorded no
//! live edges (the span resolves to an owned `"__tmp"` view the recorder
//! cannot attribute), it is never in the settle loop's stale set, so it is
//! never re-evaluated as the members move.  It therefore latches a stale sum:
//! this test measures `0` against an explicit member sum of `462` on the pin.

use formualizer_workbook::{
    CalamineAdapter, LiteralValue, LoadStrategy, SpreadsheetReader, Workbook, WorkbookConfig,
};
use std::io::{Read, Write};

const MEMBER_SHEETS: [&str; 11] = [
    "Acct1", "Acct2", "Acct3", "Acct4", "Acct5", "Acct6", "Acct7", "Acct8", "Acct9", "Acct10",
    "Acct11",
];

/// Cached value each formula carries in the saved file, i.e. the fixed point
/// for the seed value `100` the fixture is authored with.
fn seeded_caches() -> Vec<(String, f64)> {
    let mut caches: Vec<(String, f64)> = vec![
        (
            "Input!$B$2+IF(Input!$C$2&lt;0,Calc!$W$18,0)".to_string(),
            100.0,
        ),
        ("C2*1".to_string(), 100.0),
        ("D2*1".to_string(), 100.0),
        ("E2*1".to_string(), 100.0),
        ("F2*1".to_string(), 100.0),
    ];
    for (index, _) in MEMBER_SHEETS.iter().enumerate() {
        let factor = (index + 1) as f64;
        caches.push((format!("Calc!$G$2*{factor}"), 100.0 * factor));
    }
    caches
}

fn build_fixture() -> Vec<u8> {
    let mut book = umya_spreadsheet::new_file();
    book.get_sheet_mut(&0).unwrap().set_name("Input");
    book.new_sheet("Calc").unwrap();
    for sheet in MEMBER_SHEETS {
        book.new_sheet(sheet).unwrap();
    }

    book.get_sheet_by_name_mut("Input")
        .unwrap()
        .get_cell_mut("B2")
        .set_value_number(100.0);
    book.get_sheet_by_name_mut("Input")
        .unwrap()
        .get_cell_mut("C2")
        .set_value_number(1.0);

    {
        let calc = book.get_sheet_by_name_mut("Calc").unwrap();
        // Hop 1 carries the static back-edge: the `IF`'s second arm is never
        // taken at run time (`Input!C2` is positive) and whose condition reads a
        // cell OUTSIDE the component (so no live edge closes the loop), but it
        // puts the chain,
        // the eleven members and the span site `Calc!W18` in one STATIC
        // strongly connected component that is live-acyclic.
        calc.get_cell_mut("C2")
            .set_formula("Input!$B$2+IF(Input!$C$2<0,Calc!$W$18,0)");
        calc.get_cell_mut("D2").set_formula("C2*1"); // hop 2
        calc.get_cell_mut("E2").set_formula("D2*1"); // hop 3
        calc.get_cell_mut("F2").set_formula("E2*1"); // hop 4
        calc.get_cell_mut("G2").set_formula("F2*1"); // hop 5
        // The span-site family; rewritten into a shared family below.
        calc.get_cell_mut("W18")
            .set_formula("SUM(Acct1:Acct11!L18)");
        calc.get_cell_mut("W19")
            .set_formula("SUM(Acct1:Acct11!L19)");
        calc.get_cell_mut("W20")
            .set_formula("SUM(Acct1:Acct11!L20)");
    }

    for (index, sheet) in MEMBER_SHEETS.iter().enumerate() {
        let factor = index + 1;
        let ws = book.get_sheet_by_name_mut(sheet).unwrap();
        ws.get_cell_mut("M18")
            .set_formula(format!("Calc!$G$2*{factor}")); // hop 6
        ws.get_cell_mut("N18").set_formula("M18*1"); // hop 7
        ws.get_cell_mut("O18").set_formula("N18*1"); // hop 8
        ws.get_cell_mut("P18").set_formula("O18*1"); // hop 9
        ws.get_cell_mut("L18").set_formula("P18*1"); // hop 10
    }

    let mut bytes = Vec::new();
    umya_spreadsheet::writer::xlsx::write_writer(&book, &mut bytes).unwrap();

    let bytes = rewrite_entries(&bytes, |name, xml| {
        if name == "xl/workbook.xml" {
            // openpyxl/umya will not author this for us; write the
            // `<definedName>` straight into the package.
            let close = xml.find("</sheets>").expect("sheets element") + "</sheets>".len();
            return Some(format!(
                "{}<definedNames><definedName name=\"SeedInput\">Input!$B$2</definedName></definedNames>{}",
                &xml[..close],
                &xml[close..]
            ));
        }
        if name.starts_with("xl/worksheets/sheet") && xml.contains("SUM(Acct1:Acct11!L18)") {
            // A genuine shared family: master carries `t="shared" si ref`, the
            // followers carry no formula text at all.
            let rewritten = xml
                .replace(
                    "<f>SUM(Acct1:Acct11!L18)</f>",
                    "<f t=\"shared\" si=\"41\" ref=\"W18:W20\">SUM(Acct1:Acct11!L18)</f>",
                )
                .replace(
                    "<f>SUM(Acct1:Acct11!L19)</f>",
                    "<f t=\"shared\" si=\"41\"></f>",
                )
                .replace(
                    "<f>SUM(Acct1:Acct11!L20)</f>",
                    "<f t=\"shared\" si=\"41\"></f>",
                );
            assert!(rewritten.contains("t=\"shared\""));
            return Some(rewritten);
        }
        None
    });

    let caches = seeded_caches();
    let bytes = rewrite_entries(&bytes, |name, xml| {
        if !name.starts_with("xl/worksheets/sheet") {
            return None;
        }
        let mut xml = xml.to_string();
        for (formula, cached) in &caches {
            xml = xml.replace(
                &format!("<f>{formula}</f><v/>"),
                &format!("<f>{formula}</f><v>{cached}</v>"),
            );
        }
        // The shared master and its followers carry caches too.
        let master_cache = 100.0 * (1..=11).map(|k| k as f64).sum::<f64>();
        xml = xml.replace(
            "<f t=\"shared\" si=\"41\" ref=\"W18:W20\">SUM(Acct1:Acct11!L18)</f><v/>",
            &format!(
                "<f t=\"shared\" si=\"41\" ref=\"W18:W20\">SUM(Acct1:Acct11!L18)</f><v>{master_cache}</v>"
            ),
        );
        xml = xml.replace(
            "<f t=\"shared\" si=\"41\"></f><v/>",
            "<f t=\"shared\" si=\"41\"></f><v>0</v>",
        );
        Some(xml)
    });

    rewrite_entries(&bytes, |name, xml| {
        if name != "xl/workbook.xml" {
            return None;
        }
        let calc_pr =
            r#"<calcPr calcId="122211" iterate="1" iterateCount="100" iterateDelta="0.001"/>"#;
        Some(if let Some(start) = xml.find("<calcPr") {
            let end = start + xml[start..].find("/>").expect("self-closing calcPr") + 2;
            format!("{}{}{}", &xml[..start], calc_pr, &xml[end..])
        } else {
            let close = xml.rfind("</workbook>").expect("workbook element");
            format!("{}{}{}", &xml[..close], calc_pr, &xml[close..])
        })
    })
}

/// Rewrite zip entries in place with the `zip` crate directly, so the fixture
/// is independent of the crate-under-test's write path.
fn rewrite_entries(xlsx: &[u8], mut rewrite: impl FnMut(&str, &str) -> Option<String>) -> Vec<u8> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(xlsx)).unwrap();
    let mut out = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut out));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).unwrap();
            let name = entry.name().to_string();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            writer.start_file(&name, opts).unwrap();
            let replacement = String::from_utf8(bytes.clone())
                .ok()
                .and_then(|xml| rewrite(&name, &xml));
            match replacement {
                Some(xml) => writer.write_all(xml.as_bytes()).unwrap(),
                None => writer.write_all(&bytes).unwrap(),
            }
        }
        writer.finish().unwrap();
    }
    out
}

fn num(wb: &Workbook, sheet: &str, row: u32, col: u32) -> f64 {
    match wb.get_value(sheet, row, col) {
        Some(LiteralValue::Number(n)) => n,
        Some(LiteralValue::Int(i)) => i as f64,
        other => panic!("{sheet}!r{row}c{col} is not numeric: {other:?}"),
    }
}

#[test]
fn three_dimensional_span_after_value_writes_before_first_evaluation_equals_explicit_member_sum() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("span_first_evaluation.xlsx");
    std::fs::write(&path, build_fixture()).unwrap();

    let adapter = CalamineAdapter::open_path(&path).expect("load fixture from path");
    let mut wb =
        Workbook::from_reader(adapter, LoadStrategy::EagerAll, WorkbookConfig::ephemeral())
            .expect("workbook from reader");

    // A probe proving the defined name in the package really covers the input
    // cell the test writes (the caller addresses its inputs by name).
    wb.set_formula("Calc", 1, 26, "=SeedInput")
        .expect("defined-name probe");

    // Write the input BEFORE the first evaluation, exactly as the caller does.
    wb.set_value("Input", 2, 2, LiteralValue::Number(7.0))
        .expect("set input value");

    let first = wb.evaluate_all().expect("first evaluate_all");
    assert_eq!(first.cycle_errors, 0, "phantom SCC must not error");

    // Oracle: the eleven member cells as the engine itself settled them, summed
    // in span order -- an accepting-side re-derivation of what the span reads,
    // independent of the span code path.
    assert_eq!(
        num(&wb, "Calc", 1, 26),
        7.0,
        "defined name SeedInput must resolve to the written input cell"
    );

    let members: Vec<f64> = MEMBER_SHEETS.iter().map(|s| num(&wb, s, 18, 12)).collect();
    let explicit_sum = members.iter().fold(0.0f64, |acc, v| acc + v);
    let site = num(&wb, "Calc", 18, 23);

    assert_eq!(
        site.to_bits(),
        explicit_sum.to_bits(),
        "first evaluate_all: span site {site} != explicit member sum {explicit_sum} (members {members:?})"
    );

    // ... and the first evaluation is already the fixed point.
    wb.evaluate_all().expect("second evaluate_all");
    let site_again = num(&wb, "Calc", 18, 23);
    assert_eq!(
        site_again.to_bits(),
        site.to_bits(),
        "second evaluate_all moved the span site: {site} -> {site_again}"
    );
    let members_again: Vec<f64> = MEMBER_SHEETS.iter().map(|s| num(&wb, s, 18, 12)).collect();
    assert_eq!(members_again, members, "members moved on the second pass");
}
