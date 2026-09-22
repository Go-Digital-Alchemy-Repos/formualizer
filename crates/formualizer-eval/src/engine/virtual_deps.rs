use crate::engine::VertexId;
use formualizer_common::SheetId;
use crate::engine::VertexKind;
use crate::engine::eval::Engine;
use crate::engine::used_extent::{
    ExtentPolicy, OpenRangeBounds, ResolvedExtent, resolve_used_extent_with_fallback,
};
use crate::formula_plane::region_index::Region;
use crate::traits::{
    EvaluationContext, FunctionProvider, NamedRangeResolver, Range, RangeResolver,
    ReferenceResolver, Resolver, SourceResolver, Table, TableResolver,
};
use formualizer_common::{ExcelError, LiteralValue};
use formualizer_parse::parser::{ReferenceType, TableReference};
use rustc_hash::FxHashSet;
use std::sync::Mutex;

use crate::interpreter::Interpreter;

pub struct DynamicRefCollector<'a, R: EvaluationContext> {
    pub engine: &'a Engine<R>,
    pub current_sheet: &'a str,
    pub(crate) collected: Mutex<FxHashSet<VertexId>>,
    pub(crate) collected_regions: Mutex<FxHashSet<Region>>,
}

impl<'a, R: EvaluationContext> DynamicRefCollector<'a, R> {
    pub fn new(engine: &'a Engine<R>, current_sheet: &'a str) -> Self {
        Self {
            engine,
            current_sheet,
            collected: Mutex::new(FxHashSet::default()),
            collected_regions: Mutex::new(FxHashSet::default()),
        }
    }

    fn collect_formula_vertices_in_rect(
        &self,
        sheet_name: &str,
        sr: u32,
        sc: u32,
        er: u32,
        ec: u32,
    ) {
        let Some(sheet_id) = self.engine.graph.sheet_id(sheet_name) else {
            return;
        };
        let sr0 = sr.saturating_sub(1);
        let er0 = er.saturating_sub(1);
        let sc0 = sc.saturating_sub(1);
        let ec0 = ec.saturating_sub(1);
        self.collected_regions
            .lock()
            .unwrap()
            .insert(Region::rect(sheet_id, sr0, er0, sc0, ec0).normalized());
        let mut out = self.collected.lock().unwrap();
        for anchor in self
            .engine
            .graph
            .output_anchors_in_region(sheet_id, sr0, sc0, er0, ec0)
        {
            if self.engine.graph.is_dirty(anchor) || self.engine.graph.is_volatile(anchor) {
                out.insert(anchor);
            }
        }
        let Some(index) = self.engine.graph.sheet_index(sheet_id) else {
            return;
        };

        for u in index.vertices_in_col_range(sc0, ec0) {
            let Some(row0) = self.engine.graph.vertex_grid_addr(u).map(|addr| addr.row()) else {
                continue;
            };
            if row0 < sr0 || row0 > er0 {
                continue;
            }
            match self.engine.graph.get_vertex_kind(u) {
                VertexKind::FormulaScalar | VertexKind::FormulaArray => {
                    if self.engine.graph.is_dirty(u) || self.engine.graph.is_volatile(u) {
                        out.insert(u);
                    }
                }
                _ => {}
            }
        }
    }

    fn collect_formula_vertices_for_range(
        &self,
        sheet_name: &str,
        start_row: Option<u32>,
        start_col: Option<u32>,
        end_row: Option<u32>,
        end_col: Option<u32>,
    ) {
        let Some(extent) = resolve_used_extent_with_fallback(
            OpenRangeBounds {
                start_row,
                start_column: start_col,
                end_row,
                end_column: end_col,
            },
            ExtentPolicy::EvaluationCompat {
                fallback_row: None,
                fallback_column: None,
            },
            || {
                self.engine
                    .sheet_bounds(sheet_name)
                    .map(|_| self.engine.config.max_open_ended_rows)
            },
            || {
                self.engine
                    .sheet_bounds(sheet_name)
                    .map(|_| self.engine.config.max_open_ended_cols)
            },
            |first, last| self.engine.used_rows_for_columns(sheet_name, first, last),
            |first, last| self.engine.used_cols_for_rows(sheet_name, first, last),
        ) else {
            return;
        };

        self.collect_formula_vertices_in_rect(
            sheet_name,
            extent.start_row,
            extent.start_column,
            extent.end_row,
            extent.end_column,
        );
    }
}

impl<'a, R: EvaluationContext> ReferenceResolver for DynamicRefCollector<'a, R> {
    fn resolve_cell_reference(
        &self,
        sheet: Option<&str>,
        row: u32,
        col: u32,
    ) -> Result<LiteralValue, ExcelError> {
        let sheet_name = sheet.unwrap_or(self.current_sheet);
        if let Some(sheet_id) = self.engine.graph.sheet_id(sheet_name) {
            self.collected
                .lock()
                .unwrap()
                .extend(self.engine.graph.output_anchors_in_region(
                    sheet_id,
                    row.saturating_sub(1),
                    col.saturating_sub(1),
                    row.saturating_sub(1),
                    col.saturating_sub(1),
                ));
            self.collected_regions.lock().unwrap().insert(Region::point(
                sheet_id,
                row.saturating_sub(1),
                col.saturating_sub(1),
            ));
        }
        if let Some(&vid) = self
            .engine
            .graph
            .get_vertex_id_for_address(&self.engine.graph.make_cell_ref(sheet_name, row, col))
        {
            self.collected.lock().unwrap().insert(vid);
        }
        self.engine.resolve_cell_reference(sheet, row, col)
    }
}

impl<'a, R: EvaluationContext> RangeResolver for DynamicRefCollector<'a, R> {
    fn resolve_range_reference(
        &self,
        sheet: Option<&str>,
        sr: Option<u32>,
        sc: Option<u32>,
        er: Option<u32>,
        ec: Option<u32>,
    ) -> Result<Box<dyn Range>, ExcelError> {
        let sheet_name = sheet.unwrap_or(self.current_sheet);
        self.collect_formula_vertices_for_range(sheet_name, sr, sc, er, ec);
        self.engine.resolve_range_reference(sheet, sr, sc, er, ec)
    }
}

impl<'a, R: EvaluationContext> NamedRangeResolver for DynamicRefCollector<'a, R> {
    fn resolve_named_range_reference(
        &self,
        name: &str,
    ) -> Result<Vec<Vec<LiteralValue>>, ExcelError> {
        self.engine.resolve_named_range_reference(name)
    }
}

impl<'a, R: EvaluationContext> TableResolver for DynamicRefCollector<'a, R> {
    fn resolve_table_reference(&self, tref: &TableReference) -> Result<Box<dyn Table>, ExcelError> {
        self.engine.resolve_table_reference(tref)
    }
}

impl<'a, R: EvaluationContext> SourceResolver for DynamicRefCollector<'a, R> {
    fn source_scalar_version(&self, name: &str) -> Option<u64> {
        self.engine.source_scalar_version(name)
    }
    fn resolve_source_scalar(&self, name: &str) -> Result<LiteralValue, ExcelError> {
        self.engine.resolve_source_scalar(name)
    }
    fn source_table_version(&self, name: &str) -> Option<u64> {
        self.engine.source_table_version(name)
    }
    fn resolve_source_table(&self, name: &str) -> Result<Box<dyn Table>, ExcelError> {
        self.engine.resolve_source_table(name)
    }
}

impl<'a, R: EvaluationContext> Resolver for DynamicRefCollector<'a, R> {}

impl<'a, R: EvaluationContext> FunctionProvider for DynamicRefCollector<'a, R> {
    fn planning_semantic_revision(&self) -> Option<u64> {
        self.engine.planning_semantic_revision()
    }

    fn get_function(
        &self,
        ns: &str,
        name: &str,
    ) -> Option<std::sync::Arc<dyn crate::traits::Function>> {
        self.engine.get_function(ns, name)
    }

    fn get_function_for_planning(
        &self,
        ns: &str,
        name: &str,
    ) -> Option<std::sync::Arc<dyn crate::traits::Function>> {
        self.engine.get_function_for_planning(ns, name)
    }
}

impl<'a, R: EvaluationContext> EvaluationContext for DynamicRefCollector<'a, R> {
    fn cancellation_token(&self) -> Option<crate::engine::CancelToken> {
        self.engine.cancellation_token()
    }

    fn resolve_cell_format(
        &self,
        sheet: Option<&str>,
        row: u32,
        col: u32,
        current_sheet: &str,
    ) -> Option<crate::format::FormatId> {
        self.engine
            .resolve_cell_format(sheet, row, col, current_sheet)
    }

    fn format_class(
        &self,
        format: crate::format::FormatId,
    ) -> Option<formualizer_common::numfmt::FormatClass> {
        self.engine.format_class(format)
    }

    fn record_cell_derived_format(
        &self,
        sheet: &str,
        row: u32,
        col: u32,
        format: Option<crate::format::FormatId>,
    ) {
        self.engine
            .record_cell_derived_format(sheet, row, col, format)
    }

    fn resolve_range_view<'c>(
        &'c self,
        reference: &ReferenceType,
        current_sheet: &str,
    ) -> Result<crate::engine::range_view::RangeView<'c>, ExcelError> {
        // Collect vertices directly
        match reference {
            ReferenceType::Cell {
                sheet, row, col, ..
            } => {
                let sheet_name = sheet.as_deref().unwrap_or(current_sheet);
                self.collect_formula_vertices_in_rect(sheet_name, *row, *col, *row, *col);
            }
            ReferenceType::Range {
                sheet,
                start_row,
                start_col,
                end_row,
                end_col,
                ..
            } => {
                let sheet_name = sheet.as_deref().unwrap_or(current_sheet);
                self.collect_formula_vertices_for_range(
                    sheet_name, *start_row, *start_col, *end_row, *end_col,
                );
            }
            ReferenceType::NamedRange(name) => {
                let sid = self.engine.sheet_id(current_sheet);
                if let Some(s) = sid
                    && let Some(nr) = self.engine.graph.resolve_name_entry(name, s)
                {
                    let vid = nr.vertex;
                    self.collected.lock().unwrap().insert(vid);
                }
            }
            ReferenceType::Cell3D {
                sheet_first,
                sheet_last,
                row,
                col,
                ..
            } => {
                // Same owned-view blind spot as `RecordingContext` (GOD-242 /
                // CL-051): the engine resolves each span member itself and
                // returns an owned `"__tmp"` view, so nothing about the members
                // reaches this collector unless it expands the span here.  The
                // member window is the engine's own `active_span_ids` one; span
                // membership semantics (ES-010 / ES-047) are unchanged.
                if let Some(sheet_ids) = self
                    .engine
                    .graph
                    .sheet_reg()
                    .active_span_ids(sheet_first, sheet_last)
                {
                    for sheet_id in sheet_ids {
                        let sheet_name = self.engine.graph.sheet_name(sheet_id).to_string();
                        self.collect_formula_vertices_in_rect(&sheet_name, *row, *col, *row, *col);
                    }
                }
            }
            ReferenceType::Range3D {
                sheet_first,
                sheet_last,
                start_row,
                start_col,
                end_row,
                end_col,
                ..
            } => {
                if let Some(sheet_ids) = self
                    .engine
                    .graph
                    .sheet_reg()
                    .active_span_ids(sheet_first, sheet_last)
                {
                    for sheet_id in sheet_ids {
                        let sheet_name = self.engine.graph.sheet_name(sheet_id).to_string();
                        self.collect_formula_vertices_for_range(
                            &sheet_name,
                            *start_row,
                            *start_col,
                            *end_row,
                            *end_col,
                        );
                    }
                }
            }
            ReferenceType::Table(_) => {
                // Table references might be tricky, skip for now or resolve from graph if possible
            }
            _ => {}
        }

        self.engine.resolve_range_view(reference, current_sheet)
    }
}

pub struct RangeVirtualDepProvider;

impl RangeVirtualDepProvider {
    pub(crate) fn resolve_range<R: EvaluationContext>(
        engine: &Engine<R>,
        sheet_name: &str,
        range: &formualizer_common::SheetRangeRef<'_>,
    ) -> Option<ResolvedExtent> {
        resolve_used_extent_with_fallback(
            OpenRangeBounds {
                start_row: range.start_row.map(|bound| bound.index + 1),
                start_column: range.start_col.map(|bound| bound.index + 1),
                end_row: range.end_row.map(|bound| bound.index + 1),
                end_column: range.end_col.map(|bound| bound.index + 1),
            },
            ExtentPolicy::VirtualDependencyCompat {
                fallback_row: None,
                fallback_column: None,
            },
            || {
                engine
                    .sheet_bounds(sheet_name)
                    .map(|_| engine.config.max_open_ended_rows)
            },
            || {
                engine
                    .sheet_bounds(sheet_name)
                    .map(|_| engine.config.max_open_ended_cols)
            },
            |first, last| engine.used_rows_for_columns(sheet_name, first, last),
            |first, last| engine.used_cols_for_rows(sheet_name, first, last),
        )
    }

    pub(crate) fn get_soft_producers<R: EvaluationContext>(
        engine: &Engine<R>,
        v: VertexId,
    ) -> Vec<VertexId> {
        Self::get_soft_producers_memoized(engine, v, &AnchorRegionMemo::default())
    }

    pub(crate) fn get_soft_producers_memoized<R: EvaluationContext>(
        engine: &Engine<R>,
        v: VertexId,
        memo: &AnchorRegionMemo,
    ) -> Vec<VertexId> {
        let mut out = Vec::new();
        for target in engine.graph.get_dependencies(v) {
            if let Some(cell) = engine.graph.get_cell_ref(target) {
                out.extend(
                    memo.potential_output_anchors(
                        engine,
                        (
                            cell.sheet_id,
                            cell.coord.row(),
                            cell.coord.col(),
                            cell.coord.row(),
                            cell.coord.col(),
                        ),
                    )
                    .iter()
                    .copied(),
                );
            }
        }
        if let Some(ranges) = engine.graph.get_range_dependencies(v) {
            for range in ranges {
                let sheet = engine
                    .graph
                    .sheet_reg()
                    .resolve_locator(&range.sheet, engine.graph.get_vertex_sheet_id(v));
                let Ok(sheet) = sheet else {
                    continue;
                };
                let Some(extent) =
                    Self::resolve_range(engine, engine.graph.sheet_name(sheet), range)
                else {
                    continue;
                };
                out.extend(
                    memo.potential_output_anchors(
                        engine,
                        (
                            sheet,
                            extent.start_row.saturating_sub(1),
                            extent.start_column.saturating_sub(1),
                            extent.end_row.saturating_sub(1),
                            extent.end_column.saturating_sub(1),
                        ),
                    )
                    .iter()
                    .copied(),
                );
            }
        }
        out.retain(|&u| u != v && (engine.graph.is_dirty(u) || engine.graph.is_volatile(u)));
        out.sort_unstable();
        out.dedup();
        out
    }

    pub fn get_virtual_deps<R: EvaluationContext>(
        engine: &Engine<R>,
        v: VertexId,
    ) -> Vec<VertexId> {
        Self::get_virtual_deps_memoized(engine, v, &AnchorRegionMemo::default())
    }

    pub(crate) fn get_virtual_deps_memoized<R: EvaluationContext>(
        engine: &Engine<R>,
        v: VertexId,
        memo: &AnchorRegionMemo,
    ) -> Vec<VertexId> {
        let mut deps = Vec::new();
        // Direct cell reads have physical placeholder dependencies before a spill.
        // Resolve those points to their output producer as well.
        for target in engine.graph.get_dependencies(v) {
            if let Some(cell) = engine.graph.get_cell_ref(target) {
                deps.extend(
                    memo.output_anchors(
                        engine,
                        (
                            cell.sheet_id,
                            cell.coord.row(),
                            cell.coord.col(),
                            cell.coord.row(),
                            cell.coord.col(),
                        ),
                    )
                    .iter()
                    .copied()
                    .filter(|&u| engine.graph.is_dirty(u) || engine.graph.is_volatile(u)),
                );
            }
        }
        if let Some(ranges) = engine.graph.get_range_dependencies(v) {
            let current_sheet_id = engine.graph.get_vertex_sheet_id(v);
            for r in ranges {
                let sheet_id = match r.sheet {
                    formualizer_common::SheetLocator::Id(id) => id,
                    _ => current_sheet_id,
                };
                let sheet_name = engine.graph.sheet_name(sheet_id);

                let Some(extent) = Self::resolve_range(engine, sheet_name, r) else {
                    continue;
                };
                let sr = extent.start_row;
                let sc = extent.start_column;
                let er = extent.end_row;
                let ec = extent.end_column;

                deps.extend(
                    memo.output_anchors(
                        engine,
                        (
                            sheet_id,
                            sr.saturating_sub(1),
                            sc.saturating_sub(1),
                            er.saturating_sub(1),
                            ec.saturating_sub(1),
                        ),
                    )
                    .iter()
                    .copied()
                    .filter(|&u| engine.graph.is_dirty(u) || engine.graph.is_volatile(u)),
                );
                // The in-rect formula-vertex list depends only on the graph's
                // shape, so it is memoised alongside the anchor sets. The
                // `is_dirty`/`is_volatile` and `u != v` filters vary between
                // passes and per candidate, so they stay outside the cache.
                for &u in memo
                    .formula_vertices_in_region(
                        engine,
                        (
                            sheet_id,
                            sr.saturating_sub(1),
                            sc.saturating_sub(1),
                            er.saturating_sub(1),
                            ec.saturating_sub(1),
                        ),
                    )
                    .iter()
                {
                    if (engine.graph.is_dirty(u) || engine.graph.is_volatile(u)) && u != v {
                        deps.push(u);
                    }
                }
            }
        }
        deps.sort_unstable();
        deps.dedup();
        deps
    }

    /// PROTOTYPE (r8a) variant of [`Self::get_virtual_deps_memoized`] that
    /// routes each referenced region through a synthetic region node instead
    /// of emitting one edge per dirty producer per reader.
    ///
    /// Returns the reader's ordinary (non-region) virtual dependencies; the
    /// region nodes the reader must wait for are pushed onto `region_deps`,
    /// and `plan` accumulates each region node's producer list.
    ///
    /// Self-overlap: when the reader itself is one of the region's dirty
    /// producers, routing it through the region node would create a spurious
    /// 2-cycle (`v -> R -> v`). Such a reader keeps the old per-cell edges,
    /// minus itself, exactly as the per-cell path did.
    pub(crate) fn get_virtual_deps_regionized<R: EvaluationContext>(
        engine: &Engine<R>,
        v: VertexId,
        memo: &AnchorRegionMemo,
        plan: &mut RegionNodePlan,
        region_deps: &mut Vec<VertexId>,
    ) -> Vec<VertexId> {
        let mut deps = Vec::new();
        for target in engine.graph.get_dependencies(v) {
            if let Some(cell) = engine.graph.get_cell_ref(target) {
                deps.extend(
                    memo.output_anchors(
                        engine,
                        (
                            cell.sheet_id,
                            cell.coord.row(),
                            cell.coord.col(),
                            cell.coord.row(),
                            cell.coord.col(),
                        ),
                    )
                    .iter()
                    .copied()
                    .filter(|&u| engine.graph.is_dirty(u) || engine.graph.is_volatile(u)),
                );
            }
        }
        if let Some(ranges) = engine.graph.get_range_dependencies(v) {
            let current_sheet_id = engine.graph.get_vertex_sheet_id(v);
            for r in ranges {
                let sheet_id = match r.sheet {
                    formualizer_common::SheetLocator::Id(id) => id,
                    _ => current_sheet_id,
                };
                let sheet_name = engine.graph.sheet_name(sheet_id);

                let Some(extent) = Self::resolve_range(engine, sheet_name, r) else {
                    continue;
                };
                let key: RegionKey = (
                    sheet_id,
                    extent.start_row.saturating_sub(1),
                    extent.start_column.saturating_sub(1),
                    extent.end_row.saturating_sub(1),
                    extent.end_column.saturating_sub(1),
                );

                // Anchor edges are left exactly as they are: they stay
                // per-reader, per-anchor.
                deps.extend(
                    memo.output_anchors(engine, key)
                        .iter()
                        .copied()
                        .filter(|&u| engine.graph.is_dirty(u) || engine.graph.is_volatile(u)),
                );

                let node = plan.intern(engine, memo, key);
                let producers = plan
                    .producers
                    .get(&node)
                    .expect("interned region node has a producer list");
                if producers.is_empty() {
                    continue;
                }
                if producers.binary_search(&v).is_ok() {
                    // Self-overlap: fall back to per-cell edges minus self.
                    deps.extend(producers.iter().copied().filter(|&u| u != v));
                } else {
                    region_deps.push(node);
                }
            }
        }
        deps.sort_unstable();
        deps.dedup();
        deps
    }
}

/// First synthetic region-node id. Real `VertexId`s are allocated densely from
/// zero, so 2^31 leaves the whole real range untouched. [`VirtualDepBuilder::
/// build_regionized`] checks every candidate against it and falls back to the
/// per-cell build (no region nodes at all) if a real id ever reaches it.
pub const REGION_NODE_BASE: u32 = 0x8000_0000;

/// PROTOTYPE (r8a) per-build registry of synthetic region nodes.
///
/// One node per distinct [`RegionKey`] seen in this build, carrying the
/// region's dirty/volatile formula producers. Ids are assigned in first-seen
/// order above [`REGION_NODE_BASE`]; they are meaningful only inside the
/// schedule build that produced them.
#[derive(Default)]
pub struct RegionNodePlan {
    /// region node id -> its dirty producers, ascending.
    pub producers: rustc_hash::FxHashMap<VertexId, Vec<VertexId>>,
    ids: rustc_hash::FxHashMap<RegionKey, VertexId>,
}

impl RegionNodePlan {
    fn intern<R: EvaluationContext>(
        &mut self,
        engine: &Engine<R>,
        memo: &AnchorRegionMemo,
        key: RegionKey,
    ) -> VertexId {
        if let Some(&id) = self.ids.get(&key) {
            return id;
        }
        assert!(
            self.ids.len() < (u32::MAX - REGION_NODE_BASE) as usize,
            "region node id space exhausted"
        );
        let id = VertexId::new(REGION_NODE_BASE + self.ids.len() as u32);
        let producers: Vec<VertexId> = memo
            .formula_vertices_in_region(engine, key)
            .iter()
            .copied()
            .filter(|&u| engine.graph.is_dirty(u) || engine.graph.is_volatile(u))
            .collect();
        self.ids.insert(key, id);
        self.producers.insert(id, producers);
        id
    }

    /// Every region node allocated by this plan, ascending.
    pub fn node_ids(&self) -> Vec<VertexId> {
        let mut ids: Vec<VertexId> = self.producers.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    pub fn is_empty(&self) -> bool {
        self.producers.is_empty()
    }

    /// The build-independent form of this plan: region keys instead of the
    /// per-build node ids. Taken before single-reader inlining so that every
    /// interned region and its producers are present.
    fn canon(
        &self,
        vdeps: &rustc_hash::FxHashMap<VertexId, Vec<VertexId>>,
        region_edges: &rustc_hash::FxHashMap<VertexId, Vec<VertexId>>,
    ) -> RegionCanon {
        let key_of: rustc_hash::FxHashMap<VertexId, RegionKey> =
            self.ids.iter().map(|(k, &id)| (id, *k)).collect();
        let mut regions: rustc_hash::FxHashMap<VertexId, Vec<RegionKey>> =
            rustc_hash::FxHashMap::default();
        for (reader, nodes) in region_edges {
            let mut keys: Vec<RegionKey> = nodes.iter().map(|n| key_of[n]).collect();
            keys.sort_unstable();
            regions.insert(*reader, keys);
        }
        let producers = self
            .producers
            .iter()
            .map(|(id, p)| (key_of[id], p.clone()))
            .collect();
        RegionCanon {
            vdeps: vdeps.clone(),
            regions,
            producers,
        }
    }
}

/// What a region-node build says about every reader, in a form that does not
/// depend on which other readers were in the build: the reader's ordinary
/// (region-free) virtual dependencies before any single-reader inlining, the
/// regions it reads, and each region's dirty producers. Two canons built from
/// the same graph state are equal reader for reader whatever the candidate
/// set, so the post-pass recheck can compare them without a false positive
/// from the inlining heuristic, and a change in a region's producer set is
/// visible as a change for every reader of that region.
#[derive(Default, Clone, Debug)]
pub(crate) struct RegionCanon {
    pub vdeps: rustc_hash::FxHashMap<VertexId, Vec<VertexId>>,
    pub regions: rustc_hash::FxHashMap<VertexId, Vec<RegionKey>>,
    pub producers: rustc_hash::FxHashMap<RegionKey, Vec<VertexId>>,
}

impl RegionCanon {
    /// Readers whose dependencies differ between `self` (the schedule-time
    /// build) and `new` (the post-pass rebuild): a different region-free dep
    /// list, a different region list, or a region whose producers changed.
    /// Only readers in `domain` are compared; a region's producer change is
    /// charged to its readers on either side.
    pub(crate) fn changed_readers(
        &self,
        new: &RegionCanon,
        domain: &rustc_hash::FxHashSet<VertexId>,
    ) -> Vec<VertexId> {
        let mut changed: rustc_hash::FxHashSet<VertexId> = rustc_hash::FxHashSet::default();
        for &v in domain {
            if self.vdeps.get(&v) != new.vdeps.get(&v) || self.regions.get(&v) != new.regions.get(&v) {
                changed.insert(v);
            }
        }
        let mut changed_regions: rustc_hash::FxHashSet<RegionKey> = rustc_hash::FxHashSet::default();
        for (key, producers) in &self.producers {
            if new.producers.get(key) != Some(producers) {
                changed_regions.insert(*key);
            }
        }
        for key in new.producers.keys() {
            if !self.producers.contains_key(key) {
                changed_regions.insert(*key);
            }
        }
        if !changed_regions.is_empty() {
            for canon in [self, new] {
                for (reader, keys) in &canon.regions {
                    if domain.contains(reader) && keys.iter().any(|k| changed_regions.contains(k)) {
                        changed.insert(*reader);
                    }
                }
            }
        }
        let mut out: Vec<VertexId> = changed.into_iter().collect();
        out.sort_unstable();
        out
    }
}

/// Result of [`VirtualDepBuilder::build_regionized`].
pub(crate) struct RegionizedBuild {
    /// Region-free virtual dependencies per reader (anchors, resolved direct
    /// cells, dynamic reads, self-overlap fallbacks and inlined single-reader
    /// regions).
    pub vdeps: rustc_hash::FxHashMap<VertexId, Vec<VertexId>>,
    /// Reader -> region nodes it must wait for (after inlining).
    pub region_edges: rustc_hash::FxHashMap<VertexId, Vec<VertexId>>,
    /// Soft producers admitted by kind, as in [`VirtualDepBuilder::build`].
    pub augmented: Vec<VertexId>,
    /// Region node -> dirty producers (after inlining).
    pub plan: RegionNodePlan,
    /// The build-independent form, taken before inlining.
    pub canon: RegionCanon,
}

/// The old virtual-dependency map a post-pass recheck compares against, plus
/// how it was built, so the rebuild is made in the same form.
#[derive(Default)]
pub(crate) struct VdepSnapshot {
    pub map: rustc_hash::FxHashMap<VertexId, Vec<VertexId>>,
    /// `Some` when `map` came from a region-node build.
    pub canon: Option<RegionCanon>,
}

impl VdepSnapshot {
    pub(crate) fn per_cell(map: rustc_hash::FxHashMap<VertexId, Vec<VertexId>>) -> Self {
        Self { map, canon: None }
    }
}

impl std::ops::Deref for VdepSnapshot {
    type Target = rustc_hash::FxHashMap<VertexId, Vec<VertexId>>;
    fn deref(&self) -> &Self::Target {
        &self.map
    }
}


/// Memo for declared-output anchor-region lookups, held by one
/// [`VirtualDepBuilder`] and reused across every `build` pass it performs.
///
/// `output_anchors_in_region` walks the sheet's `declared_output_rows`
/// interval tree, and `potential_output_anchors_in_region` walks it twice.
/// A workbook with tens of thousands of single-cell CSE fences makes each walk
/// return a large candidate list, and a build asks the same question once per
/// range read — on the Rev FIA child, hundreds of thousands of reads over a
/// handful of distinct blocks. The graph cannot change during a build (the
/// builder holds `&Engine`), so the answer for a region is stable and is
/// computed once per distinct region instead of once per read.
///
/// The memo deliberately caches only the RAW anchor sets. Callers filter them
/// by `is_dirty`/`is_volatile` afterwards, and those flags do change between
/// builds, so the filter must stay outside the cache.
///
/// One schedule build runs the builder many times — once over the candidate
/// set, once per vertex visited by `build_demand_subgraph`, and once more over
/// the augmented candidates — and those passes ask about the same regions over
/// and over. The memo therefore outlives a single pass and is invalidated by
/// epoch rather than by scope: [`Self::refresh`] drops every cached answer as
/// soon as the engine's topology epoch or the graph's output-footprint epoch
/// moves, i.e. as soon as a fence is registered or withdrawn, a spill
/// footprint is committed or cleared, or the vertex set changes. A memo whose
/// epochs still match cannot hold a stale answer.
///
/// Cached answers are capped: once the retained vertex ids reach
/// [`ANCHOR_MEMO_MAX_IDS`] nothing further is inserted, so a build that walks
/// a very large number of distinct regions degrades to the unmemoised cost
/// instead of growing without bound.
pub(crate) struct AnchorRegionMemo {
    anchors: std::cell::RefCell<rustc_hash::FxHashMap<RegionKey, std::sync::Arc<[VertexId]>>>,
    potential: std::cell::RefCell<rustc_hash::FxHashMap<RegionKey, std::sync::Arc<[VertexId]>>>,
    /// Raw in-rect formula-vertex lists (kind-filtered only) for the
    /// dirty-formula region scan; see [`Self::formula_vertices_in_region`].
    region_formulas:
        std::cell::RefCell<rustc_hash::FxHashMap<RegionKey, std::sync::Arc<[VertexId]>>>,
    /// `(topology_epoch, output_footprint_epoch)` the cached answers belong
    /// to; `None` until the first refresh.
    epochs: std::cell::Cell<Option<(u64, u64)>>,
    cached_ids: std::cell::Cell<usize>,
}

/// Cached vertex ids retained by one memo before it stops inserting.
const ANCHOR_MEMO_MAX_IDS: usize = 4 << 20;

pub(crate) type RegionKey = (SheetId, u32, u32, u32, u32);

impl Default for AnchorRegionMemo {
    fn default() -> Self {
        Self {
            anchors: std::cell::RefCell::new(rustc_hash::FxHashMap::default()),
            potential: std::cell::RefCell::new(rustc_hash::FxHashMap::default()),
            region_formulas: std::cell::RefCell::new(rustc_hash::FxHashMap::default()),
            epochs: std::cell::Cell::new(None),
            cached_ids: std::cell::Cell::new(0),
        }
    }
}

impl AnchorRegionMemo {
    /// Drop every cached answer if anything that could change one has moved.
    pub(crate) fn refresh<R: EvaluationContext>(&self, engine: &Engine<R>) {
        let now = (
            engine.current_topology_epoch(),
            engine.graph.output_footprint_epoch(),
        );
        if self.epochs.get() == Some(now) {
            return;
        }
        self.anchors.borrow_mut().clear();
        self.potential.borrow_mut().clear();
        self.region_formulas.borrow_mut().clear();
        self.cached_ids.set(0);
        self.epochs.set(Some(now));
    }

    fn record(&self, len: usize) -> bool {
        let next = self.cached_ids.get().saturating_add(len);
        if next > ANCHOR_MEMO_MAX_IDS {
            return false;
        }
        self.cached_ids.set(next);
        true
    }

    pub(crate) fn output_anchors<R: EvaluationContext>(
        &self,
        engine: &Engine<R>,
        key: RegionKey,
    ) -> std::sync::Arc<[VertexId]> {
        if let Some(hit) = self.anchors.borrow().get(&key) {
            return std::sync::Arc::clone(hit);
        }
        let computed: std::sync::Arc<[VertexId]> = engine
            .graph
            .output_anchors_in_region(key.0, key.1, key.2, key.3, key.4)
            .into();
        if self.record(computed.len()) {
            self.anchors
                .borrow_mut()
                .insert(key, std::sync::Arc::clone(&computed));
        }
        computed
    }

    pub(crate) fn potential_output_anchors<R: EvaluationContext>(
        &self,
        engine: &Engine<R>,
        key: RegionKey,
    ) -> std::sync::Arc<[VertexId]> {
        if let Some(hit) = self.potential.borrow().get(&key) {
            return std::sync::Arc::clone(hit);
        }
        let computed: std::sync::Arc<[VertexId]> = engine
            .graph
            .potential_output_anchors_in_region(key.0, key.1, key.2, key.3, key.4)
            .into();
        if self.record(computed.len()) {
            self.potential
                .borrow_mut()
                .insert(key, std::sync::Arc::clone(&computed));
        }
        computed
    }

    /// Every formula vertex (scalar or array) placed inside the rect, in
    /// ascending vertex order.
    ///
    /// The dirty-formula region scan in
    /// [`RangeVirtualDepProvider::get_virtual_deps_memoized`] walks the whole
    /// column band returned by `vertices_in_col_range` and row-filters it, for
    /// every range read; on the Rev FIA child that band is the source of the
    /// great majority of the virtual-dependency edges, and the three build
    /// passes of one schedule repeat it over the same regions. Which vertices
    /// lie in a rect and what kind they are cannot change while the builder
    /// holds `&Engine`, and any change to the vertex set moves the topology
    /// epoch, so the raw list is cached exactly like the anchor sets.
    ///
    /// Like the anchor maps, this caches only the RAW list: the caller's
    /// `is_dirty`/`is_volatile` test (which changes between passes) and its
    /// self-exclusion must stay outside the cache.
    pub(crate) fn formula_vertices_in_region<R: EvaluationContext>(
        &self,
        engine: &Engine<R>,
        key: RegionKey,
    ) -> std::sync::Arc<[VertexId]> {
        if let Some(hit) = self.region_formulas.borrow().get(&key) {
            return std::sync::Arc::clone(hit);
        }
        let (sheet_id, sr0, sc0, er0, ec0) = key;
        let mut found: Vec<VertexId> = Vec::new();
        if let Some(index) = engine.graph.sheet_index(sheet_id) {
            for u in index.vertices_in_col_range(sc0, ec0) {
                let Some(pc) = engine.graph.vertex_grid_addr(u) else {
                    continue;
                };
                let row0 = pc.row();
                if row0 < sr0 || row0 > er0 {
                    continue;
                }
                if matches!(
                    engine.graph.get_vertex_kind(u),
                    VertexKind::FormulaScalar | VertexKind::FormulaArray
                ) {
                    found.push(u);
                }
            }
        }
        found.sort_unstable();
        found.dedup();
        let computed: std::sync::Arc<[VertexId]> = found.into();
        if self.record(computed.len()) {
            self.region_formulas
                .borrow_mut()
                .insert(key, std::sync::Arc::clone(&computed));
        }
        computed
    }
}

pub struct VirtualDepBuilder<'a, R: EvaluationContext> {
    engine: &'a Engine<R>,
    /// Shared by every `build` this builder performs; see
    /// [`AnchorRegionMemo`] for how it is invalidated.
    memo: AnchorRegionMemo,
}

impl<'a, R: EvaluationContext> VirtualDepBuilder<'a, R> {
    pub fn new(engine: &'a Engine<R>) -> Self {
        Self {
            engine,
            memo: AnchorRegionMemo::default(),
        }
    }
    pub fn build(
        &self,
        candidates: &[VertexId],
    ) -> (
        rustc_hash::FxHashMap<VertexId, Vec<VertexId>>,
        Vec<VertexId>,
    ) {
        let mut vdeps: rustc_hash::FxHashMap<VertexId, Vec<VertexId>> =
            rustc_hash::FxHashMap::default();
        let mut augmented_vertices: Vec<VertexId> = Vec::new();

        // The virtual dependencies this pass produces describe the output
        // footprint as it stands now; the post-pass recheck compares against
        // it to decide whether anything could have changed them.
        self.engine.record_vdep_build_footprint_epoch();
        // One memo for every pass this builder runs; `refresh` drops its
        // contents if a fence or the graph moved since the previous pass.
        self.memo.refresh(self.engine);
        let memo = &self.memo;
        for &v in candidates {
            augmented_vertices.extend(RangeVirtualDepProvider::get_soft_producers_memoized(
                self.engine,
                v,
                memo,
            ));
            let mut deps =
                RangeVirtualDepProvider::get_virtual_deps_memoized(self.engine, v, memo);
            let dynamic_deps = DynamicRefVirtualDepProvider::get_virtual_deps(self.engine, v);

            deps.extend(dynamic_deps);
            deps.sort_unstable();
            deps.dedup();

            if !deps.is_empty() {
                vdeps.insert(v, deps);
            }
        }

        augmented_vertices.sort_unstable();
        augmented_vertices.dedup();
        (vdeps, augmented_vertices)
    }

    /// PROTOTYPE (r8a): as [`Self::build`], but the dirty-producer edges of a
    /// referenced region are emitted once per region (producer -> region node)
    /// plus one edge per reader (region node -> reader), instead of once per
    /// (reader, producer) pair.
    ///
    /// Returns `(vdeps, region_edges, augmented, plan)` where `vdeps` is the
    /// ordinary region-free virtual-dependency map (anchors, resolved direct
    /// cells, dynamic reads, and the per-cell fallback for self-overlapping
    /// readers), `region_edges` maps a reader to the region nodes it must wait
    /// for, and `plan` owns each region node's producer list.
    pub(crate) fn build_regionized(&self, candidates: &[VertexId]) -> RegionizedBuild {
        // Real ids are allocated densely from zero; a candidate at or above
        // the synthetic base would be misread as a relay by the scheduler, so
        // such a build takes the per-cell path with no region nodes at all.
        if candidates.iter().any(|v| v.0 >= REGION_NODE_BASE) {
            let (vdeps, augmented) = self.build(candidates);
            return RegionizedBuild {
                canon: RegionCanon {
                    vdeps: vdeps.clone(),
                    ..RegionCanon::default()
                },
                vdeps,
                region_edges: rustc_hash::FxHashMap::default(),
                augmented,
                plan: RegionNodePlan::default(),
            };
        }
        let mut vdeps: rustc_hash::FxHashMap<VertexId, Vec<VertexId>> =
            rustc_hash::FxHashMap::default();
        let mut region_edges: rustc_hash::FxHashMap<VertexId, Vec<VertexId>> =
            rustc_hash::FxHashMap::default();
        let mut augmented_vertices: Vec<VertexId> = Vec::new();
        let mut plan = RegionNodePlan::default();

        self.engine.record_vdep_build_footprint_epoch();
        self.memo.refresh(self.engine);
        let memo = &self.memo;
        for &v in candidates {
            augmented_vertices.extend(RangeVirtualDepProvider::get_soft_producers_memoized(
                self.engine,
                v,
                memo,
            ));
            let mut regions: Vec<VertexId> = Vec::new();
            let mut deps = RangeVirtualDepProvider::get_virtual_deps_regionized(
                self.engine,
                v,
                memo,
                &mut plan,
                &mut regions,
            );
            let dynamic_deps = DynamicRefVirtualDepProvider::get_virtual_deps(self.engine, v);

            deps.extend(dynamic_deps);
            deps.sort_unstable();
            deps.dedup();

            if !deps.is_empty() {
                vdeps.insert(v, deps);
            }
            if !regions.is_empty() {
                regions.sort_unstable();
                regions.dedup();
                region_edges.insert(v, regions);
            }
        }

        let canon = plan.canon(&vdeps, &region_edges);

        // A region with a single reader gains nothing from the relay hop: the
        // edge count is the same either way and the extra node only lengthens
        // the layer chain. Inline those back into the reader's own deps and
        // drop the node. (On the Rev child 60 of 96 regions are single-reader.)
        // A node with no readers at all — every reader of it fell back to the
        // self-overlap path — is dropped outright.
        let mut readers_of: rustc_hash::FxHashMap<VertexId, Vec<VertexId>> =
            rustc_hash::FxHashMap::default();
        for (reader, regions) in region_edges.iter() {
            for &node in regions {
                readers_of.entry(node).or_default().push(*reader);
            }
        }
        plan.producers.retain(|node, producers| {
            match readers_of.get(node).map(|r| r.len()).unwrap_or(0) {
                0 => false,
                1 => {
                    let reader = readers_of[node][0];
                    let deps = vdeps.entry(reader).or_default();
                    deps.extend(producers.iter().copied().filter(|&u| u != reader));
                    deps.sort_unstable();
                    deps.dedup();
                    if let Some(regions) = region_edges.get_mut(&reader) {
                        regions.retain(|r| r != node);
                    }
                    false
                }
                _ => true,
            }
        });
        region_edges.retain(|_, regions| !regions.is_empty());

        augmented_vertices.sort_unstable();
        augmented_vertices.dedup();
        RegionizedBuild {
            vdeps,
            region_edges,
            augmented: augmented_vertices,
            plan,
            canon,
        }
    }
}

pub struct DynamicRefVirtualDepProvider;

impl DynamicRefVirtualDepProvider {
    fn collect<R: EvaluationContext>(
        engine: &Engine<R>,
        v: VertexId,
    ) -> (Vec<VertexId>, Vec<Region>) {
        if !engine.graph.is_dynamic(v) {
            return (Vec::new(), Vec::new());
        }
        let Some(ast_id) = engine.graph.get_formula_id(v) else {
            return (Vec::new(), Vec::new());
        };
        let sheet_id = engine.graph.get_vertex_sheet_id(v);
        let sheet_name = engine.graph.sheet_name(sheet_id);
        let collector = DynamicRefCollector::new(engine, sheet_name);
        let cell_ref = engine
            .graph
            .get_cell_ref(v)
            .unwrap_or_else(|| engine.graph.make_cell_ref(sheet_name, 0, 0));
        let interpreter = Interpreter::new_with_cell(&collector, sheet_name, cell_ref);
        let _ = interpreter.evaluate_arena_ast(
            ast_id,
            engine.graph.data_store(),
            engine.graph.sheet_reg(),
        );
        let mut deps = collector
            .collected
            .lock()
            .unwrap()
            .iter()
            .copied()
            .filter(|&dependency| dependency != v)
            .collect::<Vec<_>>();
        deps.sort_unstable();
        deps.dedup();
        let mut regions = collector
            .collected_regions
            .lock()
            .unwrap()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        regions.sort_by_key(|region| {
            let (rows, cols) = region.axis_ranges();
            (region.sheet_id(), rows.query_bounds(), cols.query_bounds())
        });
        regions.dedup();
        (deps, regions)
    }

    pub fn get_virtual_deps<R: EvaluationContext>(
        engine: &Engine<R>,
        v: VertexId,
    ) -> Vec<VertexId> {
        Self::collect(engine, v).0
    }

    pub(crate) fn get_virtual_regions<R: EvaluationContext>(
        engine: &Engine<R>,
        v: VertexId,
    ) -> Vec<Region> {
        Self::collect(engine, v).1
    }
}
