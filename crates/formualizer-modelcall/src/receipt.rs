//! What `ModelSession::calculate` returns, and the receipt key names.
//!
//! The Python shim seals these into `RunSnapshot` exactly as `runtime.py`
//! does today; `pdf_export/job.py` then reads `timings`, `session_reuse`,
//! `xcall_memo`, `child_invocations`, `diagnostics` and `compiled` from the
//! sealed dict. Key names here are byte-for-byte those names; see
//! `docs/modelcall_contract.md`.

use formualizer_common::LiteralValue;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::event::ModelCallEvent;
use crate::spec::OrderedMap;

/// Keys of the dict `ModelSession.calculate` returns to Python.
pub mod result_keys {
    pub const OUTPUTS: &str = "outputs";
    pub const TYPED_OUTPUTS: &str = "typed_outputs";
    pub const EFFECTIVE_INPUTS: &str = "effective_inputs";
    /// Sealed as `RunSnapshot.child_invocations`.
    pub const INVOCATIONS: &str = "invocations";
    pub const TIMINGS: &str = "timings";
    pub const FAULTS: &str = "faults";
    /// Sealed as `RunSnapshot.solver_results`.
    pub const SOLVERS: &str = "solvers";
    pub const DIAGNOSTICS: &str = "diagnostics";
    pub const SESSION_REUSE: &str = "session_reuse";
    /// Memo counters. Receipt key kept as `xcall_memo` this round (CONTRACT
    /// amendment 5); code names it the model-call memo.
    pub const CALL_MEMO: &str = "xcall_memo";
    pub const COMPILED: &str = "compiled";

    pub const ALL: [&str; 11] = [
        OUTPUTS, TYPED_OUTPUTS, EFFECTIVE_INPUTS, INVOCATIONS, TIMINGS, FAULTS, SOLVERS,
        DIAGNOSTICS, SESSION_REUSE, CALL_MEMO, COMPILED,
    ];
}

/// `timings` keys, in the order `CalculationSession` creates them.
pub mod timing_keys {
    /// Always present, initialised to 0.0.
    pub const BASE: [&str; 8] = [
        "load_seconds",
        "evaluation_seconds",
        "solver_seconds",
        "child_seconds",
        "preparation_seconds",
        "admission_seconds",
        "capture_seconds",
        "inspection_seconds",
    ];
    /// Present only when the compiled flag is on.
    pub const COMPILED_SECONDS: &str = "compiled_seconds";
    /// CL-097 counters (ints), only when skip-unchanged-writes keeps a record.
    pub const WRITE_STATS: [&str; 4] = [
        "writes_skipped",
        "formula_restores_skipped",
        "defaults_not_restored_overwritten",
        "date_writes_skipped",
    ];
    /// Prefetch: count (int), seconds, wait seconds, hits (int); only after
    /// the first dispatch.
    pub const PREFETCH_COUNT: &str = "prefetch_count";
    pub const PREFETCH_SECONDS: &str = "prefetch_seconds";
    pub const PREFETCH_WAIT_SECONDS: &str = "prefetch_wait_seconds";
    pub const PREFETCH_HITS: &str = "prefetch_hits";
    /// Set last, on success only.
    pub const TOTAL_SECONDS: &str = "total_seconds";
}

/// A timing is seconds (float) or a count (int), as in the Python dict.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TimingValue {
    Count(u64),
    Seconds(f64),
}

/// Insertion-ordered `timings` dict.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Timings(pub OrderedMap<TimingValue>);

impl Timings {
    /// The eight base keys at 0.0 (`CalculationSession.__init__`).
    pub fn base() -> Self {
        let mut timings = Self::default();
        for key in timing_keys::BASE {
            timings.0.0.push((key.to_owned(), TimingValue::Seconds(0.0)));
        }
        timings
    }

    fn slot(&mut self, key: &str, initial: TimingValue) -> &mut TimingValue {
        let entries = &mut self.0.0;
        let position = match entries.iter().position(|(name, _)| name == key) {
            Some(position) => position,
            None => {
                entries.push((key.to_owned(), initial));
                entries.len() - 1
            }
        };
        &mut entries[position].1
    }

    /// `timings[key] += seconds` (created at 0.0).
    pub fn add_seconds(&mut self, key: &str, seconds: f64) {
        let slot = self.slot(key, TimingValue::Seconds(0.0));
        *slot = match *slot {
            TimingValue::Seconds(value) => TimingValue::Seconds(value + seconds),
            TimingValue::Count(value) => TimingValue::Seconds(value as f64 + seconds),
        };
    }

    /// `timings[key] = timings.get(key, 0) + count`.
    pub fn add_count(&mut self, key: &str, count: u64) {
        let slot = self.slot(key, TimingValue::Count(0));
        *slot = match *slot {
            TimingValue::Count(value) => TimingValue::Count(value + count),
            TimingValue::Seconds(value) => TimingValue::Seconds(value + count as f64),
        };
    }

    /// `timings[key] = value`.
    pub fn set(&mut self, key: &str, value: TimingValue) {
        *self.slot(key, value) = value;
    }

    /// `timings.setdefault(key, value)`.
    pub fn set_default(&mut self, key: &str, value: TimingValue) {
        self.slot(key, value);
    }

    pub fn get(&self, key: &str) -> Option<TimingValue> {
        self.0.get(key).copied()
    }
}

/// `XcallRequestMemo.report()` plus `Prefetcher.report()` under `prefetch`.
/// Sealed under the receipt key `xcall_memo`; absent (empty dict) when the
/// memo is off or looked nothing up.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MemoReport {
    /// Always `true` when present.
    pub enabled: bool,
    pub hits: u64,
    pub misses: u64,
    pub stores: u64,
    pub bypassed: u64,
    pub not_stored_error: u64,
    pub port_keyed: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefetch: Option<PrefetchReport>,
}

/// `Prefetcher.report()`; empty (absent) until something was dispatched.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PrefetchReport {
    pub dispatched: u64,
    pub stored: u64,
    pub not_stored: u64,
    pub hits: u64,
    /// Present only when pre-warmed slots were configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_slot: Option<u64>,
}

/// One port value as the runtime reads it: a scalar, a range (rows) or a
/// record (field -> value), per the port's declared shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PortValue {
    Scalar(LiteralValue),
    Range(Vec<Vec<LiteralValue>>),
    Record(OrderedMap<LiteralValue>),
}

/// The result of one `calculate`. On failure the session still exposes the
/// same fields (evidence for `CalculationFailure`), with empty outputs.
#[derive(Debug, Clone, Default)]
pub struct CalculationResult {
    /// Client-facing outputs (`ports.read_outputs`, trailing null rows
    /// trimmed per `trim_trailing_null_rows`).
    pub outputs: OrderedMap<PortValue>,
    /// `ports.read_typed_outputs`.
    pub typed_outputs: OrderedMap<PortValue>,
    /// `ports.read_inputs` after evaluation.
    pub effective_inputs: OrderedMap<PortValue>,
    /// Sealed invocations (executed, then held, then inherited; renumbered).
    pub invocations: Vec<ModelCallEvent>,
    /// Faulted events (a failed run's cause is `faults[0].error`).
    pub faults: Vec<ModelCallEvent>,
    pub timings: Timings,
    /// Goal-seek records, each with `workbook` (caller identity) added.
    pub solvers: Vec<Map<String, Value>>,
    /// `ignored_input:`, `defaulted_input:`, goal-seek notes, report notes.
    pub diagnostics: Vec<String>,
    /// Empty without a session pool (`CalculationSession.session_reuse`).
    pub session_reuse: Map<String, Value>,
    /// `None` seals as the empty dict (key dropped from the receipt).
    pub call_memo: Option<MemoReport>,
    /// Empty unless the compiled flag is on.
    pub compiled: Map<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timings_keep_python_order_and_types() {
        let mut timings = Timings::base();
        timings.add_seconds("load_seconds", 0.5);
        timings.add_count("writes_skipped", 2);
        timings.add_count("writes_skipped", 1);
        timings.set_default(timing_keys::PREFETCH_SECONDS, TimingValue::Seconds(0.0));
        let value = serde_json::to_string(&timings).unwrap();
        assert!(value.starts_with(r#"{"load_seconds":0.5,"evaluation_seconds":0.0"#), "{value}");
        assert!(value.ends_with(r#""writes_skipped":3,"prefetch_seconds":0.0}"#), "{value}");
    }

    #[test]
    fn memo_report_serialises_python_keys() {
        let report = MemoReport { enabled: true, hits: 1, ..MemoReport::default() };
        let value = serde_json::to_value(&report).unwrap();
        let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["bypassed", "enabled", "hits", "misses", "not_stored_error", "port_keyed", "stores"]);
        assert_eq!(result_keys::CALL_MEMO, "xcall_memo");
    }
}
