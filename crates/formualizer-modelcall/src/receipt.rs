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

use chrono::{NaiveDate, NaiveDateTime, NaiveTime, Timelike};

use crate::event::ModelCallEvent;
use crate::spec::OrderedMap;

// ---------------------------------------------------------------------------
// Receipt values (CP1 finding 3)
// ---------------------------------------------------------------------------
//
// Normative receipt form: `snapshot._plain(value.to_python())`. The derived
// serde of `LiteralValue` / `ExcelError` (externally tagged: `{"Int": 1}`,
// `{"Text": "a"}`, `"Empty"`) is NOT the receipt form; anything that leaves
// this crate as a receipt value goes through [`plain_value`].

/// `date.isoformat()`.
pub fn py_date_iso(day: &NaiveDate) -> String {
    day.format("%Y-%m-%d").to_string()
}

/// `time.isoformat()` of the microsecond time `literal_to_py` builds.
pub fn py_time_iso(clock: &NaiveTime) -> String {
    let micros = clock.nanosecond() / 1_000;
    if micros == 0 {
        clock.format("%H:%M:%S").to_string()
    } else {
        format!("{}.{micros:06}", clock.format("%H:%M:%S"))
    }
}

/// `datetime.isoformat()` (naive, microseconds only when nonzero).
pub fn py_datetime_iso(stamp: &NaiveDateTime) -> String {
    format!("{}T{}", py_date_iso(&stamp.date()), py_time_iso(&stamp.time()))
}

/// `_plain(literal.to_python())` as JSON: int, float, str, bool, None;
/// `{"type": "date"|"datetime"|"time", "value": iso}`; an error as the dict
/// `to_python` builds (`{"type": "Error", "kind", "message"?, ...}`); an
/// array as nested lists; Pending as `{"type": "Pending"}`. A non-finite
/// number has no JSON form and becomes null (Python's seal refuses it); a
/// duration becomes `{"type": "timedelta", "value": seconds}` (Python's seal
/// refuses a timedelta).
pub fn plain_value(value: &LiteralValue) -> Value {
    let tagged = |kind: &str, text: String| {
        let mut map = Map::new();
        map.insert("type".into(), Value::String(kind.into()));
        map.insert("value".into(), Value::String(text));
        Value::Object(map)
    };
    match value {
        LiteralValue::Int(number) => Value::from(*number),
        LiteralValue::Number(number) => serde_json::Number::from_f64(*number).map_or(Value::Null, Value::Number),
        LiteralValue::Boolean(flag) => Value::Bool(*flag),
        LiteralValue::Text(text) => Value::String(text.clone()),
        LiteralValue::Empty => Value::Null,
        LiteralValue::Date(day) => tagged("date", py_date_iso(day)),
        LiteralValue::Time(clock) => tagged("time", py_time_iso(clock)),
        LiteralValue::DateTime(stamp) => tagged("datetime", py_datetime_iso(stamp)),
        LiteralValue::Duration(duration) => {
            let mut map = Map::new();
            map.insert("type".into(), Value::String("timedelta".into()));
            #[expect(clippy::cast_precision_loss, reason = "timedelta.total_seconds()")]
            let seconds = duration.num_microseconds().map_or(f64::NAN, |micros| micros as f64 / 1e6);
            map.insert("value".into(), serde_json::Number::from_f64(seconds).map_or(Value::Null, Value::Number));
            Value::Object(map)
        }
        LiteralValue::Array(rows) => {
            Value::Array(rows.iter().map(|row| Value::Array(row.iter().map(plain_value).collect())).collect())
        }
        LiteralValue::Error(error) => plain_error(error),
        LiteralValue::Pending => {
            let mut map = Map::new();
            map.insert("type".into(), Value::String("Pending".into()));
            Value::Object(map)
        }
    }
}

/// The dict `literal_to_py` builds for an error value.
pub fn plain_error(error: &formualizer_common::ExcelError) -> Value {
    let mut map = Map::new();
    map.insert("type".into(), Value::String("Error".into()));
    map.insert("kind".into(), Value::String(error.kind.kind_name().into()));
    if let Some(message) = &error.message {
        map.insert("message".into(), Value::String(message.clone()));
    }
    if let Some(context) = &error.context {
        if let Some(row) = context.row {
            map.insert("row".into(), Value::from(row));
        }
        if let Some(col) = context.col {
            map.insert("col".into(), Value::from(col));
        }
        if let Some(sheet) = &context.origin_sheet {
            map.insert("sheet".into(), Value::String(sheet.clone()));
        }
        if let Some(row) = context.origin_row {
            map.insert("origin_row".into(), Value::from(row));
        }
        if let Some(col) = context.origin_col {
            map.insert("origin_col".into(), Value::from(col));
        }
    }
    if error.extra != formualizer_common::error::ExcelErrorExtra::None
        && let Ok(extra) = serde_json::to_value(&error.extra)
    {
        map.insert("extra".into(), extra);
    }
    Value::Object(map)
}

/// A port value in receipt form.
pub fn plain_port_value(value: &PortValue) -> Value {
    let row = |cells: &[LiteralValue]| Value::Array(cells.iter().map(plain_value).collect());
    let record = |fields: &OrderedMap<LiteralValue>| {
        Value::Object(fields.iter().map(|(key, value)| (key.to_owned(), plain_value(value))).collect())
    };
    match value {
        PortValue::Scalar(value) => plain_value(value),
        PortValue::Range(rows) => Value::Array(rows.iter().map(|cells| row(cells)).collect()),
        PortValue::Record(fields) => record(fields),
        PortValue::Row(cells) => row(cells),
        PortValue::Table(rows) => Value::Array(rows.iter().map(record).collect()),
    }
}

/// `json.dumps(value, sort_keys=True, separators=(',', ':'))`-style text
/// (keys sorted at every level, whatever `serde_json`'s map order is).
pub fn canonical_json(value: &Value) -> String {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                let mut out = Map::new();
                for key in keys {
                    out.insert(key.clone(), sorted(&map[key]));
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    // serde_json writes a Map in its own order; with `preserve_order` that is
    // insertion order, which `sorted` made ascending.
    serde_json::to_string(&sorted(value)).unwrap_or_default()
}

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

    /// Lane I (CP1 finding 4): present only when the report or inspection
    /// hook returned them (`RunSnapshot.report_cells`, `conditional_results`,
    /// `formula_counts`, `inspection`).
    pub const REPORT_CELLS: &str = "report_cells";
    pub const CONDITIONAL_RESULTS: &str = "conditional_results";
    pub const FORMULA_COUNTS: &str = "formula_counts";
    pub const INSPECTION: &str = "inspection";
    pub const HOOK_KEYS: [&str; 4] = [REPORT_CELLS, CONDITIONAL_RESULTS, FORMULA_COUNTS, INSPECTION];
}

/// Keys of the `compiled` receipt map (`result_keys::COMPILED`).
///
/// `calls` and `routes` are the compiled-child hook's report
/// (`CompiledRoute.report()`). Architecture B adds the compiled parent's
/// record when a compiled parent is installed for the request's parent
/// workbook: `parent` is its route (`compiled`, `engine:<reason>` for a
/// static refusal, `fallback:<reason>` for a discarded attempt) and, on a
/// compiled parent run, `parent_xcalls` (the module's `MDL.CALLMODEL` count,
/// `cv_run_stats.xcalls`) and `parent_loaded` (`false`: no engine parent was
/// loaded, so `session_reuse` holds no parent entry). The child routes of a
/// discarded parent attempt are dropped from `routes`/`calls`; the engine
/// parent's own child calls are recorded afresh.
pub mod compiled_keys {
    pub const CALLS: &str = "calls";
    pub const ROUTES: &str = "routes";
    pub const PARENT: &str = "parent";
    pub const PARENT_XCALLS: &str = "parent_xcalls";
    pub const PARENT_LOADED: &str = "parent_loaded";
    /// Route of a compiled parent run.
    pub const ROUTE_COMPILED: &str = "compiled";
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
/// record (field -> value), per the port's declared shape. A client output
/// (`PortSession._project_output`) can also be one row (a single-row range)
/// or a table (header -> value per data row). (Lane A: `Row` and `Table`
/// added, additive contract change.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PortValue {
    Scalar(LiteralValue),
    Range(Vec<Vec<LiteralValue>>),
    Record(OrderedMap<LiteralValue>),
    Row(Vec<LiteralValue>),
    Table(Vec<OrderedMap<LiteralValue>>),
}

/// The result of one `calculate`. On failure the session still exposes the
/// same fields (evidence for `CalculationFailure`), with empty outputs.
///
/// Values here are engine literals; the receipt form of every one is
/// [`plain_value`] / [`plain_port_value`] (the binding converts with the same
/// rules). `session_reuse` is filled when the session runs on a
/// `RetainedModel`; `invocations` is then the sealed list (executed, held,
/// inherited).
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
    /// Empty without a retained model (`CalculationSession.session_reuse`).
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

    #[test]
    fn plain_values_follow_python_plain_not_derived_serde() {
        use formualizer_common::{ExcelError, ExcelErrorKind};
        assert_eq!(plain_value(&LiteralValue::Int(1)), serde_json::json!(1));
        assert_eq!(plain_value(&LiteralValue::Text("a/b".into())), serde_json::json!("a/b"));
        assert_eq!(plain_value(&LiteralValue::Empty), Value::Null);
        let day = NaiveDate::from_ymd_opt(2026, 9, 28).unwrap();
        assert_eq!(plain_value(&LiteralValue::Date(day)), serde_json::json!({"type": "date", "value": "2026-09-28"}));
        let stamp = day.and_hms_micro_opt(1, 2, 3, 4).unwrap();
        assert_eq!(
            plain_value(&LiteralValue::DateTime(stamp)),
            serde_json::json!({"type": "datetime", "value": "2026-09-28T01:02:03.000004"})
        );
        let error = ExcelError::new(ExcelErrorKind::Ref).with_message("child target must be text".to_owned());
        assert_eq!(
            plain_value(&LiteralValue::Error(error)),
            serde_json::json!({"type": "Error", "kind": "Ref", "message": "child target must be text"})
        );
        assert_eq!(canonical_json(&serde_json::json!({"b": 1, "a": [2]})), r#"{"a":[2],"b":1}"#);
    }
}
