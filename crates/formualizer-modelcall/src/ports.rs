//! Lane A: SheetPort admission (`workbook_runtime/ports.py`): the alias map,
//! the unknown-input policy, wire decoding (including the CL-105 date-text
//! rules), CL-097 skip-unchanged writes (`WriteRecord`) and typed reads.
//! Lane 0 placeholder: fallible entry points return
//! `ModelCallError::NotImplemented`; `child_port_updates` returns `None`
//! (the memo then keys by the full input vector, the GOD-379 form, which can
//! only lose hits, never answers).

use formualizer_common::LiteralValue;

use crate::key::InputPairs;
use crate::spec::{ModelSpec, UnknownInputPolicy};
use crate::ModelCallError;

/// Casefolded accepted spelling -> canonical port name (`spec_port_alias_map`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PortAliasMap(pub Vec<(String, String)>);

/// `spec_port_alias_map(spec)`.
pub fn spec_port_alias_map(_spec: &ModelSpec) -> Result<PortAliasMap, ModelCallError> {
    Err(ModelCallError::NotImplemented("ports::spec_port_alias_map"))
}

/// `canonical_port_inputs(inputs, aliases, policy)`: canonical name -> value;
/// a duplicate canonical name is an error; under `Ignore` unmatched names are
/// dropped, under `Reject` they are an error.
pub fn canonical_port_inputs(
    _inputs: &[(String, LiteralValue)],
    _aliases: &PortAliasMap,
    _policy: UnknownInputPolicy,
) -> Result<InputPairs, ModelCallError> {
    Err(ModelCallError::NotImplemented("ports::canonical_port_inputs"))
}

/// `callbacks.child_port_updates`: the port updates an `ignore`-policy child
/// admits, or `None` (keep the full vector) for any other child or when the
/// mapping fails.
pub fn child_port_updates(spec: &ModelSpec, inputs: &[(String, LiteralValue)]) -> Option<InputPairs> {
    if spec.unknown_input_policy() != UnknownInputPolicy::Ignore {
        return None;
    }
    let aliases = spec_port_alias_map(spec).ok()?;
    canonical_port_inputs(inputs, &aliases, UnknownInputPolicy::Ignore).ok()
}
