//! Per-`evaluate_all` instrumentation (GOD-383 Trial A, T1).
//!
//! Observational only: collecting these counters never changes what is
//! evaluated, in which order, or with which values. Counters are reset at the
//! start of every `Engine::evaluate_all` / `Engine::evaluate_all_cancellable`
//! call and published (copied to the value `Engine::eval_stats` returns) when
//! that call returns, successfully or not.
//!
//! Phase timers (`ns_*`) are cumulative wall nanoseconds measured with one
//! `Instant` pair per phase boundary, never per vertex. Several are nested:
//! `ns_layer_eval` and `ns_scc_settle` include the spill clear/commit,
//! output-invalidation and overlay-flush time spent inside them;
//! `ns_schedule_build` includes `ns_vdep_builder`; `ns_spill_clear` and
//! `ns_spill_commit` include `ns_output_invalidation`. `ns_replan_*` are the
//! share of `ns_schedule_build` / (`ns_layer_eval` + `ns_scc_settle`) spent in
//! passes after the first.

use std::sync::atomic::{AtomicU64, Ordering};

/// One value in the flat stats map handed to bindings.
#[derive(Debug, Clone, PartialEq)]
pub enum EvalStatValue {
    Int(i64),
    Float(f64),
    Str(String),
}

/// Origin of a `record_changed_output_invalidations` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InvalidationOrigin {
    /// A registered spill region was cleared (`SpillClear` effect or a
    /// direct projection clear).
    SpillClear,
    /// A spill commit whose new or previous footprint covers more than one
    /// cell.
    SpillCommitMulti,
    /// A dynamic-array commit whose new and previous footprints are at most
    /// the anchor cell alone.
    Commit1x1,
}

/// Call site of `output_invalidation_token`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenSite {
    /// `evaluate_vertex` (sequential layer / direct path).
    Direct = 0,
    /// `evaluate_vertex_immutable` (parallel layer path).
    Layer = 1,
    /// `evaluate_vertex_recorded` (SCC member path).
    Scc = 2,
}

/// Outcome of `output_invalidation_token` for a vertex that WAS pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenOutcome {
    Issued = 0,
    NoneDynamic = 1,
    NonePendingDep = 2,
}

/// Counters written from `&self` (possibly parallel) evaluation paths.
#[derive(Debug, Default)]
pub(crate) struct EvalStatsAtomics {
    tokens: [[AtomicU64; 3]; 3],
}

impl EvalStatsAtomics {
    #[inline]
    pub(crate) fn record_token(&self, site: TokenSite, outcome: TokenOutcome) {
        self.tokens[site as usize][outcome as usize].fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn reset(&self) {
        for row in &self.tokens {
            for c in row {
                c.store(0, Ordering::Relaxed);
            }
        }
    }

    pub(crate) fn snapshot(&self) -> [[u64; 3]; 3] {
        let mut out = [[0u64; 3]; 3];
        for (i, row) in self.tokens.iter().enumerate() {
            for (j, c) in row.iter().enumerate() {
                out[i][j] = c.load(Ordering::Relaxed);
            }
        }
        out
    }
}

/// Readings of the last post-pass virtual-dependency recheck, handed from
/// `changed_virtual_dep_vertices` to the `evaluate_all` loop.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RecheckScratch {
    pub(crate) reason: &'static str,
    pub(crate) pending_at_drain: u64,
    pub(crate) changed_readers: u64,
    pub(crate) formula_dirty: u64,
}

impl RecheckScratch {
    pub(crate) fn reason_for(pending: bool, footprint: bool, dynamic: bool) -> &'static str {
        match (pending, footprint, dynamic) {
            (true, true, true) => "pending+footprint+dynamic",
            (true, true, false) => "pending+footprint",
            (true, false, true) => "pending+dynamic",
            (true, false, false) => "pending",
            (false, true, true) => "footprint+dynamic",
            (false, true, false) => "footprint",
            (false, false, true) => "dynamic",
            (false, false, false) => "none",
        }
    }
}

/// Snapshot of a spill region taken by a clear, so the commit that follows
/// it for the same anchor can tell an identical re-commit.
#[derive(Debug, Clone)]
pub(crate) struct SpillClearSnapshot {
    pub(crate) anchor: crate::engine::vertex::VertexId,
    pub(crate) cells: Vec<crate::reference::CellRef>,
    pub(crate) values: Vec<formualizer_common::LiteralValue>,
}

/// Stats for the last `evaluate_all` / `evaluate_all_cancellable` call.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct EvalStats {
    /// `"evaluate_all"` or `"evaluate_all_cancellable"`.
    pub entry: &'static str,
    /// `"chain"`, `"full"`, `"formula_plane"`, or `""` when the call errored
    /// before choosing.
    pub path: &'static str,
    /// `"ok"` or `"error"`.
    pub outcome: &'static str,
    pub computed_vertices: u64,
    pub cycle_errors: u64,

    /// `formula_dirty` size (legacy set) at call start: the pending
    /// evaluation count before volatile or scheduler admission.
    pub pending_formula_dirty_at_start: u64,
    /// `to_evaluate.len()` of the first full pass (0 on the chain path).
    pub evaluation_vertices_first_pass: u64,

    /// Dirty-propagation BFS visits during this call.
    pub dirty_propagation_visits_delta: u64,
    /// Of which, inside `redirty_for_next_recalc` (R1 D2).
    pub dirty_propagation_visits_redirty: u64,

    pub vdep_recheck_skips: u64,
    pub vdep_recheck_rebuilds: u64,
    /// Rebuilds opened (non-exclusively) by each guard condition.
    pub recheck_open_pending: u64,
    pub recheck_open_footprint: u64,
    pub recheck_open_dynamic: u64,

    pub replan_iterations: u64,
    pub passes: u64,
    pub pass_to_evaluate: Vec<u64>,
    pub pass_static_sccs: Vec<u64>,
    pub pass_scc_tasks: Vec<u64>,
    pub pass_settle_passes: Vec<u64>,
    pub pass_computed: Vec<u64>,
    /// `"skip"` or a `+`-joined subset of `pending`, `footprint`, `dynamic`.
    pub pass_recheck: Vec<&'static str>,
    pub pass_pending_at_drain: Vec<u64>,
    pub pass_changed_readers: Vec<u64>,
    /// Final replan set after the kind/finalized filter.
    pub pass_changed: Vec<u64>,
    /// `formula_dirty` size when the recheck ran (before the pass's flags
    /// are cleared): minus `pass_to_evaluate` it bounds the mid-pass drift.
    pub pass_formula_dirty_at_recheck: Vec<u64>,
    /// `"converged"`, `"pending"`, `"changed_readers"` or `"both"`.
    pub pass_outcome: Vec<&'static str>,
    pub pending_at_drain_total: u64,
    pub pending_at_drain_max: u64,
    pub changed_readers_total: u64,

    // record_changed_output_invalidations, by origin (non-empty cells only).
    pub inv_spill_clear_calls: u64,
    pub inv_spill_clear_cells: u64,
    pub inv_spill_clear_affected: u64,
    pub inv_commit_multi_calls: u64,
    pub inv_commit_multi_cells: u64,
    pub inv_commit_multi_affected: u64,
    pub inv_commit_1x1_calls: u64,
    pub inv_commit_1x1_cells: u64,
    pub inv_commit_1x1_affected: u64,
    pub inv_affected_max: u64,
    /// Vertices added to `pending_output_invalidations` (affected minus the
    /// anchor), all origins.
    pub inv_pending_added: u64,
    /// Calls with an empty `cells` list (all origins).
    pub inv_empty_calls: u64,

    pub spill_clear_count: u64,
    pub spill_clear_cells: u64,
    pub spill_clear_followers: u64,
    pub spill_commit_count: u64,
    pub spill_commit_multi: u64,
    pub spill_commit_1x1: u64,
    /// Commits whose footprint equals the footprint just before them (the
    /// registered region, or the region the immediately preceding clear of
    /// the same anchor removed) and whose every value equals the value that
    /// cell held before that clear/commit. Since T2a the comparison is the
    /// reader-observed one (`Engine::spill_values_equal_as_read`: Int and
    /// Number alike, dates as serials, -0.0 differs from 0.0, NaN always
    /// differs, errors compared in full).
    pub spill_commit_identical: u64,
    pub spill_commit_same_footprint: u64,
    pub spill_commit_changed_cells: u64,
    /// Same-footprint commits: target cells whose reader-observed value
    /// differs from the value before the clear/commit (0 means identical).
    pub spill_commit_differing_cells: u64,
    /// `FZ_SPILL_SAME_EXTENT_UPDATE`: values-only updates of a registered
    /// spill with an unchanged extent, and those whose diff was empty.
    pub spill_same_extent_updates: u64,
    pub spill_same_extent_empty_diffs: u64,
    /// `FZ_SPILL_PENDING_BY_POSITION`: invalidation-closure vertices added to
    /// the pending set, and those skipped because the pass schedules them
    /// strictly after the committing anchor.
    pub pending_by_position_pended: u64,
    pub pending_by_position_skipped: u64,

    pub output_footprint_epoch_delta: u64,
    pub topology_epoch_delta: u64,
    pub topology_revision_delta: u64,

    pub scc_static_first_pass: u64,
    pub scc_static_total: u64,
    pub scc_tasks: u64,
    pub scc_phantom: u64,
    pub scc_live_cycles: u64,
    pub scc_settle_passes_total: u64,
    pub scc_max_passes_single: u64,
    pub scc_iterated: u64,
    pub scc_capped: u64,
    pub scc_circ_stamped: u64,

    /// `[site][outcome]`: site direct/layer/scc, outcome issued /
    /// none_dynamic / none_pending_dep, counted only for pending vertices.
    pub tokens: [[u64; 3]; 3],

    // R1 D1-D3 (last redirty of the call).
    pub clock_frozen: i64,
    pub volatile_count: u64,
    pub volatiles_needing_refresh: u64,
    pub volatiles_clock_only: u64,
    pub formula_dirty_before_redirty: u64,
    pub formula_dirty_after_redirty: u64,
    /// Formula vertices with the dirty flag set; -1 unless the env var
    /// `FZ_EVAL_STATS_DIRTY_SCAN=1` is set (a full formula-vertex scan).
    pub dirty_flag_formulas_before_redirty: i64,
    pub dirty_flag_formulas_after_redirty: i64,
    pub redirty_calls: u64,

    pub schedule_builds: u64,
    pub schedule_cache_hits: u64,
    pub vdep_candidates: u64,
    pub vdep_vertices: u64,
    pub vdep_edges: u64,

    pub spec_chain_last_path: &'static str,
    pub spec_chain_last_reason: &'static str,

    pub overlay_flushes: u64,

    pub ns_total: u64,
    pub ns_chain_walk: u64,
    pub ns_schedule_build: u64,
    pub ns_vdep_builder: u64,
    pub ns_layer_eval: u64,
    pub ns_scc_settle: u64,
    pub ns_replan_schedule_build: u64,
    pub ns_replan_eval: u64,
    pub ns_recheck: u64,
    pub ns_dirty_get_eval_vertices: u64,
    pub ns_dirty_clear_flags: u64,
    pub ns_dirty_redirty: u64,
    pub ns_spill_clear: u64,
    pub ns_spill_commit: u64,
    pub ns_output_invalidation: u64,
    pub ns_overlay_flush: u64,

    // Baselines captured at call start (not exported).
    pub(crate) base_dirty_visits: u64,
    pub(crate) base_footprint_epoch: u64,
    pub(crate) base_topology_epoch: u64,
    pub(crate) base_topology_revision: u64,
    pub(crate) base_recheck_rebuilds: u64,
    pub(crate) base_recheck_skips: u64,
    pub(crate) pass_scc_tasks_base: u64,
    pub(crate) pass_settle_base: u64,
}

fn join<T: ToString>(items: &[T]) -> String {
    items
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

impl EvalStats {
    /// Flat `(name, value)` list for bindings. Stable key names.
    pub fn to_pairs(&self) -> Vec<(&'static str, EvalStatValue)> {
        use EvalStatValue::{Int, Str};
        let u = |v: u64| Int(i64::try_from(v).unwrap_or(i64::MAX));
        let s = |v: &str| Str(v.to_string());
        let t = &self.tokens;
        vec![
            ("entry", s(self.entry)),
            ("path", s(self.path)),
            ("outcome", s(self.outcome)),
            ("computed_vertices", u(self.computed_vertices)),
            ("cycle_errors", u(self.cycle_errors)),
            (
                "pending_formula_dirty_at_start",
                u(self.pending_formula_dirty_at_start),
            ),
            (
                "evaluation_vertices_first_pass",
                u(self.evaluation_vertices_first_pass),
            ),
            (
                "dirty_propagation_visits_delta",
                u(self.dirty_propagation_visits_delta),
            ),
            (
                "dirty_propagation_visits_redirty",
                u(self.dirty_propagation_visits_redirty),
            ),
            ("vdep_recheck_skips", u(self.vdep_recheck_skips)),
            ("vdep_recheck_rebuilds", u(self.vdep_recheck_rebuilds)),
            ("recheck_open_pending", u(self.recheck_open_pending)),
            ("recheck_open_footprint", u(self.recheck_open_footprint)),
            ("recheck_open_dynamic", u(self.recheck_open_dynamic)),
            ("replan_iterations", u(self.replan_iterations)),
            ("passes", u(self.passes)),
            ("pass_to_evaluate", Str(join(&self.pass_to_evaluate))),
            ("pass_static_sccs", Str(join(&self.pass_static_sccs))),
            ("pass_scc_tasks", Str(join(&self.pass_scc_tasks))),
            ("pass_settle_passes", Str(join(&self.pass_settle_passes))),
            ("pass_computed", Str(join(&self.pass_computed))),
            ("pass_recheck", Str(self.pass_recheck.join(","))),
            (
                "pass_pending_at_drain",
                Str(join(&self.pass_pending_at_drain)),
            ),
            (
                "pass_changed_readers",
                Str(join(&self.pass_changed_readers)),
            ),
            ("pass_changed", Str(join(&self.pass_changed))),
            (
                "pass_formula_dirty_at_recheck",
                Str(join(&self.pass_formula_dirty_at_recheck)),
            ),
            ("pass_outcome", Str(self.pass_outcome.join(","))),
            ("pending_at_drain_total", u(self.pending_at_drain_total)),
            ("pending_at_drain_max", u(self.pending_at_drain_max)),
            ("changed_readers_total", u(self.changed_readers_total)),
            ("inv_spill_clear_calls", u(self.inv_spill_clear_calls)),
            ("inv_spill_clear_cells", u(self.inv_spill_clear_cells)),
            ("inv_spill_clear_affected", u(self.inv_spill_clear_affected)),
            ("inv_commit_multi_calls", u(self.inv_commit_multi_calls)),
            ("inv_commit_multi_cells", u(self.inv_commit_multi_cells)),
            (
                "inv_commit_multi_affected",
                u(self.inv_commit_multi_affected),
            ),
            ("inv_commit_1x1_calls", u(self.inv_commit_1x1_calls)),
            ("inv_commit_1x1_cells", u(self.inv_commit_1x1_cells)),
            ("inv_commit_1x1_affected", u(self.inv_commit_1x1_affected)),
            ("inv_affected_max", u(self.inv_affected_max)),
            ("inv_pending_added", u(self.inv_pending_added)),
            ("inv_empty_calls", u(self.inv_empty_calls)),
            ("spill_clear_count", u(self.spill_clear_count)),
            ("spill_clear_cells", u(self.spill_clear_cells)),
            ("spill_clear_followers", u(self.spill_clear_followers)),
            ("spill_commit_count", u(self.spill_commit_count)),
            ("spill_commit_multi", u(self.spill_commit_multi)),
            ("spill_commit_1x1", u(self.spill_commit_1x1)),
            ("spill_commit_identical", u(self.spill_commit_identical)),
            (
                "spill_commit_same_footprint",
                u(self.spill_commit_same_footprint),
            ),
            (
                "spill_commit_changed_cells",
                u(self.spill_commit_changed_cells),
            ),
            (
                "spill_commit_differing_cells",
                u(self.spill_commit_differing_cells),
            ),
            ("spill_same_extent_updates", u(self.spill_same_extent_updates)),
            (
                "spill_same_extent_empty_diffs",
                u(self.spill_same_extent_empty_diffs),
            ),
            (
                "pending_by_position_pended",
                u(self.pending_by_position_pended),
            ),
            (
                "pending_by_position_skipped",
                u(self.pending_by_position_skipped),
            ),
            (
                "output_footprint_epoch_delta",
                u(self.output_footprint_epoch_delta),
            ),
            ("topology_epoch_delta", u(self.topology_epoch_delta)),
            ("topology_revision_delta", u(self.topology_revision_delta)),
            ("scc_static_first_pass", u(self.scc_static_first_pass)),
            ("scc_static_total", u(self.scc_static_total)),
            ("scc_tasks", u(self.scc_tasks)),
            ("scc_phantom", u(self.scc_phantom)),
            ("scc_live_cycles", u(self.scc_live_cycles)),
            ("scc_settle_passes_total", u(self.scc_settle_passes_total)),
            ("scc_max_passes_single", u(self.scc_max_passes_single)),
            ("scc_iterated", u(self.scc_iterated)),
            ("scc_capped", u(self.scc_capped)),
            ("scc_circ_stamped", u(self.scc_circ_stamped)),
            ("token_direct_issued", u(t[0][0])),
            ("token_direct_none_dynamic", u(t[0][1])),
            ("token_direct_none_pending_dep", u(t[0][2])),
            ("token_layer_issued", u(t[1][0])),
            ("token_layer_none_dynamic", u(t[1][1])),
            ("token_layer_none_pending_dep", u(t[1][2])),
            ("token_scc_issued", u(t[2][0])),
            ("token_scc_none_dynamic", u(t[2][1])),
            ("token_scc_none_pending_dep", u(t[2][2])),
            ("clock_frozen", Int(self.clock_frozen)),
            ("volatile_count", u(self.volatile_count)),
            (
                "volatiles_needing_refresh",
                u(self.volatiles_needing_refresh),
            ),
            ("volatiles_clock_only", u(self.volatiles_clock_only)),
            (
                "formula_dirty_before_redirty",
                u(self.formula_dirty_before_redirty),
            ),
            (
                "formula_dirty_after_redirty",
                u(self.formula_dirty_after_redirty),
            ),
            (
                "dirty_flag_formulas_before_redirty",
                Int(self.dirty_flag_formulas_before_redirty),
            ),
            (
                "dirty_flag_formulas_after_redirty",
                Int(self.dirty_flag_formulas_after_redirty),
            ),
            ("redirty_calls", u(self.redirty_calls)),
            ("schedule_builds", u(self.schedule_builds)),
            ("schedule_cache_hits", u(self.schedule_cache_hits)),
            ("vdep_candidates", u(self.vdep_candidates)),
            ("vdep_vertices", u(self.vdep_vertices)),
            ("vdep_edges", u(self.vdep_edges)),
            ("spec_chain_last_path", s(self.spec_chain_last_path)),
            ("spec_chain_last_reason", s(self.spec_chain_last_reason)),
            ("overlay_flushes", u(self.overlay_flushes)),
            ("ns_total", u(self.ns_total)),
            ("ns_chain_walk", u(self.ns_chain_walk)),
            ("ns_schedule_build", u(self.ns_schedule_build)),
            ("ns_vdep_builder", u(self.ns_vdep_builder)),
            ("ns_layer_eval", u(self.ns_layer_eval)),
            ("ns_scc_settle", u(self.ns_scc_settle)),
            ("ns_replan_schedule_build", u(self.ns_replan_schedule_build)),
            ("ns_replan_eval", u(self.ns_replan_eval)),
            ("ns_recheck", u(self.ns_recheck)),
            (
                "ns_dirty_get_eval_vertices",
                u(self.ns_dirty_get_eval_vertices),
            ),
            ("ns_dirty_clear_flags", u(self.ns_dirty_clear_flags)),
            ("ns_dirty_redirty", u(self.ns_dirty_redirty)),
            ("ns_spill_clear", u(self.ns_spill_clear)),
            ("ns_spill_commit", u(self.ns_spill_commit)),
            ("ns_output_invalidation", u(self.ns_output_invalidation)),
            ("ns_overlay_flush", u(self.ns_overlay_flush)),
        ]
    }
}

/// GOD-383 Trial A T2a: an engine toggle read from the environment ("1" = on).
/// Read once per `Engine` construction, never cached process-wide, so tests
/// can construct engines with either setting.
pub(crate) fn env_toggle(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "1")
}

/// GOD-383 Trial A T3: an engine toggle that is on by default. Unset (or any
/// value other than `0`) = on; `0` = off (legacy behaviour). Read once per
/// `Engine` construction like [`env_toggle`].
pub(crate) fn env_toggle_default_on(name: &str) -> bool {
    !matches!(std::env::var(name).as_deref(), Ok("0"))
}

/// Whether the optional full dirty-flag scan (D3) is enabled.
pub(crate) fn dirty_scan_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("FZ_EVAL_STATS_DIRTY_SCAN").is_ok_and(|v| v == "1"))
}

/// Elapsed nanoseconds since `start`, saturating.
#[inline]
pub(crate) fn ns_since(start: crate::instant::FzInstant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

impl EvalStats {
    /// Pending SCC members whose retirement token was refused because a
    /// dependency was still pending (R1 K6, SCC path).
    pub fn token_scc_blocked(&self) -> u64 {
        self.tokens[TokenSite::Scc as usize][TokenOutcome::NonePendingDep as usize]
    }
}
