//! A SheetPort session that re-sends an input it already holds must not
//! recompute the ports that depend on it.

use formualizer_common::{ExcelError, LiteralValue};
use formualizer_sheetport::{EvalOptions, InputUpdate, PortValue, SheetPortSession};
use formualizer_workbook::{CustomFnOptions, Workbook};
use sheetport_spec::Manifest;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const MANIFEST: &str = r#"
spec: fio
spec_version: "0.3.0"
manifest:
  id: unchanged-inputs
  name: Unchanged Inputs
  workbook:
    uri: memory://unchanged.xlsx
    locale: en-US
    date_system: 1900
ports:
  - id: input_a
    dir: in
    shape: scalar
    location:
      a1: Sheet!A1
    schema:
      type: number
  - id: output_b
    dir: out
    shape: scalar
    location:
      a1: Sheet!B1
    schema:
      type: number
"#;

fn scalar(value: &PortValue) -> f64 {
    match value {
        PortValue::Scalar(LiteralValue::Number(n)) => *n,
        PortValue::Scalar(LiteralValue::Int(i)) => *i as f64,
        other => panic!("expected a scalar number, got {other:?}"),
    }
}

#[test]
fn rewriting_an_input_with_its_current_value_skips_recomputation() {
    let manifest: Manifest = Manifest::from_yaml_str(MANIFEST).expect("manifest parses");

    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);

    let mut workbook = Workbook::new();
    workbook.add_sheet("Sheet").unwrap();
    workbook
        .register_custom_function(
            "myfn",
            CustomFnOptions::default(),
            Arc::new(move |args: &[LiteralValue]| -> Result<LiteralValue, ExcelError> {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(args.first().cloned().unwrap_or(LiteralValue::Empty))
            }),
        )
        .unwrap();
    workbook
        .set_value("Sheet", 1, 1, LiteralValue::Number(5.0))
        .unwrap();
    workbook.set_formula("Sheet", 1, 2, "=MYFN(A1)").unwrap();

    let mut session = SheetPortSession::new(workbook, manifest).expect("session created");

    let outputs = session.evaluate_once(EvalOptions::default()).expect("eval");
    assert_eq!(scalar(outputs.get("output_b").expect("output")), 5.0);
    let baseline = calls.load(Ordering::SeqCst);
    assert_eq!(baseline, 1);

    // Re-send the value the input cell already holds.
    let mut update = InputUpdate::new();
    update.insert("input_a", PortValue::Scalar(LiteralValue::Number(5.0)));
    session.write_inputs(update).expect("write unchanged input");
    let outputs = session.evaluate_once(EvalOptions::default()).expect("eval");
    assert_eq!(scalar(outputs.get("output_b").expect("output")), 5.0);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        baseline,
        "an unchanged input must not re-run the dependent port"
    );

    // A real change still propagates.
    let mut update = InputUpdate::new();
    update.insert("input_a", PortValue::Scalar(LiteralValue::Number(9.0)));
    session.write_inputs(update).expect("write changed input");
    let outputs = session.evaluate_once(EvalOptions::default()).expect("eval");
    assert_eq!(scalar(outputs.get("output_b").expect("output")), 9.0);
    assert_eq!(calls.load(Ordering::SeqCst), baseline + 1);
}
