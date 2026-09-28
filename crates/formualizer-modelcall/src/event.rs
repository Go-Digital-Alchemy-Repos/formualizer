//! One recorded child-model call (the event dict of `callbacks.ChildRouter`,
//! `prefetch.Prefetcher` and `runtime.sealed_invocations`).
//!
//! Keys and their meaning are fixed by the Python receipt; see
//! `docs/modelcall_contract.md` ("Invocation events"). An absent optional
//! field is an absent dict key, never `None`.

use formualizer_common::{ExcelError, LiteralValue};
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

use crate::evaluator::ChildMatrix;
use crate::key::InputPairs;

/// `event['status']`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallStatus {
    /// Recorded before routing; a call that never finishes keeps it.
    Started,
    /// Answered from the request memo (`memo_of` names the source event).
    Memoized,
    /// The child was evaluated (engine or compiled route).
    Completed,
    /// A routing refusal; the cell got `#REF!`.
    RoutingError,
    /// A fault; the cell got `#CALC!`, the parent was cancelled, the run fails.
    InfrastructureError,
    /// Sealing only: a warm event this run relied on (`inherited_from: warm`).
    Inherited,
    /// Sealing only: a call a re-evaluated pool entry held (`held_from: reuse`).
    Held,
}

impl CallStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Memoized => "memoized",
            Self::Completed => "completed",
            Self::RoutingError => "routing_error",
            Self::InfrastructureError => "infrastructure_error",
            Self::Inherited => "inherited",
            Self::Held => "held",
        }
    }
}

/// One call, in the order the Python dict gains its keys.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelCallEvent {
    /// Position in the request's invocation list (renumbered when sealed or merged).
    pub index: usize,
    /// Caller identity (`stack[-1]`).
    pub parent: String,
    /// The call's target argument as passed (text when routable).
    pub target: LiteralValue,
    /// The call's output selector argument as passed.
    pub output: LiteralValue,
    /// Caller stack of `identity:sha256` strings, parent first.
    pub stack: Vec<String>,
    pub status: CallStatus,
    /// Child `identity:sha256`, once resolved.
    pub child: Option<String>,
    /// Casefolded name/value pairs, once parsed (a dict in the receipt).
    pub inputs: Option<InputPairs>,
    /// Memoized: index of the event whose result was reused.
    pub memo_of: Option<usize>,
    /// Completed or memoized: the typed result matrix.
    pub matrix: Option<ChildMatrix>,
    /// Routing text, or `Type: message` for a fault.
    pub error: Option<String>,
    /// The error value handed back to the cell.
    pub returned_error: Option<ExcelError>,
    /// Set on a sibling-prefetch event (`True`).
    pub prefetch: bool,
    /// Compiled-route record (`CompiledRoute._record`), when the flag is on.
    pub route: Option<Value>,
    /// `warm` on an inherited copy.
    pub inherited_from: Option<String>,
    /// `reuse` on a held copy.
    pub held_from: Option<String>,
}

impl ModelCallEvent {
    /// The `started` event the router records before anything else.
    pub fn started(index: usize, stack: &[String], target: LiteralValue, output: LiteralValue) -> Self {
        Self {
            index,
            parent: stack.last().cloned().unwrap_or_default(),
            target,
            output,
            stack: stack.to_vec(),
            status: CallStatus::Started,
            child: None,
            inputs: None,
            memo_of: None,
            matrix: None,
            error: None,
            returned_error: None,
            prefetch: false,
            route: None,
            inherited_from: None,
            held_from: None,
        }
    }
}

impl Serialize for ModelCallEvent {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("index", &self.index)?;
        map.serialize_entry("parent", &self.parent)?;
        map.serialize_entry("target", &self.target)?;
        map.serialize_entry("output", &self.output)?;
        map.serialize_entry("stack", &self.stack)?;
        map.serialize_entry("status", &self.status)?;
        if let Some(child) = &self.child {
            map.serialize_entry("child", child)?;
        }
        if let Some(inputs) = &self.inputs {
            map.serialize_entry("inputs", &crate::spec::OrderedMap(inputs.clone()))?;
        }
        if let Some(source) = self.memo_of {
            map.serialize_entry("memo_of", &source)?;
        }
        if let Some(matrix) = &self.matrix {
            map.serialize_entry("matrix", matrix)?;
        }
        if let Some(error) = &self.error {
            map.serialize_entry("error", error)?;
        }
        if let Some(returned) = &self.returned_error {
            map.serialize_entry("returned_error", returned)?;
        }
        if self.prefetch {
            map.serialize_entry("prefetch", &true)?;
        }
        if let Some(route) = &self.route {
            map.serialize_entry("route", route)?;
        }
        if let Some(from) = &self.inherited_from {
            map.serialize_entry("inherited_from", from)?;
        }
        if let Some(from) = &self.held_from {
            map.serialize_entry("held_from", from)?;
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn started_event_serialises_only_the_keys_it_has() {
        let stack = vec!["parent:psha".to_owned()];
        let event = ModelCallEvent::started(0, &stack, LiteralValue::Text("f/s".into()), LiteralValue::Text("out".into()));
        let value = serde_json::to_value(&event).unwrap();
        let keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        let mut expected = vec!["index", "output", "parent", "stack", "status", "target"];
        expected.sort();
        assert_eq!(keys, expected);
        assert_eq!(value["status"], "started");
        assert_eq!(value["parent"], "parent:psha");
        assert_eq!(CallStatus::RoutingError.as_str(), "routing_error");
    }
}
