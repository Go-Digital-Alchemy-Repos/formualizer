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

## Architecture B: native compiled workbooks

GOD-383 Amendment 7 (Thomas, 2026-09-28): per-workbook compiled modules are
linked in-process by the Rust `MDL.CALLMODEL` router; engine and compiled modes
mix per workbook with per-workbook fallback. Design:
`artifacts/private/god-383-mdl-calc-path-2026-09-28/round/review/design_fable_architecture_b.md`
(bakeoff). WP0 (this section, `evaluator.rs` seam types, `src/compiled.rs`
skeleton) is a contract change, additive.

### Seam types (`evaluator.rs`, `compiled.rs`)

- `CompiledParent::run(spec, inputs, context, xcall: &mut dyn CompiledXcall)
  -> Result<ParentAttempt, ModelCallError>` and `report()`. `ParentAttempt` is
  `Compiled(CompiledRun)` or `Declined { route }`; `Err` is an infrastructure
  fault, never a decline.
- `CompiledRun { outputs: Vec<(key, ChildMatrix)>, route, stats:
  CompiledRunStats, cells: Box<dyn CompiledCells> }` with
  `read_cells(&[(sheet, row, col)])` (1-based, `Workbook::get_value`
  addressing). Dropping it frees the module run (`cv_run_free`).
- `CompiledXcall::call(target, block, output, tail) -> Result<ChildMatrix, _>`:
  the nested router the module's call sites use.
- `compiled::NativeCompiledHook` implements `CompiledChildHook` and
  `CompiledParent` over a registry `{workbook_sha256: NativeRegistryEntry
  {native_path, engine_commit, manifest_sha256}}`; `compiled::NativeModule` is
  one loaded cdylib. In WP0 both decline with route
  `engine:native_not_implemented` (`NativeDecline::NotImplemented`), so
  installing the hook changes no result.
- `compiled::abi` holds the `#[repr(C)]` types and entry typedefs below; layout
  is pinned by `abi_layout_is_pinned_on_64_bit`.

### C ABI (`CV_NATIVE_ABI = 1`)

The module is a pyo3-free cdylib (`cv_native`) built from the same `cv_gen`
rlib as the pyo3 module, in the same build, listed in the same manifest, loaded
with `libloading`. The bakeoff copy of this ABI is
`tools/workbook_compiler/rust/cv_py_template/NATIVE_ABI.md`; the two change
together, and a change bumps `CV_NATIVE_ABI`.

Rules: every entry is `extern "C"` with `catch_unwind` inside (a panic is
`CV_ERR_PANIC`); strings are UTF-8 pointer + length; matrices are row-major;
memory the module returns is freed only by the module (`cv_meta_json` is static);
memory the host passes (ports, xcall answers) is borrowed for the call and
copied. `cv_run` drives `begin_run`, TODAY, `write_ports`, then `run_units`
itself (as `bundlerun_template/main.rs`). A ranged port is passed as
`CV_PORT_ROWS`; the module marks the port supplied (`PortIn::Value(Blank)`, so
its formula-port units are skipped) and `Store::set`s the rows cell by cell
before `run_units`; anything but the exact declared rectangle declines
`admission`.

```c
#define CV_NATIVE_ABI 1u

/* CvVal.tag */
#define CV_TAG_BLANK 0u
#define CV_TAG_NUM   1u
#define CV_TAG_BOOL  2u   /* num is 0.0 (FALSE) or 1.0 (TRUE) */
#define CV_TAG_STR   3u
#define CV_TAG_ERR   4u
/* CvVal.err_code: xlrt_rs::ErrCode in declaration order */
#define CV_XLERR_NA 0u
#define CV_XLERR_VALUE 1u
#define CV_XLERR_DIV0 2u
#define CV_XLERR_REF 3u
#define CV_XLERR_NAME 4u
#define CV_XLERR_NUM 5u
#define CV_XLERR_NULL 6u
#define CV_XLERR_SPILL 7u
#define CV_XLERR_CALC 8u
/* CvPort.kind */
#define CV_PORT_NOT_SUPPLIED 0u
#define CV_PORT_VALUE 1u
#define CV_PORT_ROWS 2u
/* CvArg.kind */
#define CV_ARG_SCALAR 0u
#define CV_ARG_ROWS 1u
/* CvError.code (also the int32 return of cv_run_read_rect / cv_run_stats) */
#define CV_OK 0u
#define CV_ERR_DECLINE 1u    /* reason = decline reason; host records fallback:<reason> */
#define CV_ERR_VIOLATION 2u  /* runtime guard tripped (RunError::Violation); unit set */
#define CV_ERR_XCALL 3u      /* host callback returned non-zero (RunError::Xcall); unit set */
#define CV_ERR_PANIC 4u      /* panic caught inside the entry */
#define CV_ERR_ARGS 5u       /* null pointer, port count, sheet index, rectangle */
#define CV_ERROR_REASON_CAP 64

typedef struct CvVal {        /* 40 bytes on 64-bit */
    uint32_t tag;
    double num;
    const uint8_t *str_ptr;   /* UTF-8, not NUL-terminated; NULL unless CV_TAG_STR */
    size_t str_len;
    uint32_t err_code;        /* only for CV_TAG_ERR */
} CvVal;

typedef struct CvMatrix {     /* row-major rows*cols; 32 bytes */
    size_t rows;
    size_t cols;
    CvVal *vals;
    void *owner;              /* module allocation cookie for cv_matrix_free; NULL if host-owned */
} CvMatrix;

typedef struct CvPort {       /* one input port, PORT_NAMES order; 80 bytes */
    uint32_t kind;            /* CV_PORT_* */
    CvVal value;              /* CV_PORT_VALUE */
    CvMatrix rows;            /* CV_PORT_ROWS: must be the exact declared rectangle, else decline admission */
} CvPort;

typedef struct CvArg {        /* nested MDL.CALLMODEL argument (xlrt_rs::Arg); 80 bytes */
    uint32_t kind;            /* CV_ARG_* */
    CvVal value;
    CvMatrix rows;
} CvArg;

typedef struct CvError {      /* host-allocated, module-filled, value-free; 88 bytes */
    uint32_t code;            /* CV_OK / CV_ERR_* */
    int64_t unit;             /* failing unit for VIOLATION / XCALL, else -1 */
    uint32_t reason_len;
    uint8_t reason[CV_ERROR_REASON_CAP];
} CvError;

typedef struct CvRunStats {   /* 48 bytes */
    uint64_t xcalls;
    uint64_t guard_views;
    uint64_t guard_probes;
    int64_t guard_max_row_minus_limit;  /* INT64_MIN when no guard view ran */
    double t_fresh_s;
    double t_run_s;
} CvRunStats;

typedef struct CvRun CvRun;   /* opaque: the run's store and string table */

/* args = [target, block, output, tail...]; 0 with *out filled (host-owned, valid until return;
   the module copies it), non-zero on failure (the host keeps its own error in ctx). */
typedef int32_t (*CvXcallFn)(void *ctx, const CvArg *args, size_t n_args, CvMatrix *out);

uint32_t cv_native_abi(void);
const uint8_t *cv_meta_json(size_t *len);            /* static META_JSON; never freed */
CvRun *cv_run(const CvPort *ports, size_t n_ports, double today,
              CvXcallFn xcall /* nullable */, void *ctx, CvError *err);   /* NULL on failure, err filled */
int32_t cv_run_read_rect(const CvRun *run, uint32_t sheet_index,
                         uint32_t r1, uint32_t c1, uint32_t r2, uint32_t c2,
                         CvMatrix *out);             /* 1-based inclusive; out module-owned */
int32_t cv_run_stats(const CvRun *run, CvRunStats *out);
void cv_matrix_free(CvMatrix *matrix);
void cv_run_free(CvRun *run);
```

### Value law (host side, from `compiled/adapter.py`; receipts stay byte-identical)

- Inputs (LiteralValue -> Val): Empty -> Blank; Int / Number -> Num(f64);
  Boolean -> Bool; Text -> Str; anything else declines `admission`.
- Outputs (Val -> LiteralValue): Blank -> Empty; Num non-finite declines
  `non_finite`; Err `#NUM!` / `#N/A` -> Error(Num / Na), kind only; any other
  Err declines `output_lane`.
- Nested-call results into the module: Int declines `xcall_lane`; Error,
  Pending, Array element, or a non-array answer declines `xcall_error`.
- Module -> router arguments: `CV_ARG_SCALAR` -> its LiteralValue;
  `CV_ARG_ROWS` -> `LiteralValue::Array`.
- Route strings are exactly `compiled`, `engine:<reason>` (no module
  attempted) and `fallback:<reason>` (attempted, discarded). Fault and
  deadline semantics are `CompiledRoute.attempt`'s: a router fault during the
  run (the fault count grew) is an infrastructure error, not a decline; a
  deadline passed after the run is `fallback:deadline`.

### Pin registry `compiled-pins-1`

A content-addressed bucket document `products/<P>/<V>/compiled/pins/<sha256>.json`,
pinned by the existing per-deploy env var (its value becomes the document's
sha256). Trust chain: env -> pins document -> `manifest.json` sha -> file
digests. With no pins document the loader's single-manifest probing is the
fallback (today's Rev/Enduris/KH/DG pins keep working).

```json
{
  "schema": "compiled-pins-1",
  "workbooks": {
    "<workbook_sha256>": {
      "manifest_sha256": "<sha256 of the artifact manifest.json>",
      "kind": "<artifact kind>",
      "role": "child | parent",
      "generator_sha256": "<rsgen generator sha256>",
      "runtime_version": "<compiled adapter runtime_version>",
      "engine_commit": "<fork commit the artifact was differentially tested against>",
      "platform": "<target triple>",
      "native_abi": 1
    },
    "<workbook_sha256>": "engine"
  }
}
```

The literal `"engine"` records a deliberate engine decision; an absent workbook
is engine. `child_routes` (target -> version -> sha) stays the only version
concept; the registry is keyed by sha alone. `role: parent` supersedes
`MDL_WHOLE_MODEL_MANIFEST*`. The parity loader hands the Rust hook a value-free
`native_registry_json` (`{sha: {native_path, engine_commit, manifest_sha256}}`)
through `ModelSession.set_native_compiled`; the hook never reads the bucket or
the environment.

### read_rect and `CompiledCells` (report rule)

`cv_run_read_rect` is the ABI primitive; `CompiledRun::read_cells` groups
addresses by sheet and reads each bounding rectangle once. The binding exposes
a pyclass `formualizer.CompiledCells` with `get_value(sheet, row, col)` so
`pdf_export.snapshot.capture_report` and `read_conditions` run unchanged.
A compiled parent serves `report` only when the template's `condition_plans`
has no non-simple-equality expressions (then `conditional_results` is `{}` per
sheet); otherwise it falls back with `engine:report_conditions`. `diagnostic`
runs fall back too (no compiled `inspect_cell`). Compiled parents are admitted
only without goal seek and with `date_system == 1900` (descriptor
`calculation_normalizations` do not refuse a parent: see "Compiled parent in the
session"); any decline discards the attempt, records
`fallback:<reason>` under a `parent` key of `compiled.routes` and builds a
fresh engine `RunCore`. A compiled parent takes no pool entry.

### Compiled parent in the session (item (c), package C)

This subsection is normative for `session.rs`, `ports.rs`, `receipt.rs` and
the binding; it supersedes the one-line description in "read_rect and
`CompiledCells`" above where they differ (the parent route is a `parent` key
of the `compiled` map, a sibling of `routes`, not an entry of the `routes`
list).

**When the parent is attempted.** `ModelSession::calculate` consults the
installed `CompiledParent` (`set_compiled_parent`; the binding's
`set_native_compiled` installs the native hook as both child hook and parent)
before any engine parent is loaded, in this order:

1. No parent hook, `flags.compiled` off, or `hook.serves(parent.workbook_sha256)`
   false: nothing changes (no `parent` key; the engine parent as before). The
   native hook serves only a registry entry with `role: parent`, so a
   child-only registry records no parent route.
2. Static refusals, route `engine:<reason>`, the module is not run:
   `goal_seek` (the spec has goal-seek blocks); operation `report` when
   `hook.report_conditions_simple(parent.workbook_sha256)` is false ->
   `report_conditions` (the native hook reads the parent entry's
   `report_conditions_simple`, which the parity loader writes from
   `pdf_export.compiled_report.conditions_rule`; absent = false; this is the
   only report rule, there is no per-call override), or with
   a report hook that cannot capture compiled cells -> `report_capture`;
   operation `diagnostic` -> `operation:diagnostic`; `date_system` (not
   1900); `port_contract` (a record or table input, or a
   `table` output). A range input is passed as an array of its full declared
   rectangle (the host's ranged-port law).
3. Admission without a workbook (`ports::admit_scenario`, the engine path's
   admission verbatim). An admission error -> `engine:admission:invalid` (the
   engine parent then raises the error with its own text); a value with no
   module form -> `engine:admission:value_type`.
4. The module runs with the session's router as its `MDL.CALLMODEL` handler
   (`ModelCallRouter::nested(core, [parent identity])`): children are
   compiled or engine through the child hook, memo and prefetch unchanged, so
   invocations carry stack `[parent]` exactly as on the engine parent.
   Module inputs follow the Lane D parent law (`ports::parent_port_literal`):
   a `date`/`datetime` is its 1900 serial computed as Python's
   `parent_port_value` does (`(value - 1899-12-30)` in days, seconds and
   microseconds), an int is a float, `None` is blank; a range port's rows
   become a `LiteralValue::Array` under the same law.
5. After the run: a nested-call fault recorded during the run fails the
   request (`CallbackInfrastructureError`, route `fallback:fault`); a hook
   error fails it too (`fallback:deadline` for a `TimeoutError`, else
   `fallback:error`); a `Declined { route }` discards the attempt; a deadline
   passed after the run -> `fallback:deadline` (the engine parent then fails at
   its first deadline check).
6. Projection: outputs from `CompiledRun.outputs` (by casefolded output key;
   a missing key is read from the cells), effective inputs read from the
   run's cells at the input rectangles, each turned into the value SheetPort
   would read for the port (`ports::port_value_from_grid`) and projected with
   the engine path's own code (`ports::project_outputs`, `project_inputs`:
   date contracts, `_project_output`, record rows). A port that cannot be
   read -> `fallback:projection`.

**Temporal typing rule (F6).** The compiled store holds serials; the engine's
`get_value` types a date-formatted cell through its temporal egress. For the
receipt the session types a Number as the engine would where the spec says
what the cell is: an input the admission wrote as a `date` / `datetime` reads
back as `Date` / `DateTime` (the engine formats the cell on that write); a
Number in a location's `date_fields` (cells with a date number format,
`package.py`) is `Date` for a whole serial and `DateTime` otherwise, through
the engine's own `try_serial_to_date_for` / `try_serial_to_datetime_for`
(`ports::engine_temporal`). Known gaps: the format's class (Date, DateTime,
Time) is not in the spec, and a formula cell whose date format the engine
derives (no style) is not in `date_fields`; such cells stay Numbers. Lane D's
serial counts from 1899-12-30 for every date, so a date before 1900-03-01
reaches the module one day above the engine's serial.

`formualizer.CompiledCells.get_value(sheet, row, col)` (report capture)
returns None / float / bool / str / the error `Workbook.get_value` returns,
with no temporal typing (the report cells have no `date_fields`). A gate
comparing report cells normalises the engine side with
`CompiledCells.serial(value)`: a `date`, `datetime`, `time` or `timedelta`
becomes its 1900-system serial by the engine's conversion, anything else is
returned unchanged. Compiled errors are kind-only (no message).

**Decline and fallback order.** Any decline discards the attempt: its core is
closed, its child routes are dropped from the child hook's `routes`/`calls`
in the receipt, a fresh request core is built and the engine parent runs as
before (retained pool, CL-097, goal seek). `report_prepare` runs only on an
engine parent (F1): never on a compiled run, always on the fallback engine
parent. For a compiled parent report run the report hook's `capture` receives
the run's cells (`ReportHook::capture_compiled`; the binding passes a
`formualizer.CompiledCells` to `report_capture`), after the output reads, as
on the engine path.

**Receipt (`compiled` map, `receipt::compiled_keys`).** When a parent hook
serves the parent: `parent` = `compiled` | `engine:<reason>` |
`fallback:<reason>`; `parent_loaded` = whether an engine parent was loaded
(`false` only for `compiled`); on a compiled run `parent_xcalls` =
`CompiledRunStats.xcalls`. A compiled parent takes no pool entry, so
`session_reuse` has no parent entry (`fresh`/`reused` list only children),
`load_seconds`/`evaluation_seconds` hold only child work, and the module's
run time is in `compiled_seconds`. `ModelSession::workbook()` is `None` after
a compiled parent run and `compiled_cells()` holds the run's cells until the
next `calculate`.

**Binding (Python).**
`ModelSession.set_native_compiled(registry_json: str | None) -> None`
(`{workbook_sha256: {native_path, engine_commit, manifest_sha256, role,
report_conditions_simple}}`, value-free; `role` `parent` or `child` (absent =
child), `report_conditions_simple` a bool on the parent entry (absent =
false); unknown fields ignored; None clears child and parent hooks);
`ModelSession.calculate(inputs, report_prepare=None, report_capture=None,
inspect=None) -> dict` (the `report_conditions_ok` keyword of package C's
first binding is removed: the registry entry decides);
`ModelSession.compiled_cells() -> CompiledCells | None`;
`CompiledCells.get_value(sheet, row, col)`, `CompiledCells.get_values([(sheet,
row, col), ...]) -> list`, `CompiledCells.serial(value)` (static).

### Work-package file ownership (no two packages edit the same file)

| WP | Files |
|---|---|
| 0 (done) | fork `crates/formualizer-modelcall/src/{evaluator,lib,compiled}.rs` (compiled.rs skeleton only), this section; bakeoff `tools/workbook_compiler/rust/cv_py_template/NATIVE_ABI.md` |
| 1 | bakeoff `tools/workbook_compiler/rsgen_pyo3.py`, `rust_build.py` (flavour `native`), `rust/cv_native_template/`, their tests |
| 2 | fork `crates/formualizer-modelcall/src/compiled.rs`, `Cargo.toml` (+`libloading`), `tests/native_compiled.rs`, `tests/fixtures/native_stub/` |
| 3 | fork `crates/formualizer-modelcall/src/{session,ports,receipt}.rs`, `bindings/python/src/modelcall.rs` (`set_native_compiled`, `CompiledCells`) |
| 4 | parity `workbook_runtime/compiled/{loader,manifest,hook}.py`, `workbook_runtime/runtime.py`, `workbook_runtime/whole_model.py`, `pdf_export/readiness.py`, tests |
| 5 | bakeoff `tools/workbook_compiler/compiled_pins.py` |

`compiled/adapter.py` and its `runtime_version` are touched by no package.

### Native compiled modules

WP2 host (`src/compiled.rs`, package B of round
god-383-archb-compiled-parent-2026-09-29). Normative for the host side of
`CV_NATIVE_ABI = 1`; the value law above is its contract.

**Module cache (F4).** `NativeModule::cached(sha, entry)` keeps one loaded
module per `workbook_sha256` for the life of the process in
`OnceLock<Mutex<BTreeMap<sha, Arc<NativeModule>>>>`; modules are never
unloaded, so entry points stay valid. A hit must be the same artifact (same
`native_path` and `manifest_sha256`), else `engine:module_identity`. The
cache lock covers lookup and `dlopen` only, never `cv_run`; a compiled
parent's nested call may run another module, or the same one, on the same
thread while the parent's `cv_run` is on the stack. `CompiledRun.cells`
holds an `Arc` to its module and frees its `CvRun` on drop.

**Load checks** (`engine:<reason>`): not in the registry `not_registered`;
entry `engine_commit` differs from `NativeCompiledHook::with_engine_commit`
`engine_mismatch`; library, symbol or `cv_meta_json` failure or inconsistent
tables `import_error`; `cv_native_abi() != 1` `native_abi`; `cv_meta_json`
names another workbook `key_mismatch`. A hook remembers each sha's outcome.

**Eligibility and admission** (adapter.py order, `engine:<reason>`):
`date_system` (not 1900), `descriptor_edits` (truthy
`calculation_normalizations`; child calls only: a parent module is gated in
parent mode against the engine parent with the edits applied, as parity
`whole_model` serves a gated parent, so neither the session plan nor
`CompiledParent::run` refuses a parent on them; integration 2026-09-29),
`solvers` (goal seek), `port_contract` (the
declared inputs are not exactly the module's `port_names` case-folded one to
one, each a scalar `{"type": "any"}` port, or a ranged port with schema
`{"kind": "range", "cell_type": "any"}`, no headers, whose rectangle is the
module's port rectangle), `port_defaults` (a `formula` port must be a
`formula_input_defaults` key without a default, every other port must have a
default; values are compared only when `cv_meta_json` carries
`port_defaults`), `shape_mismatch` (output not exactly one module output with
the same sheet, corners and shape), `admission` (canonical names, unknown-input
policy, defaults merge, then the value law; a ranged port takes only a
`LiteralValue::Array` of exactly its rectangle, cell by cell under the scalar
law; a port the merge leaves out is `CV_PORT_NOT_SUPPLIED`).

**Run outcomes.** TODAY is the UTC whole-day serial of `context.now`. After
the run: deadline passed -> route `fallback:deadline` and
`TimeoutError`; fault count grew (child path) -> `fallback:fault`, which the
session turns into `CallbackInfrastructureError`. Otherwise `CV_ERR_DECLINE`
-> `fallback:<module reason>` (a non-token reason reads `decline`);
`CV_ERR_VIOLATION` -> `fallback:probe_violation`; `CV_ERR_XCALL` ->
`fallback:<handler decline>` (`xcall_lane`, `xcall_error`, `xcall_result`);
`CV_ERR_PANIC`, `CV_ERR_ARGS` or anything else -> `fallback:exception`;
output cells under the output law (`non_finite`, `output_lane`).

**Nested calls (F3).** The module's callback is an `extern "C"` trampoline
with a per-run context. It converts the arguments (scalar -> value, rows ->
`Array`), calls the handler inside `catch_unwind`, converts the answer under
the nested law (a ragged or unreadable answer declines `xcall_result`) and
keeps it alive until the next callback. A handler `Err(Routing)` declines
`xcall_error`; any other `Err`, or a panic (`PanicException`), is stored in
the context, the callback returns non-zero, and the host fails the attempt
with `CallbackInfrastructureError: child callback infrastructure fault:
<error>` (route `fallback:fault`). A stored fault wins over whatever code the
module returns.

**Entry points.** Child: `CompiledChildHook::attempt` uses the thread's
nested router (`current_nested_router`, else `engine:no_router`), its
`fault_count`, and the request context set by `compiled::with_request_context`
(else `engine:no_request_context`); `NativeCompiledHook::attempt_child(spec,
inputs, output, context, xcall, faults)` is the same with explicit parts.
Parent: `CompiledParent::run` sets the request context for its run, reads
every declared output in `spec.outputs` order (casefolded keys) and returns
`CompiledRun { route: "compiled", stats, cells }`. `read_cells` groups
addresses by sheet (exact name, then case-folded), reads each sheet's
bounding rectangle once, and returns values under the plain law (Blank ->
Empty, Num -> Number, Bool -> Boolean, Str -> Text, Err -> Error(kind)); an
unknown sheet, a zero row or column, or a rectangle the module refuses is an
error. Child and parent routes are recorded separately (`CompiledChildHook::report`,
`CompiledParent::report`) and accumulate per hook until `clear_routes()`.

**Tests.** `tests/native_compiled.rs` against `tests/fixtures/native_stub/`
(its own workspace, built by the test into `CARGO_TARGET_TMPDIR` per
`STUB_WORKBOOK_SHA`; the fork's default build never compiles it).
