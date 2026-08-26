// Integration test for Calamine backend; run with `--features calamine,umya`.
use crate::common::build_workbook;
use formualizer_eval::engine::ingest::EngineLoadStream;
use formualizer_eval::engine::{Engine, EvalConfig};
use formualizer_workbook::{CalamineAdapter, LiteralValue, SpreadsheetReader};
use std::io::{Cursor, Read, Write};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

fn replace_zip_entry(bytes: Vec<u8>, entry_name: &str, replacement: &[u8]) -> Vec<u8> {
    let reader = Cursor::new(bytes);
    let mut archive = ZipArchive::new(reader).unwrap();
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).unwrap();
        let name = entry.name().to_string();
        if entry.is_dir() {
            writer.add_directory(name, options).unwrap();
            continue;
        }
        writer.start_file(&name, options).unwrap();
        if name == entry_name {
            writer.write_all(replacement).unwrap();
        } else {
            std::io::copy(&mut entry, &mut writer).unwrap();
        }
    }

    writer.finish().unwrap().into_inner()
}

fn inject_metadata_part(bytes: Vec<u8>, metadata_xml: &str) -> Vec<u8> {
    let reader = Cursor::new(bytes);
    let mut archive = ZipArchive::new(reader).unwrap();
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).unwrap();
        let name = entry.name().to_string();
        if entry.is_dir() {
            writer.add_directory(name, options).unwrap();
            continue;
        }
        let mut data = Vec::new();
        entry.read_to_end(&mut data).unwrap();
        if name == "[Content_Types].xml" {
            let xml = String::from_utf8(data).unwrap().replace(
                "</Types>",
                r#"<Override PartName="/xl/metadata.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheetMetadata+xml"/></Types>"#,
            );
            data = xml.into_bytes();
        } else if name == "xl/_rels/workbook.xml.rels" {
            let xml = String::from_utf8(data).unwrap().replace(
                "</Relationships>",
                r#"<Relationship Id="rIdMetadata" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sheetMetadata" Target="metadata.xml"/></Relationships>"#,
            );
            data = xml.into_bytes();
        }
        writer.start_file(name, options).unwrap();
        writer.write_all(&data).unwrap();
    }

    writer.start_file("xl/metadata.xml", options).unwrap();
    writer.write_all(metadata_xml.as_bytes()).unwrap();
    writer.finish().unwrap().into_inner()
}

fn workbook_with_raw_sheet(sheet_xml: &str) -> (std::path::PathBuf, Vec<u8>) {
    let path = build_workbook(|_| {});
    let bytes = std::fs::read(&path).unwrap();
    let bytes = replace_zip_entry(bytes, "xl/worksheets/sheet1.xml", sheet_xml.as_bytes());
    std::fs::write(&path, &bytes).unwrap();
    (path, bytes)
}

fn assert_number(
    engine: &Engine<formualizer_eval::test_workbook::TestWorkbook>,
    row: u32,
    col: u32,
    expected: f64,
) {
    assert_eq!(
        engine.get_cell_value("Sheet1", row, col),
        Some(LiteralValue::Number(expected)),
        "unexpected value at row {row}, column {col}"
    );
}

fn assert_empty(
    engine: &Engine<formualizer_eval::test_workbook::TestWorkbook>,
    row: u32,
    col: u32,
) {
    assert!(
        matches!(
            engine.get_cell_value("Sheet1", row, col),
            None | Some(LiteralValue::Empty)
        ),
        "expected empty value at row {row}, column {col}"
    );
}

fn assert_value_error(
    engine: &Engine<formualizer_eval::test_workbook::TestWorkbook>,
    row: u32,
    col: u32,
) {
    match engine.get_cell_value("Sheet1", row, col) {
        Some(LiteralValue::Error(error)) => assert_eq!(error.to_string(), "#VALUE!"),
        other => panic!("expected #VALUE! at row {row}, column {col}, got {other:?}"),
    }
}

fn inject_external_link_rels(bytes: Vec<u8>, idx: u32, target: &str) -> Vec<u8> {
    let reader = Cursor::new(bytes);
    let mut archive = ZipArchive::new(reader).unwrap();

    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).unwrap();
        let name = entry.name().to_string();
        if entry.is_dir() {
            let _ = writer.add_directory(name, options);
            continue;
        }

        let mut data = Vec::new();
        entry.read_to_end(&mut data).unwrap();
        writer.start_file(name, options).unwrap();
        writer.write_all(&data).unwrap();
    }

    let rels_name = format!("xl/externalLinks/_rels/externalLink{idx}.xml.rels");
    let rels_xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\n  <Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/externalLinkPath\" Target=\"{target}\" TargetMode=\"External\"/>\n</Relationships>\n"
    );
    let _ = writer.add_directory("xl/externalLinks/_rels/".to_string(), options);
    writer.start_file(rels_name, options).unwrap();
    writer.write_all(rels_xml.as_bytes()).unwrap();

    writer.finish().unwrap().into_inner()
}

#[test]
fn calamine_extracts_formulas_and_normalizes_equals() {
    let path = build_workbook(|book| {
        let sh = book.get_sheet_by_name_mut("Sheet1").unwrap();
        sh.get_cell_mut((1, 1)).set_value_number(10); // A1
        sh.get_cell_mut((2, 1)).set_formula("A1+5"); // B1 no leading '='
        sh.get_cell_mut((1, 2)).set_formula("=A1*2"); // A2 with leading '='
        sh.get_cell_mut((2, 2)).set_value_number(3); // B2 value only
    });

    let mut backend = CalamineAdapter::open_path(&path).unwrap();
    let ctx = formualizer_eval::test_workbook::TestWorkbook::new();
    let mut engine: Engine<_> = Engine::new(ctx, EvalConfig::default());
    backend.stream_into_engine(&mut engine).unwrap();
    engine.evaluate_all().unwrap();

    match engine.get_cell_value("Sheet1", 1, 2) {
        // B1
        Some(LiteralValue::Number(n)) => assert!((n - 15.0).abs() < 1e-9, "Expected 15 got {n}"),
        other => panic!("Unexpected B1: {other:?}"),
    }
    match engine.get_cell_value("Sheet1", 2, 1) {
        // A2
        Some(LiteralValue::Number(n)) => assert!((n - 20.0).abs() < 1e-9, "Expected 20 got {n}"),
        other => panic!("Unexpected A2: {other:?}"),
    }
}

#[test]
fn calamine_error_cells_map() {
    let path = build_workbook(|book| {
        let sh = book.get_sheet_by_name_mut("Sheet1").unwrap();
        sh.get_cell_mut((1, 1)).set_formula("=1/0"); // #DIV/0!
    });
    let mut backend = CalamineAdapter::open_path(&path).unwrap();
    let sheet = backend.read_sheet("Sheet1").unwrap();
    // Formula node will exist; value is None until evaluation – we focus on later error propagation
    assert!(sheet.cells.contains_key(&(1, 1)));
}

#[test]
fn calamine_loads_external_link_index_formulas() {
    let path = build_workbook(|book| {
        let sh = book.get_sheet_by_name_mut("Sheet1").unwrap();
        sh.get_cell_mut((1, 1))
            .set_formula("=SUM([33]Sheet1!$B:$B)");
    });

    let bytes = std::fs::read(&path).expect("read workbook bytes");
    let bytes = inject_external_link_rels(bytes, 33, "file:///C:/tmp/external.xlsx");
    std::fs::write(&path, bytes).expect("rewrite workbook with external link rels");

    let mut backend = CalamineAdapter::open_path(&path).unwrap();
    assert_eq!(
        backend.external_link_target(33),
        Some("file:///C:/tmp/external.xlsx")
    );

    let ctx = formualizer_eval::test_workbook::TestWorkbook::new();
    let mut engine: Engine<_> = Engine::new(ctx, EvalConfig::default());
    backend.stream_into_engine(&mut engine).unwrap();
    engine.build_graph_all().unwrap();
}

#[test]
fn calamine_loads_external_link_index_formulas_from_bytes() {
    let path = build_workbook(|book| {
        let sh = book.get_sheet_by_name_mut("Sheet1").unwrap();
        sh.get_cell_mut((1, 1))
            .set_formula("=SUM([33]Sheet1!$B:$B)");
    });

    let bytes = std::fs::read(&path).expect("read workbook bytes");
    let bytes = inject_external_link_rels(bytes, 33, "file:///C:/tmp/external.xlsx");

    let mut backend = CalamineAdapter::open_bytes(bytes).expect("open workbook from bytes");
    assert_eq!(
        backend.external_link_target(33),
        Some("file:///C:/tmp/external.xlsx")
    );

    let ctx = formualizer_eval::test_workbook::TestWorkbook::new();
    let mut engine: Engine<_> = Engine::new(ctx, EvalConfig::default());
    backend.stream_into_engine(&mut engine).unwrap();
    engine.build_graph_all().unwrap();
}

#[test]
fn array_formula_ref_cached_values_do_not_block() {
    let sheet_xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:G3"/>
  <sheetData>
    <row r="1">
      <c r="A1"><v>5</v></c><c r="B1"><v>6</v></c><c r="C1"><v>7</v></c>
      <c r="E1"><f t="array" ref="E1:E3">TRANSPOSE(A1:C1)</f><v>5</v></c>
      <c r="G1"><f>SUM(E1:E3)</f><v>18</v></c>
    </row>
    <row r="2"><c r="E2"><v>6</v></c></row>
    <row r="3"><c r="E3"><v>7</v></c></row>
  </sheetData>
</worksheet>"#;
    let (path, _) = workbook_with_raw_sheet(sheet_xml);
    let mut adapter = CalamineAdapter::open_path(path).unwrap();
    let mut engine = Engine::new(
        formualizer_eval::test_workbook::TestWorkbook::new(),
        EvalConfig {
            defer_graph_building: true,
            ..EvalConfig::default()
        },
    );
    adapter.stream_into_engine(&mut engine).unwrap();
    engine.evaluate_all().unwrap();

    assert_number(&engine, 1, 5, 5.0);
    assert_number(&engine, 2, 5, 6.0);
    assert_number(&engine, 3, 5, 7.0);
    assert_number(&engine, 1, 7, 18.0);
    let stats = adapter.load_stats().unwrap();
    assert_eq!(stats.formula_cells_observed, Some(2));
    assert_eq!(stats.value_cells_observed, Some(3));
}

#[test]
fn array_formula_ref_empty_members_do_not_parse() {
    let sheet_xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:E3"/>
  <sheetData>
    <row r="1">
      <c r="A1"><v>5</v></c><c r="B1"><v>6</v></c><c r="C1"><v>7</v></c>
      <c r="E1"><f t="array" ref="E1:E3">TRANSPOSE(A1:C1)</f><v>5</v></c>
    </row>
    <row r="2"><c r="E2"><f ca="1"/><v>6</v></c></row>
    <row r="3"><c r="E3"><f ca="1"/><v>7</v></c></row>
  </sheetData>
</worksheet>"#;
    let (_, bytes) = workbook_with_raw_sheet(sheet_xml);
    let mut adapter = CalamineAdapter::open_bytes(bytes).unwrap();
    let mut engine = Engine::new(
        formualizer_eval::test_workbook::TestWorkbook::new(),
        EvalConfig {
            defer_graph_building: true,
            ..EvalConfig::default()
        },
    );
    let load = adapter.stream_into_engine(&mut engine);
    assert!(
        load.is_ok(),
        "empty array members must not be parsed as formulas: {load:?}"
    );
    engine.evaluate_all().unwrap();

    assert_number(&engine, 1, 5, 5.0);
    assert_number(&engine, 2, 5, 6.0);
    assert_number(&engine, 3, 5, 7.0);
    let stats = adapter.load_stats().unwrap();
    assert_eq!(stats.formula_cells_observed, Some(1));
    assert_eq!(stats.value_cells_observed, Some(3));
}

#[test]
fn array_formula_ref_single_cell_unchanged() {
    let sheet_xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:B1"/>
  <sheetData>
    <row r="1">
      <c r="A1"><v>-5</v></c>
      <c r="B1"><f t="array" ref="B1">ABS(A1)</f><v>99</v></c>
    </row>
  </sheetData>
</worksheet>"#;
    let (_, bytes) = workbook_with_raw_sheet(sheet_xml);
    let mut adapter = CalamineAdapter::open_bytes(bytes).unwrap();
    let mut engine = Engine::new(
        formualizer_eval::test_workbook::TestWorkbook::new(),
        EvalConfig::default(),
    );
    adapter.stream_into_engine(&mut engine).unwrap();
    engine.evaluate_all().unwrap();

    assert_number(&engine, 1, 2, 5.0);
    let stats = adapter.load_stats().unwrap();
    assert_eq!(stats.formula_cells_observed, Some(1));
    assert_eq!(stats.value_cells_observed, Some(1));
}

#[test]
fn array_formula_ref_shared_unchanged() {
    let sheet_xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:B3"/>
  <sheetData>
    <row r="1"><c r="A1"><v>10</v></c><c><f t="shared" ref="B1:B3" si="0">A1+1</f><v>11</v></c></row>
    <row r="2"><c><v>20</v></c><c><f t="shared" si="0"/><v>21</v></c></row>
    <row><c><v>30</v></c><c><f t="shared" si="0"/><v>31</v></c></row>
  </sheetData>
</worksheet>"#;
    let (_, bytes) = workbook_with_raw_sheet(sheet_xml);
    let mut adapter = CalamineAdapter::open_bytes(bytes).unwrap();
    let mut engine = Engine::new(
        formualizer_eval::test_workbook::TestWorkbook::new(),
        EvalConfig::default(),
    );
    adapter.stream_into_engine(&mut engine).unwrap();
    engine.evaluate_all().unwrap();

    assert_number(&engine, 1, 2, 11.0);
    assert_number(&engine, 2, 2, 21.0);
    assert_number(&engine, 3, 2, 31.0);
    let stats = adapter.load_stats().unwrap();
    assert_eq!(stats.shared_formula_tags_observed, Some(3));
}

#[test]
fn plain_formula_final_range_uses_legacy_positional_intersection() {
    let sheet_xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:D4"/>
  <sheetData>
    <row r="1"><c r="A1"><v>11</v></c></row>
    <row r="2"><c r="A2"><v>22</v></c><c r="D2"><f>OFFSET(A1:A3,0,0)</f></c></row>
    <row r="3"><c r="A3"><v>33</v></c></row>
  </sheetData>
</worksheet>"#;
    let (_, bytes) = workbook_with_raw_sheet(sheet_xml);
    let mut adapter = CalamineAdapter::open_bytes(bytes).unwrap();
    let mut engine = Engine::new(
        formualizer_eval::test_workbook::TestWorkbook::new(),
        EvalConfig {
            defer_graph_building: true,
            ..EvalConfig::default()
        },
    );
    adapter.stream_into_engine(&mut engine).unwrap();
    engine.evaluate_all().unwrap();

    assert_number(&engine, 2, 4, 22.0);
    assert_empty(&engine, 3, 4);
    assert_empty(&engine, 4, 4);
}

#[test]
fn plain_formula_legacy_finalization_handles_horizontal_outside_2d_and_computed_arrays() {
    let sheet_xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:N13"/>
  <sheetData>
    <row r="1"><c r="A1"><v>11</v></c><c r="B1"><v>22</v></c><c r="C1"><v>33</v></c></row>
    <row r="2"><c r="A2"><v>22</v></c><c r="B2"><v>44</v></c></row>
    <row r="3"><c r="A3"><v>33</v></c></row>
    <row r="5"><c r="B5"><f>OFFSET(A1:C1,0,0)</f></c></row>
    <row r="8"><c r="G8"><f>OFFSET(A1:A3,0,0)</f></c><c r="M8"><f>OFFSET(A1:B2,0,0)</f></c><c r="N8"><f>@OFFSET(A1:B2,0,0)</f></c></row>
    <row r="12"><c r="J12"><f>SEQUENCE(2,2)</f></c></row>
  </sheetData>
</worksheet>"#;
    let (_, bytes) = workbook_with_raw_sheet(sheet_xml);
    let mut adapter = CalamineAdapter::open_bytes(bytes).unwrap();
    let mut engine = Engine::new(
        formualizer_eval::test_workbook::TestWorkbook::new(),
        EvalConfig::default(),
    );
    adapter.stream_into_engine(&mut engine).unwrap();
    engine.evaluate_all().unwrap();

    assert_number(&engine, 5, 2, 22.0);
    assert_empty(&engine, 5, 3);
    assert_empty(&engine, 5, 4);
    assert_value_error(&engine, 8, 7);
    assert_value_error(&engine, 8, 13);
    assert_value_error(&engine, 8, 14);
    assert_number(&engine, 12, 10, 1.0);
    assert_empty(&engine, 12, 11);
    assert_empty(&engine, 13, 10);
}

#[test]
fn cse_single_cell_fence_uses_top_left_and_never_implicit_intersection() {
    let sheet_xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:D4"/>
  <sheetData>
    <row r="1"><c r="A1"><v>11</v></c></row>
    <row r="2"><c r="A2"><v>22</v></c><c r="D2"><f t="array" ref="D2">OFFSET(A1:A3,0,0)</f></c></row>
    <row r="3"><c r="A3"><v>33</v></c></row>
  </sheetData>
</worksheet>"#;
    let (_, bytes) = workbook_with_raw_sheet(sheet_xml);
    let mut adapter = CalamineAdapter::open_bytes(bytes).unwrap();
    let mut engine = Engine::new(
        formualizer_eval::test_workbook::TestWorkbook::new(),
        EvalConfig::default(),
    );
    adapter.stream_into_engine(&mut engine).unwrap();
    engine.evaluate_all().unwrap();

    assert_number(&engine, 2, 4, 11.0);
    assert_empty(&engine, 3, 4);
    assert_empty(&engine, 4, 4);
}

#[test]
fn cse_multi_cell_fence_clips_result_to_authored_rectangle() {
    let sheet_xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:D4"/>
  <sheetData>
    <row r="1"><c r="A1"><v>11</v></c></row>
    <row r="2"><c r="A2"><v>22</v></c><c r="D2"><f t="array" ref="D2:D3">OFFSET(A1:A3,0,0)</f></c></row>
    <row r="3"><c r="A3"><v>33</v></c><c r="D3"><v>22</v></c></row>
  </sheetData>
</worksheet>"#;
    let (_, bytes) = workbook_with_raw_sheet(sheet_xml);
    let mut adapter = CalamineAdapter::open_bytes(bytes).unwrap();
    let mut engine = Engine::new(
        formualizer_eval::test_workbook::TestWorkbook::new(),
        EvalConfig::default(),
    );
    adapter.stream_into_engine(&mut engine).unwrap();
    engine.evaluate_all().unwrap();

    assert_number(&engine, 2, 4, 11.0);
    assert_number(&engine, 3, 4, 22.0);
    assert_empty(&engine, 4, 4);
}

#[test]
fn xldapr_dynamic_array_ignores_single_cell_array_ref_fence() {
    let sheet_xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:D4"/>
  <sheetData>
    <row r="1"><c r="A1"><v>11</v></c></row>
    <row r="2"><c r="A2"><v>22</v></c><c r="D2" cm="1"><f t="array" ref="D2">OFFSET(A1:A3,0,0)</f></c></row>
    <row r="3"><c r="A3"><v>33</v></c></row>
  </sheetData>
</worksheet>"#;
    let metadata_xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<metadata xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:xda="http://schemas.microsoft.com/office/spreadsheetml/2017/dynamicarray">
  <metadataTypes count="1"><metadataType name="XLDAPR" minSupportedVersion="120000" cellMeta="1"/></metadataTypes>
  <futureMetadata name="XLDAPR" count="1"><bk><extLst><ext uri="{bdbb8cdc-fa1e-496e-a857-3c3f30c029c3}"><xda:dynamicArrayProperties fDynamic="1" fCollapsed="0"/></ext></extLst></bk></futureMetadata>
  <cellMetadata count="1"><bk><rc t="1" v="0"/></bk></cellMetadata>
</metadata>"#;
    let (_, bytes) = workbook_with_raw_sheet(sheet_xml);
    let bytes = inject_metadata_part(bytes, metadata_xml);
    let mut adapter = CalamineAdapter::open_bytes(bytes).unwrap();
    let mut engine = Engine::new(
        formualizer_eval::test_workbook::TestWorkbook::new(),
        EvalConfig {
            defer_graph_building: true,
            ..EvalConfig::default()
        },
    );
    adapter.stream_into_engine(&mut engine).unwrap();
    engine.evaluate_all().unwrap();

    assert_number(&engine, 2, 4, 11.0);
    assert_number(&engine, 3, 4, 22.0);
    assert_number(&engine, 4, 4, 33.0);
}

#[test]
fn loaded_legacy_whole_column_and_cross_sheet_ranges_intersect_positionally() {
    let path = build_workbook(|book| {
        let sheet = book.get_sheet_by_name_mut("Sheet1").unwrap();
        sheet.get_cell_mut((1, 1)).set_value_number(11);
        sheet.get_cell_mut((1, 2)).set_value_number(22);
        sheet.get_cell_mut((1, 3)).set_value_number(33);
        sheet.get_cell_mut((2, 2)).set_formula("A:A");

        book.new_sheet("Other").unwrap();
        book.get_sheet_by_name_mut("Other")
            .unwrap()
            .get_cell_mut((4, 2))
            .set_formula("OFFSET(Sheet1!A1:A3,0,0)");
    });
    let mut adapter = CalamineAdapter::open_path(path).unwrap();
    let mut engine = Engine::new(
        formualizer_eval::test_workbook::TestWorkbook::new(),
        EvalConfig::default(),
    );
    adapter.stream_into_engine(&mut engine).unwrap();
    engine.evaluate_all().unwrap();

    assert_number(&engine, 2, 2, 22.0);
    assert_eq!(
        engine.get_cell_value("Other", 2, 4),
        Some(LiteralValue::Number(22.0))
    );
    assert!(matches!(
        engine.get_cell_value("Other", 3, 4),
        None | Some(LiteralValue::Empty)
    ));
}
