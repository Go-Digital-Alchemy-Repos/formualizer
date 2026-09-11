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

/// GOD-289 / OT-197 (4): the invariant the CL-085 + CL-087 merge rests on.
///
/// CL-085 gave `ArgumentHandle::local_named_reference` a `value_override`
/// guard: a handle carrying a lift's scalar substitute never reports a
/// reference, so a LET/LAMBDA local cannot be resolved through one. GOD-280
/// recorded that guard as UNREACHABLE, on the grounds that no element-wise
/// lifted position is also a slot the callee consumes as a reference -- and
/// then CL-087 replaced a 16-name allowlist with a registry-wide declaration
/// covering 48 functions, nine of them with hand-written position overrides.
/// Nobody had re-asserted the invariant since. This asserts it.
///
/// WHY THE `ArgSchema::by_ref` FLAG ALONE IS NOT THE TEST. The first draft of
/// this test asserted `!spec.by_ref` over each lifted position, and the
/// GOD-289 pre-build review measured that assertion near-vacuous:
/// `by_ref: true` occurs THREE times in the whole non-test tree
/// (`builtins/reference_fns.rs:53`, `builtins/lookup/reference_info.rs:93`
/// and `:383`), while the functions that actually take a slot as a reference
/// do it by calling `ArgumentHandle::as_reference_or_eval()` on it with
/// `by_ref: false` in their schema. `args.rs` is the only consumer of the flag.
/// So the flag is checked AND the call sites are enumerated below.
///
/// REFERENCE_CONSUMERS is that enumeration: every `as_reference_or_eval` call
/// site in `builtins/`, resolved to its owning function and argument position,
/// each cited at the line it was read from. It is a hand-maintained table and
/// it will go stale if a new reference consumer is added without updating it;
/// `reference_consumers_are_all_registered` below is the tripwire for a table
/// entry that stops naming a real function, and the round that adds a
/// reference consumer is the round that adds its row.
///
/// If either assertion fails, the merge has a live interaction: the named
/// function lifts a slot it also consumes as a reference, so inside the lift a
/// LET range local in that slot falls back to the workbook-name route and
/// returns a spurious `#NAME?`. The fix is not to widen the table but to
/// decide which of the two the slot is.
///
/// The clean-room arm of the same question is the round's P3 probe
/// (`p3_interaction_probe.py`), which runs the shape on a real wheel. This
/// test bounds it statically; the probe measures it.
struct ReferenceConsumer {
    /// Registered function name.
    name: &'static str,
    /// Positions consumed as a reference, read from the call site.
    fixed: &'static [usize],
    /// `Some(n)` when every position from `n` onward is consumed as a
    /// reference (a variadic reference tail).
    variadic_from: Option<usize>,
    /// `file:line` of the `as_reference_or_eval` call, at `b9be70bf`.
    site: &'static str,
}

const REFERENCE_CONSUMERS: &[ReferenceConsumer] = &[
    ReferenceConsumer { name: "OFFSET", fixed: &[0], variadic_from: None,
        site: "builtins/reference_fns.rs:705 (schema by_ref: true at :53)" },
    ReferenceConsumer { name: "ISREF", fixed: &[0], variadic_from: None,
        site: "builtins/info.rs:706" },
    ReferenceConsumer { name: "FORMULATEXT", fixed: &[0], variadic_from: None,
        site: "builtins/info.rs:788" },
    ReferenceConsumer { name: "SHEET", fixed: &[0], variadic_from: None,
        site: "builtins/info.rs:886" },
    ReferenceConsumer { name: "SHEETS", fixed: &[0], variadic_from: None,
        site: "builtins/info.rs:989" },
    ReferenceConsumer { name: "MATCH", fixed: &[1], variadic_from: None,
        site: "builtins/lookup/core.rs:342 (lookup_array)" },
    ReferenceConsumer { name: "VLOOKUP", fixed: &[1], variadic_from: None,
        site: "builtins/lookup/core.rs:625 (table_array)" },
    ReferenceConsumer { name: "HLOOKUP", fixed: &[1], variadic_from: None,
        site: "builtins/lookup/core.rs:893 (table_array)" },
    ReferenceConsumer { name: "LOOKUP", fixed: &[1, 2], variadic_from: None,
        site: "builtins/lookup/legacy.rs:401 (lookup_vector, result_vector)" },
    ReferenceConsumer { name: "ROW", fixed: &[0], variadic_from: None,
        site: "builtins/lookup/reference_info.rs:123 (by_ref: true at :93)" },
    ReferenceConsumer { name: "ROWS", fixed: &[0], variadic_from: None,
        site: "builtins/lookup/reference_info.rs:255" },
    ReferenceConsumer { name: "COLUMN", fixed: &[0], variadic_from: None,
        site: "builtins/lookup/reference_info.rs:413 (by_ref: true at :383)" },
    ReferenceConsumer { name: "COLUMNS", fixed: &[0], variadic_from: None,
        site: "builtins/lookup/reference_info.rs:545" },
    ReferenceConsumer { name: "CHOOSE", fixed: &[], variadic_from: Some(1),
        site: "builtins/lookup/choose.rs:220 (every value arm)" },
    ReferenceConsumer { name: "HSTACK", fixed: &[], variadic_from: Some(0),
        site: "builtins/lookup/stack.rs:30 (every argument)" },
    ReferenceConsumer { name: "VSTACK", fixed: &[], variadic_from: Some(0),
        site: "builtins/lookup/stack.rs:30 (every argument)" },
];

fn consumes_position_as_reference(entry: &ReferenceConsumer, position: usize)
    -> bool
{
    entry.fixed.contains(&position)
        || entry.variadic_from.is_some_and(|from| position >= from)
}

#[test]
fn no_lifted_position_is_also_consumed_as_a_reference() {
    let rows = registry_rows();
    static BUILTINS: std::sync::Once = std::sync::Once::new();
    BUILTINS.call_once(crate::builtins::load_builtins);

    let mut checked = 0usize;
    let mut lifting = 0usize;
    let mut overlap_candidates = 0usize;
    for (namespace, name, function) in crate::function_registry::snapshot_registered() {
        if HOSTILE_TEST_FIXTURES.contains(&name.as_str()) {
            continue;
        }
        let qualified = if namespace.is_empty() {
            name.clone()
        } else {
            format!("{namespace}::{name}")
        };
        let arity = probe_arity(function.as_ref());
        checked += 1;
        let Some(lifted) = function.elementwise_lifted_positions(arity) else {
            continue;
        };
        if lifted.is_empty() {
            continue;
        }
        lifting += 1;
        let schema = function.arg_schema();
        let consumer = REFERENCE_CONSUMERS.iter().find(|e| e.name == name);
        if consumer.is_some() {
            overlap_candidates += 1;
        }
        for position in &lifted {
            // (a) the schema flag. A fixed-arity function whose schema is
            // shorter than a lifted position is a defect in the lifted set,
            // not something to paper over with the last slot, so only a
            // VARIADIC function may fall back to its repeating tail.
            let spec = match schema.get(*position) {
                Some(spec) => Some(spec),
                None if function.variadic() => schema.last(),
                None => panic!(
                    "{qualified} lifts position {position} but its schema has                      only {} slots and it is not variadic",
                    schema.len()
                ),
            };
            if let Some(spec) = spec {
                assert!(
                    !spec.by_ref,
                    "{qualified} lifts position {position} element-wise but                      its schema takes that slot by reference; CL-085's                      value_override guard would send a LET local in it back                      to the workbook-name route (OT-197 (4))"
                );
            }
            // (b) the call sites, which is where the reference consumption
            // actually lives.
            if let Some(entry) = consumer {
                assert!(
                    !consumes_position_as_reference(entry, *position),
                    "{qualified} lifts position {position} element-wise but                      consumes it as a reference at {}; CL-085's                      value_override guard would send a LET local in it back                      to the workbook-name route (OT-197 (4))",
                    entry.site
                );
            }
        }
    }
    assert!(checked > 300, "expected the whole registry, saw {checked}");
    assert!(lifting > 20, "expected the lifting set, saw {lifting}");
    // The assertion is only worth anything if some lifting function is also a
    // reference consumer; today MATCH, VLOOKUP, HLOOKUP and LOOKUP are.
    assert!(
        overlap_candidates >= 4,
        "expected at least four functions that both lift and consume a          reference, saw {overlap_candidates}; if the lifted set shrank this          test stopped testing anything"
    );
    // Keep the row count in step with the anchor above.
    assert_eq!(checked, rows.len());
}

/// The tripwire for `REFERENCE_CONSUMERS` going stale in the easy direction:
/// an entry that no longer names a registered function.
#[test]
fn reference_consumers_are_all_registered() {
    static BUILTINS: std::sync::Once = std::sync::Once::new();
    BUILTINS.call_once(crate::builtins::load_builtins);
    for entry in REFERENCE_CONSUMERS {
        assert!(
            crate::function_registry::get("", entry.name).is_some(),
            "REFERENCE_CONSUMERS names {} ({}), which is not registered; the              table has gone stale",
            entry.name,
            entry.site
        );
        assert!(
            !entry.fixed.is_empty() || entry.variadic_from.is_some(),
            "{} declares no reference position at all",
            entry.name
        );
    }
}
