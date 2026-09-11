//! CL-087 / ES-008: registry-wide regression anchor for element-wise lifting.
//!
//! `elementwise_lifted_positions` is the single source of truth for which
//! argument positions the interpreter lifts element-wise when a range or array
//! arrives in a scalar-shaped slot. This module materialises that map for every
//! registered function and asserts it against a checked-in expected table, so
//! that any future change to the lifted set shows up as a diff in
//! `elementwise_lifted_positions.txt` rather than silently changing semantics.
//!
//! Regenerate the table with:
//!
//! ```text
//! FORMUALIZER_WRITE_LIFT_SNAPSHOT=crates/formualizer-eval/src/tests/elementwise_lifted_positions.txt \
//!   cargo test -p formualizer-eval --all-features \
//!   tests::elementwise_lifting_registry::elementwise_lifted_positions_registry_anchor -- --exact
//! ```
//!
//! Run it on its own (as above) so that no other test's locally registered
//! probe function leaks into the snapshot.

use crate::function::Function;
use std::collections::BTreeMap;

/// The arity at which each function's lifted positions are materialised.
///
/// Fixed and self-describing so the snapshot is reproducible: the declared
/// schema width (never below `min_args`), plus two extra slots for variadic
/// functions so that a repeating scalar tail is exercised.
fn probe_arity(function: &dyn Function) -> usize {
    let width = function.arg_schema().len().max(function.min_args());
    if function.variadic() {
        width + 2
    } else {
        width
    }
}

fn caps_names(function: &dyn Function) -> String {
    let names: Vec<&str> = function.caps().iter_names().map(|(name, _)| name).collect();
    if names.is_empty() {
        "-".to_string()
    } else {
        names.join("|")
    }
}

fn scalar_slot_indices(function: &dyn Function, arity: usize) -> Vec<usize> {
    crate::function::elementwise_positions_from_schema(function.arg_schema(), arity)
}

fn format_positions(positions: Option<&Vec<usize>>) -> String {
    match positions {
        None => "none".to_string(),
        Some(values) if values.is_empty() => "empty".to_string(),
        Some(values) => values
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(","),
    }
}

/// Probe fixtures registered globally by OTHER tests in this binary whose
/// `arg_schema()` deliberately panics (see
/// `function_registry::tests::PanickingSchemaFn`). They are never builtins and
/// must not be probed here.
const HOSTILE_TEST_FIXTURES: &[&str] = &["PANICKING_SCHEMA"];

struct Row {
    name: String,
    arity: usize,
    lifted: Option<Vec<usize>>,
}

fn registry_rows() -> Vec<Row> {
    static BUILTINS: std::sync::Once = std::sync::Once::new();
    BUILTINS.call_once(crate::builtins::load_builtins);

    let mut rows: Vec<Row> = crate::function_registry::snapshot_registered()
        .into_iter()
        .filter(|(_, name, _)| !HOSTILE_TEST_FIXTURES.contains(&name.as_str()))
        .map(|(namespace, name, function)| {
            let arity = probe_arity(function.as_ref());
            let qualified = if namespace.is_empty() {
                name
            } else {
                format!("{namespace}::{name}")
            };
            Row {
                name: qualified,
                arity,
                lifted: function.elementwise_lifted_positions(arity),
            }
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

const EXPECTED_TABLE: &str = include_str!("elementwise_lifted_positions.txt");

fn parse_expected() -> BTreeMap<String, (usize, String)> {
    EXPECTED_TABLE
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next().expect("name field").to_string();
            let arity: usize = fields
                .next()
                .expect("arity field")
                .parse()
                .expect("numeric arity");
            let positions = fields.next().expect("positions field").to_string();
            assert!(
                fields.next().is_none(),
                "unexpected extra field in {line:?}"
            );
            (name, (arity, positions))
        })
        .collect()
}

fn write_snapshot(path: &str, rows: &[Row]) {
    let mut text = String::new();
    text.push_str("# CL-087 / ES-008 element-wise lifting anchor. GENERATED - see\n");
    text.push_str("# tests/elementwise_lifting_registry.rs for the regeneration command.\n");
    text.push_str("# NAME<TAB>PROBE_ARITY<TAB>elementwise_lifted_positions(PROBE_ARITY)\n");
    text.push_str(
        "# \"none\" means the function does not lift; \"empty\" means it lifts no slot.\n",
    );
    for row in rows {
        text.push_str(&format!(
            "{}\t{}\t{}\n",
            row.name,
            row.arity,
            format_positions(row.lifted.as_ref())
        ));
    }
    std::fs::write(path, text).expect("write lifting snapshot");

    // Companion audit dump: every function's caps and scalar-shaped slots, used
    // to build the CL-087 function audit receipt.
    let audit: Vec<serde_json::Value> = crate::function_registry::snapshot_registered()
        .into_iter()
        .filter(|(_, name, _)| !HOSTILE_TEST_FIXTURES.contains(&name.as_str()))
        .map(|(namespace, name, function)| {
            let arity = probe_arity(function.as_ref());
            serde_json::json!({
                "namespace": namespace,
                "name": name,
                "probe_arity": arity,
                "min_args": function.min_args(),
                "variadic": function.variadic(),
                "schema_len": function.arg_schema().len(),
                "caps": caps_names(function.as_ref()),
                "scalar_slots": scalar_slot_indices(function.as_ref(), arity),
                "lifted": function.elementwise_lifted_positions(arity),
            })
        })
        .collect();
    std::fs::write(
        format!("{path}.audit.json"),
        serde_json::to_string_pretty(&audit).expect("serialize audit dump"),
    )
    .expect("write audit dump");
}

#[test]
fn elementwise_lifted_positions_registry_anchor() {
    let rows = registry_rows();

    if let Ok(path) = std::env::var("FORMUALIZER_WRITE_LIFT_SNAPSHOT") {
        write_snapshot(&path, &rows);
    }

    let expected = parse_expected();
    let mut actual_names = Vec::new();

    for row in &rows {
        actual_names.push(row.name.clone());
        let actual = format_positions(row.lifted.as_ref());
        match expected.get(&row.name) {
            Some((arity, positions)) => {
                assert_eq!(
                    (*arity, positions.as_str()),
                    (row.arity, actual.as_str()),
                    "{} changed its element-wise lifted positions; regenerate \
                     elementwise_lifted_positions.txt and justify the change",
                    row.name
                );
            }
            None => {
                // Functions registered by other tests in this process are not in
                // the table. They are allowed only while they do not lift: a new
                // lifting function must be recorded in the expected table.
                assert!(
                    row.lifted.is_none(),
                    "{} lifts element-wise but is missing from \
                     elementwise_lifted_positions.txt",
                    row.name
                );
            }
        }
    }

    for name in expected.keys() {
        assert!(
            actual_names.iter().any(|candidate| candidate == name),
            "{name} is in elementwise_lifted_positions.txt but is no longer registered"
        );
    }
}

/// CL-087: registry-wide anchor for the SECOND lifting declaration.
///
/// `elementwise_lifted_positions.txt` records WHICH slots lift, but not the
/// Analysis-ToolPak asymmetry — "lifts over an array VALUE, refuses a live
/// multi-cell RANGE REFERENCE" — which is declared separately by
/// `Function::elementwise_lift_refuses_range_reference`. Exactly two functions
/// may carry it, and both of them must also lift; anything else is a
/// declaration that the table above would not show as a diff.
#[test]
fn only_the_atp_date_offsets_refuse_a_range_reference() {
    static BUILTINS: std::sync::Once = std::sync::Once::new();
    BUILTINS.call_once(crate::builtins::load_builtins);

    let mut refusing: Vec<String> = crate::function_registry::snapshot_registered()
        .into_iter()
        .filter(|(_, name, _)| !HOSTILE_TEST_FIXTURES.contains(&name.as_str()))
        .filter_map(|(namespace, name, function)| {
            if !function.elementwise_lift_refuses_range_reference() {
                return None;
            }
            // A refusal only means anything on a function that lifts at all.
            assert!(
                function
                    .elementwise_lifted_positions(probe_arity(function.as_ref()))
                    .is_some(),
                "{name} declares the range-reference refusal but does not lift"
            );
            Some(if namespace.is_empty() {
                name
            } else {
                format!("{namespace}::{name}")
            })
        })
        .collect();
    refusing.sort();

    assert_eq!(refusing, vec!["EDATE".to_string(), "EOMONTH".to_string()]);
}
