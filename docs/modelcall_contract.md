# Model-call calculation path: Lane 0 contract (GOD-383)

Crate `crates/formualizer-modelcall`, Python class `formualizer.ModelSession`.
This is the Rust port of the parity repository's `workbook_runtime` request
core (`runtime.py`, `callbacks.py`, `ports.py`, `prefetch.py`) and of the goal
seek in `replayer/engine_adapter.py`. Parity base `a0ed5bf`, fork base `1e408baf`.
Behaviour is defined by those Python files; where this document and the Python
disagree, the Python wins and this document is corrected.

## Names

- Our function name is `MDL.CALLMODEL`. The imported spellings
  `_xldudf_CS_SPARK_XCALL` and `CS.SPARK.XCALL` appear in Rust only in
  `import_boundary::IMPORTED_CALL_NAMES`; in Python only in
  `workbook_runtime/import_boundary.py`. Registration
  (`register_import_aliases`) and the sibling-plan formula scan read that table.
- The goal-seek block prefix (`Xsolve_`) is `import_boundary::GOAL_SEEK_BLOCK_PREFIX`.
  Our term is goal seek (`goal_seek`, `GoalSeekSpec`); settings are `method`,
  `formula_cell`, `target_value`, `variable_cell`, `max_change`,
  `max_iterations`, `initial_value`, `lower_bound`, `upper_bound`, `run_if`.
- Code renames: `XcallRequestMemo` -> `ModelCallMemo`, `ChildRouter` ->
  `ModelCallRouter`, `xcall_memo` (code) -> call memo. The **receipt key stays
  `xcall_memo`** this round (CONTRACT amendment 5).

## Receipt keys (byte-compatible)

`ModelSession.calculate(inputs)` returns a dict with exactly these keys
(`receipt::result_keys`). The Python shim seals them into `RunSnapshot` as
today; the right-hand column is the sealed name.

| Result key | Sealed `RunSnapshot` field | Notes |
|---|---|---|
| `outputs` | `outputs` | `ports.read_outputs(trim_trailing_null_rows=...)` |
| `typed_outputs` | `typed_outputs` | dropped from `to_dict` when empty |
| `effective_inputs` | `effective_inputs` | `ports.read_inputs()` after evaluation |
| `invocations` | `child_invocations` | sealed list: executed, then held, then inherited; renumbered |
| `timings` | `timings` | ordered; see below |
| `faults` | (not sealed) | first fault's `error` is the failure text |
| `solvers` | `solver_results` | goal-seek records, each with `workbook` |
| `diagnostics` | `diagnostics` | see below |
| `session_reuse` | `session_reuse` | empty without a pool; dropped when empty |
| `xcall_memo` | `xcall_memo` | memo counters; empty when off/unused; dropped when empty |
| `compiled` | `compiled` | `{calls, routes}` only when the compiled flag is on |
| `report_cells`, `conditional_results`, `formula_counts` | same | Lane I: only when `report_capture` returned them |
| `inspection` | `inspection` | Lane I: only when `inspect` returned something |

**Value form (normative, Lane I, CP1 finding 3).** Every value in the result
dict is `snapshot._plain(value.to_python())` of what the Python runtime held:
int, float, str, bool, None; `{"type": "date" | "datetime" | "time", "value":
isoformat}`; an engine error as the dict `LiteralValue.to_python()` builds
(`{"type": "Error", "kind": "Ref", "message": ...[, "row", "col", "sheet",
"origin_row", "origin_col", "extra"]}`; this is what `_plain` does with a
`LiteralValue` error, not the `{"type": "error", "kind", "display"}` branch,
which only objects with a `.kind` attribute reach); arrays and tuples as lists
(`stack` is a list). Rust: `receipt::plain_value` / `plain_port_value`; the
binding converts with the same rules. **The derived serde of `LiteralValue`,
`ExcelError`, `PortValue` and `ModelCallEvent` (externally tagged,
`{"Int": 1}`, `{"Text": "a/b"}`, `"Empty"`) is non-normative**: it is for
debugging and in-crate tests only and is never a receipt. The hooks receive
native values (`report_capture(workbook, outputs)` gets `literal_to_py`
outputs, as `capture_report` did).

Consumers that must keep working unchanged: `pdf_export/job.py` reads
`sealed["timings"]` (re-emitted as `runtime_<key>`), `sealed.get("session_reuse", {})`,
`sealed.get("xcall_memo", {})`, `len(sealed["child_invocations"])`,
`sealed["diagnostics"]` (the `defaulted_input:` prefix feeds the Default Values
warnings) and `sealed.get("compiled")`; `pdf_export/calculation_api.py` reads
`calculation["timings"]`. `RunSnapshot.sha256` hashes `to_dict()` with sorted
keys, so a key that is absent today must stay absent.

**`timings`** (insertion order): `load_seconds`, `evaluation_seconds`,
`solver_seconds`, `child_seconds`, `preparation_seconds`, `admission_seconds`,
`capture_seconds`, `inspection_seconds` (always, from 0.0); `compiled_seconds`
(compiled flag only); CL-097 int counters `writes_skipped`,
`formula_restores_skipped`, `defaults_not_restored_overwritten`,
`date_writes_skipped` (write record only); `prefetch_count` (int),
`prefetch_seconds`, `prefetch_wait_seconds`, `prefetch_hits` (int) after the
first dispatch; `total_seconds` last, success only.

**`xcall_memo`**: `{enabled: true, hits, misses, stores, bypassed,
not_stored_error, port_keyed}` plus `prefetch: {dispatched, stored, not_stored,
hits[, on_slot]}` when something was dispatched. Empty unless
hits + misses + bypassed > 0.

**`session_reuse`** (session on a `RetainedModel` only): `{pool: true, fresh,
reused, warmed, fresh_count, reused_count, warmed_count}` (identities as
`identity:sha256`, in acquisition order).

**`diagnostics`**: `ignored_input:<name>`; `defaulted_input:<name>` (report
operation only); goal-seek notes (`xsolve_ran:<name> iterations=<n>`,
`xsolve_failed:<name>:<reason>`, `xsolve_skipped:<name>:<reason>`; note text is
receipt content and keeps its spelling this round); report capture notes.

**Invocation events** (`ModelCallEvent`): `index`, `parent`, `target`,
`output`, `stack`, `status`, then as they become known `child`, `inputs`
(casefolded names), `memo_of`, `matrix`, `error`, `returned_error`, `prefetch`
(true on sibling events), `route` (compiled), `inherited_from: warm`,
`held_from: reuse`. Status is one of `started`, `memoized`, `completed`,
`routing_error`, `infrastructure_error`, `inherited`, `held`. Sealing drops
`memo_of`/`inherited_from`/`held_from` from inherited and held copies as
`runtime.sealed_invocations` does.

**Errors**: a routing refusal returns `#REF!` with its message
(`ModelCallError::Routing`); anything else returns `#CALC!` with message
`child infrastructure failure`, is appended to `faults` with `error =
"<Type>: <message>"`, cancels the parent and fails the run after evaluation
(`ModelCallError::Infrastructure`). Placeholders return
`ModelCallError::NotImplemented`, treated as infrastructure.

**Solver records**: converged: `asdict(_SolveRecord)` (`suffix`, `cells`,
`change_cell`, `target_cell`, `target_value`, `root`, `iterations`) +
`status: converged` + `workbook`; failed: `{type: solver_failure, status:
failed, name, reason, category[, exception_type][, diagnostic_observed_cells |
diagnostic_read_error]}` + `workbook`.

## Environment variables

The Rust context never reads the environment; the Python side resolves each
flag with `mdl_env(name)` (parity `workbook_runtime/mdl_env.py`): the new name
if set, else the old name. App variables are not changed this round, so the old
names must keep working.

| New name | Old name (still honoured) | Meaning | Context field |
|---|---|---|---|
| `MDL_CALL_MEMO` | `WORKBOOK_XCALL_MEMO` | memo on unless `0` | `flags.call_memo` |
| `MDL_PREFETCH` | `WORKBOOK_XCALL_PREFETCH` | sibling prefetch when `1` | `flags.prefetch` |
| `MDL_PREFETCH_MAX` | `WORKBOOK_XCALL_PREFETCH_MAX` | flights, default 1 | `flags.prefetch_max` |
| `MDL_PREFETCH_ERRORS` | `WORKBOOK_XCALL_PREFETCH_ERRORS` | admit error results when `1` | `flags.prefetch_errors` |
| `MDL_COMPILED` | `WORKBOOK_XCALL_COMPILED` | compiled child when `1` | `flags.compiled` |
| `MDL_COMPILED_MANIFEST_SHA256` | `WORKBOOK_COMPILED_MANIFEST_SHA256` | compiled manifest pin | Python only |
| `MDL_COMPILED_DIR` | `WORKBOOK_COMPILED_DIR` | compiled artifact dir | Python only |
| `MDL_COMPILED_SOURCE_DIR` | `WORKBOOK_COMPILED_SOURCE_DIR` | compiled source dir | Python only |

Unchanged: `WORKBOOK_SESSION_POOL`, `WORKBOOK_SESSION_WARM`,
`WORKBOOK_SESSION_PERSIST`, `WORKBOOK_SKIP_UNCHANGED_WRITES`
(`flags.skip_unchanged_writes`), `WORKBOOK_PUBLICATION_DESCRIPTOR`, `PDF_EXPORT_*`.

## Context

`ModelSession(package_json, context)`: `context` is a dict or JSON with `now`
(timezone-aware RFC 3339; naive is refused), `operation`
(`client`/`report`/`diagnostic`), `random_seed` (147), `deadline_seconds`
(seconds from construction; Python passes `deadline - time.monotonic()`),
`max_depth` (32, must be >= 1) and `flags` (above). The package JSON is
`spec::ModelPackage`: `package_id`, `parent`, `children` (version id -> spec),
`child_routes` (alias `routes`), `engine_identity`, `report`,
`publication_identity`; each `ModelSpec` has `identity`, `workbook_path`,
`workbook_sha256`, `manifest` (alias `fio_manifest`), `inputs`, `outputs`,
`defaults`, `goal_seek` (alias `solver_blocks`), `descriptor`. Object key order
is kept.

## Reentrancy (binding)

1. The call handler never touches the parent `Workbook` it runs inside. No
   reads, writes, evaluation, registration or unregistration from a handler.
2. A child is always a **separate** `Workbook` instance (fresh load or a pool
   entry), never the parent and never a workbook another active call in the
   stack is using. The caller stack refuses a cycle (`active workbook child
   cycle rejected`) and depth beyond `max_depth` (`maximum child depth exceeded`).
3. The only call allowed on a workbook from inside a handler, or from another
   thread (deadline watchdog, prefetch abandon), is `cancel()`; `CancelToken`
   carries it. After a fault the handler cancels the parent and returns `#CALC!`.
4. Parallel engine evaluation stays off (`enable_parallel = False`); the
   function is registered `thread_safe = False`, `deterministic = true`,
   `volatile = false` (the opposite two for an injected workbook factory).
5. The memo belongs to one request and never crosses a request boundary.

## Run-state seam (Lane I, CP1 finding 2)

An in-line child call runs on the **same request core** as its caller
(`session::RunCore`, the Rust `CalculationSession`): one invocation list, one
memo keyed by caller stack, one fault list, one timings dict, shared through
`Mutex<RunState>`; the caller stack grows by the child's identity. A prefetch
flight is the other seam (`ChildEvaluator`, its own sub-request). Rule
(binding): **no run-state lock (run state, prefetch, pool) is held across
`evaluate`, `calculate_child`, a load, or a hook call**; each lock is taken for
one bookkeeping step and dropped. The only lock held across an evaluation is
the evaluated workbook's own `RwLock`, and a handler never touches that
workbook. Lock order where two nest: run state, then pool. Probe:
`tests/retained_model.rs` (parent -> child -> grandchild -> leaf, leaf called
twice: four events, stack lengths 1, 2, 3, 3, the repeat `memoized` with
`memo_of`; a 0.5 s deadline cancelling mid-leaf returns without a hang).

## Retained model (Lane I, CP1 finding 1)

`formualizer.RetainedModel(package_json, context_json, *,
retain_scenarios=False)` is `sessions.SessionPool` for one package: every model
(parent and children) is loaded once, on first use or by `warm`, keyed by
`(identity, sha256, random_seed)`, and lent to one request at a time (a second
concurrent acquire raises `RuntimeError: retained workbook session is already
acquired: <identity>`). Each entry keeps its call binding (a run only rebinds
the router), its pinned goal-seek cells (`solver_written_cells`: the
`Xsolve_` blocks' rectangles and `By changing` cells, restored before every
re-entry), its port session and CL-097 `WriteRecord`, and the events a later
request may inherit (`warm_invocations`, `warmed`, `retained_scenario`).

- `warm(inputs=None)`: `SessionPool.warm`, children first then the parent
  (parent gets `inputs`, children their defaults), each evaluated once with a
  router of its own, no goal seek, no deadline; returns `{warmed,
  warm_timings, warm_invocations, warm_session_timings, invocations}`.
- `close()` (`discard_all`), `forget_scenarios()`, `stats()`, `len()`.
- `retain_scenarios=True` is the persistent worker (`WORKBOOK_SESSION_PERSIST`):
  a completed request re-points the entries it entered at its sealed events.
- `ModelSession(package_json, context_json, retained=model)`: loads go through
  the pool (`_reuse`: cancel reset, clock, router rebound, pinned cells
  restored, port session re-entered so formula defaults are restored and
  unchanged writes skipped); fresh loads are admitted. Invocations are sealed
  as `runtime.sealed_invocations` (executed, held with `held_from: reuse`,
  inherited with `inherited_from: warm`, renumbered), also in the failure
  evidence. The caller (Python pool) decides `forget_scenarios` / `close`
  after a failure; the session releases every entry it acquired.
- Prefetch flights never use the pool (fresh loads), as before.

## Seams between lanes

- `evaluator::ChildEvaluator` (A implements on its session, B's prefetch calls
  it from flight threads): one child call as its own sub-request, own events,
  own memo, prefetch off; returns `ChildOutcome`.
- `evaluator::CompiledChildHook`: the optional compiled child, consulted before
  engine evaluation.
- `evaluator::SolveModel` (A implements over a `Workbook`, B's goal seek uses it).
- `memo::ModelCallMemo` signatures (A implements, B's prefetch uses `peek_key`,
  `contains`, `adopt`).
- `key` (normalisation, done in Lane 0, unit-tested): `callback_inputs`,
  `memo_token`, `MemoKey`, `matrix_is_memoisable`, `matrix_is_finished`.

## File ownership

| Lane | Files |
|---|---|
| 0 (done) | `crates/formualizer-modelcall/{Cargo.toml, src/lib.rs, context.rs, error.rs, evaluator.rs, event.rs, import_boundary.rs, key.rs, receipt.rs, spec.rs}`, workspace `Cargo.toml`, `docs/modelcall_contract.md`; parity `workbook_runtime/{import_boundary,mdl_env}.py` |
| A | `crates/formualizer-modelcall/src/{session,router,memo,ports}.rs`, `bindings/python/src/{modelcall,lib}.rs`, `bindings/python/Cargo.toml` |
| B | `crates/formualizer-modelcall/src/{prefetch,goal_seek}.rs` |
| I | `crates/formualizer-modelcall/src/{session,router,ports,receipt,event,evaluator,spec,context,error,lib,retained}.rs`, `bindings/python/src/{modelcall,lib,workbook}.rs`, this document, `tests/retained_model.rs` |
| P | `crates/formualizer-modelcall/src/{prefetch,memo,batch_child}.rs` |
| C | parity repository only (`workbook_runtime/*`, `pdf_export/*`, `replayer/xcall.py` rename, tests) |

A change to a Lane 0 file after this commit is a contract change: make it in
one small commit, say so in the lane handoff, and keep it additive where possible.
Lanes may add crate dependencies (A: `formualizer-workbook`, `formualizer-sheetport`;
B: `formualizer-parse`) in `crates/formualizer-modelcall/Cargo.toml`, appending only.

## Lane A additions (contract changes, additive)

- `receipt::PortValue` gains `Row` (a single-row ranged client output) and
  `Table` (header -> value per data row), the two shapes
  `PortSession._project_output` produces.
- Package defaults (`ModelSpec.defaults`) that are native temporal values
  travel as one-key objects `{"$date": "YYYY-MM-DD"}`, `{"$datetime": ISO}`,
  `{"$time": ISO}`; every other default is plain JSON as Python's `json` reads it.
- Python: `ModelSession(package_json, context, *, compiled_child=None)`;
  `calculate(inputs, *, report_prepare=None, report_capture=None)` (inputs a
  dict, request order kept). A failed run raises `ModelCalculationError`
  (`RuntimeError`) with `error_type` (the runtime's Python exception name:
  `TimeoutError`, `CallbackInfrastructureError`, `ValueError`,
  `RequiredSolverFailure`, `ExcelEvaluationError`, ...), `error_message` and
  `evidence` (result dict of the failed run; diagnostics end with
  `Type: message`, as `runtime.calculate` seals them).
- Report capture (operation `report`): `report_prepare(workbook)` runs after
  admission and before evaluation (`prepare_report_conditions`),
  `report_capture(workbook, outputs)` after the outputs are read
  (`capture_report`); `workbook` is a `formualizer.Workbook` over the
  session's own parent. The capture's return is kept as `ModelSession.report`;
  its `diagnostics` join the run's. `ModelSession.workbook()` returns the
  evaluated parent after `calculate` (inspection for `diagnostic`, which the
  Rust session does not capture itself).
- Compiled child: `compiled_child.attempt(identity, workbook_sha256, inputs,
  output_location, stack, xcall)` returns `None` or `(matrix_or_None,
  route_or_None)`; `xcall(target, block, output, *tail)` routes the compiled
  child's own calls with the child's stack; `compiled_child.report()` gives
  `{calls, routes}`.
- (Superseded by Lane I: the session pool / warm, held and inherited events,
  `inspect`, and the CL-097 skip logic on re-entered workbooks are now in
  Rust; see "Retained model".) Still not in Rust: engine identity
  verification, prefetch slot pools.

## Lane I additions (contract changes)

- Python: `ModelSession(package_json, context_json, retained=None, *,
  compiled_child=None)`; `calculate(inputs, report_prepare=None,
  report_capture=None, inspect=None)`; `set_compiled_child(hook)`;
  `workbook()`, `cancel()`, `partial_result()`, `report`.
  `RetainedModel` as above.
- Result values are in the normative plain form (above); `typed_outputs`,
  event `matrix`, `target`, `output`, `inputs` and `returned_error` are no
  longer `LiteralValue` objects.
- `ReportHook` gains `inspect(workbook)` (operation `diagnostic`, after
  capture, timed as `inspection_seconds`, then the deadline is checked).
- `SolveModel::get_formula` is implemented over the workbook, so goal seek
  resolves `Target cell` / `By changing` references.
- `goal_seek::goal_seek_written_cells` (additive) and goal-seek diagnostic
  cell values in plain form.
- `CalculationFlags` unchanged; `skip_unchanged_writes` now also skips on
  re-entered workbooks (`writes_skipped`, `formula_restores_skipped`,
  `defaults_not_restored_overwritten`, `date_writes_skipped`).
