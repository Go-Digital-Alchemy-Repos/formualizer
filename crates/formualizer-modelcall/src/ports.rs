//! Lane A: SheetPort admission (`workbook_runtime/ports.py`): the alias map,
//! the unknown-input policy, wire decoding (including the CL-105 date-text
//! rules), CL-097 write statistics (`WriteRecord`), typed reads and the
//! client output projection (`_project_output`, `output_dates.project_date`).
//!
//! Values. `ports.py` works on Python objects; [`WireValue`] is that object
//! model (`None`, `bool`, `int`, `float`, `str`, `date`, `datetime`, `time`,
//! `list`, `dict`, and an engine value passed through unconverted, which is
//! what an engine error or Pending is to the Python code). A client request
//! is decoded from JSON (`decode_wire = true`); a child call's inputs arrive as
//! engine values and are converted exactly as the Python callback conversion
//! (`literal_to_py`) does. The native write converts back as `py_to_literal`
//! and `py_to_port_value` do.
//!
//! Defaults. `ModelSpec.defaults` is JSON. A default that is a native date,
//! datetime or time (an openpyxl cell value in Python) is carried as a
//! one-key object `{"$date": "YYYY-MM-DD"}`, `{"$datetime": "..."}` or
//! `{"$time": "..."}` (ISO text); every other JSON value is read as Python's
//! `json` module reads it.
//!
//! CL-097 (Lane I). A workbook re-entered from a `RetainedModel` keeps its
//! `PortSession` and with it its [`WriteRecord`]; `write_scenario` then skips
//! port writes whose cells still hold exactly what the record last wrote and
//! formula-default restores that are still in place, as `ports.py`
//! (`_restore_formula_defaults`, `_unchanged`, `_write_changed`,
//! `_read_back_dates`) does. A fresh load has written nothing, so nothing is
//! skipped on it.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use formualizer_common::LiteralValue;
use formualizer_sheetport::{
    BoundPort, InputUpdate, ManifestBindings, PortValue as SheetValue, SheetPort, SheetPortError, TableValue,
};
use formualizer_workbook::Workbook;
use serde_json::{Map, Value};
use sheetport_spec::Manifest;
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::evaluator::ChildMatrix;
use crate::key::{InputPairs, casefold};
use crate::receipt::PortValue;
use crate::spec::{CellRange, ModelSpec, OrderedMap, PortLocation, UnknownInputPolicy};
use crate::ModelCallError;

// ---------------------------------------------------------------------------
// Python object model
// ---------------------------------------------------------------------------

/// A Python value as `ports.py` sees it.
#[derive(Debug, Clone, PartialEq)]
pub enum WireValue {
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Date(NaiveDate),
    DateTime(NaiveDateTime),
    Time(NaiveTime),
    List(Vec<WireValue>),
    Dict(OrderedMap<WireValue>),
    /// An engine value with no plain Python form (error, Pending, duration).
    Literal(LiteralValue),
}

impl WireValue {
    /// A JSON value as Python's `json` module reads it, plus the `$date`,
    /// `$datetime` and `$time` tags for native temporal defaults.
    pub fn from_json(value: &Value) -> Self {
        match value {
            Value::Null => Self::None,
            Value::Bool(flag) => Self::Bool(*flag),
            Value::Number(number) => {
                if let Some(integer) = number.as_i64() {
                    Self::Int(integer)
                } else if number.is_u64() {
                    // Beyond i64: Python keeps an int the engine cannot take.
                    Self::Float(number.as_f64().unwrap_or(f64::NAN))
                } else {
                    Self::Float(number.as_f64().unwrap_or(f64::NAN))
                }
            }
            Value::String(text) => Self::Str(text.clone()),
            Value::Array(items) => Self::List(items.iter().map(Self::from_json).collect()),
            Value::Object(map) => {
                if map.len() == 1
                    && let Some(tagged) = Self::tagged_temporal(map)
                {
                    return tagged;
                }
                Self::Dict(OrderedMap(map.iter().map(|(key, value)| (key.clone(), Self::from_json(value))).collect()))
            }
        }
    }

    fn tagged_temporal(map: &Map<String, Value>) -> Option<Self> {
        let (tag, value) = map.iter().next()?;
        let text = value.as_str()?;
        match tag.as_str() {
            "$date" => parse_iso_date(text).map(Self::Date),
            "$datetime" => parse_iso_datetime(text).map(Self::DateTime),
            "$time" => parse_iso_time(text).map(|(time, _)| Self::Time(time)),
            _ => None,
        }
    }

    /// The callback conversion (`literal_to_py`) of an engine argument.
    pub fn from_literal(value: &LiteralValue) -> Self {
        match value {
            LiteralValue::Empty => Self::None,
            LiteralValue::Boolean(flag) => Self::Bool(*flag),
            LiteralValue::Int(number) => Self::Int(*number),
            LiteralValue::Number(number) => Self::Float(*number),
            LiteralValue::Text(text) => Self::Str(text.clone()),
            LiteralValue::Date(day) => Self::Date(*day),
            LiteralValue::DateTime(stamp) => Self::DateTime(*stamp),
            LiteralValue::Time(clock) => Self::Time(*clock),
            LiteralValue::Array(rows) => {
                Self::List(rows.iter().map(|row| Self::List(row.iter().map(Self::from_literal).collect())).collect())
            }
            LiteralValue::Error(_) | LiteralValue::Pending | LiteralValue::Duration(_) => Self::Literal(value.clone()),
        }
    }

    /// Python's `type(value).__name__`.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::None => "NoneType",
            Self::Bool(_) => "bool",
            Self::Int(_) => "int",
            Self::Float(_) => "float",
            Self::Str(_) => "str",
            Self::Date(_) => "date",
            Self::DateTime(_) => "datetime",
            Self::Time(_) => "time",
            Self::List(_) => "list",
            Self::Dict(_) => "dict",
            Self::Literal(LiteralValue::Duration(_)) => "timedelta",
            Self::Literal(_) => "dict",
        }
    }

    /// `py_to_literal`.
    pub fn to_literal(&self) -> Result<LiteralValue, PortError> {
        Ok(match self {
            Self::None => LiteralValue::Empty,
            Self::Bool(flag) => LiteralValue::Boolean(*flag),
            Self::Int(number) => LiteralValue::Int(*number),
            Self::Float(number) => LiteralValue::Number(*number),
            Self::Str(text) => LiteralValue::Text(text.clone()),
            Self::Date(day) => LiteralValue::Date(*day),
            Self::DateTime(stamp) => LiteralValue::DateTime(*stamp),
            Self::Time(clock) => LiteralValue::Time(*clock),
            Self::Literal(value) => value.clone(),
            Self::List(items) => {
                let mut rows = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    let Self::List(cells) = item else {
                        return Err(PortError::Value(format!("Array row {} must be a list", index + 1)));
                    };
                    rows.push(cells.iter().map(Self::to_literal).collect::<Result<Vec<_>, _>>()?);
                }
                if let Some(first) = rows.first() {
                    let expected = first.len();
                    for (index, row) in rows.iter().enumerate() {
                        if row.len() != expected {
                            return Err(PortError::Value(format!(
                                "Array rows must be rectangular (row {} has length {}, expected {expected})",
                                index + 1,
                                row.len()
                            )));
                        }
                    }
                }
                LiteralValue::Array(rows)
            }
            Self::Dict(_) => return Err(PortError::Type("Unsupported value type for LiteralValue".into())),
        })
    }

    fn is_blank_text(&self) -> bool {
        matches!(self, Self::Str(text) if text.is_empty())
    }
}

/// Python `==` between two plain values (header membership).
fn py_equal(left: &WireValue, right: &WireValue) -> bool {
    use WireValue as W;
    match (left, right) {
        (W::Int(a), W::Float(b)) | (W::Float(b), W::Int(a)) => (*a as f64) == *b,
        (W::Bool(a), W::Int(b)) | (W::Int(b), W::Bool(a)) => i64::from(*a) == *b,
        (W::Bool(a), W::Float(b)) | (W::Float(b), W::Bool(a)) => f64::from(u8::from(*a)) == *b,
        _ => left == right,
    }
}

/// Python `repr` of a `str`.
pub fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ if ch == quote => {
                out.push('\\');
                out.push(ch);
            }
            _ if (ch as u32) < 0x20 || ch as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", ch as u32)),
            _ => out.push(ch),
        }
    }
    out.push(quote);
    out
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A failure inside port admission or a port read, by Python exception type.
#[derive(Debug, Clone, PartialEq)]
pub enum PortError {
    /// `ValueError`: the request does not satisfy the interface.
    Value(String),
    /// `SheetPortConstraintError`.
    Constraint(String),
    /// `TypeError` from the native conversion.
    Type(String),
    /// Anything else (`KeyError`, engine errors, ...).
    Other { kind: String, message: String },
}

impl PortError {
    pub fn kind(&self) -> &str {
        match self {
            Self::Value(_) => "ValueError",
            Self::Constraint(_) => "SheetPortConstraintError",
            Self::Type(_) => "TypeError",
            Self::Other { kind, .. } => kind,
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::Value(message) | Self::Constraint(message) | Self::Type(message) => message,
            Self::Other { message, .. } => message,
        }
    }

    /// Whether a child call reports it as a routing refusal (`ValueError` or
    /// `SheetPortConstraintError` in `calculate_child`).
    pub fn is_admission(&self) -> bool {
        matches!(self, Self::Value(_) | Self::Constraint(_))
    }

    fn key(name: &str) -> Self {
        Self::Other { kind: "KeyError".into(), message: py_repr(name) }
    }

    /// The error as a request failure (`Type: message`).
    pub fn into_error(self) -> ModelCallError {
        ModelCallError::infrastructure(self.kind().to_owned(), self.message().to_owned())
    }
}

impl From<SheetPortError> for PortError {
    fn from(error: SheetPortError) -> Self {
        match error {
            SheetPortError::ConstraintViolation { .. } => {
                Self::Constraint("value did not satisfy manifest constraints".into())
            }
            SheetPortError::Engine { source } => {
                Self::Other { kind: "ExcelEvaluationError".into(), message: source.to_string() }
            }
            SheetPortError::Workbook { source: formualizer_workbook::IoError::Engine(source) } => {
                Self::Other { kind: "ExcelEvaluationError".into(), message: source.to_string() }
            }
            SheetPortError::Workbook { source } => {
                Self::Other { kind: "SheetPortWorkbookError".into(), message: source.to_string() }
            }
            SheetPortError::InvalidManifest { .. } => {
                Self::Other { kind: "SheetPortManifestError".into(), message: "manifest validation failed".into() }
            }
            other => Self::Other { kind: "SheetPortError".into(), message: other.to_string() },
        }
    }
}

// ---------------------------------------------------------------------------
// Alias map and canonical names
// ---------------------------------------------------------------------------

/// Casefolded accepted spelling -> canonical port name (`spec_port_alias_map`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PortAliasMap(pub Vec<(String, String)>);

impl PortAliasMap {
    fn insert(&mut self, folded: String, canonical: String) {
        match self.0.iter_mut().find(|(existing, _)| *existing == folded) {
            Some(slot) => slot.1 = canonical,
            None => self.0.push((folded, canonical)),
        }
    }

    pub fn get(&self, folded: &str) -> Option<&str> {
        self.0.iter().find(|(existing, _)| existing == folded).map(|(_, canonical)| canonical.as_str())
    }
}

/// `port_alias_map(defaults, spec)`.
fn port_alias_map(spec: &ModelSpec) -> PortAliasMap {
    let mut aliases = PortAliasMap::default();
    for key in spec.defaults.keys() {
        aliases.insert(casefold(key), key.to_owned());
    }
    for (folded, location) in spec.inputs.iter() {
        aliases.insert(folded.to_owned(), location.key.clone());
    }
    aliases
}

/// `spec_port_alias_map(spec)`.
pub fn spec_port_alias_map(spec: &ModelSpec) -> Result<PortAliasMap, ModelCallError> {
    Ok(port_alias_map(spec))
}

/// `canonical_port_inputs` over any value type; `ignored` collects names the
/// `ignore` policy drops.
fn canonical_pairs<V: Clone>(
    inputs: &[(String, V)],
    aliases: &PortAliasMap,
    policy: UnknownInputPolicy,
    mut ignored: Option<&mut Vec<String>>,
) -> Result<Vec<(String, V)>, PortError> {
    let mut result: Vec<(String, V)> = Vec::new();
    for (key, value) in inputs {
        let Some(canonical) = aliases.get(&casefold(key)) else {
            if policy == UnknownInputPolicy::Ignore {
                if let Some(ignored) = ignored.as_deref_mut() {
                    ignored.push(key.clone());
                }
                continue;
            }
            return Err(PortError::Value(format!("Undeclared input {}", py_repr(key))));
        };
        if result.iter().any(|(existing, _)| existing == canonical) {
            return Err(PortError::Value(format!("Duplicate input {}", py_repr(canonical))));
        }
        result.push((canonical.to_owned(), value.clone()));
    }
    Ok(result)
}

/// `canonical_port_inputs(inputs, aliases, policy)`: canonical name -> value;
/// a duplicate canonical name is an error; under `Ignore` unmatched names are
/// dropped, under `Reject` they are an error.
pub fn canonical_port_inputs(
    inputs: &[(String, LiteralValue)],
    aliases: &PortAliasMap,
    policy: UnknownInputPolicy,
) -> Result<InputPairs, ModelCallError> {
    canonical_pairs(inputs, aliases, policy, None).map_err(PortError::into_error)
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

// ---------------------------------------------------------------------------
// Python parsers used by wire decoding
// ---------------------------------------------------------------------------

/// Python `float(text)` (text already stripped by the caller).
pub fn py_float(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let cleaned: String = if trimmed.contains('_') {
        let chars: Vec<char> = trimmed.chars().collect();
        for (index, ch) in chars.iter().enumerate() {
            if *ch == '_' {
                let before = index.checked_sub(1).and_then(|i| chars.get(i)).is_some_and(char::is_ascii_digit);
                let after = chars.get(index + 1).is_some_and(char::is_ascii_digit);
                if !(before && after) {
                    return None;
                }
            }
        }
        chars.into_iter().filter(|ch| *ch != '_').collect()
    } else {
        trimmed.to_owned()
    };
    let lowered = cleaned.to_ascii_lowercase();
    let unsigned = lowered.trim_start_matches(['+', '-']);
    if lowered.len() - unsigned.len() > 1 {
        return None;
    }
    let negative = lowered.starts_with('-');
    let special = match unsigned {
        "inf" | "infinity" => Some(f64::INFINITY),
        "nan" => Some(f64::NAN),
        _ => None,
    };
    if let Some(value) = special {
        return Some(if negative { -value } else { value });
    }
    if !unsigned.chars().next().is_some_and(|ch| ch.is_ascii_digit() || ch == '.') {
        return None;
    }
    cleaned.parse::<f64>().ok()
}

fn digits(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// `date.fromisoformat` (extended `YYYY-MM-DD` or basic `YYYYMMDD`).
pub fn parse_iso_date(text: &str) -> Option<NaiveDate> {
    let (year, month, day) = match text.len() {
        10 if text.as_bytes()[4] == b'-' && text.as_bytes()[7] == b'-' => {
            (digits(text.get(0..4)?)?, digits(text.get(5..7)?)?, digits(text.get(8..10)?)?)
        }
        8 => (digits(text.get(0..4)?)?, digits(text.get(4..6)?)?, digits(text.get(6..8)?)?),
        _ => return None,
    };
    if year == 0 {
        return None;
    }
    NaiveDate::from_ymd_opt(i32::try_from(year).ok()?, month, day)
}

/// `time.fromisoformat` of a time part (optionally with a UTC offset, which
/// is validated and returned in seconds).
pub fn parse_iso_time(text: &str) -> Option<(NaiveTime, Option<i32>)> {
    let split = text.find(['+', '-', 'Z']);
    let (clock, offset) = match split {
        Some(index) => (&text[..index], Some(&text[index..])),
        None => (text, None),
    };
    let offset_seconds = match offset {
        None => None,
        Some("Z") => Some(0),
        Some(zone) => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let (time, _) = parse_clock(&zone[1..])?;
            Some(sign * i32::try_from(time.num_seconds_from_midnight()).ok()?)
        }
    };
    let (time, _) = parse_clock(clock)?;
    Some((time, offset_seconds))
}

/// `HH[:MM[:SS[.f+]]]` or `HH[MM[SS[.f+]]]`.
fn parse_clock(text: &str) -> Option<(NaiveTime, ())> {
    let (main, fraction) = match text.find(['.', ',']) {
        Some(index) => (&text[..index], Some(&text[index + 1..])),
        None => (text, None),
    };
    let parts: Vec<&str> = if main.contains(':') {
        main.split(':').collect()
    } else {
        if main.len() % 2 != 0 || main.len() > 6 {
            return None;
        }
        (0..main.len()).step_by(2).map(|i| &main[i..i + 2]).collect()
    };
    if parts.is_empty() || parts.len() > 3 || parts.iter().any(|part| part.len() != 2) {
        return None;
    }
    let hour = digits(parts[0])?;
    let minute = parts.get(1).map_or(Some(0), |part| digits(part))?;
    let second = parts.get(2).map_or(Some(0), |part| digits(part))?;
    let micro = match fraction {
        None => 0,
        Some(fraction) => {
            if parts.len() != 3 || fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let mut padded: String = fraction.chars().take(6).collect();
            while padded.len() < 6 {
                padded.push('0');
            }
            digits(&padded)?
        }
    };
    NaiveTime::from_hms_micro_opt(hour, minute, second, micro).map(|time| (time, ()))
}

/// `datetime.fromisoformat(text).replace(tzinfo=None)`.
pub fn parse_iso_datetime(text: &str) -> Option<NaiveDateTime> {
    let bytes = text.as_bytes();
    let (date, rest) = if bytes.len() >= 10 && bytes.get(4) == Some(&b'-') {
        (parse_iso_date(text.get(0..10)?)?, text.get(10..)?)
    } else if bytes.len() >= 8 {
        (parse_iso_date(text.get(0..8)?)?, text.get(8..)?)
    } else {
        return None;
    };
    if rest.is_empty() {
        return Some(date.and_time(NaiveTime::MIN));
    }
    let mut chars = rest.chars();
    chars.next()?; // any single separator character
    let (time, _) = parse_iso_time(chars.as_str())?;
    Some(date.and_time(time))
}

fn invalid_isoformat(text: &str) -> PortError {
    PortError::Value(format!("Invalid isoformat string: {}", py_repr(text)))
}

/// `PortSession._temporal(value, kind)`.
fn temporal(value: &WireValue, kind: &str) -> Result<WireValue, PortError> {
    let WireValue::Str(text) = value else { return Ok(value.clone()) };
    if text.is_empty() {
        return Ok(value.clone());
    }
    match kind {
        "datetime" => {
            let replaced = text.replace('Z', "+00:00");
            parse_iso_datetime(&replaced).map(WireValue::DateTime).ok_or_else(|| invalid_isoformat(&replaced))
        }
        "date" => {
            let head: String = text.chars().take(10).collect();
            parse_iso_date(&head).map(WireValue::Date).ok_or_else(|| invalid_isoformat(&head))
        }
        "number" | "integer" => {
            let stripped = text.trim();
            let number = py_float(stripped).ok_or_else(|| {
                PortError::Value(format!("could not convert string to float: {}", py_repr(stripped)))
            })?;
            if kind == "integer" {
                if !(number.is_finite() && number.fract() == 0.0) {
                    return Err(PortError::Value("Input requires an integer".into()));
                }
                if number.abs() < 9_223_372_036_854_775_808.0 {
                    return Ok(WireValue::Int(integral_i64(number)));
                }
                return Err(PortError::Other {
                    kind: "OverflowError".into(),
                    message: "Python int too large to convert to C long".into(),
                });
            }
            Ok(WireValue::Float(number))
        }
        _ => Ok(value.clone()),
    }
}

/// An integral, in-range float as `i64` (`int(number)`).
fn integral_i64(number: f64) -> i64 {
    // Checked integral and within i64 by the caller.
    number as i64
}

fn iso_date_prefix(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() >= 10
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(u8::is_ascii_digit)
}

// ---------------------------------------------------------------------------
// CL-097 write record
// ---------------------------------------------------------------------------

/// CL-097 counters (`PortSession.write_stats`), in Python's key order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriteStats {
    pub writes_skipped: u64,
    pub formula_restores_skipped: u64,
    pub defaults_not_restored_overwritten: u64,
    pub date_writes_skipped: u64,
}

impl WriteStats {
    pub fn entries(&self) -> [(&'static str, u64); 4] {
        [
            ("writes_skipped", self.writes_skipped),
            ("formula_restores_skipped", self.formula_restores_skipped),
            ("defaults_not_restored_overwritten", self.defaults_not_restored_overwritten),
            ("date_writes_skipped", self.date_writes_skipped),
        ]
    }
}

/// One cell `(sheet, row, col)`, 1-based.
pub type CellKey = (String, u32, u32);

/// `written_token(value)`: exact type plus value (`repr`). `1`, `1.0` and
/// `True` differ; `-0.0` differs from `0.0` (bits); NaN, time and every other
/// type have none.
#[derive(Debug, Clone, PartialEq)]
enum WrittenToken {
    None,
    Bool(bool),
    Int(i64),
    Float(u64),
    Str(String),
    Date(NaiveDate),
    DateTime(NaiveDateTime),
}

fn written_token(value: &WireValue) -> Option<WrittenToken> {
    Some(match value {
        WireValue::None => WrittenToken::None,
        WireValue::Bool(flag) => WrittenToken::Bool(*flag),
        WireValue::Int(number) => WrittenToken::Int(*number),
        WireValue::Float(number) if number.is_nan() => return None,
        WireValue::Float(number) => WrittenToken::Float(number.to_bits()),
        WireValue::Str(text) => WrittenToken::Str(text.clone()),
        WireValue::Date(day) => WrittenToken::Date(*day),
        WireValue::DateTime(stamp) => WrittenToken::DateTime(*stamp),
        _ => return None,
    })
}

/// `_incoming_literal` / `_stored_literal`: the comparable stored literal.
#[derive(Debug, Clone, PartialEq)]
enum StoredKey {
    Empty,
    Boolean(bool),
    Number(u64),
    Text(String),
    DateTime(NaiveDateTime),
    Date(NaiveDate),
    Time(NaiveTime),
}

fn number_key(number: f64) -> Option<StoredKey> {
    (!number.is_nan()).then(|| StoredKey::Number(number.to_bits()))
}

fn incoming_key(value: &WireValue) -> Option<StoredKey> {
    match value {
        WireValue::None => Some(StoredKey::Empty),
        WireValue::Bool(flag) => Some(StoredKey::Boolean(*flag)),
        #[expect(clippy::cast_precision_loss, reason = "Python float(int), round-to-nearest")]
        WireValue::Int(number) => number_key(*number as f64),
        WireValue::Float(number) => number_key(*number),
        WireValue::Str(text) => Some(StoredKey::Text(text.clone())),
        WireValue::DateTime(stamp) => Some(StoredKey::DateTime(truncate_micros(*stamp))),
        WireValue::Date(day) => Some(StoredKey::Date(*day)),
        WireValue::Time(clock) => Some(StoredKey::Time(*clock)),
        _ => None,
    }
}

fn truncate_micros(stamp: NaiveDateTime) -> NaiveDateTime {
    stamp.with_nanosecond(stamp.nanosecond() / 1_000 * 1_000).unwrap_or(stamp)
}

/// `stored_literal_key(literal)`.
fn stored_key(literal: &LiteralValue) -> Option<StoredKey> {
    match literal {
        LiteralValue::Empty => Some(StoredKey::Empty),
        LiteralValue::Boolean(flag) => Some(StoredKey::Boolean(*flag)),
        #[expect(clippy::cast_precision_loss, reason = "Python float(int), round-to-nearest")]
        LiteralValue::Int(number) => number_key(*number as f64),
        LiteralValue::Number(number) => number_key(*number),
        LiteralValue::Text(text) => Some(StoredKey::Text(text.clone())),
        LiteralValue::DateTime(stamp) => Some(StoredKey::DateTime(truncate_micros(*stamp))),
        LiteralValue::Date(day) => Some(StoredKey::Date(*day)),
        LiteralValue::Time(clock) => {
            Some(StoredKey::Time(clock.with_nanosecond(clock.nanosecond() / 1_000 * 1_000).unwrap_or(*clock)))
        }
        _ => None,
    }
}

/// `stored_literal_equal(value, literal)`.
fn stored_literal_equal(value: &WireValue, literal: &LiteralValue) -> bool {
    match (incoming_key(value), stored_key(literal)) {
        (Some(incoming), Some(stored)) => incoming == stored,
        _ => false,
    }
}

/// `_is_temporal`: a date or datetime (not a time).
fn is_temporal(value: &WireValue) -> bool {
    matches!(value, WireValue::Date(_) | WireValue::DateTime(_))
}

/// What one loaded workbook's port sessions last wrote, per cell (CL-097,
/// `ports.WriteRecord`). A record that could be stale is dropped: on an error
/// inside `write_scenario`, when another write path touches cells
/// (`invalidate`), on a scenario reset (`clear`) and in a forked process.
#[derive(Debug, Clone)]
pub struct WriteRecord {
    pid: u32,
    cells: HashMap<CellKey, WrittenToken>,
    formulas: HashMap<CellKey, String>,
    overridden: HashSet<CellKey>,
    readback: HashMap<CellKey, StoredKey>,
}

impl Default for WriteRecord {
    fn default() -> Self {
        Self {
            pid: std::process::id(),
            cells: HashMap::new(),
            formulas: HashMap::new(),
            overridden: HashSet::new(),
            readback: HashMap::new(),
        }
    }
}

impl WriteRecord {
    pub fn new() -> Self {
        Self::default()
    }

    /// A forked process drops what its parent recorded.
    pub fn check_process(&mut self) {
        let pid = std::process::id();
        if pid != self.pid {
            self.clear();
            self.pid = pid;
        }
    }

    /// The record no longer vouches for the workbook (report conditions,
    /// `forget_scenarios`).
    pub fn clear(&mut self) {
        self.cells.clear();
        self.formulas.clear();
        self.overridden.clear();
        self.readback.clear();
    }

    /// Drop what the record knows of one cell's last port write.
    pub fn forget(&mut self, cell: &CellKey) {
        self.cells.remove(cell);
        self.readback.remove(cell);
    }

    /// Another write path touched these cells.
    pub fn invalidate<'a>(&mut self, cells: impl IntoIterator<Item = &'a CellKey>) {
        for cell in cells {
            self.forget(cell);
            self.formulas.remove(cell);
        }
    }

    /// Number of cells with a recorded port write (tests, stats).
    pub fn recorded_cells(&self) -> usize {
        self.cells.len()
    }
}

/// `_port_cells(loc, shape, value)`: `(cell, field, value)` per cell of one
/// native port update, or `None` for a geometry the record cannot follow.
fn port_cells(location: &PortLocation, shape: &str, value: &WireValue) -> Option<Vec<(CellKey, Option<String>, WireValue)>> {
    let range = &location.range;
    let sheet = &range.sheet;
    let (height, width) = (range.rows(), range.cols());
    match shape {
        "scalar" => Some(vec![((sheet.clone(), range.start_row, range.start_col), None, value.clone())]),
        "record" => {
            let WireValue::Dict(fields) = value else { return None };
            let mut cells = Vec::with_capacity(fields.len());
            for (field, cell) in fields.iter() {
                let rest = field.strip_prefix('r')?;
                let (row, col) = rest.split_once("_c")?;
                let (row, col) = (py_int(row)?, py_int(col)?);
                if row < 0 || col < 0 || row >= i64::from(height) || col >= i64::from(width) {
                    return None;
                }
                let (row, col) = (u32::try_from(row).ok()?, u32::try_from(col).ok()?);
                cells.push(((sheet.clone(), range.start_row + row, range.start_col + col), Some(field.to_owned()), cell.clone()));
            }
            Some(cells)
        }
        _ => {
            let WireValue::List(rows) = value else { return None };
            if rows.len() > height as usize {
                return None;
            }
            let mut cells = Vec::new();
            for (r, row) in rows.iter().enumerate() {
                let WireValue::List(row) = row else { return None };
                if row.len() > width as usize {
                    return None;
                }
                for (c, cell) in row.iter().enumerate() {
                    let (r, c) = (u32::try_from(r).ok()?, u32::try_from(c).ok()?);
                    cells.push(((sheet.clone(), range.start_row + r, range.start_col + c), None, cell.clone()));
                }
            }
            Some(cells)
        }
    }
}

/// Python `int(text)` for the `r{r}_c{c}` field parts.
fn py_int(text: &str) -> Option<i64> {
    let trimmed = text.trim();
    let digits = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
    if digits.is_empty() || !digits.chars().all(|ch| ch.is_ascii_digit() || ch == '_') || digits.starts_with('_') {
        return None;
    }
    trimmed.replace('_', "").parse().ok()
}

// ---------------------------------------------------------------------------
// The port session
// ---------------------------------------------------------------------------

/// One admitted scenario (`PortSession._admit`), computed without a
/// workbook: the effective inputs by declared key, the native update by port
/// id, and the names the admission dropped or left at their defaults. The
/// engine path writes it with [`PortSession::apply`]; a compiled parent hands
/// it to its module ([`parent_port_literal`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Admitted {
    /// Effective inputs, declared key -> admitted value (defaults first).
    pub effective: Vec<(String, WireValue)>,
    /// Native update, port id -> value, in `effective` order.
    pub by_port: Vec<(String, WireValue)>,
    /// Request names the `ignore` policy dropped.
    pub ignored_inputs: Vec<String>,
    /// Declared inputs left at their defaults, declared spelling, casefold-sorted.
    pub defaulted_inputs: Vec<String>,
}

impl Admitted {
    /// What `write_scenario` returns: key -> the value written for its port.
    pub fn returned(&self, spec: &ModelSpec) -> Result<Vec<(String, WireValue)>, PortError> {
        let mut returned = Vec::with_capacity(self.effective.len());
        for (key, _) in &self.effective {
            let port_id = &PortAdmission::location(spec, key)?.port_id;
            let value = self.by_port.iter().find(|(id, _)| id == port_id).map(|(_, value)| value.clone());
            returned.push((key.clone(), value.unwrap_or(WireValue::None)));
        }
        Ok(returned)
    }
}

/// `admit_scenario(spec, wire)`: the engine path's admission (alias map,
/// unknown-input policy, wire decoding, record/range/table shaping) with no
/// workbook. Byte-identical to what `PortSession::write_scenario` admits.
pub fn admit_scenario(spec: &ModelSpec, inputs: &[(String, WireValue)], decode_wire: bool) -> Result<Admitted, PortError> {
    let admission = PortAdmission::new(spec)?;
    let (mut ignored, mut defaulted) = (Vec::new(), Vec::new());
    let (effective, by_port) = admission.admit(spec, inputs, decode_wire, &mut ignored, &mut defaulted)?;
    Ok(Admitted { effective, by_port, ignored_inputs: ignored, defaulted_inputs: defaulted })
}

/// A parent input as the compiled module receives it (architecture B, Lane D
/// `whole_model.parent_port_value` + `compiled.adapter.admitted_value`): a
/// date or datetime becomes its 1900 serial (`(value - 1899-12-30)` in days,
/// seconds and microseconds as Python computes it), an int becomes a float
/// (the engine cell stores a Number), `None` is blank; everything else is
/// `py_to_literal`. A range port's rows (a list of lists) become an array of
/// cells under the same law; a record (dict) is refused (the session
/// declines record inputs statically).
pub fn parent_port_literal(value: &WireValue) -> Result<LiteralValue, PortError> {
    Ok(match value {
        WireValue::List(rows) if rows.iter().all(|row| matches!(row, WireValue::List(_))) => {
            let mut matrix = Vec::with_capacity(rows.len());
            for row in rows {
                let WireValue::List(cells) = row else { continue };
                matrix.push(cells.iter().map(parent_port_literal).collect::<Result<Vec<_>, _>>()?);
            }
            LiteralValue::Array(matrix)
        }
        WireValue::Date(day) => LiteralValue::Number(python_serial_days(day.and_time(NaiveTime::MIN))),
        WireValue::DateTime(stamp) => LiteralValue::Number(python_serial_days(*stamp)),
        #[expect(clippy::cast_precision_loss, reason = "Python float(int), round-to-nearest")]
        WireValue::Int(number) => LiteralValue::Number(*number as f64),
        WireValue::List(_) | WireValue::Dict(_) => {
            return Err(PortError::Type("a compiled parent admits scalar and range inputs only".into()));
        }
        other => other.to_literal()?,
    })
}

/// `(value - datetime(1899, 12, 30))` as Python's `parent_port_value` turns it
/// into a float: `float(delta.days) + (delta.seconds + delta.microseconds /
/// 1e6) / 86400.0`, with `timedelta`'s normalisation (days floored, seconds
/// and microseconds non-negative). Sub-microsecond parts are dropped.
fn python_serial_days(stamp: NaiveDateTime) -> f64 {
    let origin = NaiveDate::from_ymd_opt(1899, 12, 30).map(|day| day.and_time(NaiveTime::MIN));
    let micros = origin.and_then(|origin| (stamp - origin).num_microseconds()).unwrap_or(0);
    const DAY: i64 = 86_400_000_000;
    let days = micros.div_euclid(DAY);
    let rest = micros.rem_euclid(DAY);
    let (seconds, micro) = (rest / 1_000_000, rest % 1_000_000);
    #[expect(clippy::cast_precision_loss, reason = "Python float(int) of timedelta fields")]
    let serial = days as f64 + (seconds as f64 + micro as f64 / 1e6) / 86_400.0;
    serial
}

#[derive(Debug, Clone)]
struct PortDecl {
    id: String,
    shape: String,
    schema: Value,
}

/// The workbook-free half of `PortSession`: declared ports, alias map,
/// defaults and the unknown-input policy (`PortSession._admit` needs no
/// engine call).
#[derive(Debug, Clone)]
pub struct PortAdmission {
    ports: Vec<PortDecl>,
    aliases: PortAliasMap,
    defaults: Vec<(String, WireValue)>,
    policy: UnknownInputPolicy,
}

/// `PortSession` for a published `ModelSpec` (the only kind the runtime loads).
pub struct PortSession {
    bindings: ManifestBindings,
    admission: PortAdmission,
    source_loaded: bool,
    /// Request names the `ignore` policy dropped, per scenario.
    pub ignored_inputs: Vec<String>,
    /// Declared inputs left at their defaults, declared spelling, casefold-sorted.
    pub defaulted_inputs: Vec<String>,
    pub write_record: Option<WriteRecord>,
    pub write_stats: Option<WriteStats>,
}

/// Where `write_scenario_with` takes its admission from.
enum Admission<'a> {
    Wire { inputs: &'a [(String, WireValue)], decode_wire: bool },
    Given(&'a Admitted),
}

fn json_manifest(spec: &ModelSpec) -> Result<Manifest, PortError> {
    let text = serde_json::to_string(&spec.manifest)
        .map_err(|error| PortError::Other { kind: "TypeError".into(), message: error.to_string() })?;
    Manifest::from_yaml_str(&text)
        .map_err(|error| PortError::Other { kind: "SheetPortManifestError".into(), message: error.to_string() })
}

impl PortAdmission {
    /// The declared ports, alias map, defaults and policy of `spec`.
    pub fn new(spec: &ModelSpec) -> Result<Self, PortError> {
        let ports = spec
            .manifest
            .get("ports")
            .and_then(Value::as_array)
            .map(|ports| {
                ports
                    .iter()
                    .filter(|port| port.get("dir").and_then(Value::as_str) == Some("in"))
                    .map(|port| PortDecl {
                        id: port.get("id").and_then(Value::as_str).unwrap_or_default().to_owned(),
                        shape: port.get("shape").and_then(Value::as_str).unwrap_or_default().to_owned(),
                        schema: port.get("schema").cloned().unwrap_or(Value::Null),
                    })
                    .collect()
            })
            .unwrap_or_default();
        match spec.descriptor.get("unknown_input_policy").and_then(Value::as_str) {
            None | Some("reject") | Some("ignore") => {}
            Some(_) => return Err(PortError::Value("Unsupported unknown input policy".into())),
        }
        let defaults = spec.defaults.iter().map(|(key, value)| (key.to_owned(), WireValue::from_json(value))).collect();
        Ok(Self { ports, aliases: port_alias_map(spec), defaults, policy: spec.unknown_input_policy() })
    }

    fn port(&self, id: &str) -> Result<&PortDecl, PortError> {
        self.ports.iter().find(|port| port.id == id).ok_or_else(|| PortError::key(id))
    }

    fn location<'a>(spec: &'a ModelSpec, key: &str) -> Result<&'a PortLocation, PortError> {
        spec.inputs.get(&casefold(key)).ok_or_else(|| PortError::key(&casefold(key)))
    }

    /// `_admit`: (effective inputs, native update by port id); no engine call.
    #[expect(clippy::type_complexity, reason = "(effective by key, native update by port id)")]
    fn admit(
        &self,
        spec: &ModelSpec,
        inputs: &[(String, WireValue)],
        decode_wire: bool,
        ignored_inputs: &mut Vec<String>,
        defaulted_inputs: &mut Vec<String>,
    ) -> Result<(Vec<(String, WireValue)>, Vec<(String, WireValue)>), PortError> {
        let mut effective = self.defaults.clone();
        let mut ignored = Vec::new();
        let updates = canonical_pairs(inputs, &self.aliases, self.policy, Some(&mut ignored));
        *ignored_inputs = ignored;
        let updates = updates?;
        let supplied: Vec<String> = updates.iter().map(|(key, _)| casefold(key)).collect();
        let mut defaulted: Vec<String> = spec
            .inputs
            .iter()
            .filter(|(folded, _)| !supplied.iter().any(|name| name == folded))
            .map(|(_, location)| location.key.clone())
            .collect();
        defaulted.sort_by_key(|key| casefold(key));
        *defaulted_inputs = defaulted;

        for (key, value) in updates {
            let location = Self::location(spec, &key)?;
            let port = self.port(&location.port_id)?.clone();
            let mut value = value;
            if let WireValue::List(items) = &value
                && port.shape != "scalar"
                && items.first().is_none_or(|first| matches!(first, WireValue::Dict(_)))
            {
                let headers: Vec<WireValue> =
                    location.headers.iter().flatten().map(WireValue::from_json).collect();
                let mut rows = vec![WireValue::List(headers.clone())];
                for item in items {
                    let WireValue::Dict(row) = item else {
                        return Err(PortError::Other {
                            kind: "AttributeError".into(),
                            message: format!("'{}' object has no attribute 'get'", item.type_name()),
                        });
                    };
                    if row.keys().any(|column| !headers.iter().any(|h| py_equal(&WireValue::Str(column.to_owned()), h))) {
                        return Err(PortError::Value(format!("{key} has an undeclared column")));
                    }
                    rows.push(WireValue::List(
                        headers
                            .iter()
                            .map(|header| match header {
                                WireValue::Str(name) => row.get(name).cloned().unwrap_or(WireValue::None),
                                _ => WireValue::None,
                            })
                            .collect(),
                    ));
                }
                value = WireValue::List(rows);
            }
            let slot = match effective.iter().position(|(existing, _)| *existing == key) {
                Some(position) => position,
                None => {
                    effective.push((key.clone(), WireValue::None));
                    effective.len() - 1
                }
            };
            let had_default = self.defaults.iter().any(|(existing, _)| *existing == key);
            match port.shape.as_str() {
                "record" => {
                    if let WireValue::Dict(update) = &value {
                        let update = if decode_wire {
                            self.native_one(spec, &key, &value, true)?
                        } else {
                            WireValue::Dict(update.clone())
                        };
                        if !had_default {
                            return Err(PortError::key(&key));
                        }
                        let WireValue::Dict(current) = &mut effective[slot].1 else {
                            return Err(PortError::Other {
                                kind: "AttributeError".into(),
                                message: "object has no attribute 'update'".into(),
                            });
                        };
                        if let WireValue::Dict(update) = update {
                            for (field, cell) in update.0 {
                                match current.0.iter_mut().find(|(existing, _)| *existing == field) {
                                    Some(existing) => existing.1 = cell,
                                    None => current.0.push((field, cell)),
                                }
                            }
                        }
                    } else {
                        let rows = as_rows(&value)?;
                        let formula_fields: Vec<String> = spec
                            .descriptor
                            .get("formula_input_defaults")
                            .and_then(|defaults| defaults.get(&key))
                            .and_then(Value::as_array)
                            .map(|entries| {
                                entries
                                    .iter()
                                    .filter_map(|entry| entry.get("field").and_then(Value::as_str).map(str::to_owned))
                                    .collect()
                            })
                            .unwrap_or_default();
                        let fields: Vec<String> = port
                            .schema
                            .get("fields")
                            .and_then(Value::as_object)
                            .map(|fields| fields.keys().cloned().collect())
                            .unwrap_or_default();
                        let mut cleared: Vec<(String, WireValue)> = fields
                            .iter()
                            .filter(|field| !formula_fields.contains(field))
                            .map(|field| (field.clone(), WireValue::None))
                            .collect();
                        for (r, row) in rows.iter().enumerate() {
                            let WireValue::List(cells) = row else {
                                return Err(PortError::Type(format!(
                                    "'{}' object is not iterable",
                                    row.type_name()
                                )));
                            };
                            for (c, cell) in cells.iter().enumerate() {
                                let field = format!("r{r}_c{c}");
                                if !fields.contains(&field) {
                                    return Err(PortError::Value(format!("{key} exceeds its declared rectangle")));
                                }
                                match cleared.iter_mut().find(|(existing, _)| *existing == field) {
                                    Some(slot) => slot.1 = cell.clone(),
                                    None => cleared.push((field, cell.clone())),
                                }
                            }
                        }
                        let mut cleared = WireValue::Dict(OrderedMap(cleared));
                        if decode_wire {
                            cleared = self.native_one(spec, &key, &cleared, true)?;
                        }
                        effective[slot].1 = cleared;
                    }
                }
                "range" => {
                    if !had_default {
                        return Err(PortError::key(&key));
                    }
                    let baseline = match &effective[slot].1 {
                        WireValue::List(rows) => rows.clone(),
                        _ => Vec::new(),
                    };
                    let WireValue::List(_) = &value else {
                        return Err(PortError::Value(format!("{key} requires a matrix")));
                    };
                    let rows = as_rows(&value)?;
                    let width = match baseline.first() {
                        Some(WireValue::List(first)) => first.len(),
                        _ => 0,
                    };
                    let too_wide = rows.iter().any(|row| match row {
                        WireValue::List(cells) => cells.len() > width,
                        _ => false,
                    });
                    if rows.len() > baseline.len() || too_wide {
                        return Err(PortError::Value(format!("{key} exceeds its declared rectangle")));
                    }
                    let mut cleared: Vec<Vec<WireValue>> = baseline
                        .iter()
                        .map(|row| match row {
                            WireValue::List(cells) => vec![WireValue::None; cells.len()],
                            _ => Vec::new(),
                        })
                        .collect();
                    let rows = if decode_wire {
                        match self.native_one(spec, &key, &WireValue::List(rows), true)? {
                            WireValue::List(rows) => rows,
                            other => vec![other],
                        }
                    } else {
                        rows
                    };
                    for (r, row) in rows.iter().enumerate() {
                        if let WireValue::List(cells) = row {
                            for (c, cell) in cells.iter().enumerate() {
                                if let Some(target) = cleared.get_mut(r).and_then(|row| row.get_mut(c)) {
                                    *target = cell.clone();
                                }
                            }
                        }
                    }
                    effective[slot].1 = WireValue::List(cleared.into_iter().map(WireValue::List).collect());
                }
                _ => {
                    effective[slot].1 = self.native_one(spec, &key, &value, decode_wire)?;
                }
            }
        }
        let mut admitted = Vec::with_capacity(effective.len());
        for (key, value) in &effective {
            admitted.push((Self::location(spec, key)?.port_id.clone(), value.clone()));
        }
        Ok((effective, admitted))
    }

    /// `_native_update({key: value}, decode_wire)[port_id]`.
    fn native_one(&self, spec: &ModelSpec, key: &str, value: &WireValue, decode_wire: bool) -> Result<WireValue, PortError> {
        let location = Self::location(spec, key)?;
        let port = self.port(&location.port_id)?;
        if !decode_wire {
            return Ok(value.clone());
        }
        let schema = &port.schema;
        match port.shape.as_str() {
            "scalar" => {
                let wire_format = spec
                    .descriptor
                    .get("wire_formats")
                    .and_then(|formats| formats.get(casefold(key)))
                    .and_then(Value::as_str);
                match wire_format {
                    Some("excel-number") => {
                        if matches!(value, WireValue::None) || value.is_blank_text() {
                            return Ok(value.clone());
                        }
                        if !matches!(value, WireValue::Str(_) | WireValue::Int(_) | WireValue::Float(_)) {
                            return Err(PortError::Value(
                                "Excel number wire input requires text, a finite number or blank".into(),
                            ));
                        }
                        // CL-105: non-numeric text binds as text.
                        let number = temporal(value, "number").unwrap_or_else(|_| value.clone());
                        let finite = match &number {
                            WireValue::Str(_) => return Ok(value.clone()),
                            WireValue::Float(number) => number.is_finite(),
                            _ => true,
                        };
                        if !finite {
                            return Err(PortError::Value("Excel number wire input requires a finite number".into()));
                        }
                        Ok(number)
                    }
                    Some("excel-datetime") => {
                        if !matches!(
                            value,
                            WireValue::None
                                | WireValue::Str(_)
                                | WireValue::Int(_)
                                | WireValue::Float(_)
                                | WireValue::Date(_)
                                | WireValue::DateTime(_)
                        ) {
                            return Err(PortError::Value(
                                "Excel date wire input requires text or a native date/serial/blank".into(),
                            ));
                        }
                        match temporal(value, "datetime") {
                            Ok(decoded) => Ok(decoded),
                            // CL-105: non-ISO-shaped text binds as text.
                            Err(error) => match value {
                                WireValue::Str(text) if !iso_date_prefix(text) => Ok(value.clone()),
                                _ => Err(error),
                            },
                        }
                    }
                    _ => {
                        let kind = schema.get("type").and_then(Value::as_str).ok_or_else(|| PortError::key("type"))?;
                        temporal(value, kind)
                    }
                }
            }
            "record" => {
                let WireValue::Dict(cells) = value else {
                    return Err(PortError::Other {
                        kind: "AttributeError".into(),
                        message: format!("'{}' object has no attribute 'items'", value.type_name()),
                    });
                };
                let mut decoded = Vec::with_capacity(cells.len());
                for (field, cell) in cells.iter() {
                    let kind = schema
                        .get("fields")
                        .and_then(|fields| fields.get(field))
                        .ok_or_else(|| PortError::key(field))?
                        .get("type")
                        .and_then(Value::as_str)
                        .ok_or_else(|| PortError::key("type"))?;
                    decoded.push((field.to_owned(), temporal(cell, kind)?));
                }
                Ok(WireValue::Dict(OrderedMap(decoded)))
            }
            "range" => {
                let kind = schema.get("cell_type").and_then(Value::as_str).ok_or_else(|| PortError::key("cell_type"))?;
                let WireValue::List(rows) = value else {
                    return Err(PortError::Type(format!("'{}' object is not iterable", value.type_name())));
                };
                let mut decoded = Vec::with_capacity(rows.len());
                for row in rows {
                    let WireValue::List(cells) = row else {
                        return Err(PortError::Type(format!("'{}' object is not iterable", row.type_name())));
                    };
                    decoded.push(WireValue::List(cells.iter().map(|cell| temporal(cell, kind)).collect::<Result<_, _>>()?));
                }
                Ok(WireValue::List(decoded))
            }
            _ => Ok(value.clone()),
        }
    }

}

impl PortSession {
    /// Bind the spec's manifest to `workbook` (`PortSession(workbook, spec,
    /// source_loaded=..., write_record=...)`).
    pub fn new(workbook: &mut Workbook, spec: &ModelSpec, source_loaded: bool, write_record: bool) -> Result<Self, PortError> {
        let manifest = json_manifest(spec)?;
        let sheetport = SheetPort::new(workbook, manifest)?;
        let (_, bindings) = sheetport.into_parts();
        let admission = PortAdmission::new(spec)?;
        Ok(Self {
            bindings,
            admission,
            source_loaded,
            ignored_inputs: Vec::new(),
            defaulted_inputs: Vec::new(),
            write_record: write_record.then(WriteRecord::new),
            write_stats: None,
        })
    }

    fn port(&self, id: &str) -> Result<&PortDecl, PortError> {
        self.admission.port(id)
    }

    /// Re-enter this session on its retained workbook (`RetainedSession`
    /// reuse): the next scenario restores formula defaults.
    pub fn reenter(&mut self) {
        self.source_loaded = false;
    }

    /// `write_scenario(inputs, decode_wire=...)`: admit, write natively and
    /// return the effective inputs (`key -> admitted value`).
    pub fn write_scenario(
        &mut self,
        workbook: &mut Workbook,
        spec: &ModelSpec,
        inputs: &[(String, WireValue)],
        decode_wire: bool,
    ) -> Result<Vec<(String, WireValue)>, PortError> {
        self.write_admission(workbook, spec, Admission::Wire { inputs, decode_wire })
    }

    /// `apply(workbook, admitted)`: write a scenario [`admit_scenario`]
    /// admitted, exactly as `write_scenario` writes its own admission
    /// (formula-default restores, CL-097 skips, the write record), and
    /// return the effective inputs (`key -> admitted value`).
    pub fn apply(
        &mut self,
        workbook: &mut Workbook,
        spec: &ModelSpec,
        admitted: &Admitted,
    ) -> Result<Vec<(String, WireValue)>, PortError> {
        self.write_admission(workbook, spec, Admission::Given(admitted))
    }

    fn write_admission(
        &mut self,
        workbook: &mut Workbook,
        spec: &ModelSpec,
        admission: Admission<'_>,
    ) -> Result<Vec<(String, WireValue)>, PortError> {
        let Some(mut record) = self.write_record.take() else {
            return self.write_scenario_with(workbook, spec, admission, None);
        };
        record.check_process();
        self.write_stats = Some(WriteStats::default());
        let result = self.write_scenario_with(workbook, spec, admission, Some(&mut record));
        if result.is_err() {
            // A write that did not finish leaves cells nobody can account for.
            record.clear();
        }
        self.write_record = Some(record);
        result
    }

    /// `_admit` into this session's per-scenario state.
    fn admit(&mut self, spec: &ModelSpec, admission: Admission<'_>) -> Result<Admitted, PortError> {
        match admission {
            Admission::Given(admitted) => {
                self.ignored_inputs.clone_from(&admitted.ignored_inputs);
                self.defaulted_inputs.clone_from(&admitted.defaulted_inputs);
                Ok(admitted.clone())
            }
            Admission::Wire { inputs, decode_wire } => {
                self.ignored_inputs = Vec::new();
                self.defaulted_inputs = Vec::new();
                let (effective, by_port) = self.admission.admit(
                    spec,
                    inputs,
                    decode_wire,
                    &mut self.ignored_inputs,
                    &mut self.defaulted_inputs,
                )?;
                Ok(Admitted {
                    effective,
                    by_port,
                    ignored_inputs: self.ignored_inputs.clone(),
                    defaulted_inputs: self.defaulted_inputs.clone(),
                })
            }
        }
    }

    fn write_scenario_with(
        &mut self,
        workbook: &mut Workbook,
        spec: &ModelSpec,
        admission: Admission<'_>,
        record: Option<&mut WriteRecord>,
    ) -> Result<Vec<(String, WireValue)>, PortError> {
        let source_loaded = std::mem::replace(&mut self.source_loaded, false);
        let restore = !source_loaded;
        let admitted = match record {
            None => {
                if restore {
                    self.restore_all_formula_defaults(workbook, spec)?;
                }
                let admitted = self.admit(spec, admission)?;
                self.write_native(workbook, &admitted.by_port)?;
                admitted
            }
            Some(record) => {
                let admitted = match self.admit(spec, admission) {
                    Ok(admission) => admission,
                    Err(error) => {
                        if restore {
                            self.restore_all_formula_defaults(workbook, spec)?;
                        }
                        return Err(error);
                    }
                };
                if restore {
                    let overwritten = self.overwritten_cells(spec, &admitted.by_port);
                    self.restore_formula_defaults(workbook, spec, record, &overwritten)?;
                }
                self.write_changed(workbook, spec, &admitted.by_port, record)?;
                admitted
            }
        };
        admitted.returned(spec)
    }

    fn formula_default_entries(spec: &ModelSpec) -> Vec<(CellKey, String)> {
        let Some(entries) = spec.descriptor.get("formula_input_defaults").and_then(Value::as_object) else {
            return Vec::new();
        };
        entries
            .values()
            .filter_map(Value::as_array)
            .flatten()
            .map(|entry| {
                let sheet = entry.get("sheet").and_then(Value::as_str).unwrap_or_default().to_owned();
                let number = |key: &str| {
                    entry.get(key).and_then(Value::as_u64).and_then(|value| u32::try_from(value).ok()).unwrap_or(u32::MAX)
                };
                let formula = entry.get("formula").and_then(Value::as_str).unwrap_or_default();
                ((sheet, number("row"), number("col")), formula.trim_start_matches('=').to_owned())
            })
            .collect()
    }

    /// `_overwritten_cells(admitted)`.
    fn overwritten_cells(&self, spec: &ModelSpec, admitted: &[(String, WireValue)]) -> HashSet<CellKey> {
        let mut covered = HashSet::new();
        for (port_id, value) in admitted {
            let Some(location) = spec.inputs.iter().map(|(_, location)| location).find(|location| &location.port_id == port_id)
            else {
                continue;
            };
            let Ok(port) = self.port(port_id) else { continue };
            if let Some(cells) = port_cells(location, &port.shape, value) {
                covered.extend(cells.into_iter().map(|(cell, _, _)| cell));
            }
        }
        covered
    }

    /// `_restore_formula_defaults(record, overwritten)`.
    fn restore_formula_defaults(
        &mut self,
        workbook: &mut Workbook,
        spec: &ModelSpec,
        record: &mut WriteRecord,
        overwritten: &HashSet<CellKey>,
    ) -> Result<(), PortError> {
        let mut entries = Vec::new();
        for (cell, formula) in Self::formula_default_entries(spec) {
            if overwritten.contains(&cell) {
                record.formulas.remove(&cell);
                record.overridden.insert(cell);
                if let Some(stats) = self.write_stats.as_mut() {
                    stats.defaults_not_restored_overwritten += 1;
                }
            } else {
                entries.push((cell, formula));
            }
        }
        let in_place = entries.iter().all(|(cell, _)| {
            !record.overridden.contains(cell)
                && record.formulas.get(cell).is_some_and(|text| workbook.get_formula(&cell.0, cell.1, cell.2).as_ref() == Some(text))
        });
        if in_place {
            if let Some(stats) = self.write_stats.as_mut() {
                stats.formula_restores_skipped += entries.len() as u64;
            }
            return Ok(());
        }
        for (cell, formula) in &entries {
            record.forget(cell);
            record.formulas.remove(cell);
            workbook
                .set_formula(&cell.0, cell.1, cell.2, formula)
                .map_err(|error| PortError::Other { kind: "RuntimeError".into(), message: error.to_string() })?;
        }
        for (cell, _) in &entries {
            record.overridden.remove(cell);
            if let Some(text) = workbook.get_formula(&cell.0, cell.1, cell.2) {
                record.formulas.insert(cell.clone(), text);
            }
        }
        Ok(())
    }

    /// `_unchanged(record, loc, cells, whole)`.
    fn unchanged(
        workbook: &Workbook,
        record: &WriteRecord,
        location: &PortLocation,
        cells: &[(CellKey, Option<String>, WireValue)],
        whole: bool,
    ) -> Vec<bool> {
        let matches: Vec<bool> = cells
            .iter()
            .map(|(cell, _, value)| match (record.cells.get(cell), written_token(value)) {
                (Some(recorded), Some(token)) => *recorded == token,
                _ => false,
            })
            .collect();
        if !matches.iter().any(|same| *same) || (whole && !matches.iter().all(|same| *same)) {
            return vec![false; cells.len()];
        }
        let Ok(grid) = read_typed_matrix(workbook, &location.range) else { return vec![false; cells.len()] };
        let range = &location.range;
        matches
            .iter()
            .zip(cells)
            .map(|(same, (cell, _, value))| {
                if !same {
                    return false;
                }
                let literal = grid
                    .get((cell.1 - range.start_row) as usize)
                    .and_then(|row| row.get((cell.2 - range.start_col) as usize));
                match literal {
                    None => false,
                    Some(literal) if is_temporal(value) => {
                        record.readback.get(cell).is_some_and(|expected| stored_key(literal).as_ref() == Some(expected))
                    }
                    Some(literal) => stored_literal_equal(value, literal),
                }
            })
            .collect()
    }

    /// `_write_changed(admitted, record)`: the one native write, less the
    /// ports and record fields that are unchanged.
    fn write_changed(
        &mut self,
        workbook: &mut Workbook,
        spec: &ModelSpec,
        admitted: &[(String, WireValue)],
        record: &mut WriteRecord,
    ) -> Result<(), PortError> {
        let formula_cells: HashSet<CellKey> =
            Self::formula_default_entries(spec).into_iter().map(|(cell, _)| cell).collect();
        let mut update: Vec<(String, WireValue)> = Vec::new();
        let mut written: Vec<(CellKey, WireValue)> = Vec::new();
        let (mut skipped, mut dates_skipped) = (0u64, 0u64);
        for (port_id, value) in admitted {
            let location = spec.inputs.iter().map(|(_, location)| location).find(|location| &location.port_id == port_id);
            let shape = self.port(port_id)?.shape.clone();
            let cells = location.and_then(|location| port_cells(location, &shape, value));
            let (Some(location), Some(cells)) = (location, cells) else {
                update.push((port_id.clone(), value.clone()));
                if let Some(location) = location {
                    let range = &location.range;
                    let all: Vec<CellKey> = (range.start_row..=range.end_row)
                        .flat_map(|row| (range.start_col..=range.end_col).map(move |col| (range.sheet.clone(), row, col)))
                        .collect();
                    record.invalidate(&all);
                }
                continue;
            };
            let unchanged = Self::unchanged(workbook, record, location, &cells, shape != "record");
            if shape == "record" {
                let mut kept = Vec::new();
                for ((cell, field, cell_value), same) in cells.iter().zip(&unchanged) {
                    if *same {
                        skipped += 1;
                        if is_temporal(cell_value) {
                            dates_skipped += 1;
                        }
                    } else {
                        kept.push((field.clone().unwrap_or_default(), cell_value.clone()));
                        written.push((cell.clone(), cell_value.clone()));
                    }
                }
                if !kept.is_empty() {
                    update.push((port_id.clone(), WireValue::Dict(OrderedMap(kept))));
                }
            } else if unchanged.iter().all(|same| *same) {
                skipped += cells.len() as u64;
                dates_skipped += cells.iter().filter(|(_, _, cell_value)| is_temporal(cell_value)).count() as u64;
            } else {
                update.push((port_id.clone(), value.clone()));
                written.extend(cells.into_iter().map(|(cell, _, cell_value)| (cell, cell_value)));
            }
        }
        if !update.is_empty() {
            self.write_native(workbook, &update)?;
        }
        let readback = Self::read_back_dates(workbook, spec, &written);
        for (cell, value) in written {
            let token = written_token(&value);
            record.forget(&cell);
            if let Some(token) = token {
                if let Some(key) = readback.get(&cell) {
                    record.readback.insert(cell.clone(), key.clone());
                }
                record.cells.insert(cell.clone(), token);
            }
            if formula_cells.contains(&cell) {
                record.overridden.insert(cell);
            }
        }
        if let Some(stats) = self.write_stats.as_mut() {
            stats.writes_skipped += skipped;
            stats.date_writes_skipped += dates_skipped;
        }
        Ok(())
    }

    /// `_read_back_dates(written)`: cell -> stored literal right after a date write.
    fn read_back_dates(workbook: &Workbook, spec: &ModelSpec, written: &[(CellKey, WireValue)]) -> HashMap<CellKey, StoredKey> {
        let mut wanted: Vec<&CellKey> =
            written.iter().filter(|(_, value)| is_temporal(value)).map(|(cell, _)| cell).collect();
        let mut result = HashMap::new();
        if wanted.is_empty() {
            return result;
        }
        for (_, location) in spec.inputs.iter() {
            let range = &location.range;
            let (inside, rest): (Vec<&CellKey>, Vec<&CellKey>) = wanted.into_iter().partition(|cell| {
                cell.0 == range.sheet
                    && (range.start_row..=range.end_row).contains(&cell.1)
                    && (range.start_col..=range.end_col).contains(&cell.2)
            });
            wanted = rest;
            if inside.is_empty() {
                continue;
            }
            let Ok(grid) = read_typed_matrix(workbook, range) else { continue };
            for cell in inside {
                let key = grid
                    .get((cell.1 - range.start_row) as usize)
                    .and_then(|row| row.get((cell.2 - range.start_col) as usize))
                    .and_then(stored_key);
                if let Some(key) = key {
                    result.insert(cell.clone(), key);
                }
            }
        }
        result
    }

    fn restore_all_formula_defaults(&self, workbook: &mut Workbook, spec: &ModelSpec) -> Result<(), PortError> {
        for ((sheet, row, col), formula) in Self::formula_default_entries(spec) {
            workbook
                .set_formula(&sheet, row, col, &formula)
                .map_err(|error| PortError::Other { kind: "RuntimeError".into(), message: error.to_string() })?;
        }
        Ok(())
    }

    /// `self.native.write_inputs(admitted)` (`py_to_input_update`).
    fn write_native(&mut self, workbook: &mut Workbook, admitted: &[(String, WireValue)]) -> Result<(), PortError> {
        let mut update = InputUpdate::default();
        for (port_id, value) in admitted {
            let binding = self
                .bindings
                .get(port_id)
                .ok_or_else(|| PortError::Type(format!("unknown port id `{port_id}`")))?;
            update.insert(port_id.clone(), port_value(&binding.kind, value)?);
        }
        let mut sheetport = SheetPort::from_bindings(workbook, self.bindings.clone())?;
        sheetport.write_inputs(update)?;
        let (_, bindings) = sheetport.into_parts();
        self.bindings = bindings;
        Ok(())
    }

    fn snapshot_inputs(&mut self, workbook: &mut Workbook) -> Result<BTreeMap<String, SheetValue>, PortError> {
        let mut sheetport = SheetPort::from_bindings(workbook, self.bindings.clone())?;
        let values = sheetport.read_inputs()?;
        let (_, bindings) = sheetport.into_parts();
        self.bindings = bindings;
        Ok(values.into_inner())
    }

    fn snapshot_outputs(&mut self, workbook: &mut Workbook) -> Result<BTreeMap<String, SheetValue>, PortError> {
        let mut sheetport = SheetPort::from_bindings(workbook, self.bindings.clone())?;
        let values = sheetport.read_outputs()?;
        let (_, bindings) = sheetport.into_parts();
        self.bindings = bindings;
        Ok(values.into_inner())
    }

    /// `read_inputs()`: declared key -> value.
    pub fn read_inputs(&mut self, workbook: &mut Workbook, spec: &ModelSpec) -> Result<OrderedMap<PortValue>, PortError> {
        let values = self.snapshot_inputs(workbook)?;
        let mut result = Vec::with_capacity(spec.inputs.len());
        for (_, location) in spec.inputs.iter() {
            let value = values.get(&location.port_id).ok_or_else(|| PortError::key(&location.port_id))?;
            result.push((location.key.clone(), receipt_value(value)));
        }
        Ok(OrderedMap(result))
    }

    /// `read_outputs(trim_trailing_null_rows=...)`: the client projection.
    pub fn read_outputs(
        &mut self,
        workbook: &mut Workbook,
        spec: &ModelSpec,
        trim_trailing_null_rows: bool,
    ) -> Result<OrderedMap<PortValue>, PortError> {
        let values = self.snapshot_outputs(workbook)?;
        client_outputs(spec, &|port_id| values.get(port_id).map(receipt_value), trim_trailing_null_rows)
    }

    /// `read_typed_outputs()`: records as their row matrix.
    pub fn read_typed_outputs(&mut self, workbook: &mut Workbook, spec: &ModelSpec) -> Result<OrderedMap<PortValue>, PortError> {
        let values = self.snapshot_outputs(workbook)?;
        typed_outputs(spec, &|port_id| values.get(port_id).map(receipt_value))
    }
}

/// A port value source: port id -> the value SheetPort would read.
type PortValues<'a> = dyn Fn(&str) -> Option<PortValue> + 'a;

/// The client projection of every declared output (`read_outputs`): date
/// contracts, then `_project_output`.
fn client_outputs(spec: &ModelSpec, values: &PortValues<'_>, trim: bool) -> Result<OrderedMap<PortValue>, PortError> {
    let contracts = spec.descriptor.get("output_wire_formats");
    let mut result = Vec::with_capacity(spec.outputs.len());
    for (key, location) in spec.outputs.iter() {
        let mut value = values(&location.port_id).ok_or_else(|| PortError::key(&location.port_id))?;
        let contract = contracts.and_then(|contracts| contracts.get(key)).filter(|contract| is_truthy(contract));
        if let Some(contract) = contract {
            value = apply_date_contract(value, location, contract)?;
        }
        result.push((location.key.clone(), project_output(value, location, trim)?));
    }
    Ok(OrderedMap(result))
}

/// Every declared output typed (`read_typed_outputs`): records as row matrices.
fn typed_outputs(spec: &ModelSpec, values: &PortValues<'_>) -> Result<OrderedMap<PortValue>, PortError> {
    let mut result = Vec::with_capacity(spec.outputs.len());
    for (_, location) in spec.outputs.iter() {
        let value = values(&location.port_id).ok_or_else(|| PortError::key(&location.port_id))?;
        let value = if location.shape == "record" { record_rows(&value, &location.range)? } else { value };
        result.push((location.key.clone(), value));
    }
    Ok(OrderedMap(result))
}

/// `project_outputs(values)`: the engine path's two output reads
/// (`read_typed_outputs`, `read_outputs`) over a port id -> value map instead
/// of a workbook. Returns `(typed_outputs, outputs)`.
pub fn project_outputs(
    spec: &ModelSpec,
    values: &BTreeMap<String, PortValue>,
    trim_trailing_null_rows: bool,
) -> Result<(OrderedMap<PortValue>, OrderedMap<PortValue>), PortError> {
    let lookup = |port_id: &str| values.get(port_id).cloned();
    let typed = typed_outputs(spec, &lookup)?;
    let outputs = client_outputs(spec, &lookup, trim_trailing_null_rows)?;
    Ok((typed, outputs))
}

/// `project_inputs(values)`: `read_inputs` over a port id -> value map.
pub fn project_inputs(spec: &ModelSpec, values: &BTreeMap<String, PortValue>) -> Result<OrderedMap<PortValue>, PortError> {
    let mut result = Vec::with_capacity(spec.inputs.len());
    for (_, location) in spec.inputs.iter() {
        let value = values.get(&location.port_id).ok_or_else(|| PortError::key(&location.port_id))?;
        result.push((location.key.clone(), value.clone()));
    }
    Ok(OrderedMap(result))
}

/// What is known about a compiled cell's temporal type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemporalHint {
    /// The cell has a date number format (`date_fields`), class unknown.
    DateCell,
    /// A `date` was written to the cell (the engine formats it `DATE`).
    Date,
    /// A `datetime` was written to the cell (the engine formats it `DATETIME`).
    DateTime,
}

/// A compiled cell's value as the engine's `get_value` would type it, given
/// what is known of the cell's format (architecture B typing rule,
/// `docs/modelcall_contract.md` "Compiled parent"). Only a Number changes:
/// `Date` -> the engine's `try_serial_to_date_for(1900, serial)`, `DateTime`
/// -> `try_serial_to_datetime_for(1900, serial)`, `DateCell` -> Date for a
/// whole serial and DateTime otherwise (the format's class, Date / DateTime /
/// Time, is not in the spec; a time-only or datetime format holding a whole
/// serial is typed Date here and DateTime/Time by the engine). A serial the
/// engine cannot convert stays a Number, as in the engine.
pub fn engine_temporal(value: LiteralValue, hint: TemporalHint) -> LiteralValue {
    let LiteralValue::Number(serial) = value else { return value };
    let system = formualizer_common::DateSystem::Excel1900;
    let as_date = || formualizer_common::try_serial_to_date_for(system, serial).map(LiteralValue::Date);
    let as_datetime = || formualizer_common::try_serial_to_datetime_for(system, serial).map(LiteralValue::DateTime);
    let typed = match hint {
        TemporalHint::Date => as_date(),
        TemporalHint::DateTime => as_datetime(),
        TemporalHint::DateCell if serial.fract() == 0.0 => as_date(),
        TemporalHint::DateCell => as_datetime(),
    };
    typed.unwrap_or(LiteralValue::Number(serial))
}

/// The declared record field names of a port in the manifest (`schema.fields`).
fn record_field_names(spec: &ModelSpec, port_id: &str) -> Option<Vec<String>> {
    let ports = spec.manifest.get("ports")?.as_array()?;
    let port = ports.iter().find(|port| port.get("id").and_then(Value::as_str) == Some(port_id))?;
    let fields = port.get("schema")?.get("fields")?.as_object()?;
    Some(fields.keys().cloned().collect())
}

/// Cells of one port rectangle the admission wrote, as `(row, col)` offsets,
/// each with the temporal hint of the written value (`None`: not a date).
pub type WrittenCells = HashMap<(usize, usize), Option<TemporalHint>>;

/// The cells a write of `value` to `location` sets, with their hints
/// (`PortSession::write_changed`'s `port_cells`). A value `port_cells` cannot
/// split is written whole by SheetPort: every cell of the rectangle, and
/// `None` (decline) when it holds a temporal. A written `time` is `None`: the
/// engine types it `Time`, which the compiled typing does not produce.
pub fn written_cells(location: &PortLocation, value: &WireValue) -> Option<WrittenCells> {
    let range = &location.range;
    let Some(cells) = port_cells(location, &location.shape, value) else {
        if contains_temporal(value) {
            return None;
        }
        return Some(
            (0..range.rows() as usize).flat_map(|r| (0..range.cols() as usize).map(move |c| ((r, c), None))).collect(),
        );
    };
    let mut written = WrittenCells::with_capacity(cells.len());
    for ((_, row, col), _, cell) in cells {
        let hint = match cell {
            WireValue::Date(_) => Some(TemporalHint::Date),
            WireValue::DateTime(_) => Some(TemporalHint::DateTime),
            WireValue::Time(_) => return None,
            _ => None,
        };
        written.insert(((row - range.start_row) as usize, (col - range.start_col) as usize), hint);
    }
    Some(written)
}

fn contains_temporal(value: &WireValue) -> bool {
    match value {
        WireValue::Date(_) | WireValue::DateTime(_) | WireValue::Time(_) => true,
        WireValue::List(items) => items.iter().any(contains_temporal),
        WireValue::Dict(fields) => fields.0.iter().any(|(_, item)| contains_temporal(item)),
        _ => false,
    }
}

/// `(row, col)` offsets of one class list of `engine_temporal_fields`.
fn temporal_offsets(fields: &Value, class: &str) -> Option<Vec<(usize, usize)>> {
    let Some(list) = fields.get(class) else { return Some(Vec::new()) };
    list.as_array()?
        .iter()
        .map(|cell| {
            let cell = cell.as_array()?;
            Some((usize::try_from(cell.first()?.as_u64()?).ok()?, usize::try_from(cell.get(1)?.as_u64()?).ok()?))
        })
        .collect()
}

/// Type a compiled grid's serials as the engine's temporal egress would.
/// Written cells take the written value's kind. The other cells follow the
/// location's `engine_temporal_fields` (`engine_temporal.py`: the engine's
/// format class per cell, a formula cell's derived format and not its style):
/// `date` / `datetime` type a Number, and a Number in an `unknown` cell
/// declines (`None`): the class there depends on values. A location without
/// that field (an older package) keeps the style rule: `date_fields` ->
/// [`TemporalHint::DateCell`].
fn type_temporal(location: &PortLocation, grid: &mut ChildMatrix, written: &WrittenCells) -> Option<()> {
    let apply = |grid: &mut ChildMatrix, (r, c): (usize, usize), hint: TemporalHint| {
        if let Some(cell) = grid.get_mut(r).and_then(|row| row.get_mut(c)) {
            *cell = engine_temporal(std::mem::replace(cell, LiteralValue::Empty), hint);
        }
    };
    for (&cell, hint) in written {
        if let Some(hint) = hint {
            apply(grid, cell, *hint);
        }
    }
    let Some(fields) = location.extra.get("engine_temporal_fields") else {
        for &(r, c) in &location.date_fields {
            let cell = (r as usize, c as usize);
            if !written.contains_key(&cell) {
                apply(grid, cell, TemporalHint::DateCell);
            }
        }
        return Some(());
    };
    for cell in temporal_offsets(fields, "unknown")? {
        let number = matches!(grid.get(cell.0).and_then(|row| row.get(cell.1)), Some(LiteralValue::Number(_)));
        if number && !written.contains_key(&cell) {
            return None;
        }
    }
    for (class, hint) in [("date", TemporalHint::Date), ("datetime", TemporalHint::DateTime)] {
        for cell in temporal_offsets(fields, class)? {
            if !written.contains_key(&cell) {
                apply(grid, cell, hint);
            }
        }
    }
    Some(())
}

/// A port's cells (read from a store that is not a workbook, e.g. a compiled
/// run) as the value SheetPort reads for that port: a scalar is its one
/// cell, a record its declared `r{r}_c{c}` fields (in the name order SheetPort
/// returns them), a range its rows. `grid` is the port's rectangle,
/// row-major. A table port, or a record field that is not an `r{r}_c{c}`
/// offset inside the rectangle, is `None` (the caller declines).
///
/// Temporal typing (the compiled store holds serials, the engine's
/// `get_value` types a cell through its temporal egress): see
/// `type_temporal`; `written` are the cells the admission wrote.
pub fn port_value_from_grid(spec: &ModelSpec, location: &PortLocation, grid: &ChildMatrix) -> Option<PortValue> {
    port_value_from_written_grid(spec, location, grid, &WrittenCells::new())
}

/// [`port_value_from_grid`] for an input rectangle the admission wrote.
pub fn port_value_from_written_grid(
    spec: &ModelSpec,
    location: &PortLocation,
    grid: &ChildMatrix,
    written: &WrittenCells,
) -> Option<PortValue> {
    let range = &location.range;
    if grid.len() != range.rows() as usize || grid.iter().any(|row| row.len() != range.cols() as usize) {
        return None;
    }
    let mut grid = grid.clone();
    type_temporal(location, &mut grid, written)?;
    let cell = |r: usize, c: usize| grid.get(r).and_then(|row| row.get(c)).cloned();
    match location.shape.as_str() {
        "scalar" => cell(0, 0).map(PortValue::Scalar),
        "range" => Some(PortValue::Range(grid.clone())),
        "record" => {
            let mut fields = BTreeMap::new();
            for field in record_field_names(spec, &location.port_id)? {
                let (row, col) = field.strip_prefix('r')?.split_once("_c")?;
                let (row, col) = (py_int(row)?, py_int(col)?);
                let (row, col) = (usize::try_from(row).ok()?, usize::try_from(col).ok()?);
                fields.insert(field, cell(row, col)?);
            }
            Some(PortValue::Record(OrderedMap(fields.into_iter().collect())))
        }
        _ => None,
    }
}

fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Object(map) => !map.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::String(text) => !text.is_empty(),
        Value::Number(number) => number.as_f64() != Some(0.0),
    }
}

/// `rows = value; if rows and not isinstance(rows[0], (list, tuple)): rows = [[v] for v in rows]`.
fn as_rows(value: &WireValue) -> Result<Vec<WireValue>, PortError> {
    match value {
        WireValue::List(items) => {
            if items.first().is_some_and(|first| !matches!(first, WireValue::List(_))) {
                Ok(items.iter().map(|item| WireValue::List(vec![item.clone()])).collect())
            } else {
                Ok(items.clone())
            }
        }
        other => Err(PortError::Type(format!("'{}' object is not subscriptable", other.type_name()))),
    }
}

/// `py_to_port_value(binding, value)`.
fn port_value(kind: &BoundPort, value: &WireValue) -> Result<SheetValue, PortError> {
    if matches!(value, WireValue::None) {
        return Ok(match kind {
            BoundPort::Scalar(_) => SheetValue::Scalar(LiteralValue::Empty),
            BoundPort::Record(_) => SheetValue::Record(BTreeMap::new()),
            BoundPort::Range(_) => SheetValue::Range(Vec::new()),
            BoundPort::Table(_) => SheetValue::Table(TableValue::default()),
        });
    }
    match kind {
        BoundPort::Scalar(_) => Ok(SheetValue::Scalar(value.to_literal()?)),
        BoundPort::Record(record) => {
            let WireValue::Dict(cells) = value else {
                return Err(PortError::Type("record inputs must be dictionaries".into()));
            };
            let mut map = BTreeMap::new();
            for (field, cell) in cells.iter() {
                if !record.fields.contains_key(field) {
                    return Err(PortError::Type(format!("record update includes unknown field `{field}`")));
                }
                map.insert(field.to_owned(), cell.to_literal()?);
            }
            Ok(SheetValue::Record(map))
        }
        BoundPort::Range(_) => {
            let WireValue::List(rows) = value else {
                return Err(PortError::Type("range inputs must be an iterable of rows".into()));
            };
            let mut converted: Vec<Vec<LiteralValue>> = Vec::with_capacity(rows.len());
            let mut width: Option<usize> = None;
            for (index, row) in rows.iter().enumerate() {
                let WireValue::List(cells) = row else {
                    return Err(PortError::Type(format!("range row {} must be iterable", index + 1)));
                };
                let cells = cells.iter().map(WireValue::to_literal).collect::<Result<Vec<_>, _>>()?;
                match width {
                    Some(expected) if expected != cells.len() => {
                        return Err(PortError::Type(format!(
                            "range rows must be rectangular (row {} has {}, expected {})",
                            index + 1,
                            cells.len(),
                            expected
                        )));
                    }
                    Some(_) => {}
                    None => width = Some(cells.len()),
                }
                converted.push(cells);
            }
            Ok(SheetValue::Range(converted))
        }
        BoundPort::Table(_) => Err(PortError::Type("table inputs must be an iterable of row mappings".into())),
    }
}

/// A SheetPort value as the receipt holds it.
fn receipt_value(value: &SheetValue) -> PortValue {
    match value {
        SheetValue::Scalar(value) => PortValue::Scalar(value.clone()),
        SheetValue::Record(fields) => {
            PortValue::Record(OrderedMap(fields.iter().map(|(key, value)| (key.clone(), value.clone())).collect()))
        }
        SheetValue::Range(rows) => PortValue::Range(rows.clone()),
        SheetValue::Table(table) => PortValue::Table(
            table
                .rows
                .iter()
                .map(|row| OrderedMap(row.values.iter().map(|(key, value)| (key.clone(), value.clone())).collect()))
                .collect(),
        ),
    }
}

/// A record's fields as the rows of its rectangle.
fn record_rows(value: &PortValue, range: &CellRange) -> Result<PortValue, PortError> {
    let PortValue::Record(fields) = value else { return Ok(value.clone()) };
    let mut rows = Vec::with_capacity(range.rows() as usize);
    for r in 0..range.rows() {
        let mut row = Vec::with_capacity(range.cols() as usize);
        for c in 0..range.cols() {
            let field = format!("r{r}_c{c}");
            row.push(fields.get(&field).cloned().ok_or_else(|| PortError::key(&field))?);
        }
        rows.push(row);
    }
    Ok(PortValue::Range(rows))
}

fn apply_date_contract(value: PortValue, location: &PortLocation, contract: &Value) -> Result<PortValue, PortError> {
    let cells: Vec<(usize, usize)> = contract
        .get("cells")
        .and_then(Value::as_array)
        .map(|cells| {
            cells
                .iter()
                .filter_map(|cell| {
                    let cell = cell.as_array()?;
                    Some((usize::try_from(cell.first()?.as_u64()?).ok()?, usize::try_from(cell.get(1)?.as_u64()?).ok()?))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(match (location.shape.as_str(), value) {
        ("scalar", PortValue::Scalar(value)) => PortValue::Scalar(project_date(&value, contract)?),
        ("record", PortValue::Record(mut fields)) => {
            for (row, col) in cells {
                let field = format!("r{row}_c{col}");
                let slot = fields.0.iter_mut().find(|(name, _)| *name == field).ok_or_else(|| PortError::key(&field))?;
                slot.1 = project_date(&slot.1, contract)?;
            }
            PortValue::Record(fields)
        }
        (_, PortValue::Range(mut rows)) => {
            for (row, col) in cells {
                let cell = rows
                    .get_mut(row)
                    .and_then(|cells| cells.get_mut(col))
                    .ok_or_else(|| PortError::Other { kind: "IndexError".into(), message: "list index out of range".into() })?;
                *cell = project_date(cell, contract)?;
            }
            PortValue::Range(rows)
        }
        (_, other) => other,
    })
}

/// `output_dates.project_date(value, contract)`.
pub fn project_date(value: &LiteralValue, contract: &Value) -> Result<LiteralValue, PortError> {
    let number = match value {
        LiteralValue::Int(number) => *number as f64,
        LiteralValue::Number(number) => *number,
        _ => return Ok(value.clone()),
    };
    let fail = |message: &str| Err(PortError::Value(message.to_owned()));
    if !number.is_finite() {
        return fail("Nonfinite output date serial");
    }
    let sentinels = contract.get("sentinels").and_then(Value::as_array);
    if sentinels.is_some_and(|sentinels| sentinels.iter().filter_map(Value::as_f64).any(|s| s == number)) {
        return Ok(value.clone());
    }
    let policy = |name: &str| {
        contract.get("edge_policy").and_then(|policy| policy.get(name)).and_then(Value::as_str).unwrap_or_default()
    };
    if number < 0.0 {
        if policy("negative") == "preserve" {
            return Ok(value.clone());
        }
        return fail("Negative output date serial is not admitted");
    }
    let whole = number.floor();
    if number != whole && policy("fractional") == "reject" {
        return fail("Fractional output date serial is not admitted");
    }
    if number == 0.0 {
        match policy("zero") {
            "preserve" => return Ok(value.clone()),
            "reject" => return fail("Zero output date serial is not admitted"),
            _ => {}
        }
    }
    let whole = integral_i64(whole.min(1e9));
    let (origin, offset) = match contract.get("date_system").and_then(Value::as_i64) {
        Some(1900) => {
            if whole == 60 {
                if policy("serial60") == "preserve" {
                    return Ok(value.clone());
                }
                return fail("Excel fictional 1900 leap day has no ISO date representation");
            }
            if whole == 0 {
                return fail("1900 serial zero requires preserve or reject policy");
            }
            (NaiveDate::from_ymd_opt(1899, 12, 31), whole - i64::from(whole > 60))
        }
        Some(1904) => (NaiveDate::from_ymd_opt(1904, 1, 1), whole),
        _ => return fail("Output date contract has an unsupported source epoch"),
    };
    let day = origin
        .and_then(|origin| origin.checked_add_signed(Duration::try_days(offset)?))
        .filter(|day| (1..=9999).contains(&day.year()));
    match day {
        Some(day) => Ok(LiteralValue::Text(day.format("%Y-%m-%d").to_string())),
        None => fail("Output date serial exceeds ISO calendar bounds"),
    }
}

/// openpyxl `get_column_letter`.
pub fn column_letter(mut col: u32) -> String {
    let mut letters = Vec::new();
    while col > 0 {
        let rem = (col - 1) % 26;
        letters.push(char::from(b'A' + u8::try_from(rem).unwrap_or(0)));
        col = (col - 1) / 26;
    }
    letters.iter().rev().collect()
}

/// Python `format(value, '.15g')`.
pub fn format_g15(value: f64) -> String {
    if value.is_nan() {
        return "nan".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if value == 0.0 {
        return if value.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    let scientific = format!("{value:.14e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    if (-4..15).contains(&exponent) {
        let decimals = usize::try_from(14 - exponent).unwrap_or(0);
        let fixed = format!("{value:.decimals$}");
        strip_fraction(&fixed)
    } else {
        let mantissa = strip_fraction(mantissa);
        let sign = if exponent < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exponent.abs())
    }
}

fn strip_fraction(text: &str) -> String {
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        text.to_owned()
    }
}

/// Python `str(datetime)`.
fn python_datetime_str(stamp: &NaiveDateTime) -> String {
    let micro = stamp.nanosecond() / 1_000;
    if micro == 0 {
        stamp.format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
        format!("{}.{micro:06}", stamp.format("%Y-%m-%d %H:%M:%S"))
    }
}

/// `PortSession._header(cell, col)`.
fn header(cell: &LiteralValue, col: u32) -> Result<String, PortError> {
    Ok(match cell {
        LiteralValue::Empty => format!("Column {}", column_letter(col)),
        LiteralValue::Boolean(flag) => (if *flag { "TRUE" } else { "FALSE" }).to_owned(),
        LiteralValue::Number(number) => {
            if number.is_finite() && number.fract() == 0.0 {
                let text = format!("{number:.0}");
                if text == "-0" { "0".to_owned() } else { text }
            } else {
                format_g15(*number)
            }
        }
        LiteralValue::Int(number) => number.to_string(),
        LiteralValue::Text(text) => text.clone(),
        LiteralValue::Date(day) => day.format("%Y-%m-%d").to_string(),
        LiteralValue::DateTime(stamp) => python_datetime_str(stamp),
        _ => {
            return Err(PortError::Value(
                "A client table header must be a scalar label, not an error or composite value".into(),
            ));
        }
    })
}

/// `PortSession._project_output(value, loc, trim_trailing_null_rows)`.
fn project_output(value: PortValue, location: &PortLocation, trim: bool) -> Result<PortValue, PortError> {
    if location.shape == "scalar" {
        return Ok(value);
    }
    let value = if location.shape == "record" { record_rows(&value, &location.range)? } else { value };
    let PortValue::Range(mut rows) = value else { return Ok(value) };
    if rows.len() == 1 {
        return Ok(PortValue::Row(rows.remove(0)));
    }
    let Some(first) = rows.first() else {
        return Err(PortError::Other { kind: "IndexError".into(), message: "list index out of range".into() });
    };
    let headers = first
        .iter()
        .enumerate()
        .map(|(c, cell)| header(cell, location.range.start_col + u32::try_from(c).unwrap_or(0)))
        .collect::<Result<Vec<_>, _>>()?;
    let mut data: Vec<Vec<LiteralValue>> = rows.drain(1..).collect();
    while trim && data.last().is_some_and(|row| row.iter().all(|cell| matches!(cell, LiteralValue::Empty))) {
        data.pop();
    }
    let table = data
        .into_iter()
        .map(|row| {
            let mut record: Vec<(String, LiteralValue)> = Vec::with_capacity(headers.len());
            for (name, cell) in headers.iter().zip(row) {
                match record.iter_mut().find(|(existing, _)| existing == name) {
                    Some(slot) => slot.1 = cell,
                    None => record.push((name.clone(), cell)),
                }
            }
            OrderedMap(record)
        })
        .collect();
    Ok(PortValue::Table(table))
}

/// `read_typed_matrix(workbook, location)`: native literals, no evaluation.
pub fn read_typed_matrix(workbook: &Workbook, range: &CellRange) -> Result<ChildMatrix, PortError> {
    if workbook.engine().sheet_id(&range.sheet).is_none() {
        return Err(PortError::Value(format!("Unknown sheet: {}", range.sheet)));
    }
    Ok((range.start_row..=range.end_row)
        .map(|row| {
            (range.start_col..=range.end_col)
                .map(|col| workbook.engine().get_typed_cell_value(&range.sheet, row, col).unwrap_or(LiteralValue::Empty))
                .collect()
        })
        .collect())
}

/// `native_matrix(matrix)`: a nonempty rectangle, else `ValueError`.
pub fn native_matrix(matrix: &ChildMatrix) -> Result<LiteralValue, ModelCallError> {
    let Some(first) = matrix.first() else {
        return Err(ModelCallError::infrastructure("ValueError", "Child result must be a nonempty rectangle"));
    };
    if first.is_empty() || matrix.iter().any(|row| row.len() != first.len()) {
        return Err(ModelCallError::infrastructure("ValueError", "Child result must be a nonempty rectangle"));
    }
    Ok(LiteralValue::Array(matrix.clone()))
}

/// The Python value `PortSession` hands back for a written input, as a
/// receipt value (effective inputs of a failed run).
pub fn wire_to_port_value(value: &WireValue) -> PortValue {
    match value {
        WireValue::Dict(fields) => PortValue::Record(OrderedMap(
            fields.iter().map(|(key, cell)| (key.to_owned(), cell.to_literal().unwrap_or(LiteralValue::Empty))).collect(),
        )),
        WireValue::List(rows) if rows.iter().all(|row| matches!(row, WireValue::List(_))) => PortValue::Range(
            rows.iter()
                .map(|row| match row {
                    WireValue::List(cells) => {
                        cells.iter().map(|cell| cell.to_literal().unwrap_or(LiteralValue::Empty)).collect()
                    }
                    _ => Vec::new(),
                })
                .collect(),
        ),
        other => PortValue::Scalar(other.to_literal().unwrap_or(LiteralValue::Empty)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_float_and_iso_parsers() {
        assert_eq!(py_float(" 1_000.5 "), Some(1000.5));
        assert_eq!(py_float("1__0"), None);
        assert_eq!(py_float("-Infinity"), Some(f64::NEG_INFINITY));
        assert!(py_float("abc").is_none());
        assert!(py_float("1e").is_none());
        assert_eq!(parse_iso_date("2024-02-29"), NaiveDate::from_ymd_opt(2024, 2, 29));
        assert_eq!(parse_iso_date("20240229"), NaiveDate::from_ymd_opt(2024, 2, 29));
        assert!(parse_iso_date("2023-02-29").is_none());
        let stamp = parse_iso_datetime("2024-01-02T03:04:05.5+01:00").unwrap();
        assert_eq!(stamp.to_string(), "2024-01-02 03:04:05.500");
        assert!(parse_iso_datetime("01/02/2024").is_none());
        assert_eq!(parse_iso_datetime("2024-01-02").unwrap().to_string(), "2024-01-02 00:00:00");
    }

    #[test]
    fn repr_and_formatting_match_python() {
        assert_eq!(py_repr("abc"), "'abc'");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(format_g15(0.1 + 0.2), "0.3");
        assert_eq!(format_g15(1.5e-7), "1.5e-07");
        assert_eq!(format_g15(1.234_567_890_123_456_7e20), "1.23456789012346e+20");
        assert_eq!(format_g15(2.5), "2.5");
        assert_eq!(column_letter(1), "A");
        assert_eq!(column_letter(28), "AB");
        assert_eq!(header(&LiteralValue::Number(3.0), 1).unwrap(), "3");
        assert_eq!(header(&LiteralValue::Empty, 3).unwrap(), "Column C");
    }

    #[test]
    fn cl105_number_text_binds_as_text_and_dates_decode() {
        assert_eq!(temporal(&WireValue::Str("12".into()), "number").unwrap(), WireValue::Float(12.0));
        assert!(temporal(&WireValue::Str("n/a".into()), "number").is_err());
        assert_eq!(
            temporal(&WireValue::Str("2024-03-01".into()), "date").unwrap(),
            WireValue::Date(NaiveDate::from_ymd_opt(2024, 3, 1).unwrap())
        );
        assert!(iso_date_prefix("2024-13-01"));
        assert!(!iso_date_prefix("March 2024"));
    }

    #[test]
    fn parent_port_literal_follows_the_lane_d_law() {
        let day = NaiveDate::from_ymd_opt(2024, 3, 1).unwrap();
        assert_eq!(parent_port_literal(&WireValue::Date(day)).unwrap(), LiteralValue::Number(45352.0));
        let noon = day.and_hms_micro_opt(12, 0, 0, 500_000).unwrap();
        let expected = 45352.0_f64 + (43_200.0 + 0.5) / 86_400.0;
        let LiteralValue::Number(serial) = parent_port_literal(&WireValue::DateTime(noon)).unwrap() else { panic!() };
        assert_eq!(serial.to_bits(), expected.to_bits());
        // Lane D counts from 1899-12-30 for every date (the engine skips the phantom 1900-02-29).
        let early = NaiveDate::from_ymd_opt(1900, 1, 1).unwrap();
        assert_eq!(parent_port_literal(&WireValue::Date(early)).unwrap(), LiteralValue::Number(2.0));
        let before = NaiveDate::from_ymd_opt(1899, 12, 29).unwrap().and_hms_opt(18, 0, 0).unwrap();
        assert_eq!(parent_port_literal(&WireValue::DateTime(before)).unwrap(), LiteralValue::Number(-1.0 + 0.75));
        assert_eq!(parent_port_literal(&WireValue::Int(7)).unwrap(), LiteralValue::Number(7.0));
        assert_eq!(parent_port_literal(&WireValue::None).unwrap(), LiteralValue::Empty);
        assert_eq!(parent_port_literal(&WireValue::Str("x".into())).unwrap(), LiteralValue::Text("x".into()));
        let rows = WireValue::List(vec![WireValue::List(vec![WireValue::Int(1), WireValue::None])]);
        assert_eq!(
            parent_port_literal(&rows).unwrap(),
            LiteralValue::Array(vec![vec![LiteralValue::Number(1.0), LiteralValue::Empty]])
        );
        assert!(parent_port_literal(&WireValue::List(vec![WireValue::Int(1)])).is_err());
        assert!(parent_port_literal(&WireValue::Dict(OrderedMap::default())).is_err());
    }

    #[test]
    fn type_temporal_follows_engine_temporal_fields_and_writes() {
        let mut location: PortLocation = serde_json::from_value(serde_json::json!({
            "sheet": "S", "start_row": 1, "start_col": 1, "end_row": 1, "end_col": 4, "name": "Xoutput_Row",
            "key": "Row", "port_id": "output_row", "shape": "range", "date_system": 1900,
            "date_fields": [[0, 0], [0, 1], [0, 2], [0, 3]],
            "engine_temporal_fields": {"date": [[0, 1]], "datetime": [[0, 2]], "unknown": [[0, 3]]}
        }))
        .unwrap();
        let day = NaiveDate::from_ymd_opt(2024, 3, 1).unwrap();
        let row = |last: LiteralValue| vec![vec![LiteralValue::Number(45352.0), LiteralValue::Number(45352.5), LiteralValue::Number(45352.0), last]];
        // Style alone (date_fields) types nothing once the engine classes are known.
        let mut grid = row(LiteralValue::Text("x".into()));
        type_temporal(&location, &mut grid, &WrittenCells::new()).unwrap();
        assert_eq!(grid[0][0], LiteralValue::Number(45352.0));
        assert_eq!(grid[0][1], LiteralValue::Date(day), "Date class truncates like try_serial_to_date_for");
        assert_eq!(grid[0][2], LiteralValue::DateTime(day.and_hms_opt(0, 0, 0).unwrap()));
        // A Number where the class depends on values: decline.
        assert!(type_temporal(&location, &mut row(LiteralValue::Number(1.0)), &WrittenCells::new()).is_none());
        // Written cells follow the write, whatever the workbook says.
        let written: WrittenCells = [((0, 1), None), ((0, 3), None), ((0, 0), Some(TemporalHint::Date))].into_iter().collect();
        let mut grid = row(LiteralValue::Number(1.0));
        type_temporal(&location, &mut grid, &written).unwrap();
        assert_eq!(grid[0][0], LiteralValue::Date(day));
        assert_eq!(grid[0][1], LiteralValue::Number(45352.5));
        assert_eq!(grid[0][3], LiteralValue::Number(1.0));
        // An older location: the style rule.
        location.extra.remove("engine_temporal_fields");
        let mut grid = row(LiteralValue::Number(1.0));
        type_temporal(&location, &mut grid, &WrittenCells::new()).unwrap();
        assert_eq!(grid[0][0], LiteralValue::Date(day));
        assert_eq!(grid[0][1], LiteralValue::DateTime(day.and_hms_opt(12, 0, 0).unwrap()));
    }

    #[test]
    fn written_cells_split_like_the_native_write() {
        let location: PortLocation = serde_json::from_value(serde_json::json!({
            "sheet": "S", "start_row": 2, "start_col": 3, "end_row": 3, "end_col": 4, "name": "Xinput_Grid",
            "key": "Grid", "port_id": "grid", "shape": "range", "date_system": 1900, "date_fields": []
        }))
        .unwrap();
        let day = NaiveDate::from_ymd_opt(2024, 3, 1).unwrap();
        let value = WireValue::List(vec![WireValue::List(vec![WireValue::Int(1), WireValue::Date(day)])]);
        let written = written_cells(&location, &value).unwrap();
        assert_eq!(written.len(), 2);
        assert_eq!(written.get(&(0, 0)), Some(&None));
        assert_eq!(written.get(&(0, 1)), Some(&Some(TemporalHint::Date)));
        // Not splittable (a row wider than the port): written whole; a date in it declines.
        let wide = |cell: WireValue| WireValue::List(vec![WireValue::List(vec![WireValue::Int(1), WireValue::Int(2), cell])]);
        assert_eq!(written_cells(&location, &wide(WireValue::Int(3))).unwrap().len(), 4);
        assert!(written_cells(&location, &wide(WireValue::Date(day))).is_none());
        let time = WireValue::List(vec![WireValue::List(vec![WireValue::Time(NaiveTime::MIN)])]);
        assert!(written_cells(&location, &time).is_none());
    }

    #[test]
    fn engine_temporal_types_serials_like_the_engine_egress() {
        let day = NaiveDate::from_ymd_opt(2024, 3, 1).unwrap();
        assert_eq!(engine_temporal(LiteralValue::Number(45352.0), TemporalHint::DateCell), LiteralValue::Date(day));
        assert_eq!(
            engine_temporal(LiteralValue::Number(45352.5), TemporalHint::DateCell),
            LiteralValue::DateTime(day.and_hms_opt(12, 0, 0).unwrap())
        );
        assert_eq!(
            engine_temporal(LiteralValue::Number(45352.0), TemporalHint::DateTime),
            LiteralValue::DateTime(day.and_hms_opt(0, 0, 0).unwrap())
        );
        assert_eq!(engine_temporal(LiteralValue::Text("x".into()), TemporalHint::Date), LiteralValue::Text("x".into()));
        assert_eq!(engine_temporal(LiteralValue::Number(-5.0), TemporalHint::Date), LiteralValue::Number(-5.0));
    }

    #[test]
    fn project_date_follows_contract() {
        let contract = serde_json::json!({
            "format": "excel-date", "date_system": 1900, "sentinels": [0],
            "edge_policy": {"zero": "preserve", "negative": "reject", "serial60": "reject", "fractional": "floor"},
            "cells": []
        });
        assert_eq!(project_date(&LiteralValue::Number(45292.5), &contract).unwrap(), LiteralValue::Text("2024-01-01".into()));
        assert_eq!(project_date(&LiteralValue::Int(0), &contract).unwrap(), LiteralValue::Int(0));
        assert!(project_date(&LiteralValue::Number(-1.0), &contract).is_err());
        assert!(project_date(&LiteralValue::Number(60.0), &contract).is_err());
        assert_eq!(project_date(&LiteralValue::Text("x".into()), &contract).unwrap(), LiteralValue::Text("x".into()));
    }
}
