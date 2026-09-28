//! Lane I: workbooks retained across requests (`sessions.SessionPool`,
//! `RetainedSession`, `SessionPool.warm`) and the receipt rules that come
//! with them (`runtime.sealed_invocations`, `surviving_calls`,
//! `_record_evaluation`, `retain_scenario`).
//!
//! A [`RetainedModel`] owns one pool for one package: every model (parent and
//! children) is loaded once, keyed by `(identity, sha256, seed)`, and lent to
//! at most one request at a time. Reuse is sound because every scenario
//! re-admits the whole declared input surface (the port session restores
//! formula defaults and clears range tails once it is re-entered) and because
//! the cells goal seek writes outside that surface are pinned at first load
//! ([`solver_written_cells`]) and restored before the next scenario.
//!
//! Lock order: the pool mutex is taken for one bookkeeping step and never
//! held across an evaluation, a load or a call into the request core.

use formualizer_common::LiteralValue;
use formualizer_workbook::Workbook;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use crate::evaluator::SolveModel;
use crate::event::{CallStatus, ModelCallEvent};
use crate::goal_seek::goal_seek_written_cells;
use crate::ports::{CellKey, PortSession, WireValue};
use crate::receipt::{Timings, canonical_json, plain_value};
use crate::router::RouterSlot;
use crate::session::{PathWorkbookSource, SharedWorkbook, WorkbookSource, warm_model};
use crate::spec::{ModelPackage, ModelSpec};
use crate::{CalculationContext, ModelCallError};

/// `SessionPool.key(spec, random_seed)`: the seed is a load-time engine
/// configuration, so a retained workbook cannot be reseeded.
pub type PoolKey = (String, String, u64);

pub fn pool_key(spec: &ModelSpec, seed: u64) -> PoolKey {
    (spec.identity.clone(), spec.workbook_sha256.clone(), seed)
}

/// One pinned cell (`solver_written_cells`): its formula text, or its
/// literal when it has none.
#[derive(Debug, Clone, PartialEq)]
pub struct PinnedCell {
    pub sheet: String,
    pub row: u32,
    pub col: u32,
    pub formula: Option<String>,
    pub value: LiteralValue,
}

/// `solver_written_cells(workbook)`: every cell goal seek can write (the
/// blocks' rectangles and their `By changing` cells), as the source holds it.
pub fn solver_written_cells(model: &dyn SolveModel) -> Result<Vec<PinnedCell>, ModelCallError> {
    let cells = goal_seek_written_cells(model)?;
    let mut pinned = Vec::with_capacity(cells.len());
    for (sheet, row, col) in cells {
        let formula = model.get_formula(&sheet, row, col)?;
        let value = if formula.is_none() { model.get_value(&sheet, row, col)? } else { LiteralValue::Empty };
        pinned.push(PinnedCell { sheet, row, col, formula, value });
    }
    Ok(pinned)
}

/// `restore_pinned_cells(workbook, pinned, write_record)`.
pub(crate) fn restore_pinned_cells(
    workbook: &mut Workbook,
    pinned: &[PinnedCell],
    ports: Option<&mut PortSession>,
) -> Result<(), ModelCallError> {
    if let Some(record) = ports.and_then(|ports| ports.write_record.as_mut()) {
        let cells: Vec<CellKey> = pinned.iter().map(|cell| (cell.sheet.clone(), cell.row, cell.col)).collect();
        record.invalidate(&cells);
    }
    for cell in pinned {
        let outcome = match &cell.formula {
            None => workbook.set_value(&cell.sheet, cell.row, cell.col, cell.value.clone()),
            Some(formula) => workbook.set_formula(&cell.sheet, cell.row, cell.col, formula),
        };
        outcome.map_err(|error| ModelCallError::infrastructure("RuntimeError", error.to_string()))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The pool
// ---------------------------------------------------------------------------

/// `RetainedSession`: one retained workbook plus what a later scenario must undo.
struct Entry {
    serial: u64,
    key: PoolKey,
    identity: String,
    workbook: SharedWorkbook,
    /// The one call binding registered with this workbook at load.
    slot: Arc<RouterSlot>,
    /// The workbook's cancel flag (what the deadline watchdog sets).
    flag: Arc<AtomicBool>,
    restore: Arc<Vec<PinnedCell>>,
    /// The port session and its CL-097 write record; `None` while lent out.
    ports: Option<PortSession>,
    in_use: bool,
    warm_invocations: Vec<ModelCallEvent>,
    warmed: bool,
    retained_scenario: bool,
}

/// What `acquire` lends a request.
pub(crate) struct Acquired {
    pub(crate) serial: u64,
    pub(crate) identity: String,
    pub(crate) workbook: SharedWorkbook,
    pub(crate) slot: Arc<RouterSlot>,
    pub(crate) flag: Arc<AtomicBool>,
    pub(crate) restore: Arc<Vec<PinnedCell>>,
    pub(crate) ports: Option<PortSession>,
    pub(crate) warmed: bool,
    /// Warmed, or carrying a retained scenario: the first acquire inherits.
    pub(crate) inherited: bool,
    pub(crate) warm_invocations: Vec<ModelCallEvent>,
}

/// What `admit` records for a fresh load.
pub(crate) struct Admission {
    pub(crate) workbook: SharedWorkbook,
    pub(crate) slot: Arc<RouterSlot>,
    pub(crate) flag: Arc<AtomicBool>,
    pub(crate) restore: Vec<PinnedCell>,
}

#[derive(Default)]
struct PoolInner {
    entries: Vec<Entry>,
    next_serial: u64,
    retain_scenarios: bool,
    warm_timings: Vec<(String, f64)>,
    warm_counts: Vec<(String, usize)>,
    warm_session_timings: Vec<(String, Timings)>,
}

fn upsert<V>(list: &mut Vec<(String, V)>, key: &str, value: V) {
    match list.iter_mut().find(|(existing, _)| existing == key) {
        Some(slot) => slot.1 = value,
        None => list.push((key.to_owned(), value)),
    }
}

/// `SessionPool`: single-request-at-a-time retention keyed by pinned identity.
#[derive(Default)]
pub struct RetainedPool {
    inner: Mutex<PoolInner>,
}

impl std::fmt::Debug for RetainedPool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("RetainedPool").field("entries", &self.len()).finish()
    }
}

impl RetainedPool {
    pub fn new(retain_scenarios: bool) -> Self {
        Self { inner: Mutex::new(PoolInner { retain_scenarios, ..PoolInner::default() }) }
    }

    fn lock(&self) -> MutexGuard<'_, PoolInner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn retain_scenarios(&self) -> bool {
        self.lock().retain_scenarios
    }

    pub fn set_retain_scenarios(&self, retain: bool) {
        self.lock().retain_scenarios = retain;
    }

    pub fn contains(&self, spec: &ModelSpec, seed: u64) -> bool {
        let key = pool_key(spec, seed);
        self.lock().entries.iter().any(|entry| entry.key == key)
    }

    /// No entry is lent out.
    pub fn idle(&self) -> bool {
        self.lock().entries.iter().all(|entry| !entry.in_use)
    }

    /// `acquire(spec, seed)`: `None` when nothing is retained for the key; a
    /// lent-out entry is refused. Clears `warmed` / `retained_scenario` (only
    /// the first acquire inherits).
    pub(crate) fn acquire(&self, spec: &ModelSpec, seed: u64) -> Result<Option<Acquired>, ModelCallError> {
        let key = pool_key(spec, seed);
        let mut inner = self.lock();
        let Some(entry) = inner.entries.iter_mut().find(|entry| entry.key == key) else { return Ok(None) };
        if entry.in_use {
            return Err(ModelCallError::infrastructure(
                "RuntimeError",
                format!("retained workbook session is already acquired: {}", entry.identity),
            ));
        }
        entry.in_use = true;
        let warmed = entry.warmed;
        let inherited = warmed || entry.retained_scenario;
        entry.warmed = false;
        entry.retained_scenario = false;
        Ok(Some(Acquired {
            serial: entry.serial,
            identity: entry.identity.clone(),
            workbook: entry.workbook.clone(),
            slot: entry.slot.clone(),
            flag: entry.flag.clone(),
            restore: entry.restore.clone(),
            ports: entry.ports.take(),
            warmed,
            inherited,
            warm_invocations: entry.warm_invocations.clone(),
        }))
    }

    /// `admit(spec, seed, workbook, restore, binding, write_record)`: the new
    /// entry is lent out to the caller that loaded it.
    pub(crate) fn admit(&self, spec: &ModelSpec, seed: u64, admission: Admission) -> Result<u64, ModelCallError> {
        let key = pool_key(spec, seed);
        let mut inner = self.lock();
        if inner.entries.iter().any(|entry| entry.key == key) {
            return Err(ModelCallError::infrastructure(
                "RuntimeError",
                "a workbook session for this identity is already retained",
            ));
        }
        let serial = inner.next_serial;
        inner.next_serial += 1;
        inner.entries.push(Entry {
            serial,
            key,
            identity: spec.model_identity(),
            workbook: admission.workbook,
            slot: admission.slot,
            flag: admission.flag,
            restore: Arc::new(admission.restore),
            ports: None,
            in_use: true,
            warm_invocations: Vec::new(),
            warmed: false,
            retained_scenario: false,
        });
        Ok(serial)
    }

    /// `release(entry)`: the port session (and its write record) goes back
    /// with the workbook.
    pub(crate) fn release(&self, serial: u64, ports: Option<PortSession>) {
        let mut inner = self.lock();
        if let Some(entry) = inner.entries.iter_mut().find(|entry| entry.serial == serial) {
            entry.in_use = false;
            if ports.is_some() {
                entry.ports = ports;
            }
        }
    }

    /// `discard(entry)`: forget a workbook whose state can no longer be
    /// trusted; its binding is unbound so the workbook can be freed.
    pub(crate) fn discard(&self, serial: u64) {
        let removed = {
            let mut inner = self.lock();
            let position = inner.entries.iter().position(|entry| entry.serial == serial);
            position.map(|position| inner.entries.remove(position))
        };
        if let Some(entry) = removed {
            entry.slot.unbind();
        }
    }

    pub(crate) fn set_warm_invocations(&self, serial: u64, events: Vec<ModelCallEvent>, retained_scenario: bool) {
        let mut inner = self.lock();
        if let Some(entry) = inner.entries.iter_mut().find(|entry| entry.serial == serial) {
            entry.warm_invocations = events;
            if retained_scenario {
                entry.retained_scenario = true;
            }
        }
    }

    fn mark_warmed_except(&self, before: &[u64]) {
        let mut inner = self.lock();
        for entry in &mut inner.entries {
            if !before.contains(&entry.serial) {
                entry.warmed = true;
            }
        }
    }

    fn serials(&self) -> Vec<u64> {
        self.lock().entries.iter().map(|entry| entry.serial).collect()
    }

    fn record_warm(&self, identity: &str, seconds: f64, events: usize, timings: Timings) {
        let mut inner = self.lock();
        upsert(&mut inner.warm_timings, identity, seconds);
        upsert(&mut inner.warm_counts, identity, events);
        upsert(&mut inner.warm_session_timings, identity, timings);
    }

    /// `forget_scenarios()`: keep the loaded workbooks; inherit nothing from
    /// what just ran (after a failed request).
    pub fn forget_scenarios(&self) {
        let mut inner = self.lock();
        for entry in &mut inner.entries {
            entry.warmed = false;
            entry.retained_scenario = false;
            entry.warm_invocations.clear();
            if let Some(record) = entry.ports.as_mut().and_then(|ports| ports.write_record.as_mut()) {
                record.clear();
            }
        }
    }

    /// `discard_all()`: forget every retained workbook; returns how many.
    pub fn discard_all(&self) -> usize {
        let entries = {
            let mut inner = self.lock();
            inner.warm_timings.clear();
            inner.warm_counts.clear();
            inner.warm_session_timings.clear();
            std::mem::take(&mut inner.entries)
        };
        let count = entries.len();
        for entry in entries {
            entry.slot.unbind();
        }
        count
    }

    /// Evidence about what the pool holds (plain JSON).
    pub fn stats(&self) -> Value {
        let inner = self.lock();
        let entries: Vec<Value> = inner
            .entries
            .iter()
            .map(|entry| {
                let record = entry.ports.as_ref().and_then(|ports| ports.write_record.as_ref());
                json!({
                    "identity": entry.identity,
                    "random_seed": entry.key.2,
                    "in_use": entry.in_use,
                    "warmed": entry.warmed,
                    "retained_scenario": entry.retained_scenario,
                    "warm_invocations": entry.warm_invocations.len(),
                    "restore_cells": entry.restore.len(),
                    "write_record": record.is_some(),
                    "write_record_cells": record.map_or(0, crate::ports::WriteRecord::recorded_cells),
                })
            })
            .collect();
        let mut stats = Map::new();
        stats.insert("entries".into(), Value::Array(entries));
        stats.insert("count".into(), json!(inner.entries.len()));
        stats.insert("retain_scenarios".into(), json!(inner.retain_scenarios));
        stats.insert("warm_timings".into(), ordered_object(&inner.warm_timings, |seconds| json!(seconds)));
        stats.insert("warm_invocations".into(), ordered_object(&inner.warm_counts, |count| json!(count)));
        stats.insert(
            "warm_session_timings".into(),
            ordered_object(&inner.warm_session_timings, |timings| serde_json::to_value(timings).unwrap_or(Value::Null)),
        );
        Value::Object(stats)
    }
}

fn ordered_object<V>(list: &[(String, V)], convert: impl Fn(&V) -> Value) -> Value {
    Value::Object(list.iter().map(|(key, value)| (key.clone(), convert(value))).collect())
}

// ---------------------------------------------------------------------------
// The retained model (pyo3 `RetainedModel`)
// ---------------------------------------------------------------------------

/// One warmed model: identity, seconds, the warm run's events and timings.
#[derive(Debug, Clone)]
pub struct WarmedModel {
    pub identity: String,
    pub seconds: f64,
    pub invocations: Vec<ModelCallEvent>,
    pub timings: Timings,
}

/// What `warm` did, in order (`SessionPool.warm` plus `warm_timings`,
/// `warm_invocations`, `warm_session_timings`).
#[derive(Debug, Clone, Default)]
pub struct WarmReport {
    pub models: Vec<WarmedModel>,
}

impl WarmReport {
    pub fn warmed(&self) -> Vec<String> {
        self.models.iter().map(|model| model.identity.clone()).collect()
    }
}

/// One package's retained workbooks, held by the Python pool and lent to one
/// `ModelSession` at a time.
pub struct RetainedModel {
    pub(crate) package: Arc<ModelPackage>,
    pub(crate) context: CalculationContext,
    pub(crate) pool: Arc<RetainedPool>,
    pub(crate) source: Arc<dyn WorkbookSource>,
}

impl RetainedModel {
    pub fn new(package: Arc<ModelPackage>, context: CalculationContext, retain_scenarios: bool) -> Self {
        Self {
            package,
            context,
            pool: Arc::new(RetainedPool::new(retain_scenarios)),
            source: Arc::new(PathWorkbookSource),
        }
    }

    /// Replace the pinned-path loader (tests, injected factories).
    pub fn with_workbook_source(mut self, source: Arc<dyn WorkbookSource>) -> Self {
        self.source = source;
        self
    }

    pub fn package(&self) -> &Arc<ModelPackage> {
        &self.package
    }

    pub fn context(&self) -> &CalculationContext {
        &self.context
    }

    pub fn pool(&self) -> &Arc<RetainedPool> {
        &self.pool
    }

    pub fn source(&self) -> &Arc<dyn WorkbookSource> {
        &self.source
    }

    /// `SessionPool.warm(package, context, include_parent=...)`: load every
    /// model not yet retained (children first, then the parent), admit it,
    /// write its default scenario (the parent gets `parent_inputs`), evaluate
    /// once with a router of its own and leave it marked warmed. Goal seek does
    /// not run in a warm, as in Python.
    pub fn warm(&self, parent_inputs: &[(String, WireValue)], include_parent: bool) -> Result<WarmReport, ModelCallError> {
        let before = self.pool.serials();
        let mut report = WarmReport::default();
        let mut models: Vec<&ModelSpec> = self.package.children.iter().map(|(_, spec)| spec).collect();
        if include_parent {
            models.push(&self.package.parent);
        }
        for spec in models {
            if self.pool.contains(spec, self.context.random_seed) {
                continue;
            }
            let started = Instant::now();
            let is_parent = std::ptr::eq(spec, &self.package.parent);
            let inputs: &[(String, WireValue)] = if is_parent { parent_inputs } else { &[] };
            let (invocations, timings) = warm_model(self, spec, inputs)?;
            let identity = spec.model_identity();
            let seconds = started.elapsed().as_secs_f64();
            self.pool.record_warm(&identity, seconds, invocations.len(), timings.clone());
            report.models.push(WarmedModel { identity, seconds, invocations, timings });
        }
        self.pool.mark_warmed_except(&before);
        Ok(report)
    }

    /// Forget every retained workbook (`discard_all`).
    pub fn close(&self) -> usize {
        self.pool.discard_all()
    }

    pub fn stats(&self) -> Value {
        self.pool.stats()
    }
}

impl Drop for RetainedModel {
    fn drop(&mut self) {
        // CL-095: break each workbook -> binding -> run cycle.
        self.pool.discard_all();
    }
}

// ---------------------------------------------------------------------------
// Per-request reuse bookkeeping and sealing
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct EvaluationState {
    evaluations: u32,
    holding: Vec<ModelCallEvent>,
}

/// What one request did with the pool (`CalculationSession._session_reuse`,
/// `_acquired`, `_entered`, `_inherited`, `_evaluation_state`, `_held`).
#[derive(Debug, Default)]
pub(crate) struct ReuseState {
    pub(crate) pooled: bool,
    fresh: Vec<String>,
    reused: Vec<String>,
    warmed: Vec<String>,
    pub(crate) acquired: Vec<u64>,
    entered: Vec<(u64, String)>,
    inherited: Vec<Vec<ModelCallEvent>>,
    evaluation: Vec<(u64, EvaluationState)>,
    held: Vec<ModelCallEvent>,
}

impl ReuseState {
    pub(crate) fn new(pooled: bool) -> Self {
        Self { pooled, ..Self::default() }
    }

    pub(crate) fn note_fresh(&mut self, identity: String) {
        self.fresh.push(identity);
    }

    pub(crate) fn note_admitted(&mut self, serial: u64, identity: &str) {
        self.acquired.push(serial);
        self.entered.push((serial, identity.to_owned()));
        self.track_evaluations(serial, identity, false, &[]);
    }

    pub(crate) fn note_reused(&mut self, acquired: &Acquired) {
        self.acquired.push(acquired.serial);
        self.entered.push((acquired.serial, acquired.identity.clone()));
        if acquired.inherited {
            self.inherited.push(acquired.warm_invocations.clone());
        }
        self.track_evaluations(acquired.serial, &acquired.identity, acquired.inherited, &acquired.warm_invocations);
        if acquired.warmed {
            self.warmed.push(acquired.identity.clone());
        } else {
            self.reused.push(acquired.identity.clone());
        }
    }

    pub(crate) fn note_released(&mut self, serial: u64) {
        self.acquired.retain(|existing| *existing != serial);
    }

    /// `_track_evaluations(entry, inherited)`.
    fn track_evaluations(&mut self, serial: u64, identity: &str, inherited: bool, warm: &[ModelCallEvent]) {
        if self.evaluation.iter().any(|(existing, _)| *existing == serial) {
            return;
        }
        let holding =
            if inherited { warm.iter().filter(|event| event.parent == identity).cloned().collect() } else { Vec::new() };
        self.evaluation.push((serial, EvaluationState { evaluations: 0, holding }));
    }

    /// `_record_evaluation(workbook, stack, start)`.
    pub(crate) fn record_evaluation(&mut self, serial: u64, invocations: &[ModelCallEvent], stack: &[String], start: usize) {
        let Some((_, state)) = self.evaluation.iter_mut().find(|(existing, _)| *existing == serial) else { return };
        let fired: Vec<ModelCallEvent> =
            invocations.iter().skip(start).filter(|event| event.stack == stack).cloned().collect();
        let held = surviving_calls(&state.holding, &fired);
        if state.evaluations > 0 {
            self.held.extend(held.iter().cloned());
        }
        state.holding = fired.into_iter().chain(held).collect();
        state.evaluations += 1;
    }

    /// `session_reuse()`: empty without a pool.
    pub(crate) fn report(&self) -> Map<String, Value> {
        let mut report = Map::new();
        if !self.pooled {
            return report;
        }
        report.insert("pool".into(), json!(true));
        report.insert("fresh".into(), json!(self.fresh));
        report.insert("reused".into(), json!(self.reused));
        report.insert("warmed".into(), json!(self.warmed));
        report.insert("fresh_count".into(), json!(self.fresh.len()));
        report.insert("reused_count".into(), json!(self.reused.len()));
        report.insert("warmed_count".into(), json!(self.warmed.len()));
        report
    }

    /// `_warm_invocations()`: every warm event this run inherited, each
    /// distinct call as often as the entry that saw it most saw it.
    fn warm_invocations(&self) -> Vec<ModelCallEvent> {
        let mut seen: HashMap<String, usize> = HashMap::new();
        let mut events = Vec::new();
        for entry in &self.inherited {
            let mut local: HashMap<String, usize> = HashMap::new();
            for event in entry {
                let key = invocation_key(event);
                let count = local.entry(key.clone()).or_insert(0);
                *count += 1;
                if *count > seen.get(&key).copied().unwrap_or(0) {
                    events.push(event.clone());
                }
            }
            for (key, count) in local {
                let slot = seen.entry(key).or_insert(0);
                *slot = (*slot).max(count);
            }
        }
        events
    }

    /// `sealed_invocations()`: executed, then held, then inherited; renumbered.
    pub(crate) fn seal(&self, invocations: &[ModelCallEvent]) -> Vec<ModelCallEvent> {
        let warm = self.warm_invocations();
        if warm.is_empty() && self.held.is_empty() {
            return invocations.to_vec();
        }
        let mut events = invocations.to_vec();
        events.extend(self.held.iter().map(|event| {
            let mut copy = event.clone();
            copy.memo_of = None;
            copy.inherited_from = None;
            copy.status = CallStatus::Held;
            copy.held_from = Some("reuse".into());
            copy
        }));
        events.extend(surviving_calls(&warm, invocations).into_iter().map(|mut copy| {
            copy.memo_of = None;
            copy.status = CallStatus::Inherited;
            copy.inherited_from = Some("warm".into());
            copy
        }));
        for (index, event) in events.iter_mut().enumerate() {
            event.index = index;
        }
        events
    }

    /// `retain_scenario(invocations)`: re-point every entered entry at what
    /// this scenario left in it.
    pub(crate) fn retain_scenario(&self, pool: &RetainedPool, sealed: &[ModelCallEvent]) {
        for (serial, identity) in &self.entered {
            let state = self.evaluation.iter().find(|(existing, _)| existing == serial).map(|(_, state)| state);
            let events = match state {
                Some(state) if state.evaluations > 1 => state.holding.clone(),
                _ => sealed.iter().filter(|event| &event.parent == identity).cloned().collect(),
            };
            pool.set_warm_invocations(*serial, events, true);
        }
    }
}

/// `invocation_key(event)`: (parent, target, output, inputs), each canonical;
/// the group is everything before the last separator.
pub fn invocation_key(event: &ModelCallEvent) -> String {
    let inputs = match &event.inputs {
        None => Value::Null,
        Some(pairs) => Value::Object(pairs.iter().map(|(name, value)| (name.clone(), plain_value(value))).collect()),
    };
    format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}",
        event.parent,
        canonical_json(&plain_value(&event.target)),
        canonical_json(&plain_value(&event.output)),
        canonical_json(&inputs)
    )
}

fn group_of(key: &str) -> &str {
    key.rsplit_once('\u{1f}').map_or(key, |(group, _)| group)
}

/// `surviving_calls(earlier, executed)`: the earlier calls that the executed
/// ones neither repeated identically nor may have superseded.
pub fn surviving_calls(earlier: &[ModelCallEvent], executed: &[ModelCallEvent]) -> Vec<ModelCallEvent> {
    let mut unmatched: HashMap<String, usize> = HashMap::new();
    for event in executed {
        *unmatched.entry(invocation_key(event)).or_insert(0) += 1;
    }
    let mut kept = Vec::new();
    for event in earlier {
        let key = invocation_key(event);
        match unmatched.get_mut(&key) {
            Some(count) if *count > 0 => *count -= 1,
            _ => kept.push((key, event)),
        }
    }
    let recomputed: Vec<String> =
        unmatched.iter().filter(|(_, count)| **count > 0).map(|(key, _)| group_of(key).to_owned()).collect();
    kept.into_iter()
        .filter(|(key, _)| !recomputed.iter().any(|group| group == group_of(key)))
        .map(|(_, event)| event.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(parent: &str, target: &str, amount: i64) -> ModelCallEvent {
        let mut event = ModelCallEvent::started(
            0,
            &[parent.to_owned()],
            LiteralValue::Text(target.into()),
            LiteralValue::Text("result".into()),
        );
        event.inputs = Some(vec![("amount".into(), LiteralValue::Int(amount))]);
        event.status = CallStatus::Completed;
        event
    }

    #[test]
    fn surviving_calls_drops_repeats_and_recomputed_groups() {
        let earlier = vec![event("p", "a", 1), event("p", "a", 2), event("p", "b", 1)];
        // Identical repeat of a/1 consumes it; nothing else in group a changed.
        let kept = surviving_calls(&earlier, &[event("p", "a", 1)]);
        assert_eq!(kept.len(), 2);
        // A new a/3 recomputes group a: both earlier a calls are dropped.
        let kept = surviving_calls(&earlier, &[event("p", "a", 3)]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].target, LiteralValue::Text("b".into()));
    }

    #[test]
    fn seal_adds_held_then_inherited_and_renumbers() {
        let mut state = ReuseState { pooled: true, ..ReuseState::default() };
        state.inherited.push(vec![event("p", "a", 1), event("p", "b", 1)]);
        let mut held = event("p", "c", 1);
        held.memo_of = Some(4);
        state.held.push(held);
        let executed = vec![event("p", "a", 1)];
        let sealed = state.seal(&executed);
        let statuses: Vec<_> = sealed.iter().map(|event| event.status).collect();
        assert_eq!(statuses, [CallStatus::Completed, CallStatus::Held, CallStatus::Inherited]);
        assert_eq!(sealed.iter().map(|event| event.index).collect::<Vec<_>>(), [0, 1, 2]);
        assert_eq!(sealed[1].held_from.as_deref(), Some("reuse"));
        assert_eq!(sealed[1].memo_of, None);
        assert_eq!(sealed[2].inherited_from.as_deref(), Some("warm"));
        assert_eq!(sealed[2].target, LiteralValue::Text("b".into()));
    }
}
