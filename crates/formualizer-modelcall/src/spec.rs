//! The pinned model package, deserialised from the JSON `package.py` emits.
//!
//! Field names follow the Python `ModelPackage` / `ModelSpec` dataclasses so
//! Lane C can serialise them with `dataclasses.asdict` plus two renames
//! (`fio_manifest` -> `manifest`, `solver_blocks` -> `goal_seek`; both old
//! spellings are accepted as aliases). Object key order is preserved
//! (Python dicts are insertion-ordered and ports are written in that order).

use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use std::fmt;
use std::marker::PhantomData;

use crate::import_boundary::GOAL_SEEK_BLOCK_PREFIX;

/// An insertion-ordered string map (a JSON object read in document order).
#[derive(Debug, Clone, PartialEq)]
pub struct OrderedMap<V>(pub Vec<(String, V)>);

impl<V> Default for OrderedMap<V> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<V> OrderedMap<V> {
    pub fn get(&self, key: &str) -> Option<&V> {
        self.0.iter().find(|(name, _)| name == key).map(|(_, value)| value)
    }
    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }
    pub fn iter(&self) -> impl Iterator<Item = (&str, &V)> {
        self.0.iter().map(|(name, value)| (name.as_str(), value))
    }
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(name, _)| name.as_str())
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<V: Serialize> Serialize for OrderedMap<V> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in &self.0 {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for OrderedMap<V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OrderedVisitor<V>(PhantomData<V>);
        impl<'de, V: Deserialize<'de>> Visitor<'de> for OrderedVisitor<V> {
            type Value = OrderedMap<V>;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut entries = Vec::with_capacity(access.size_hint().unwrap_or(0));
                while let Some((key, value)) = access.next_entry::<String, V>()? {
                    if entries.iter().any(|(existing, _): &(String, V)| *existing == key) {
                        return Err(serde::de::Error::custom(format!("duplicate key {key:?}")));
                    }
                    entries.push((key, value));
                }
                Ok(OrderedMap(entries))
            }
        }
        deserializer.deserialize_map(OrderedVisitor(PhantomData))
    }
}

/// A bounded rectangle, 1-based inclusive rows and columns (openpyxl style).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CellRange {
    pub sheet: String,
    pub start_row: u32,
    pub start_col: u32,
    pub end_row: u32,
    pub end_col: u32,
}

impl CellRange {
    pub fn is_single_cell(&self) -> bool {
        self.start_row == self.end_row && self.start_col == self.end_col
    }
    pub fn rows(&self) -> u32 {
        self.end_row + 1 - self.start_row
    }
    pub fn cols(&self) -> u32 {
        self.end_col + 1 - self.start_col
    }
}

/// One declared input or output (`ModelSpec.inputs` / `.outputs` values).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortLocation {
    #[serde(flatten)]
    pub range: CellRange,
    /// The workbook's defined name as spelled (import form).
    pub name: String,
    /// The public key (defined name less its prefix), original case.
    pub key: String,
    /// SheetPort port id: the casefolded key, `output_` + key for outputs.
    pub port_id: String,
    /// `scalar`, `range` or `record`.
    pub shape: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<Vec<Value>>,
    pub date_system: u16,
    /// `(row, col)` offsets within the range of cells with a date format.
    #[serde(default)]
    pub date_fields: Vec<(u32, u32)>,
    /// Anything else `package.py` adds later, kept verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// How a model admits an input name its ports do not declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UnknownInputPolicy {
    #[default]
    Reject,
    Ignore,
}

/// One goal-seek block (`ModelSpec.solver_blocks` entry).
///
/// `package.py` emits the block's name and rectangle only; the runtime reads
/// every setting from the block's label column after evaluation, exactly as
/// `engine_adapter._execute_xsolve` does (and `solve.run_solves` rediscovers
/// the blocks from the workbook's defined names, sorted by name). The optional
/// fields name the *setting cell* (the value column next to the label) when a
/// later `package.py` resolves labels at publication time; `None` means
/// "find it by label at solve time". `formula_cell` and `variable_cell` hold a
/// reference that is itself resolved at solve time (`Target cell`,
/// `By changing`). Settings map (NAMING_STANDARD.md): Solve algorithm ->
/// `method`, Target cell -> `formula_cell`, Target value -> `target_value`,
/// By changing -> `variable_cell`, Max change -> `max_change`, Max iterations ->
/// `max_iterations`, Initial guess -> `initial_value`, Lower/Upper bound ->
/// `lower_bound`/`upper_bound`, Run if -> `run_if`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoalSeekSpec {
    /// The defined name as spelled in the workbook (import form).
    pub name: String,
    #[serde(flatten)]
    pub block: CellRange,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<CellRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formula_cell: Option<CellRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_value: Option<CellRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variable_cell: Option<CellRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_change: Option<CellRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<CellRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_value: Option<CellRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lower_bound: Option<CellRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upper_bound: Option<CellRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_if: Option<CellRange>,
}

impl GoalSeekSpec {
    /// The block's public name: the defined name less the import prefix.
    pub fn suffix(&self) -> &str {
        self.name.strip_prefix(GOAL_SEEK_BLOCK_PREFIX).unwrap_or(&self.name)
    }
}

/// One pinned workbook: its identity, SheetPort manifest, ports, defaults and
/// goal-seek blocks (`package.ModelSpec`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelSpec {
    /// Version id of the model (`ModelSpec.identity`).
    pub identity: String,
    pub workbook_path: String,
    pub workbook_sha256: String,
    /// The SheetPort (FIO) manifest `package.py` compiles.
    #[serde(alias = "fio_manifest")]
    pub manifest: Value,
    /// Casefolded key -> declared input.
    pub inputs: OrderedMap<PortLocation>,
    /// Casefolded key -> declared output.
    pub outputs: OrderedMap<PortLocation>,
    /// Literal defaults by key (formula-backed cells excluded).
    #[serde(default)]
    pub defaults: OrderedMap<Value>,
    #[serde(default, alias = "solver_blocks")]
    pub goal_seek: Vec<GoalSeekSpec>,
    /// The publication descriptor (`unknown_input_policy`, `port_types`,
    /// `output_wire_formats`, `formula_input_defaults`, ...), verbatim.
    #[serde(default)]
    pub descriptor: Map<String, Value>,
}

impl ModelSpec {
    /// `identity:sha256`, the string the call stack and memo use
    /// (`CalculationSession.model_identity`).
    pub fn model_identity(&self) -> String {
        format!("{}:{}", self.identity, self.workbook_sha256)
    }

    /// Output by selector, casefolded (`ModelSpec.resolve_output`).
    pub fn resolve_output(&self, selector: &str) -> Option<&PortLocation> {
        self.outputs.get(&crate::key::casefold(selector))
    }

    /// `descriptor.unknown_input_policy`, default `reject`. An unknown
    /// spelling is refused at package compile time in Python; here it reads
    /// as `reject`, the safe side.
    pub fn unknown_input_policy(&self) -> UnknownInputPolicy {
        match self.descriptor.get("unknown_input_policy").and_then(Value::as_str) {
            Some("ignore") => UnknownInputPolicy::Ignore,
            _ => UnknownInputPolicy::Reject,
        }
    }

    /// `manifest.workbook.date_system` (1900 or 1904).
    pub fn date_system(&self) -> Option<u16> {
        self.manifest
            .pointer("/manifest/workbook/date_system")
            .and_then(Value::as_u64)
            .and_then(|value| u16::try_from(value).ok())
    }
}

/// The whole pinned package: parent, children and the child routes
/// (`package.ModelPackage`). This is the JSON `ModelSession` is built from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelPackage {
    pub package_id: String,
    pub parent: ModelSpec,
    /// Child version id -> spec.
    #[serde(default)]
    pub children: OrderedMap<ModelSpec>,
    /// Call target (`folder/service` as the workbook spells it) -> child
    /// version id. Package-wide: a child's own calls resolve here too.
    #[serde(default, alias = "routes")]
    pub child_routes: OrderedMap<String>,
    #[serde(default)]
    pub engine_identity: Value,
    #[serde(default)]
    pub report: Value,
    #[serde(default)]
    pub publication_identity: Value,
}

impl ModelPackage {
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// `CalculationSession.resolve_child`: route text -> pinned child spec.
    pub fn resolve_child(&self, target: &str) -> Result<&ModelSpec, crate::ModelCallError> {
        let key = self
            .child_routes
            .get(target)
            .ok_or_else(|| crate::ModelCallError::routing("child target is not in pinned package routes"))?;
        self.children
            .get(key)
            .ok_or_else(|| crate::ModelCallError::routing("pinned child package is unavailable"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec_json(identity: &str) -> Value {
        json!({
            "identity": identity,
            "workbook_path": "/tmp/model.xlsx",
            "workbook_sha256": "ab",
            "fio_manifest": {"spec": "fio", "manifest": {"workbook": {"date_system": 1900}}, "ports": []},
            "inputs": {
                "zeta": {"sheet": "S", "start_row": 1, "start_col": 1, "end_row": 1, "end_col": 1,
                          "name": "Xinput_Zeta", "key": "Zeta", "port_id": "zeta", "shape": "scalar",
                          "date_system": 1900, "date_fields": []},
                "alpha": {"sheet": "S", "start_row": 2, "start_col": 1, "end_row": 3, "end_col": 2,
                          "name": "Xinput_Alpha", "key": "Alpha", "port_id": "alpha", "shape": "range",
                          "headers": ["a", "b"], "date_system": 1900, "date_fields": [[1, 0]]}
            },
            "outputs": {},
            "defaults": {"zeta": 1},
            "solver_blocks": [{"name": "Xsolve_Rate", "sheet": "S", "start_row": 5, "start_col": 1,
                               "end_row": 12, "end_col": 2}],
            "descriptor": {"unknown_input_policy": "ignore"}
        })
    }

    #[test]
    fn package_json_keeps_key_order_and_accepts_python_names() {
        let package: ModelPackage = serde_json::from_value(json!({
            "package_id": "p",
            "parent": spec_json("parent"),
            "children": {"child": spec_json("child")},
            "routes": {"folder/service": "child"},
            "engine_identity": {}
        }))
        .unwrap();
        let keys: Vec<_> = package.parent.inputs.keys().collect();
        assert_eq!(keys, ["zeta", "alpha"]);
        assert_eq!(package.parent.goal_seek[0].suffix(), "Rate");
        assert_eq!(package.parent.unknown_input_policy(), UnknownInputPolicy::Ignore);
        assert_eq!(package.parent.date_system(), Some(1900));
        assert_eq!(package.parent.model_identity(), "parent:ab");
        assert_eq!(package.resolve_child("folder/service").unwrap().identity, "child");
        assert!(package.resolve_child("other").unwrap_err().is_routing());
        assert_eq!(package.parent.inputs.get("alpha").unwrap().date_fields, vec![(1, 0)]);
    }
}
