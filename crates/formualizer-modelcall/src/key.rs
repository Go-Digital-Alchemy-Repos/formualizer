//! Call-argument normalisation and the memo key (port of `callbacks.py`
//! `callback_inputs`, `_memo_token`, `XcallRequestMemo._key`,
//! `matrix_is_memoisable`, `matrix_is_finished`).
//!
//! Equivalence with the Python spelling. `_memo_token` receives the Python
//! conversion of each engine value; the mapping here is one-to-one with it:
//!
//! | engine value | Python value | token |
//! |---|---|---|
//! | `Empty` | `None` | `None` |
//! | `Boolean` | `bool` | `Bool` |
//! | `Int` | `int` | `Int` |
//! | `Number` (not NaN) | `float` spelled by `repr` | `Float(to_bits)` |
//! | `Text` | `str` | `Str` |
//! | `DateTime` / `Date` / `Time` | naive `datetime` / `date` / `time`, `fold == 0` | same variant |
//! | `Array` | list of row lists | `List` of row `List`s |
//! | `Error`, `Pending`, `Duration`, NaN | refused | [`Unmemoisable`] |
//!
//! `repr` of a Python float is shortest-round-trip and keeps the sign of zero,
//! so two non-NaN floats have equal `repr` exactly when their bits are equal:
//! `Float(to_bits)` is the same equivalence. A naive datetime's `isoformat`
//! and `fold` (always 0 for engine values) identify it exactly, as the chrono
//! value does. Python tuples never arrive from the engine; the `('tuple', ..)`
//! tag has no Rust counterpart.

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use formualizer_common::LiteralValue;

use crate::ModelCallError;

/// Casefolded input name -> value, in first-seen order (a Python dict).
pub type InputPairs = Vec<(String, LiteralValue)>;

/// Python `str.casefold` for input names and output selectors.
///
/// Per-character lowercase plus the full-folding cases that differ from it
/// (`ß`/`ẞ` -> `ss`, final sigma -> `σ`). Other multi-character folds (rare
/// ligatures) are not reproduced; published names are ASCII.
pub fn casefold(text: &str) -> String {
    let mut folded = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            'ß' | 'ẞ' => folded.push_str("ss"),
            'ς' => folded.push('σ'),
            _ => folded.extend(ch.to_lowercase()),
        }
    }
    folded
}

fn is_blank(value: &LiteralValue) -> bool {
    matches!(value, LiteralValue::Empty) || matches!(value, LiteralValue::Text(text) if text.is_empty())
}

/// `callback_inputs(block, tail)`: the call's name/value pairs.
///
/// `block` is absent (`Empty`, or a numeric zero) or a two-column array of
/// name/value rows; a row with a blank name must have a blank value and is
/// skipped. `tail` is alternating names and values. Names are text and are
/// casefolded; a duplicate within the block or within the tail is refused,
/// and a tail name overrides the same block name in place. Every refusal is a
/// routing error with the Python message.
pub fn callback_inputs(block: &LiteralValue, tail: &[LiteralValue]) -> Result<InputPairs, ModelCallError> {
    let mut pairs: InputPairs = Vec::new();
    let absent = match block {
        LiteralValue::Empty => true,
        LiteralValue::Int(value) => *value == 0,
        // Python `block == 0` on a float: exact, and true for -0.0.
        LiteralValue::Number(value) => *value == 0.0,
        _ => false,
    };
    if !absent {
        let LiteralValue::Array(rows) = block else {
            return Err(ModelCallError::routing("child input block must have two columns"));
        };
        if rows.iter().any(|row| row.len() != 2) {
            return Err(ModelCallError::routing("child input block must have two columns"));
        }
        for row in rows {
            let (name, value) = (&row[0], &row[1]);
            if is_blank(name) {
                if !is_blank(value) {
                    return Err(ModelCallError::routing("unnamed child input has a value"));
                }
                continue;
            }
            let LiteralValue::Text(name) = name else {
                return Err(ModelCallError::routing("duplicate or invalid child input name"));
            };
            let folded = casefold(name);
            if pairs.iter().any(|(existing, _)| *existing == folded) {
                return Err(ModelCallError::routing("duplicate or invalid child input name"));
            }
            pairs.push((folded, value.clone()));
        }
    }
    if !tail.len().is_multiple_of(2) {
        return Err(ModelCallError::routing("child tail must contain name/value pairs"));
    }
    let mut seen: Vec<String> = Vec::new();
    for pair in tail.chunks_exact(2) {
        let LiteralValue::Text(name) = &pair[0] else {
            return Err(ModelCallError::routing("duplicate or invalid child tail input name"));
        };
        let folded = casefold(name);
        if seen.contains(&folded) {
            return Err(ModelCallError::routing("duplicate or invalid child tail input name"));
        }
        seen.push(folded.clone());
        match pairs.iter_mut().find(|(existing, _)| *existing == folded) {
            Some(slot) => slot.1 = pair[1].clone(),
            None => pairs.push((folded, pair[1].clone())),
        }
    }
    Ok(pairs)
}

/// A value the memo cannot spell exactly; the call is not memoised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unmemoisable(pub &'static str);

/// A type-tagged, exact spelling of one call argument (`_memo_token`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MemoToken {
    None,
    Bool(bool),
    Int(i64),
    /// `f64::to_bits`; NaN is refused, `-0.0` and `0.0` differ.
    Float(u64),
    Str(String),
    DateTime(NaiveDateTime),
    Date(NaiveDate),
    Time(NaiveTime),
    List(Vec<MemoToken>),
}

/// `_memo_token(value)`.
pub fn memo_token(value: &LiteralValue) -> Result<MemoToken, Unmemoisable> {
    Ok(match value {
        LiteralValue::Empty => MemoToken::None,
        LiteralValue::Boolean(flag) => MemoToken::Bool(*flag),
        LiteralValue::Int(number) => MemoToken::Int(*number),
        LiteralValue::Number(number) => {
            if number.is_nan() {
                return Err(Unmemoisable("nan"));
            }
            MemoToken::Float(number.to_bits())
        }
        LiteralValue::Text(text) => MemoToken::Str(text.clone()),
        LiteralValue::DateTime(stamp) => MemoToken::DateTime(*stamp),
        LiteralValue::Date(day) => MemoToken::Date(*day),
        LiteralValue::Time(clock) => MemoToken::Time(*clock),
        LiteralValue::Array(rows) => MemoToken::List(
            rows.iter()
                .map(|row| row.iter().map(memo_token).collect::<Result<Vec<_>, _>>().map(MemoToken::List))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        LiteralValue::Duration(_) => return Err(Unmemoisable("timedelta")),
        LiteralValue::Pending => return Err(Unmemoisable("pending")),
        LiteralValue::Error(_) => return Err(Unmemoisable("error")),
    })
}

/// Which input vector a key carries; the two never compare equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum KeyForm {
    /// Every call name/value pair (GOD-379).
    Inputs,
    /// The child's canonical port updates, for an `ignore`-policy child
    /// (GOD-380, `child_port_updates`).
    Ports,
}

/// `XcallRequestMemo._key`: caller stack, child identity (`identity:sha256`),
/// output selector, form and the name-sorted, type-tagged input vector.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MemoKey {
    pub stack: Vec<String>,
    pub identity: String,
    pub output: MemoToken,
    pub form: KeyForm,
    pub vector: Vec<(String, MemoToken)>,
}

impl MemoKey {
    /// Build the key, or `Err` when any value cannot be spelled exactly.
    /// `pairs` is the call's inputs for [`KeyForm::Inputs`], or the child's
    /// port updates for [`KeyForm::Ports`] (computed by `ports`).
    pub fn new(
        stack: &[String],
        identity: &str,
        output: &LiteralValue,
        form: KeyForm,
        pairs: &[(String, LiteralValue)],
    ) -> Result<Self, Unmemoisable> {
        let mut vector = pairs
            .iter()
            .map(|(name, value)| memo_token(value).map(|token| (name.clone(), token)))
            .collect::<Result<Vec<_>, _>>()?;
        vector.sort();
        Ok(Self {
            stack: stack.to_vec(),
            identity: identity.to_owned(),
            output: memo_token(output)?,
            form,
            vector,
        })
    }
}

/// `_clean_cell` for an engine value: finished and not an error.
fn clean_cell(value: &LiteralValue) -> bool {
    !matches!(value, LiteralValue::Error(_) | LiteralValue::Pending | LiteralValue::Array(_))
}

/// `_finished_cell` for an engine value: not Pending and not an array.
fn finished_cell(value: &LiteralValue) -> bool {
    !matches!(value, LiteralValue::Pending | LiteralValue::Array(_))
}

fn rectangle_all(matrix: &[Vec<LiteralValue>], cell: fn(&LiteralValue) -> bool) -> bool {
    let Some(first) = matrix.first() else { return false };
    if first.is_empty() || matrix.iter().any(|row| row.len() != first.len()) {
        return false;
    }
    matrix.iter().flatten().all(cell)
}

/// A nonempty rectangle of clean cells, and nothing else, may be stored.
pub fn matrix_is_memoisable(matrix: &[Vec<LiteralValue>]) -> bool {
    rectangle_all(matrix, clean_cell)
}

/// A nonempty rectangle of clean cells or error values (prefetch opt-in).
pub fn matrix_is_finished(matrix: &[Vec<LiteralValue>]) -> bool {
    rectangle_all(matrix, finished_cell)
}

#[cfg(test)]
mod tests {
    use super::*;
    use formualizer_common::{ExcelError, ExcelErrorKind};

    const PARENT: &str = "parent:psha";

    fn text(value: &str) -> LiteralValue {
        LiteralValue::Text(value.into())
    }

    fn key_with(stack: &[&str], identity: &str, output: &str, inputs: &[(&str, LiteralValue)]) -> Option<MemoKey> {
        let stack: Vec<String> = stack.iter().map(|item| (*item).to_owned()).collect();
        let pairs: InputPairs = inputs.iter().map(|(name, value)| ((*name).to_owned(), value.clone())).collect();
        MemoKey::new(&stack, identity, &text(output), KeyForm::Inputs, &pairs).ok()
    }

    fn key(value: LiteralValue) -> Option<MemoKey> {
        key_with(&[PARENT], "child:csha", "result", &[("a", value)])
    }

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn array(rows: Vec<Vec<LiteralValue>>) -> LiteralValue {
        LiteralValue::Array(rows)
    }

    #[test]
    fn equal_vectors_share_a_key_regardless_of_order() {
        let one = key_with(&[PARENT], "c:s", "result", &[("a", LiteralValue::Int(1)), ("b", text("x"))]);
        let two = key_with(&[PARENT], "c:s", "result", &[("b", text("x")), ("a", LiteralValue::Int(1))]);
        assert!(one.is_some());
        assert_eq!(one, two);
    }

    #[test]
    fn key_is_type_tagged_and_exact() {
        use LiteralValue::*;
        assert_ne!(key(Boolean(true)), key(Int(1)));
        assert_ne!(key(Boolean(false)), key(Int(0)));
        assert_ne!(key(Int(1)), key(Number(1.0)));
        assert_ne!(key(Number(-0.0)), key(Number(0.0)));
        assert_ne!(key(Number(0.1 + 0.2)), key(Number(0.3)));
        assert_ne!(key(Empty), key(text("")));
        assert_ne!(key(text("")), key(Int(0)));
        let midnight = date(2026, 1, 1).and_hms_opt(0, 0, 0).unwrap();
        assert_ne!(key(DateTime(midnight)), key(Date(date(2026, 1, 1))));
        assert_ne!(key(DateTime(midnight)), key(DateTime(date(2026, 1, 2).and_hms_opt(0, 0, 0).unwrap())));
        assert_ne!(key(text("2026-01-01")), key(text("2026-01-02")));
        assert_ne!(key(text("2026-01-01")), key(Date(date(2026, 1, 1))));
        assert_eq!(key(Number(2.5)), key(Number(2.5)));
        let base = key_with(&[PARENT], "child:csha", "result", &[]);
        assert_ne!(base, key_with(&[PARENT], "child:csha", "other", &[]));
        assert_ne!(base, key_with(&[PARENT], "child:other", "result", &[]));
        assert_ne!(base, key_with(&["other:sha"], "child:csha", "result", &[]));
    }

    #[test]
    fn forms_never_compare_equal() {
        let pairs: InputPairs = vec![("a".into(), LiteralValue::Int(1))];
        let stack = vec![PARENT.to_owned()];
        let inputs = MemoKey::new(&stack, "c:s", &text("r"), KeyForm::Inputs, &pairs).unwrap();
        let ports = MemoKey::new(&stack, "c:s", &text("r"), KeyForm::Ports, &pairs).unwrap();
        assert_ne!(inputs, ports);
    }

    #[test]
    fn refuses_values_it_cannot_spell_exactly() {
        use LiteralValue::*;
        let refused = [
            Error(ExcelError::new(ExcelErrorKind::Value)),
            Pending,
            Number(f64::NAN),
            Duration(chrono::Duration::zero()),
            array(vec![vec![Int(1), Number(f64::NAN)]]),
            array(vec![vec![Int(1), Error(ExcelError::new(ExcelErrorKind::Na))]]),
        ];
        for value in refused {
            assert_eq!(key(value.clone()), None, "{value:?}");
        }
        // The output selector is spelled too.
        let stack = vec![PARENT.to_owned()];
        assert!(MemoKey::new(&stack, "c:s", &Number(f64::NAN), KeyForm::Inputs, &[]).is_err());
    }

    #[test]
    fn list_inputs_are_keyed_by_shape_type_and_every_element() {
        use LiteralValue::*;
        let nested = || array(vec![vec![Int(1), Number(2.5)], vec![text("x"), Empty]]);
        assert!(key(nested()).is_some());
        assert_eq!(key(nested()), key(nested()));
        assert_ne!(key(array(vec![vec![Int(1), Int(2)]])), key(array(vec![vec![Int(1), Int(3)]])));
        assert_ne!(key(array(vec![vec![Int(1), Int(2)]])), key(array(vec![vec![Int(1)], vec![Int(2)]])));
        assert_ne!(key(array(vec![vec![Int(1), Int(2)]])), key(array(vec![vec![Int(1), Number(2.0)]])));
        assert_ne!(key(array(vec![vec![Boolean(true)]])), key(array(vec![vec![Int(1)]])));
        assert_ne!(key(array(vec![vec![Int(1)]])), key(Int(1)));
    }

    #[test]
    fn callback_inputs_casefolds_and_merges_block_and_tail() {
        use LiteralValue::*;
        let block = array(vec![vec![text("Amount"), Int(1)], vec![Empty, text("")], vec![text("Rate"), Number(0.5)]]);
        let pairs = callback_inputs(&block, &[text("AMOUNT"), Int(2), text("Term"), Int(3)]).unwrap();
        assert_eq!(pairs, vec![("amount".into(), Int(2)), ("rate".into(), Number(0.5)), ("term".into(), Int(3))]);
        // The block and tail spellings of one input produce the same key.
        let from_block = callback_inputs(&array(vec![vec![text("Amount"), Int(1)]]), &[]).unwrap();
        let from_tail = callback_inputs(&Int(0), &[text("AMOUNT"), Int(1)]).unwrap();
        assert_eq!(from_block, from_tail);
        assert_eq!(callback_inputs(&Number(-0.0), &[]).unwrap(), vec![]);
        assert_eq!(callback_inputs(&Empty, &[]).unwrap(), vec![]);
    }

    #[test]
    fn callback_inputs_refusals_are_routing_errors() {
        use LiteralValue::*;
        let cases: Vec<(LiteralValue, Vec<LiteralValue>, &str)> = vec![
            (Int(1), vec![], "child input block must have two columns"),
            (Boolean(false), vec![], "child input block must have two columns"),
            (array(vec![vec![text("a")]]), vec![], "child input block must have two columns"),
            (array(vec![vec![Empty, Int(1)]]), vec![], "unnamed child input has a value"),
            (array(vec![vec![text("a"), Int(1)], vec![text("A"), Int(2)]]), vec![], "duplicate or invalid child input name"),
            (array(vec![vec![Int(3), Int(1)]]), vec![], "duplicate or invalid child input name"),
            (Empty, vec![text("a")], "child tail must contain name/value pairs"),
            (Empty, vec![text("a"), Int(1), text("A"), Int(2)], "duplicate or invalid child tail input name"),
            (Empty, vec![Int(1), Int(1)], "duplicate or invalid child tail input name"),
        ];
        for (block, tail, message) in cases {
            assert_eq!(callback_inputs(&block, &tail), Err(ModelCallError::routing(message)), "{block:?} {tail:?}");
        }
    }

    #[test]
    fn casefold_matches_python_for_published_spellings() {
        assert_eq!(casefold("IssueDate"), "issuedate");
        assert_eq!(casefold("Straße"), "strasse");
        assert_eq!(casefold("ΣΑΣ"), "σασ");
        assert_eq!(casefold("ς"), "σ");
    }

    #[test]
    fn matrix_checks_match_the_python_rules() {
        use LiteralValue::*;
        let error = || Error(ExcelError::new(ExcelErrorKind::Div));
        assert!(matrix_is_memoisable(&[vec![Int(1), Empty], vec![text("x"), Number(2.0)]]));
        assert!(!matrix_is_memoisable(&[]));
        assert!(!matrix_is_memoisable(&[vec![]]));
        assert!(!matrix_is_memoisable(&[vec![Int(1)], vec![Int(1), Int(2)]]));
        assert!(!matrix_is_memoisable(&[vec![error()]]));
        assert!(!matrix_is_memoisable(&[vec![Pending]]));
        assert!(matrix_is_finished(&[vec![error(), Int(1)]]));
        assert!(!matrix_is_finished(&[vec![Pending]]));
        assert!(!matrix_is_finished(&[vec![array(vec![vec![Int(1)]])]]));
    }
}
