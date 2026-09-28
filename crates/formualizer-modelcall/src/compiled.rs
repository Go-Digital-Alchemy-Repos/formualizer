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
//! * [`NativeModule`] (one loaded cdylib) and [`NativeCompiledHook`] (the
//!   [`CompiledChildHook`] / [`CompiledParent`] over a registry of modules).
//!
//! WP0 (seam commit) ships the types only. Every entry point declines with
//! [`NativeDecline::NotImplemented`] (route `engine:native_not_implemented`),
//! so installing the hook changes no result: the engine path runs. WP2 owns
//! the body (module cache, `libloading`, value law, ranged ports, `read_cells`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use formualizer_common::LiteralValue;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::evaluator::{CompiledAttempt, CompiledChildHook, CompiledParent, CompiledXcall, ParentAttempt};
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

/// Why the native path did not answer. `reason()` is the text after
/// `engine:` / `fallback:` in the route record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeDecline {
    /// WP0 skeleton: the native path is not built yet.
    NotImplemented,
    /// No registry entry for this workbook_sha256 (or the entry is `"engine"`).
    NotRegistered,
    /// The module reports a different `cv_native_abi()`.
    AbiMismatch { found: u32 },
}

impl NativeDecline {
    pub fn reason(&self) -> &'static str {
        match self {
            Self::NotImplemented => "native_not_implemented",
            Self::NotRegistered => "not_registered",
            Self::AbiMismatch { .. } => "native_abi",
        }
    }

    /// The route record for a call that never reached a module.
    pub fn engine_route(&self) -> Value {
        Value::String(format!("engine:{}", self.reason()))
    }
}

/// One entry of the value-free native registry the parity loader hands to
/// `ModelSession.set_native_compiled` (`{sha: {native_path, engine_commit,
/// manifest_sha256}}`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NativeRegistryEntry {
    pub native_path: PathBuf,
    pub engine_commit: String,
    pub manifest_sha256: String,
}

/// One loaded native module (WP2: a `libloading::Library` plus the resolved
/// entry points and the parsed `cv_meta_json`).
#[derive(Debug)]
pub struct NativeModule {
    pub workbook_sha256: String,
    pub entry: NativeRegistryEntry,
}

impl NativeModule {
    /// WP0: always declines `NotImplemented`.
    pub fn load(_workbook_sha256: &str, _entry: &NativeRegistryEntry) -> Result<Self, NativeDecline> {
        Err(NativeDecline::NotImplemented)
    }
}

/// The native compiled hook over a registry of modules, keyed by
/// `workbook_sha256`. Implements both the child and the parent seam.
#[derive(Debug, Default)]
pub struct NativeCompiledHook {
    entries: BTreeMap<String, NativeRegistryEntry>,
    routes: Mutex<Vec<Value>>,
}

impl NativeCompiledHook {
    pub fn new(entries: BTreeMap<String, NativeRegistryEntry>) -> Self {
        Self { entries, routes: Mutex::new(Vec::new()) }
    }

    /// Parse the value-free registry document (`native_registry_json`).
    pub fn from_registry_json(text: &str) -> Result<Self, ModelCallError> {
        let entries: BTreeMap<String, NativeRegistryEntry> = serde_json::from_str(text)
            .map_err(|error| ModelCallError::infrastructure("ValueError", format!("native registry: {error}")))?;
        Ok(Self::new(entries))
    }

    pub fn entries(&self) -> &BTreeMap<String, NativeRegistryEntry> {
        &self.entries
    }

    fn decline_for(&self, workbook_sha256: &str) -> NativeDecline {
        match self.entries.get(workbook_sha256) {
            None => NativeDecline::NotRegistered,
            Some(entry) => match NativeModule::load(workbook_sha256, entry) {
                Ok(_) | Err(NativeDecline::NotImplemented) => NativeDecline::NotImplemented,
                Err(other) => other,
            },
        }
    }

    fn record(&self, route: &Value) {
        if let Ok(mut routes) = self.routes.lock() {
            routes.push(route.clone());
        }
    }
}

impl CompiledChildHook for NativeCompiledHook {
    fn attempt(
        &self,
        spec: &ModelSpec,
        _inputs: &[(String, LiteralValue)],
        _output: &PortLocation,
        _stack: &[String],
    ) -> Result<CompiledAttempt, ModelCallError> {
        let route = self.decline_for(&spec.workbook_sha256).engine_route();
        self.record(&route);
        Ok(CompiledAttempt { matrix: None, route: Some(route) })
    }

    fn report(&self) -> Map<String, Value> {
        let routes = self.routes.lock().map(|routes| routes.clone()).unwrap_or_default();
        let mut report = Map::new();
        report.insert("calls".into(), Value::from(routes.len()));
        report.insert("routes".into(), Value::Array(routes));
        report
    }
}

impl CompiledParent for NativeCompiledHook {
    fn run(
        &self,
        spec: &ModelSpec,
        _inputs: &[(String, LiteralValue)],
        _context: &CalculationContext,
        _xcall: &mut dyn CompiledXcall,
    ) -> Result<ParentAttempt, ModelCallError> {
        let route = self.decline_for(&spec.workbook_sha256).engine_route();
        self.record(&route);
        Ok(ParentAttempt::Declined { route })
    }

    fn report(&self) -> Map<String, Value> {
        CompiledChildHook::report(self)
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
    fn skeleton_hook_declines_to_the_engine() {
        let registry = r#"{"abc": {"native_path": "/nowhere/libcv_native.so", "engine_commit": "0027ee70", "manifest_sha256": "m"}}"#;
        let hook = NativeCompiledHook::from_registry_json(registry).expect("registry parses");
        assert_eq!(hook.decline_for("abc"), NativeDecline::NotImplemented);
        assert_eq!(hook.decline_for("other"), NativeDecline::NotRegistered);
        assert_eq!(NativeDecline::NotImplemented.engine_route(), Value::from("engine:native_not_implemented"));
        assert!(NativeCompiledHook::from_registry_json("[]").is_err());
    }
}
