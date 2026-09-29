//! Architecture B (GOD-383): native compiled workbooks loaded in-process.
//!
//! A generated workbook crate (`cv_gen`) is shipped as a pyo3-free C-ABI
//! cdylib (`cv_native`, bakeoff `tools/workbook_compiler/rust/cv_native_template/`)
//! next to the pyo3 module, keyed by `workbook_sha256` in the `compiled-pins-1`
//! registry. This module holds the host side:
//!
//! * [`abi`]: the `#[repr(C)]` types and entry-point signatures of
//!   `CV_NATIVE_ABI = 1` (mirrored in the bakeoff's
//!   `rust/cv_py_template/NATIVE_ABI.md`; the two must change together);
//! * [`NativeModule`]: one loaded cdylib, held for the life of the process in
//!   a global cache keyed by `workbook_sha256` ([`NativeModule::cached`]);
//! * [`NativeCompiledHook`]: the [`CompiledChildHook`] and [`CompiledParent`]
//!   over a registry of modules, with the value law of the parity
//!   `compiled/adapter.py` (`CompiledRoute.attempt`) so routes and values
//!   match the Python compiled path. `docs/modelcall_contract.md`, section
//!   "Native compiled modules", is the normative text.
//!
//! Reentrancy (contract Amendment 1, F4): a compiled parent's nested call runs
//! the router, which may run another module (or the same one) on the same
//! thread while the parent's `cv_run` is on the stack. No lock is held across
//! `cv_run`; each run has its own callback context; modules are never
//! unloaded, so a function pointer taken from one stays valid.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use chrono::{NaiveDate, Utc};
use formualizer_common::{ExcelError, ExcelErrorKind, LiteralValue};
use libloading::Library;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::evaluator::{
    CellAddress, ChildMatrix, CompiledAttempt, CompiledCells, CompiledChildHook, CompiledParent, CompiledRun,
    CompiledRunStats, CompiledXcall, ParentAttempt,
};
use crate::key::casefold;
use crate::ports::{WireValue, canonical_port_inputs, spec_port_alias_map};
use crate::router::{ModelCallRouter, current_nested_router};
use crate::spec::{ModelSpec, PortLocation};
use crate::{CalculationContext, ModelCallError};


/// The C ABI of a native compiled workbook module, version 1.
///
/// Conventions (binding for both sides):
/// * every entry is `extern "C"` and catches unwinding inside (a panic is
///   reported as [`CV_ERR_PANIC`], never unwinds across the boundary);
/// * strings are UTF-8, pointer + length, not NUL-terminated;
/// * matrices are row-major, `rows * cols` values;
/// * memory the module returns (`cv_meta_json` excepted, which is static) is
///   freed only by the module (`cv_matrix_free`, `cv_run_free`); memory the
///   host passes in (ports, xcall answers) is borrowed for the call and
///   copied by the module before it returns;
/// * rows and columns in `cv_run_read_rect` are 1-based and inclusive;
///   `sheet_index` indexes the module's `SHEETS` table (0-based, the order
///   in `cv_meta_json`'s `sheets`).
#[expect(non_camel_case_types, reason = "entry-point typedefs keep the C symbol names")]
pub mod abi {
    use std::ffi::c_void;

    /// The ABI version this host understands (`cv_native_abi()` must equal it).
    pub const CV_NATIVE_ABI: u32 = 1;

    // CvVal.tag
    pub const CV_TAG_BLANK: u32 = 0;
    pub const CV_TAG_NUM: u32 = 1;
    /// `num` is 0.0 (FALSE) or 1.0 (TRUE).
    pub const CV_TAG_BOOL: u32 = 2;
    pub const CV_TAG_STR: u32 = 3;
    pub const CV_TAG_ERR: u32 = 4;

    // CvVal.err_code: `xlrt_rs::ErrCode` in declaration order.
    pub const CV_XLERR_NA: u32 = 0;
    pub const CV_XLERR_VALUE: u32 = 1;
    pub const CV_XLERR_DIV0: u32 = 2;
    pub const CV_XLERR_REF: u32 = 3;
    pub const CV_XLERR_NAME: u32 = 4;
    pub const CV_XLERR_NUM: u32 = 5;
    pub const CV_XLERR_NULL: u32 = 6;
    pub const CV_XLERR_SPILL: u32 = 7;
    pub const CV_XLERR_CALC: u32 = 8;

    // CvPort.kind
    pub const CV_PORT_NOT_SUPPLIED: u32 = 0;
    pub const CV_PORT_VALUE: u32 = 1;
    pub const CV_PORT_ROWS: u32 = 2;

    // CvArg.kind
    pub const CV_ARG_SCALAR: u32 = 0;
    pub const CV_ARG_ROWS: u32 = 1;

    // CvError.code
    pub const CV_OK: u32 = 0;
    /// The module declined before or during the run; `reason` is the decline
    /// reason (`admission`, ...), the host records `fallback:<reason>`.
    pub const CV_ERR_DECLINE: u32 = 1;
    /// A runtime guard tripped (`RunError::Violation`); `unit` is the unit.
    pub const CV_ERR_VIOLATION: u32 = 2;
    /// The host's xcall callback returned non-zero (`RunError::Xcall`); the
    /// host holds its own error in `ctx`.
    pub const CV_ERR_XCALL: u32 = 3;
    /// A panic was caught inside the entry.
    pub const CV_ERR_PANIC: u32 = 4;
    /// Bad arguments (null pointer, port count, sheet index, rectangle).
    pub const CV_ERR_ARGS: u32 = 5;

    /// Capacity of `CvError.reason` in bytes.
    pub const CV_ERROR_REASON_CAP: usize = 64;

    /// One value (`xlrt_rs::Val`). For `CV_TAG_STR`, `str_ptr`/`str_len` are
    /// the UTF-8 bytes; otherwise null / 0. `err_code` is meaningful only for
    /// `CV_TAG_ERR`.
    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    pub struct CvVal {
        pub tag: u32,
        pub num: f64,
        pub str_ptr: *const u8,
        pub str_len: usize,
        pub err_code: u32,
    }

    /// A row-major matrix. `owner` is the module's allocation cookie for
    /// `cv_matrix_free` (null for a host-owned matrix).
    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    pub struct CvMatrix {
        pub rows: usize,
        pub cols: usize,
        pub vals: *mut CvVal,
        pub owner: *mut c_void,
    }

    /// One input port, in the module's `PORT_NAMES` order.
    /// `CV_PORT_NOT_SUPPLIED`: the default / formula port runs;
    /// `CV_PORT_VALUE`: `value`; `CV_PORT_ROWS`: `rows`, which must be the
    /// port's exact declared rectangle (else decline `admission`).
    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    pub struct CvPort {
        pub kind: u32,
        pub value: CvVal,
        pub rows: CvMatrix,
    }

    /// One argument of a nested `MDL.CALLMODEL` call (`xlrt_rs::Arg`).
    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    pub struct CvArg {
        pub kind: u32,
        pub value: CvVal,
        pub rows: CvMatrix,
    }

    /// Value-free failure record, host-allocated and filled by the module.
    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    pub struct CvError {
        pub code: u32,
        /// The failing unit for `CV_ERR_VIOLATION` / `CV_ERR_XCALL`, else -1.
        pub unit: i64,
        pub reason_len: u32,
        pub reason: [u8; CV_ERROR_REASON_CAP],
    }

    impl Default for CvError {
        fn default() -> Self {
            Self { code: CV_OK, unit: -1, reason_len: 0, reason: [0; CV_ERROR_REASON_CAP] }
        }
    }

    /// Value-free statistics of a run (`cv_run_stats`).
    #[repr(C)]
    #[derive(Debug, Clone, Copy, Default)]
    pub struct CvRunStats {
        pub xcalls: u64,
        pub guard_views: u64,
        pub guard_probes: u64,
        /// `i64::MIN` when no guard view ran.
        pub guard_max_row_minus_limit: i64,
        pub t_fresh_s: f64,
        pub t_run_s: f64,
    }

    /// Opaque finished run (the module's store and string table).
    #[repr(C)]
    pub struct CvRun {
        _private: [u8; 0],
    }

    /// Nested call callback: `args` is `[target, block, output, tail...]`.
    /// Returns 0 with `out` filled (host-owned, valid until the callback
    /// returns; the module copies it), non-zero on failure.
    pub type CvXcallFn =
        unsafe extern "C" fn(ctx: *mut c_void, args: *const CvArg, n_args: usize, out: *mut CvMatrix) -> i32;

    pub type cv_native_abi_fn = unsafe extern "C" fn() -> u32;
    /// The crate's static `META_JSON` (UTF-8, `*len` bytes); never freed.
    pub type cv_meta_json_fn = unsafe extern "C" fn(len: *mut usize) -> *const u8;
    /// One run: fresh store, TODAY = `today`, ports, RUN_ORDER. Null on
    /// failure with `err` filled.
    pub type cv_run_fn = unsafe extern "C" fn(
        ports: *const CvPort,
        n_ports: usize,
        today: f64,
        xcall: Option<CvXcallFn>,
        ctx: *mut c_void,
        err: *mut CvError,
    ) -> *mut CvRun;
    /// Read `[r1..=r2] x [c1..=c2]` of sheet `sheet_index` into `out`
    /// (module-owned; free with `cv_matrix_free`). 0 or a `CV_ERR_*` code.
    pub type cv_run_read_rect_fn = unsafe extern "C" fn(
        run: *const CvRun,
        sheet_index: u32,
        r1: u32,
        c1: u32,
        r2: u32,
        c2: u32,
        out: *mut CvMatrix,
    ) -> i32;
    pub type cv_run_stats_fn = unsafe extern "C" fn(run: *const CvRun, out: *mut CvRunStats) -> i32;
    pub type cv_matrix_free_fn = unsafe extern "C" fn(matrix: *mut CvMatrix);
    pub type cv_run_free_fn = unsafe extern "C" fn(run: *mut CvRun);

    /// Exported symbol names (NUL-terminated, for `libloading`).
    pub const SYM_CV_NATIVE_ABI: &[u8] = b"cv_native_abi\0";
    pub const SYM_CV_META_JSON: &[u8] = b"cv_meta_json\0";
    pub const SYM_CV_RUN: &[u8] = b"cv_run\0";
    pub const SYM_CV_RUN_READ_RECT: &[u8] = b"cv_run_read_rect\0";
    pub const SYM_CV_RUN_STATS: &[u8] = b"cv_run_stats\0";
    pub const SYM_CV_MATRIX_FREE: &[u8] = b"cv_matrix_free\0";
    pub const SYM_CV_RUN_FREE: &[u8] = b"cv_run_free\0";
}


use self::abi::*;

// ------------------------------------------------------------------ declines

/// Why a module could not be used for a workbook. `reason()` is the text
/// after `engine:` in the route record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeDecline {
    /// WP0 skeleton reason; no longer produced (kept for callers that match it).
    NotImplemented,
    /// No registry entry for this workbook_sha256.
    NotRegistered,
    /// The entry's `engine_commit` is not the running engine's
    /// ([`NativeCompiledHook::with_engine_commit`]).
    EngineMismatch,
    /// The library did not load, lacks an entry point, or its `cv_meta_json`
    /// does not parse or is inconsistent.
    Import,
    /// The module reports a different `cv_native_abi()`.
    AbiMismatch { found: u32 },
    /// The module's `cv_meta_json` names another workbook.
    KeyMismatch,
    /// The process already holds a module for this sha from another path or
    /// manifest (the cache never replaces a loaded module).
    ModuleIdentity,
}

impl NativeDecline {
    pub fn reason(&self) -> &'static str {
        match self {
            Self::NotImplemented => "native_not_implemented",
            Self::NotRegistered => "not_registered",
            Self::EngineMismatch => "engine_mismatch",
            Self::Import => "import_error",
            Self::AbiMismatch { .. } => "native_abi",
            Self::KeyMismatch => "key_mismatch",
            Self::ModuleIdentity => "module_identity",
        }
    }

    /// The route record for a call that never reached a module.
    pub fn engine_route(&self) -> Value {
        Value::String(format!("engine:{}", self.reason()))
    }
}

/// One entry of the value-free native registry the parity loader hands to
/// `ModelSession.set_native_compiled` (`{sha: {native_path, engine_commit,
/// manifest_sha256, role, report_conditions_simple}}`).
///
/// `role` is `parent` for the whole-model parent (the only entry
/// [`CompiledParent::serves`] answers true for) and `child` otherwise; an
/// absent role is a child. `report_conditions_simple` (parent entries; parity
/// `pdf_export.compiled_report.conditions_rule`) is the one report rule: a
/// `report` run may use the compiled parent iff it is true (absent = false,
/// the route is then `engine:report_conditions`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct NativeRegistryEntry {
    pub native_path: PathBuf,
    pub engine_commit: String,
    pub manifest_sha256: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub report_conditions_simple: bool,
}

/// The registry role of the whole-model parent entry.
pub const ROLE_PARENT: &str = "parent";

impl NativeRegistryEntry {
    /// True for the `role: parent` entry.
    pub fn is_parent(&self) -> bool {
        self.role.as_deref() == Some(ROLE_PARENT)
    }
}

// ------------------------------------------------------------------ module metadata

/// A rectangle of the module's store: sheet index, 1-based inclusive,
/// normalised so `r1 <= r2` and `c1 <= c2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    sheet: u32,
    r1: u32,
    c1: u32,
    r2: u32,
    c2: u32,
}

impl Rect {
    fn from_meta([sheet, r1, c1, r2, c2]: [u32; 5]) -> Self {
        Self { sheet, r1: r1.min(r2), c1: c1.min(c2), r2: r1.max(r2), c2: c1.max(c2) }
    }

    fn rows(&self) -> usize {
        (self.r2 - self.r1 + 1) as usize
    }

    fn cols(&self) -> usize {
        (self.c2 - self.c1 + 1) as usize
    }

    /// Same sheet and rectangle as a declared port location.
    fn matches(&self, meta: &NativeMeta, location: &PortLocation) -> bool {
        let range = &location.range;
        meta.sheets.get(self.sheet as usize).is_some_and(|sheet| *sheet == range.sheet)
            && (self.r1, self.c1, self.r2, self.c2) == (range.start_row, range.start_col, range.end_row, range.end_col)
    }
}

/// The fields of the module's `cv_meta_json` (`god383-rs-cv-meta-1`) the host
/// uses. Rectangles are `[sheet_index, r1, c1, r2, c2]`.
#[derive(Debug, Clone, Deserialize)]
struct NativeMeta {
    workbook_sha256: String,
    sheets: Vec<String>,
    port_names: Vec<String>,
    ports: BTreeMap<String, [u32; 5]>,
    port_default_kind: Vec<String>,
    outputs: BTreeMap<String, [u32; 5]>,
    output_shape: BTreeMap<String, [u32; 2]>,
    /// Constant port defaults by port name, when the module publishes them;
    /// then `port_defaults` also compares values (adapter.py `port_defaults`).
    #[serde(default)]
    port_defaults: Option<BTreeMap<String, Value>>,
}

impl NativeMeta {
    /// The tables agree with each other (the pyo3 binding's `check_meta`).
    fn consistent(&self) -> bool {
        let n_sheets = self.sheets.len();
        let in_bounds = |rect: &[u32; 5]| {
            (rect[0] as usize) < n_sheets && rect[1] > 0 && rect[2] > 0 && rect[3] > 0 && rect[4] > 0
        };
        self.port_names.len() == self.ports.len()
            && self.port_default_kind.len() == self.port_names.len()
            && self.port_names.iter().all(|name| self.ports.get(name).is_some_and(in_bounds))
            && self.outputs.iter().all(|(name, rect)| {
                let shape = Rect::from_meta(*rect);
                in_bounds(rect)
                    && self.output_shape.get(name).is_some_and(|&[rows, cols]| {
                        (rows as usize, cols as usize) == (shape.rows(), shape.cols())
                    })
            })
    }

    fn sheet_index(&self, sheet: &str) -> Option<u32> {
        let index = self.sheets.iter().position(|name| name == sheet).or_else(|| {
            let folded = casefold(sheet);
            self.sheets.iter().position(|name| casefold(name) == folded)
        })?;
        u32::try_from(index).ok()
    }
}

// ------------------------------------------------------------------ loaded module

#[derive(Clone, Copy)]
struct NativeFns {
    run: cv_run_fn,
    read_rect: cv_run_read_rect_fn,
    stats: cv_run_stats_fn,
    matrix_free: cv_matrix_free_fn,
    run_free: cv_run_free_fn,
}

/// One loaded native module: the library, its resolved entry points and its
/// parsed `cv_meta_json`. Obtain shared instances with [`NativeModule::cached`].
pub struct NativeModule {
    pub workbook_sha256: String,
    pub entry: NativeRegistryEntry,
    meta: NativeMeta,
    fns: NativeFns,
    // Last field: dropped after nothing can call through `fns` any more.
    _library: Library,
}

impl fmt::Debug for NativeModule {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeModule")
            .field("workbook_sha256", &self.workbook_sha256)
            .field("entry", &self.entry)
            .field("ports", &self.meta.port_names.len())
            .field("outputs", &self.meta.outputs.len())
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One exported entry point, copied out of the library.
fn symbol<T: Copy>(library: &Library, name: &[u8]) -> Result<T, NativeDecline> {
    // SAFETY: `T` is the entry's typedef from `abi`, the signature
    // CV_NATIVE_ABI 1 fixes for that symbol; the copied pointer is only
    // called while the library is loaded (it lives in the same NativeModule,
    // and cached modules are never dropped).
    unsafe { library.get::<T>(name) }.map(|entry| *entry).map_err(|_| NativeDecline::Import)
}

fn module_cache() -> &'static Mutex<BTreeMap<String, Arc<NativeModule>>> {
    static CACHE: OnceLock<Mutex<BTreeMap<String, Arc<NativeModule>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

impl NativeModule {
    /// Load the library at `entry.native_path` and check it: ABI version,
    /// entry points, `cv_meta_json` (parses, tables consistent, names
    /// `workbook_sha256`). Uncached; serving uses [`NativeModule::cached`].
    pub fn load(workbook_sha256: &str, entry: &NativeRegistryEntry) -> Result<Self, NativeDecline> {
        // SAFETY: loading runs the library's initialisers. The registry names
        // only artifacts the parity loader verified through the pins trust
        // chain (env -> pins document -> manifest -> file digest), built from
        // our own generator; a Rust cdylib's initialisers are std's.
        let library = unsafe { Library::new(&entry.native_path) }.map_err(|_| NativeDecline::Import)?;
        let abi_version: cv_native_abi_fn = symbol(&library, SYM_CV_NATIVE_ABI)?;
        // SAFETY: `cv_native_abi` takes nothing and returns a u32 in every ABI version.
        let found = unsafe { abi_version() };
        if found != CV_NATIVE_ABI {
            return Err(NativeDecline::AbiMismatch { found });
        }
        let meta_json: cv_meta_json_fn = symbol(&library, SYM_CV_META_JSON)?;
        let mut len = 0_usize;
        // SAFETY: ABI 1 `cv_meta_json` writes the length and returns the static META_JSON.
        let bytes = unsafe { meta_json(&raw mut len) };
        if bytes.is_null() {
            return Err(NativeDecline::Import);
        }
        // SAFETY: META_JSON is `len` static bytes, never freed.
        let bytes = unsafe { std::slice::from_raw_parts(bytes, len) };
        let meta: NativeMeta = serde_json::from_slice(bytes).map_err(|_| NativeDecline::Import)?;
        if !meta.consistent() {
            return Err(NativeDecline::Import);
        }
        if meta.workbook_sha256 != workbook_sha256 {
            return Err(NativeDecline::KeyMismatch);
        }
        let fns = NativeFns {
            run: symbol(&library, SYM_CV_RUN)?,
            read_rect: symbol(&library, SYM_CV_RUN_READ_RECT)?,
            stats: symbol(&library, SYM_CV_RUN_STATS)?,
            matrix_free: symbol(&library, SYM_CV_MATRIX_FREE)?,
            run_free: symbol(&library, SYM_CV_RUN_FREE)?,
        };
        Ok(Self { workbook_sha256: workbook_sha256.to_owned(), entry: entry.clone(), meta, fns, _library: library })
    }

    /// The process-wide module for `workbook_sha256`, loaded on first use and
    /// never unloaded (contract F4). A hit must be the same artifact (same
    /// `native_path` and `manifest_sha256`), else [`NativeDecline::ModuleIdentity`].
    /// The cache lock is held only for the lookup and the load, never across
    /// `cv_run`. A failed load is not cached here.
    pub fn cached(workbook_sha256: &str, entry: &NativeRegistryEntry) -> Result<Arc<Self>, NativeDecline> {
        let mut cache = lock(module_cache());
        if let Some(module) = cache.get(workbook_sha256) {
            return if module.entry.native_path == entry.native_path && module.entry.manifest_sha256 == entry.manifest_sha256
            {
                Ok(Arc::clone(module))
            } else {
                Err(NativeDecline::ModuleIdentity)
            };
        }
        let module = Arc::new(Self::load(workbook_sha256, entry)?);
        cache.insert(workbook_sha256.to_owned(), Arc::clone(&module));
        Ok(module)
    }

    /// Sheet names in `SHEETS` order.
    pub fn sheets(&self) -> &[String] {
        &self.meta.sheets
    }

    /// Port names in `PORT_NAMES` order (the order `cv_run` takes ports in).
    pub fn port_names(&self) -> &[String] {
        &self.meta.port_names
    }
}

// ------------------------------------------------------------------ value law

/// Integers of at most this magnitude are admitted (as floats); adapter.py `EXACT_INT`.
const EXACT_INT: i64 = 1 << 53;

/// A value handed to the module (a port value or a nested-call answer cell).
#[derive(Debug, Clone, PartialEq)]
enum HostVal {
    Blank,
    Num(f64),
    Bool(bool),
    Str(String),
}

const BLANK_VAL: CvVal = CvVal { tag: CV_TAG_BLANK, num: 0.0, str_ptr: std::ptr::null(), str_len: 0, err_code: 0 };
const EMPTY_MATRIX: CvMatrix =
    CvMatrix { rows: 0, cols: 0, vals: std::ptr::null_mut(), owner: std::ptr::null_mut() };

/// `CvVal` borrowing `value`'s text; valid while `value` is neither moved
/// out of its heap buffer nor dropped.
fn cv_val(value: &HostVal) -> CvVal {
    match value {
        HostVal::Blank => BLANK_VAL,
        HostVal::Num(number) => CvVal { tag: CV_TAG_NUM, num: *number, ..BLANK_VAL },
        HostVal::Bool(flag) => CvVal { tag: CV_TAG_BOOL, num: if *flag { 1.0 } else { 0.0 }, ..BLANK_VAL },
        HostVal::Str(text) => CvVal { tag: CV_TAG_STR, str_ptr: text.as_ptr(), str_len: text.len(), ..BLANK_VAL },
    }
}

/// adapter.py `admitted_value`: the port value the module receives, or `None`
/// (decline `admission`). Empty -> Blank, Boolean, Text, finite Number, Int
/// of magnitude at most 2**53 -> Num; anything else (a date, NaN, an array,
/// an error) is doubt.
fn admitted_scalar(value: &LiteralValue) -> Option<HostVal> {
    match value {
        LiteralValue::Empty => Some(HostVal::Blank),
        LiteralValue::Boolean(flag) => Some(HostVal::Bool(*flag)),
        LiteralValue::Text(text) => Some(HostVal::Str(text.clone())),
        LiteralValue::Number(number) if number.is_finite() => Some(HostVal::Num(*number)),
        // Exact: |number| <= 2**53.
        LiteralValue::Int(number) if (-EXACT_INT..=EXACT_INT).contains(number) => Some(HostVal::Num(*number as f64)),
        _ => None,
    }
}

/// adapter.py `same_value`: type-exact, a float by its bits.
fn same_value(left: &HostVal, right: &HostVal) -> bool {
    match (left, right) {
        (HostVal::Num(a), HostVal::Num(b)) => a.to_bits() == b.to_bits(),
        _ => left == right,
    }
}

/// adapter.py `nested_cell`: one cell of a nested call's answer, or the decline reason.
fn nested_cell(value: &LiteralValue) -> Result<HostVal, &'static str> {
    match value {
        LiteralValue::Empty => Ok(HostVal::Blank),
        LiteralValue::Number(number) => Ok(HostVal::Num(*number)),
        LiteralValue::Boolean(flag) => Ok(HostVal::Bool(*flag)),
        LiteralValue::Text(text) => Ok(HostVal::Str(text.clone())),
        LiteralValue::Error(_) | LiteralValue::Pending | LiteralValue::Array(_) => Err("xcall_error"),
        LiteralValue::Int(_)
        | LiteralValue::Date(_)
        | LiteralValue::DateTime(_)
        | LiteralValue::Time(_)
        | LiteralValue::Duration(_) => Err("xcall_lane"),
    }
}

fn error_kind(code: u32) -> Option<ExcelErrorKind> {
    Some(match code {
        CV_XLERR_NA => ExcelErrorKind::Na,
        CV_XLERR_VALUE => ExcelErrorKind::Value,
        CV_XLERR_DIV0 => ExcelErrorKind::Div,
        CV_XLERR_REF => ExcelErrorKind::Ref,
        CV_XLERR_NAME => ExcelErrorKind::Name,
        CV_XLERR_NUM => ExcelErrorKind::Num,
        CV_XLERR_NULL => ExcelErrorKind::Null,
        CV_XLERR_SPILL => ExcelErrorKind::Spill,
        CV_XLERR_CALC => ExcelErrorKind::Calc,
        _ => return None,
    })
}

/// The text of a `CV_TAG_STR` value, `None` when unreadable (null or not UTF-8).
///
/// # Safety
/// For `CV_TAG_STR`, `str_ptr` must point to `str_len` readable bytes (ABI rule).
unsafe fn cv_text(value: &CvVal) -> Option<String> {
    if value.str_len == 0 {
        return Some(String::new());
    }
    if value.str_ptr.is_null() {
        return None;
    }
    // SAFETY: the caller's contract.
    let bytes = unsafe { std::slice::from_raw_parts(value.str_ptr, value.str_len) };
    std::str::from_utf8(bytes).ok().map(str::to_owned)
}

/// The plain law (module -> router arguments, `read_cells`): Blank -> Empty,
/// Num -> Number, Bool -> Boolean, Str -> Text, Err -> Error(kind). `None` for
/// an unknown tag or error code or unreadable text.
///
/// # Safety
/// As [`cv_text`].
unsafe fn plain_literal(value: &CvVal) -> Option<LiteralValue> {
    Some(match value.tag {
        CV_TAG_BLANK => LiteralValue::Empty,
        CV_TAG_NUM => LiteralValue::Number(value.num),
        CV_TAG_BOOL => LiteralValue::Boolean(value.num != 0.0),
        // SAFETY: the caller's contract.
        CV_TAG_STR => LiteralValue::Text(unsafe { cv_text(value) }?),
        CV_TAG_ERR => LiteralValue::Error(ExcelError::new(error_kind(value.err_code)?)),
        _ => return None,
    })
}

/// adapter.py `output_matrix` for one cell: Blank -> Empty, finite Num ->
/// Number (else `non_finite`), Bool, Str, Err `#NUM!` / `#N/A` -> Error(kind
/// only); any other error or value declines `output_lane`.
///
/// # Safety
/// As [`cv_text`].
unsafe fn output_literal(value: &CvVal) -> Result<LiteralValue, &'static str> {
    match value.tag {
        CV_TAG_BLANK => Ok(LiteralValue::Empty),
        CV_TAG_NUM if value.num.is_finite() => Ok(LiteralValue::Number(value.num)),
        CV_TAG_NUM => Err("non_finite"),
        CV_TAG_BOOL => Ok(LiteralValue::Boolean(value.num != 0.0)),
        // SAFETY: the caller's contract.
        CV_TAG_STR => unsafe { cv_text(value) }.map(LiteralValue::Text).ok_or("output_lane"),
        CV_TAG_ERR if value.err_code == CV_XLERR_NUM => Ok(LiteralValue::Error(ExcelError::new(ExcelErrorKind::Num))),
        CV_TAG_ERR if value.err_code == CV_XLERR_NA => Ok(LiteralValue::Error(ExcelError::new(ExcelErrorKind::Na))),
        _ => Err("output_lane"),
    }
}

/// The `rows * cols` values of a matrix (empty for an empty matrix), `None`
/// for a null buffer or an overflowing size.
///
/// # Safety
/// A non-null `vals` must point to `rows * cols` values valid for `'m`.
unsafe fn matrix_values(matrix: &CvMatrix) -> Option<&[CvVal]> {
    let len = matrix.rows.checked_mul(matrix.cols)?;
    if len == 0 {
        return Some(&[]);
    }
    if matrix.vals.is_null() {
        return None;
    }
    // SAFETY: the caller's contract.
    Some(unsafe { std::slice::from_raw_parts(matrix.vals, len) })
}

/// A nested-call argument as the router takes it: scalar -> its value, rows ->
/// `LiteralValue::Array`.
///
/// # Safety
/// The argument's value and matrix follow the ABI rules for the call.
unsafe fn arg_literal(arg: &CvArg) -> Option<LiteralValue> {
    match arg.kind {
        // SAFETY: the caller's contract.
        CV_ARG_SCALAR => unsafe { plain_literal(&arg.value) },
        CV_ARG_ROWS => {
            // SAFETY: the caller's contract.
            let values = unsafe { matrix_values(&arg.rows) }?;
            if arg.rows.cols == 0 {
                return Some(LiteralValue::Array(Vec::new()));
            }
            let mut rows = Vec::with_capacity(arg.rows.rows);
            for row in values.chunks(arg.rows.cols) {
                // SAFETY: the caller's contract.
                rows.push(row.iter().map(|value| unsafe { plain_literal(value) }).collect::<Option<Vec<_>>>()?);
            }
            Some(LiteralValue::Array(rows))
        }
        _ => None,
    }
}

/// A module-supplied decline reason: a non-empty `[a-z0-9_]` token, else `decline`.
fn reason_token(error: &CvError) -> String {
    let len = (error.reason_len as usize).min(CV_ERROR_REASON_CAP);
    match std::str::from_utf8(&error.reason[..len]) {
        Ok(text) if !text.is_empty() && text.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') => {
            text.to_owned()
        }
        _ => "decline".to_owned(),
    }
}

/// adapter.py `today_serial`: the whole-day serial of `now` in UTC (1900 system).
fn today_serial(context: &CalculationContext) -> f64 {
    let Some(epoch) = NaiveDate::from_ymd_opt(1899, 12, 30) else { return 0.0 };
    // Exact: day counts are far below 2**53.
    (context.now.with_timezone(&Utc).date_naive() - epoch).num_days() as f64
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "panic".to_owned()
    }
}

/// The error a run fails with when its nested-call handler faulted
/// (`CallbackInfrastructureError`, as `CompiledRoute.attempt` raises it).
fn callback_fault(error: &ModelCallError) -> ModelCallError {
    ModelCallError::infrastructure(
        "CallbackInfrastructureError",
        format!("child callback infrastructure fault: {}", error.event_error()),
    )
}

// ------------------------------------------------------------------ nested-call trampoline

enum XcallFailure {
    /// The answer is outside the law: the run declines with this reason.
    Decline(&'static str),
    /// The handler failed (an infrastructure error or a panic): the run fails (F3).
    Fault(ModelCallError),
}

/// Per-run callback context (`ctx` of `cv_run`): the handler, the first
/// failure, and the last answer, which stays alive until the next callback or
/// the end of the run (the module copies it when the callback returns).
struct XcallCtx<'h> {
    handler: &'h mut dyn CompiledXcall,
    failure: Option<XcallFailure>,
    cells: Vec<HostVal>,
    answer: Vec<CvVal>,
}

impl XcallCtx<'_> {
    /// # Safety
    /// `args` points to `n_args` arguments valid for the call and `out` is a
    /// valid out-parameter (ABI rules for `CvXcallFn`).
    unsafe fn answer(&mut self, args: *const CvArg, n_args: usize, out: *mut CvMatrix) -> Result<(), XcallFailure> {
        if out.is_null() || (args.is_null() && n_args > 0) {
            return Err(XcallFailure::Decline("xcall_result"));
        }
        let args = if n_args == 0 {
            &[][..]
        } else {
            // SAFETY: the caller's contract.
            unsafe { std::slice::from_raw_parts(args, n_args) }
        };
        let mut values = Vec::with_capacity(args.len());
        for arg in args {
            // SAFETY: the caller's contract.
            values.push(unsafe { arg_literal(arg) }.ok_or(XcallFailure::Decline("xcall_result"))?);
        }
        let [target, block, output, tail @ ..] = values.as_slice() else {
            return Err(XcallFailure::Decline("xcall_error"));
        };
        let matrix = match self.handler.call(target, block, output, tail) {
            Ok(matrix) => matrix,
            // adapter.py `nested_matrix`: a routing error answers one error value.
            Err(ModelCallError::Routing(_)) => return Err(XcallFailure::Decline("xcall_error")),
            Err(error) => return Err(XcallFailure::Fault(error)),
        };
        let cols = matrix.first().map_or(0, Vec::len);
        if matrix.iter().any(|row| row.len() != cols) {
            return Err(XcallFailure::Decline("xcall_result"));
        }
        let mut cells = Vec::with_capacity(matrix.len() * cols);
        for value in matrix.iter().flatten() {
            cells.push(nested_cell(value).map_err(XcallFailure::Decline)?);
        }
        self.cells = cells;
        self.answer = self.cells.iter().map(cv_val).collect();
        let answer = CvMatrix { rows: matrix.len(), cols, vals: self.answer.as_mut_ptr(), owner: std::ptr::null_mut() };
        // SAFETY: `out` is a valid out-parameter (caller's contract).
        unsafe { out.write(answer) };
        Ok(())
    }
}

/// The `CvXcallFn` every run gets. Never unwinds: a panic in the handler is
/// caught and stored as an infrastructure fault (F3), and any failure returns
/// non-zero, which ends the run with `CV_ERR_XCALL`.
unsafe extern "C" fn xcall_trampoline(ctx: *mut c_void, args: *const CvArg, n_args: usize, out: *mut CvMatrix) -> i32 {
    if ctx.is_null() {
        return 1;
    }
    // SAFETY: `ctx` is the `XcallCtx` that `execute` passed to `cv_run` for
    // this run: it lives on `execute`'s frame for the whole run, and no other
    // reference to it is live while the module is inside this callback.
    let ctx = unsafe { &mut *ctx.cast::<XcallCtx<'_>>() };
    // SAFETY: the module passes `args`/`out` per the ABI.
    let outcome = catch_unwind(AssertUnwindSafe(|| unsafe { ctx.answer(args, n_args, out) }));
    let failure = match outcome {
        Ok(Ok(())) => return 0,
        Ok(Err(failure)) => failure,
        Err(payload) => XcallFailure::Fault(ModelCallError::infrastructure("PanicException", panic_text(&*payload))),
    };
    ctx.failure = Some(failure);
    1
}

// ------------------------------------------------------------------ a run

/// The ports of one run in `PORT_NAMES` order, with the buffers they point into.
struct PortBuffers {
    ports: Vec<CvPort>,
    _rows: Vec<Vec<CvVal>>,
    _admitted: Vec<Admitted>,
}

#[derive(Debug, Clone, PartialEq)]
enum Admitted {
    NotSupplied,
    Value(HostVal),
    Rows { rows: usize, cols: usize, cells: Vec<HostVal> },
}

impl PortBuffers {
    fn new(admitted: Vec<Admitted>) -> Self {
        let rows: Vec<Vec<CvVal>> = admitted
            .iter()
            .map(|port| match port {
                Admitted::Rows { cells, .. } => cells.iter().map(cv_val).collect(),
                _ => Vec::new(),
            })
            .collect();
        // Moving `rows` and `admitted` into `Self` moves no heap buffer, so the
        // pointers below stay valid for the life of `Self`.
        let ports = admitted
            .iter()
            .zip(&rows)
            .map(|(port, values)| match port {
                Admitted::NotSupplied => CvPort { kind: CV_PORT_NOT_SUPPLIED, value: BLANK_VAL, rows: EMPTY_MATRIX },
                Admitted::Value(value) => CvPort { kind: CV_PORT_VALUE, value: cv_val(value), rows: EMPTY_MATRIX },
                Admitted::Rows { rows, cols, .. } => CvPort {
                    kind: CV_PORT_ROWS,
                    value: BLANK_VAL,
                    rows: CvMatrix { rows: *rows, cols: *cols, vals: values.as_ptr().cast_mut(), owner: std::ptr::null_mut() },
                },
            })
            .collect();
        Self { ports, _rows: rows, _admitted: admitted }
    }
}

/// Why a run produced no result.
enum RunFailure {
    /// Discard the attempt: `fallback:<reason>`.
    Decline(String),
    /// The nested-call handler failed: an infrastructure error.
    Fault(ModelCallError),
}

/// A finished run: owns the module's `CvRun` and frees it on drop.
struct NativeRun {
    module: Arc<NativeModule>,
    run: NonNull<CvRun>,
}

// SAFETY: a `CvRun` is an owned store with no thread affinity (the module
// keeps no thread-local state in it); `NativeRun` owns it exclusively, reads
// it through `&self` only with the module's read entries, and frees it once.
unsafe impl Send for NativeRun {}

impl Drop for NativeRun {
    fn drop(&mut self) {
        // SAFETY: `run` came from this module's `cv_run` and is freed only here.
        unsafe { (self.module.fns.run_free)(self.run.as_ptr()) };
    }
}

/// Frees a module-owned matrix on drop.
struct ModuleMatrix<'m> {
    fns: &'m NativeFns,
    matrix: CvMatrix,
}

impl Drop for ModuleMatrix<'_> {
    fn drop(&mut self) {
        // SAFETY: the matrix came from this module's `cv_run_read_rect`.
        unsafe { (self.fns.matrix_free)(&raw mut self.matrix) };
    }
}

impl NativeRun {
    /// Read one rectangle and convert its values with `convert` before the
    /// module's matrix is freed. `Err` is the module's code.
    fn with_rect<T>(&self, rect: Rect, convert: impl FnOnce(&[CvVal]) -> T) -> Result<T, i32> {
        let fns = &self.module.fns;
        let mut out = EMPTY_MATRIX;
        // SAFETY: the run is live (owned by `self`); `out` is a valid out-parameter.
        let code = unsafe { (fns.read_rect)(self.run.as_ptr(), rect.sheet, rect.r1, rect.c1, rect.r2, rect.c2, &raw mut out) };
        if code != 0 {
            return Err(code);
        }
        let matrix = ModuleMatrix { fns, matrix: out };
        if (matrix.matrix.rows, matrix.matrix.cols) != (rect.rows(), rect.cols()) {
            return Err(CV_ERR_ARGS as i32);
        }
        // SAFETY: a successful read fills `out` with `rows * cols` values
        // owned by the module until `cv_matrix_free` (dropping `matrix`).
        let values = unsafe { matrix_values(&matrix.matrix) }.ok_or(CV_ERR_ARGS as i32)?;
        Ok(convert(values))
    }

    /// One declared output under the output law.
    fn output(&self, rect: Rect) -> Result<ChildMatrix, String> {
        let cols = rect.cols();
        self.with_rect(rect, |values| {
            values
                .chunks(cols)
                // SAFETY: values of a live module matrix (ABI string rule).
                .map(|row| row.iter().map(|value| unsafe { output_literal(value) }).collect::<Result<Vec<_>, _>>())
                .collect::<Result<ChildMatrix, _>>()
        })
        .map_err(|_| "exception".to_owned())?
        .map_err(str::to_owned)
    }

    fn stats(&self) -> CompiledRunStats {
        let mut stats = CvRunStats::default();
        // SAFETY: the run is live; `stats` is a valid out-parameter.
        if unsafe { (self.module.fns.stats)(self.run.as_ptr(), &raw mut stats) } != 0 {
            return CompiledRunStats::default();
        }
        CompiledRunStats {
            xcalls: stats.xcalls,
            guard_views: stats.guard_views,
            guard_probes: stats.guard_probes,
            guard_max_row_minus_limit: (stats.guard_max_row_minus_limit != i64::MIN)
                .then_some(stats.guard_max_row_minus_limit),
            t_fresh_s: stats.t_fresh_s,
            t_run_s: stats.t_run_s,
        }
    }
}

/// One `cv_run` with `handler` behind the nested-call callback. No lock is held.
fn execute(
    module: &Arc<NativeModule>,
    ports: &PortBuffers,
    today: f64,
    handler: &mut dyn CompiledXcall,
) -> Result<NativeRun, RunFailure> {
    let mut ctx = XcallCtx { handler, failure: None, cells: Vec::new(), answer: Vec::new() };
    let mut error = CvError::default();
    // SAFETY: `ports` point into `PortBuffers` (alive for the call, ABI:
    // borrowed and copied by the module); `ctx` lives on this frame for the
    // whole run and is only touched by `xcall_trampoline`; `error` is a
    // valid out-parameter.
    let run = unsafe {
        (module.fns.run)(
            ports.ports.as_ptr(),
            ports.ports.len(),
            today,
            Some(xcall_trampoline),
            std::ptr::from_mut(&mut ctx).cast::<c_void>(),
            &raw mut error,
        )
    };
    let run = NonNull::new(run).map(|run| NativeRun { module: Arc::clone(module), run });
    match (ctx.failure.take(), run) {
        // F3: a stored handler fault wins whatever the module reports.
        (Some(XcallFailure::Fault(fault)), _) => Err(RunFailure::Fault(fault)),
        (_, Some(run)) => Ok(run),
        (failure, None) => Err(RunFailure::Decline(match error.code {
            CV_ERR_XCALL => match failure {
                Some(XcallFailure::Decline(reason)) => reason.to_owned(),
                _ => "xcall_result".to_owned(),
            },
            CV_ERR_DECLINE => reason_token(&error),
            CV_ERR_VIOLATION => "probe_violation".to_owned(),
            _ => "exception".to_owned(),
        })),
    }
}

/// Report-capture reads over a finished compiled parent run (`CompiledCells`).
struct NativeCells {
    run: NativeRun,
}

impl CompiledCells for NativeCells {
    fn read_cells(&self, cells: &[CellAddress]) -> Result<Vec<LiteralValue>, ModelCallError> {
        let meta = &self.run.module.meta;
        let mut boxes: BTreeMap<u32, Rect> = BTreeMap::new();
        let mut sheets = Vec::with_capacity(cells.len());
        for (sheet, row, col) in cells {
            let index = meta
                .sheet_index(sheet)
                .ok_or_else(|| ModelCallError::infrastructure("ValueError", "compiled read: sheet is not in the module"))?;
            if *row == 0 || *col == 0 {
                return Err(ModelCallError::infrastructure("ValueError", "compiled read: rows and columns are 1-based"));
            }
            boxes
                .entry(index)
                .and_modify(|rect| {
                    rect.r1 = rect.r1.min(*row);
                    rect.c1 = rect.c1.min(*col);
                    rect.r2 = rect.r2.max(*row);
                    rect.c2 = rect.c2.max(*col);
                })
                .or_insert(Rect { sheet: index, r1: *row, c1: *col, r2: *row, c2: *col });
            sheets.push(index);
        }
        let mut read: BTreeMap<u32, (Rect, Vec<LiteralValue>)> = BTreeMap::new();
        for (index, rect) in boxes {
            let values = self
                .run
                // SAFETY: values of a live module matrix (ABI string rule).
                .with_rect(rect, |values| values.iter().map(|value| unsafe { plain_literal(value) }).collect::<Option<Vec<_>>>())
                .map_err(|code| {
                    ModelCallError::infrastructure("RuntimeError", format!("compiled read: cv_run_read_rect returned {code}"))
                })?
                .ok_or_else(|| ModelCallError::infrastructure("RuntimeError", "compiled read: unreadable value"))?;
            read.insert(index, (rect, values));
        }
        cells
            .iter()
            .zip(sheets)
            .map(|((_, row, col), index)| {
                read.get(&index)
                    .and_then(|(rect, values)| {
                        values.get((row - rect.r1) as usize * rect.cols() + (col - rect.c1) as usize).cloned()
                    })
                    .ok_or_else(|| ModelCallError::infrastructure("RuntimeError", "compiled read: cell outside its rectangle"))
            })
            .collect()
    }
}

// ------------------------------------------------------------------ eligibility and admission

/// How one module port is fed.
#[derive(Debug, Clone, PartialEq)]
enum PortKind {
    Scalar,
    /// A ranged port: only its exact declared rectangle is admitted.
    Rows { rows: usize, cols: usize },
}

/// One module port (in `PORT_NAMES` order) and the declared input key feeding it.
#[derive(Debug, Clone, PartialEq)]
struct PortPlan {
    key: String,
    kind: PortKind,
}

/// Everything checked before a run: the module, the admitted ports and the
/// output rectangles (casefolded key, rectangle) the run is read for.
struct Prepared {
    module: Arc<NativeModule>,
    ports: PortBuffers,
    outputs: Vec<(String, Rect)>,
}

/// JSON truthiness (Python `bool(value)`).
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// A JSON default as the engine path reads it (`thaw`), `None` if it has no value form.
fn default_literal(value: &Value) -> Option<LiteralValue> {
    WireValue::from_json(value).to_literal().ok()
}

/// adapter.py `eligible_ports` (the checks that need no module tables).
fn eligible(spec: &ModelSpec) -> Result<(), &'static str> {
    if spec.date_system() != Some(1900) {
        return Err("date_system");
    }
    if spec.descriptor.get("calculation_normalizations").is_some_and(truthy) {
        return Err("descriptor_edits");
    }
    if !spec.goal_seek.is_empty() {
        return Err("solvers");
    }
    Ok(())
}

/// The kind of module port `index` for declared input `location`, or `port_contract`.
fn port_kind(port: &Value, location: &PortLocation, meta: &NativeMeta, index: usize) -> Result<PortKind, &'static str> {
    let shape = port.get("shape").and_then(Value::as_str);
    let schema = port.get("schema");
    match shape {
        Some("scalar") if schema == Some(&serde_json::json!({"type": "any"})) => Ok(PortKind::Scalar),
        Some("range")
            if location.shape == "range"
                && schema == Some(&serde_json::json!({"kind": "range", "cell_type": "any"}))
                && location.headers.as_ref().is_none_or(Vec::is_empty) =>
        {
            let name = &meta.port_names[index];
            let rect = meta.ports.get(name).map(|rect| Rect::from_meta(*rect)).ok_or("port_contract")?;
            if !rect.matches(meta, location) {
                return Err("port_contract");
            }
            Ok(PortKind::Rows { rows: rect.rows(), cols: rect.cols() })
        }
        _ => Err("port_contract"),
    }
}

/// adapter.py `port_map` + `port_defaults`: the declared inputs are exactly
/// the module's ports (case-folded names, one to one), each scalar `any` or
/// a ranged port whose rectangle is the module's, and the defaults agree.
fn port_plan(spec: &ModelSpec, meta: &NativeMeta) -> Result<Vec<PortPlan>, &'static str> {
    let mut by_folded: BTreeMap<String, usize> = BTreeMap::new();
    for (index, name) in meta.port_names.iter().enumerate() {
        if by_folded.insert(casefold(name), index).is_some() {
            return Err("port_contract");
        }
    }
    let manifest_ports: Vec<&Value> = spec
        .manifest
        .get("ports")
        .and_then(Value::as_array)
        .map(|ports| ports.iter().filter(|port| port.get("dir").and_then(Value::as_str) == Some("in")).collect())
        .unwrap_or_default();
    let mut slots: Vec<Option<PortPlan>> = vec![None; meta.port_names.len()];
    for (_, location) in spec.inputs.iter() {
        let port = manifest_ports
            .iter()
            .find(|port| port.get("id").and_then(Value::as_str) == Some(location.port_id.as_str()))
            .ok_or("port_contract")?;
        let index = *by_folded.get(&casefold(&location.key)).ok_or("port_contract")?;
        let kind = port_kind(port, location, meta, index)?;
        let slot = &mut slots[index];
        if slot.is_some() {
            return Err("port_contract");
        }
        *slot = Some(PortPlan { key: location.key.clone(), kind });
    }
    let plan: Vec<PortPlan> = slots.into_iter().collect::<Option<_>>().ok_or("port_contract")?;
    port_defaults(spec, meta, &plan)?;
    Ok(plan)
}

/// adapter.py `port_defaults`: a `formula` port is exactly a
/// `formula_input_defaults` key without a default; every other port has a
/// default. When the module publishes its defaults (meta `port_defaults`),
/// each admissible default must equal the module's in type and value.
fn port_defaults(spec: &ModelSpec, meta: &NativeMeta, plan: &[PortPlan]) -> Result<(), &'static str> {
    let formula_keys: Vec<&str> = match spec.descriptor.get("formula_input_defaults") {
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
        Some(Value::Object(map)) => map.keys().map(String::as_str).collect(),
        _ => Vec::new(),
    };
    for (index, port) in plan.iter().enumerate() {
        let in_defaults = spec.defaults.contains_key(&port.key);
        let in_formula = formula_keys.contains(&port.key.as_str());
        if meta.port_default_kind[index] == "formula" {
            if in_defaults || !in_formula {
                return Err("port_defaults");
            }
            continue;
        }
        if in_formula || !in_defaults {
            return Err("port_defaults");
        }
        let Some(module_defaults) = &meta.port_defaults else { continue };
        // A default admission would refuse is left to the per-call check.
        let Some(expected) = spec.defaults.get(&port.key).and_then(default_literal).as_ref().and_then(admitted_scalar)
        else {
            continue;
        };
        let module = module_defaults
            .get(&meta.port_names[index])
            .and_then(default_literal)
            .as_ref()
            .and_then(admitted_scalar);
        if module.is_none_or(|module| !same_value(&expected, &module)) {
            return Err("port_defaults");
        }
    }
    Ok(())
}

/// adapter.py `output_name`: the module output that is exactly this declared
/// rectangle (same sheet, same corners, same shape), or `shape_mismatch`.
fn output_rect(meta: &NativeMeta, location: &PortLocation) -> Result<Rect, &'static str> {
    let wanted = casefold(&location.key);
    let mut names = meta.outputs.keys().filter(|name| casefold(name) == wanted);
    let (Some(name), None) = (names.next(), names.next()) else { return Err("shape_mismatch") };
    let rect = meta.outputs.get(name).map(|rect| Rect::from_meta(*rect)).ok_or("shape_mismatch")?;
    let shape = meta.output_shape.get(name).ok_or("shape_mismatch")?;
    if !rect.matches(meta, location) || (shape[0] as usize, shape[1] as usize) != (rect.rows(), rect.cols()) {
        return Err("shape_mismatch");
    }
    Ok(rect)
}

fn admit_port(value: &LiteralValue, kind: &PortKind) -> Result<Admitted, &'static str> {
    match kind {
        PortKind::Scalar => admitted_scalar(value).map(Admitted::Value).ok_or("admission"),
        PortKind::Rows { rows, cols } => {
            let LiteralValue::Array(matrix) = value else { return Err("admission") };
            if matrix.len() != *rows || matrix.iter().any(|row| row.len() != *cols) {
                return Err("admission");
            }
            let cells = matrix.iter().flatten().map(admitted_scalar).collect::<Option<Vec<_>>>().ok_or("admission")?;
            Ok(Admitted::Rows { rows: *rows, cols: *cols, cells })
        }
    }
}

/// adapter.py `admit`: the ports admitted as `PortSession.write_scenario`
/// admits them (canonical names over the alias map, the unknown-input
/// policy, the defaults merge), then the value law. A port the merge leaves
/// out (a formula port the request did not supply) is `NotSupplied`.
fn admit(spec: &ModelSpec, inputs: &[(String, LiteralValue)], plan: &[PortPlan]) -> Result<Vec<Admitted>, &'static str> {
    let aliases = spec_port_alias_map(spec).map_err(|_| "admission")?;
    let updates = canonical_port_inputs(inputs, &aliases, spec.unknown_input_policy()).map_err(|_| "admission")?;
    let mut effective: Vec<(String, Option<LiteralValue>)> =
        spec.defaults.iter().map(|(key, value)| (key.to_owned(), default_literal(value))).collect();
    for (key, value) in updates {
        match effective.iter_mut().find(|(existing, _)| *existing == key) {
            Some(slot) => slot.1 = Some(value),
            None => effective.push((key, Some(value))),
        }
    }
    if effective.iter().any(|(key, _)| !plan.iter().any(|port| port.key == *key)) {
        return Err("admission");
    }
    plan.iter()
        .map(|port| match effective.iter().find(|(key, _)| *key == port.key) {
            None => Ok(Admitted::NotSupplied),
            Some((_, None)) => Err("admission"),
            Some((_, Some(value))) => admit_port(value, &port.kind),
        })
        .collect()
}

// ------------------------------------------------------------------ request context

thread_local! {
    static REQUEST_CONTEXT: RefCell<Vec<CalculationContext>> = const { RefCell::new(Vec::new()) };
}

/// Run `f` with `context` as the request context a compiled child attempt on
/// this thread uses for TODAY and the deadline. `CompiledParent::run` sets it
/// around its module run, so the parent's nested compiled children see it.
///
/// Stop-gap until the router exposes its request context
/// (`round/receipts/packageB/SEAM_REQUESTS.md`, S1): a child attempt with no
/// context on its thread declines `engine:no_request_context`.
pub fn with_request_context<T>(context: &CalculationContext, f: impl FnOnce() -> T) -> T {
    REQUEST_CONTEXT.with(|stack| stack.borrow_mut().push(context.clone()));
    struct Pop;
    impl Drop for Pop {
        fn drop(&mut self) {
            REQUEST_CONTEXT.with(|stack| {
                stack.borrow_mut().pop();
            });
        }
    }
    let _pop = Pop;
    f()
}

fn request_context() -> Option<CalculationContext> {
    REQUEST_CONTEXT.with(|stack| stack.borrow().last().cloned())
}

/// The nested router as a `CompiledXcall` (adapter.py `ChildRouter`): an
/// array answers the call; any other value (`#REF!` for a routing error,
/// `#CALC!` for a fault the router recorded) declines `xcall_error`.
struct RouterXcall {
    router: ModelCallRouter,
}

impl CompiledXcall for RouterXcall {
    fn call(
        &mut self,
        target: &LiteralValue,
        block: &LiteralValue,
        output: &LiteralValue,
        tail: &[LiteralValue],
    ) -> Result<ChildMatrix, ModelCallError> {
        let mut args = Vec::with_capacity(3 + tail.len());
        args.extend([target.clone(), block.clone(), output.clone()]);
        args.extend(tail.iter().cloned());
        match self.router.call(&args) {
            LiteralValue::Array(rows) => Ok(rows),
            _ => Err(ModelCallError::routing("nested call answered an error value")),
        }
    }
}

// ------------------------------------------------------------------ the hook

/// The native compiled hook over a registry of modules, keyed by
/// `workbook_sha256`. Implements both the child and the parent seam.
///
/// Routes accumulate per hook (`report()`), children and parent separately,
/// like one `CompiledRoute` per request: install one hook per request, or call
/// [`NativeCompiledHook::clear_routes`] between requests.
#[derive(Default)]
pub struct NativeCompiledHook {
    entries: BTreeMap<String, NativeRegistryEntry>,
    engine_commit: Option<String>,
    modules: Mutex<BTreeMap<String, Result<Arc<NativeModule>, NativeDecline>>>,
    routes: Mutex<Vec<Value>>,
    parent_routes: Mutex<Vec<Value>>,
}

impl fmt::Debug for NativeCompiledHook {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeCompiledHook")
            .field("entries", &self.entries)
            .field("engine_commit", &self.engine_commit)
            .finish_non_exhaustive()
    }
}

fn report_of(routes: &Mutex<Vec<Value>>) -> Map<String, Value> {
    let routes = lock(routes).clone();
    let mut report = Map::new();
    report.insert("calls".into(), Value::from(routes.len()));
    report.insert("routes".into(), Value::Array(routes));
    report
}

impl NativeCompiledHook {
    pub fn new(entries: BTreeMap<String, NativeRegistryEntry>) -> Self {
        Self { entries, ..Self::default() }
    }

    /// Parse the value-free registry document (`native_registry_json`,
    /// `{sha: {native_path, engine_commit, manifest_sha256, role,
    /// report_conditions_simple}}`; unknown fields are ignored). Modules load
    /// lazily, on the first call for their sha.
    pub fn from_registry_json(text: &str) -> Result<Self, ModelCallError> {
        let entries: BTreeMap<String, NativeRegistryEntry> = serde_json::from_str(text)
            .map_err(|error| ModelCallError::infrastructure("ValueError", format!("native registry: {error}")))?;
        Ok(Self::new(entries))
    }

    /// Decline `engine:engine_mismatch` for every entry whose `engine_commit`
    /// is not `commit` (the running engine's build commit), as adapter.py
    /// `eligible_ports` does. Unset, the commit is not checked here.
    #[must_use]
    pub fn with_engine_commit(mut self, commit: impl Into<String>) -> Self {
        self.engine_commit = Some(commit.into());
        self
    }

    pub fn entries(&self) -> &BTreeMap<String, NativeRegistryEntry> {
        &self.entries
    }

    /// The module serving `workbook_sha256`, or why none does. The outcome is
    /// remembered per hook, so a module that fails to load is tried once.
    pub fn module(&self, workbook_sha256: &str) -> Result<Arc<NativeModule>, NativeDecline> {
        let entry = self.entries.get(workbook_sha256).ok_or(NativeDecline::NotRegistered)?;
        if self.engine_commit.as_ref().is_some_and(|commit| *commit != entry.engine_commit) {
            return Err(NativeDecline::EngineMismatch);
        }
        let mut modules = lock(&self.modules);
        modules.entry(workbook_sha256.to_owned()).or_insert_with(|| NativeModule::cached(workbook_sha256, entry)).clone()
    }

    /// Forget the recorded routes (start of a new request on a reused hook).
    pub fn clear_routes(&self) {
        lock(&self.routes).clear();
        lock(&self.parent_routes).clear();
    }

    fn record(routes: &Mutex<Vec<Value>>, route: &str) -> Value {
        let route = Value::String(route.to_owned());
        lock(routes).push(route.clone());
        route
    }

    fn child_declined(&self, reason: &str) -> CompiledAttempt {
        CompiledAttempt { matrix: None, route: Some(Self::record(&self.routes, &format!("engine:{reason}"))) }
    }

    fn prepare(
        &self,
        spec: &ModelSpec,
        inputs: &[(String, LiteralValue)],
        output: Option<&PortLocation>,
    ) -> Result<Prepared, &'static str> {
        let module = self.module(&spec.workbook_sha256).map_err(|decline| decline.reason())?;
        eligible(spec)?;
        let plan = port_plan(spec, &module.meta)?;
        let outputs = match output {
            Some(location) => vec![(casefold(&location.key), output_rect(&module.meta, location)?)],
            None => spec
                .outputs
                .iter()
                .map(|(folded, location)| Ok((folded.to_owned(), output_rect(&module.meta, location)?)))
                .collect::<Result<Vec<_>, &'static str>>()?,
        };
        let admitted = admit(spec, inputs, &plan)?;
        Ok(Prepared { module, ports: PortBuffers::new(admitted), outputs })
    }

    /// One compiled child call (adapter.py `CompiledRoute.attempt`) with an
    /// explicit request context, nested-call handler and fault counter. The
    /// `CompiledChildHook` impl calls this with the thread's nested router
    /// (`current_nested_router`) and its `fault_count`.
    ///
    /// `Ok` with a matrix and route `compiled`; `Ok` without a matrix and
    /// route `engine:<reason>` (never ran) or `fallback:<reason>` (ran,
    /// discarded; `fallback:fault` when `faults()` grew during the run, which
    /// the session turns into the infrastructure error); `Err` for a deadline
    /// passed after the run (route `fallback:deadline`) or a handler fault (F3).
    pub fn attempt_child(
        &self,
        spec: &ModelSpec,
        inputs: &[(String, LiteralValue)],
        output: &PortLocation,
        context: &CalculationContext,
        xcall: &mut dyn CompiledXcall,
        faults: &dyn Fn() -> usize,
    ) -> Result<CompiledAttempt, ModelCallError> {
        match self.prepare(spec, inputs, Some(output)) {
            Ok(prepared) => self.run_child(&prepared, context, xcall, faults),
            Err(reason) => Ok(self.child_declined(reason)),
        }
    }

    fn run_child(
        &self,
        prepared: &Prepared,
        context: &CalculationContext,
        xcall: &mut dyn CompiledXcall,
        faults: &dyn Fn() -> usize,
    ) -> Result<CompiledAttempt, ModelCallError> {
        let before = faults();
        let result = execute(&prepared.module, &prepared.ports, today_serial(context), xcall).and_then(|run| {
            let (_, rect) = prepared.outputs.first().ok_or_else(|| RunFailure::Decline("shape_mismatch".into()))?;
            run.output(*rect).map_err(RunFailure::Decline)
        });
        // CompiledRoute.attempt (R1 #7): a passed deadline or a new router
        // fault fails the call now, as the engine path would.
        if context.check_deadline().is_err() {
            Self::record(&self.routes, "fallback:deadline");
            return Err(ModelCallError::deadline());
        }
        if faults() > before {
            let route = Self::record(&self.routes, "fallback:fault");
            return Ok(CompiledAttempt { matrix: None, route: Some(route) });
        }
        match result {
            Ok(matrix) => Ok(CompiledAttempt { matrix: Some(matrix), route: Some(Self::record(&self.routes, "compiled")) }),
            Err(RunFailure::Decline(reason)) => {
                Ok(CompiledAttempt { matrix: None, route: Some(Self::record(&self.routes, &format!("fallback:{reason}"))) })
            }
            Err(RunFailure::Fault(error)) => {
                Self::record(&self.routes, "fallback:fault");
                Err(callback_fault(&error))
            }
        }
    }
}

impl CompiledChildHook for NativeCompiledHook {
    fn attempt(
        &self,
        spec: &ModelSpec,
        inputs: &[(String, LiteralValue)],
        output: &PortLocation,
        _stack: &[String],
    ) -> Result<CompiledAttempt, ModelCallError> {
        let prepared = match self.prepare(spec, inputs, Some(output)) {
            Ok(prepared) => prepared,
            Err(reason) => return Ok(self.child_declined(reason)),
        };
        let Some(router) = current_nested_router() else { return Ok(self.child_declined("no_router")) };
        let Some(context) = request_context() else { return Ok(self.child_declined("no_request_context")) };
        let faults = router.clone();
        let mut xcall = RouterXcall { router };
        self.run_child(&prepared, &context, &mut xcall, &|| faults.fault_count())
    }

    fn report(&self) -> Map<String, Value> {
        report_of(&self.routes)
    }
}

impl CompiledParent for NativeCompiledHook {
    /// True only for a `role: parent` registry entry: a child-only registry
    /// records no parent route.
    fn serves(&self, workbook_sha256: &str) -> bool {
        self.entries.get(workbook_sha256).is_some_and(NativeRegistryEntry::is_parent)
    }

    /// The parent entry's `report_conditions_simple` (false when absent or
    /// when the sha is not a parent entry).
    fn report_conditions_simple(&self, workbook_sha256: &str) -> bool {
        self.entries.get(workbook_sha256).is_some_and(|entry| entry.is_parent() && entry.report_conditions_simple)
    }

    /// A compiled parent run (design item (c)): eligibility and admission as
    /// for a child, every declared output read under the output law. The
    /// module's nested calls go to `xcall`; the request context is set for
    /// this thread while the module runs ([`with_request_context`]).
    fn run(
        &self,
        spec: &ModelSpec,
        inputs: &[(String, LiteralValue)],
        context: &CalculationContext,
        xcall: &mut dyn CompiledXcall,
    ) -> Result<ParentAttempt, ModelCallError> {
        let prepared = match self.prepare(spec, inputs, None) {
            Ok(prepared) => prepared,
            Err(reason) => {
                let route = Self::record(&self.parent_routes, &format!("engine:{reason}"));
                return Ok(ParentAttempt::Declined { route });
            }
        };
        let today = today_serial(context);
        let result = with_request_context(context, || execute(&prepared.module, &prepared.ports, today, &mut *xcall))
            .and_then(|run| {
                let outputs = prepared
                    .outputs
                    .iter()
                    .map(|(key, rect)| Ok((key.clone(), run.output(*rect).map_err(RunFailure::Decline)?)))
                    .collect::<Result<Vec<_>, RunFailure>>()?;
                Ok((run, outputs))
            });
        if context.check_deadline().is_err() {
            Self::record(&self.parent_routes, "fallback:deadline");
            return Err(ModelCallError::deadline());
        }
        match result {
            Ok((run, outputs)) => {
                let route = Self::record(&self.parent_routes, "compiled");
                let stats = run.stats();
                Ok(ParentAttempt::Compiled(CompiledRun { outputs, route, stats, cells: Box::new(NativeCells { run }) }))
            }
            Err(RunFailure::Decline(reason)) => {
                let route = Self::record(&self.parent_routes, &format!("fallback:{reason}"));
                Ok(ParentAttempt::Declined { route })
            }
            Err(RunFailure::Fault(error)) => {
                Self::record(&self.parent_routes, "fallback:fault");
                Err(callback_fault(&error))
            }
        }
    }

    fn report(&self) -> Map<String, Value> {
        report_of(&self.parent_routes)
    }
}

#[cfg(test)]
mod tests {
    use super::abi::*;
    use super::*;

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn abi_layout_is_pinned_on_64_bit() {
        use std::mem::{align_of, offset_of, size_of};
        assert_eq!(size_of::<CvVal>(), 40);
        assert_eq!(offset_of!(CvVal, num), 8);
        assert_eq!(offset_of!(CvVal, str_ptr), 16);
        assert_eq!(offset_of!(CvVal, str_len), 24);
        assert_eq!(offset_of!(CvVal, err_code), 32);
        assert_eq!(size_of::<CvMatrix>(), 32);
        assert_eq!(size_of::<CvPort>(), 80);
        assert_eq!(offset_of!(CvPort, value), 8);
        assert_eq!(offset_of!(CvPort, rows), 48);
        assert_eq!(size_of::<CvArg>(), 80);
        assert_eq!(size_of::<CvError>(), 88);
        assert_eq!(offset_of!(CvError, reason), 20);
        assert_eq!(size_of::<CvRunStats>(), 48);
        assert_eq!(align_of::<CvVal>(), 8);
        assert_eq!(CV_NATIVE_ABI, 1);
    }

    #[test]
    fn registry_parses_and_unloadable_entries_decline() {
        let registry = r#"{"abc": {"native_path": "/nowhere/libcv_native.so", "engine_commit": "0027ee70", "manifest_sha256": "m"}}"#;
        let hook = NativeCompiledHook::from_registry_json(registry).expect("registry parses");
        assert_eq!(hook.module("abc").unwrap_err(), NativeDecline::Import);
        assert_eq!(hook.module("other").unwrap_err(), NativeDecline::NotRegistered);
        let pinned = NativeCompiledHook::from_registry_json(registry).unwrap().with_engine_commit("63338a7a");
        assert_eq!(pinned.module("abc").unwrap_err(), NativeDecline::EngineMismatch);
        assert_eq!(NativeDecline::Import.engine_route(), Value::from("engine:import_error"));
        assert!(NativeCompiledHook::from_registry_json("[]").is_err());
    }

    #[test]
    fn port_values_follow_the_admission_law() {
        assert_eq!(admitted_scalar(&LiteralValue::Empty), Some(HostVal::Blank));
        assert_eq!(admitted_scalar(&LiteralValue::Int(3)), Some(HostVal::Num(3.0)));
        assert_eq!(admitted_scalar(&LiteralValue::Int(EXACT_INT)), Some(HostVal::Num(9_007_199_254_740_992.0)));
        assert_eq!(admitted_scalar(&LiteralValue::Int(EXACT_INT + 1)), None);
        assert_eq!(admitted_scalar(&LiteralValue::Number(f64::NAN)), None);
        assert_eq!(admitted_scalar(&LiteralValue::Boolean(true)), Some(HostVal::Bool(true)));
        assert_eq!(admitted_scalar(&LiteralValue::Text("x".into())), Some(HostVal::Str("x".into())));
        assert_eq!(admitted_scalar(&LiteralValue::Date(NaiveDate::from_ymd_opt(2026, 1, 1).unwrap())), None);
        assert_eq!(admitted_scalar(&LiteralValue::Array(vec![vec![LiteralValue::Empty]])), None);
        assert!(!same_value(&HostVal::Num(0.0), &HostVal::Num(-0.0)));
        assert!(same_value(&HostVal::Num(1.5), &HostVal::Num(1.5)));
    }

    #[test]
    fn nested_answers_and_outputs_follow_the_value_law() {
        assert_eq!(nested_cell(&LiteralValue::Int(1)), Err("xcall_lane"));
        assert_eq!(nested_cell(&LiteralValue::Pending), Err("xcall_error"));
        assert_eq!(nested_cell(&LiteralValue::Error(ExcelError::new(ExcelErrorKind::Na))), Err("xcall_error"));
        assert_eq!(nested_cell(&LiteralValue::Number(2.0)), Ok(HostVal::Num(2.0)));
        let num = |num: f64| CvVal { tag: CV_TAG_NUM, num, ..BLANK_VAL };
        let err = |err_code: u32| CvVal { tag: CV_TAG_ERR, err_code, ..BLANK_VAL };
        // SAFETY: no text values.
        unsafe {
            assert_eq!(output_literal(&num(f64::INFINITY)), Err("non_finite"));
            assert_eq!(output_literal(&err(CV_XLERR_DIV0)), Err("output_lane"));
            assert_eq!(output_literal(&err(CV_XLERR_NA)), Ok(LiteralValue::Error(ExcelError::new(ExcelErrorKind::Na))));
            assert_eq!(output_literal(&BLANK_VAL), Ok(LiteralValue::Empty));
            assert_eq!(plain_literal(&err(CV_XLERR_DIV0)), Some(LiteralValue::Error(ExcelError::new(ExcelErrorKind::Div))));
        }
        let mut error = CvError { reason_len: 9, ..CvError::default() };
        error.reason[..9].copy_from_slice(b"admission");
        assert_eq!(reason_token(&error), "admission");
        error.reason[0] = b'A';
        assert_eq!(reason_token(&error), "decline");
    }

    #[test]
    fn today_is_the_utc_whole_day_serial() {
        let spec = crate::context::CalculationContextSpec {
            now: "2026-09-29T23:30:00-05:00".into(),
            operation: crate::Operation::Client,
            random_seed: 0,
            deadline_seconds: None,
            max_depth: 8,
            flags: crate::CalculationFlags::default(),
        };
        let context = CalculationContext::from_spec(&spec).unwrap();
        // 2026-09-30 in UTC; 2026-09-30 is serial 46295.
        assert_eq!(today_serial(&context).to_bits(), 46295.0_f64.to_bits());
    }
}
