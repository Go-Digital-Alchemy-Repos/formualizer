//! Lane A: the per-request call memo (`callbacks.XcallRequestMemo`, renamed
//! `ModelCallMemo`). Lane 0 placeholder: signatures fixed, bodies inert (every
//! lookup misses, nothing is stored, the report is empty), so a caller
//! compiled against it behaves as memo-off.
//!
//! Contract for Lane A (port exactly): `key` counts `bypassed` when the key
//! cannot be spelled and `port_keyed` for a `KeyForm::Ports` key; `peek_key`
//! counts nothing; `lookup` counts `hits`/`misses`; `store` admits only
//! `matrix_is_memoisable` and otherwise counts `not_stored_error`; `adopt`
//! (prefetch) admits `matrix_is_memoisable`, or `matrix_is_finished` with
//! `allow_errors`, never overwrites and never counts; `not_stored` counts
//! `not_stored_error`; `report` is `None` unless hits+misses+bypassed > 0.

use formualizer_common::LiteralValue;

use crate::evaluator::ChildMatrix;
use crate::key::MemoKey;
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
    _private: (),
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
        _stack: &[String],
        _identity: &str,
        _output: &LiteralValue,
        _inputs: &[(String, LiteralValue)],
        _spec: Option<&ModelSpec>,
    ) -> Option<MemoKey> {
        None
    }

    /// `key` without counting (sibling prefetch).
    pub fn peek_key(
        &self,
        _stack: &[String],
        _identity: &str,
        _output: &LiteralValue,
        _inputs: &[(String, LiteralValue)],
        _spec: Option<&ModelSpec>,
    ) -> Option<MemoKey> {
        None
    }

    /// Whether `key` is stored, without counting.
    pub fn contains(&self, _key: &MemoKey) -> bool {
        false
    }

    /// A hit returns the stored entry (counted); `None` counts a miss.
    pub fn lookup(&mut self, _key: &MemoKey) -> Option<MemoEntry> {
        None
    }

    /// Store an in-line result; true when admitted.
    pub fn store(&mut self, _key: MemoKey, _source_index: usize, _matrix: &ChildMatrix) -> bool {
        false
    }

    /// Store a prefetched sibling's result under its own key; true when admitted.
    pub fn adopt(&mut self, _key: MemoKey, _source_index: usize, _matrix: &ChildMatrix, _allow_errors: bool) -> bool {
        false
    }

    /// A missed call that failed instead of completing.
    pub fn not_stored(&mut self) {}

    /// Sealed counters; `None` when this request looked nothing up.
    pub fn report(&self) -> Option<MemoReport> {
        None
    }
}
