//! Lane A: the per-request call memo (`callbacks.XcallRequestMemo`, renamed
//! `ModelCallMemo`).
//!
//! Within one request a call whose caller stack, child identity (which carries
//! the child's workbook sha256), output selector and normalised input vector
//! (for a child that ignores unknown inputs, the port updates it admits) are
//! identical to an earlier call that completed without an error value returns
//! that earlier call's typed matrix instead of re-evaluating the child
//! (GOD-379). The memo is owned by the request's session and never crosses a
//! request boundary.
//!
//! Counting (ported exactly): `key` counts `bypassed` when the key cannot be
//! spelled and `port_keyed` for a `KeyForm::Ports` key; `peek_key` counts
//! nothing; `lookup` counts `hits`/`misses`; `store` admits only
//! `matrix_is_memoisable` and otherwise counts `not_stored_error`; `adopt`
//! (prefetch) admits `matrix_is_memoisable`, or `matrix_is_finished` with
//! `allow_errors`, never overwrites and never counts; `not_stored` counts
//! `not_stored_error`; `report` is `None` unless hits+misses+bypassed > 0.

use formualizer_common::LiteralValue;
use std::collections::HashMap;

use crate::evaluator::ChildMatrix;
use crate::key::{KeyForm, MemoKey, matrix_is_finished, matrix_is_memoisable};
use crate::ports::child_port_updates;
use crate::receipt::MemoReport;
use crate::spec::ModelSpec;

/// A stored result: the index of the event that produced it and its matrix.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoEntry {
    pub source_index: usize,
    pub matrix: ChildMatrix,
}

/// Per-request memo; owned by one session, never shared across requests.
#[derive(Debug, Default)]
pub struct ModelCallMemo {
    entries: HashMap<MemoKey, MemoEntry>,
    hits: u64,
    misses: u64,
    stores: u64,
    bypassed: u64,
    not_stored_error: u64,
    port_keyed: u64,
}

/// `XcallRequestMemo._key`: `(key, port_keyed)`.
fn build_key(
    stack: &[String],
    identity: &str,
    output: &LiteralValue,
    inputs: &[(String, LiteralValue)],
    spec: Option<&ModelSpec>,
) -> (Option<MemoKey>, bool) {
    let ports = spec.and_then(|spec| child_port_updates(spec, inputs));
    let (form, pairs) = match &ports {
        Some(ports) => (KeyForm::Ports, ports.as_slice()),
        None => (KeyForm::Inputs, inputs),
    };
    match MemoKey::new(stack, identity, output, form, pairs) {
        Ok(key) => (Some(key), ports.is_some()),
        Err(_) => (None, false),
    }
}

impl ModelCallMemo {
    pub fn new() -> Self {
        Self::default()
    }

    /// The lookup key (counted), or `None` (counted as bypassed). `spec` is
    /// the resolved child: an `ignore`-policy child is keyed by its port
    /// updates (`ports::child_port_updates`).
    pub fn key(
        &mut self,
        stack: &[String],
        identity: &str,
        output: &LiteralValue,
        inputs: &[(String, LiteralValue)],
        spec: Option<&ModelSpec>,
    ) -> Option<MemoKey> {
        let (key, port_keyed) = build_key(stack, identity, output, inputs, spec);
        if key.is_none() {
            self.bypassed += 1;
        } else if port_keyed {
            self.port_keyed += 1;
        }
        key
    }

    /// `key` without counting (sibling prefetch).
    pub fn peek_key(
        &self,
        stack: &[String],
        identity: &str,
        output: &LiteralValue,
        inputs: &[(String, LiteralValue)],
        spec: Option<&ModelSpec>,
    ) -> Option<MemoKey> {
        build_key(stack, identity, output, inputs, spec).0
    }

    /// Whether `key` is stored, without counting.
    pub fn contains(&self, key: &MemoKey) -> bool {
        self.entries.contains_key(key)
    }

    /// A hit returns the stored entry (counted); `None` counts a miss.
    pub fn lookup(&mut self, key: &MemoKey) -> Option<MemoEntry> {
        let found = self.entries.get(key).cloned();
        if found.is_some() {
            self.hits += 1;
        } else {
            self.misses += 1;
        }
        found
    }

    /// Store an in-line result; true when admitted.
    pub fn store(&mut self, key: MemoKey, source_index: usize, matrix: &ChildMatrix) -> bool {
        if matrix_is_memoisable(matrix) {
            self.entries.insert(key, MemoEntry { source_index, matrix: matrix.clone() });
            self.stores += 1;
            return true;
        }
        self.not_stored_error += 1;
        false
    }

    /// Store a prefetched sibling's result under its own key; true when admitted.
    pub fn adopt(&mut self, key: MemoKey, source_index: usize, matrix: &ChildMatrix, allow_errors: bool) -> bool {
        let admissible = if allow_errors { matrix_is_finished(matrix) } else { matrix_is_memoisable(matrix) };
        if self.entries.contains_key(&key) || !admissible {
            return false;
        }
        self.entries.insert(key, MemoEntry { source_index, matrix: matrix.clone() });
        true
    }

    /// CL-109: at a declined compiled parent's hand-off only, adopt a call the
    /// attempt completed in line under its own key, errors admitted
    /// (`matrix_is_finished`), as a finished prefetch flight is. Same request,
    /// same key, deterministic child: the attempt's answer is the answer the
    /// engine parent would compute. Never overwrites and never counts; `store`
    /// keeps rejecting errors everywhere else.
    pub fn adopt_declined_attempt(&mut self, key: MemoKey, source_index: usize, matrix: &ChildMatrix) -> bool {
        self.adopt(key, source_index, matrix, true)
    }

    /// A missed call that failed instead of completing.
    pub fn not_stored(&mut self) {
        self.not_stored_error += 1;
    }

    /// Sealed counters; `None` when this request looked nothing up.
    pub fn report(&self) -> Option<MemoReport> {
        if self.hits == 0 && self.misses == 0 && self.bypassed == 0 {
            return None;
        }
        Some(MemoReport {
            enabled: true,
            hits: self.hits,
            misses: self.misses,
            stores: self.stores,
            bypassed: self.bypassed,
            not_stored_error: self.not_stored_error,
            port_keyed: self.port_keyed,
            prefetch: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> LiteralValue {
        LiteralValue::Text(value.into())
    }

    #[test]
    fn memo_counts_like_python() {
        let mut memo = ModelCallMemo::new();
        assert!(memo.report().is_none());
        let stack = vec!["parent:p".to_owned()];
        let inputs = vec![("a".to_owned(), LiteralValue::Number(1.0))];
        let key = memo.key(&stack, "child:c", &text("out"), &inputs, None).unwrap();
        assert!(memo.lookup(&key).is_none());
        let matrix = vec![vec![LiteralValue::Number(2.0)]];
        assert!(memo.store(key.clone(), 0, &matrix));
        let hit = memo.lookup(&key).unwrap();
        assert_eq!(hit.source_index, 0);
        assert_eq!(hit.matrix, matrix);
        let error = vec![vec![LiteralValue::Error(formualizer_common::ExcelError::new(
            formualizer_common::ExcelErrorKind::Div,
        ))]];
        let other = memo.key(&stack, "child:c", &text("other"), &inputs, None).unwrap();
        assert!(!memo.store(other.clone(), 1, &error));
        assert!(!memo.adopt(other.clone(), 1, &error, false));
        assert!(memo.adopt(other.clone(), 1, &error, true));
        assert!(!memo.adopt(other, 2, &matrix, false), "adopt never overwrites");
        let third = memo.key(&stack, "child:c", &text("third"), &inputs, None).unwrap();
        assert!(!memo.store(third.clone(), 3, &error), "store still rejects errors");
        assert!(memo.adopt_declined_attempt(third.clone(), 3, &error), "the hand-off admits a finished error");
        assert!(!memo.adopt_declined_attempt(third, 4, &matrix), "and never overwrites");
        let nan = vec![("a".to_owned(), LiteralValue::Number(f64::NAN))];
        assert!(memo.key(&stack, "child:c", &text("out"), &nan, None).is_none());
        memo.not_stored();
        let report = memo.report().unwrap();
        assert_eq!(
            (report.hits, report.misses, report.stores, report.bypassed, report.not_stored_error, report.port_keyed),
            (1, 1, 1, 1, 3, 0)
        );
    }
}
