//! A hand-written `CV_NATIVE_ABI = 1` module for the host tests in
//! `formualizer-modelcall/tests/native_compiled.rs`. No generated code, no
//! pyo3, no dependencies. The workbook identity comes from the build
//! environment (`STUB_WORKBOOK_SHA`), so one source gives several modules.
//!
//! Workbook: sheet `Calc` (20 x 6) and sheet `Report` (3 x 3).
//! Ports (`PORT_NAMES` order): `amount` A1 (default 1), `mode` A2 (default
//! `plain`), `table` A10:C11 (a 2x3 ranged port, default [[1,2,3],[4,5,6]]).
//! Outputs: `result` C1:D2 (2x2) and `total` A5 (amount + sum(table)).
//! `mode` selects what the run does to `result` (see `run_mode`).

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};

pub const CV_NATIVE_ABI: u32 = 1;
const TAG_BLANK: u32 = 0;
const TAG_NUM: u32 = 1;
const TAG_BOOL: u32 = 2;
const TAG_STR: u32 = 3;
const TAG_ERR: u32 = 4;
const XLERR_NA: u32 = 0;
const XLERR_DIV0: u32 = 2;
const XLERR_NUM: u32 = 5;
const PORT_NOT_SUPPLIED: u32 = 0;
const PORT_VALUE: u32 = 1;
const PORT_ROWS: u32 = 2;
const ARG_SCALAR: u32 = 0;
const ARG_ROWS: u32 = 1;
const OK: u32 = 0;
const ERR_DECLINE: u32 = 1;
const ERR_VIOLATION: u32 = 2;
const ERR_XCALL: u32 = 3;
const ERR_PANIC: u32 = 4;
const ERR_ARGS: u32 = 5;
const REASON_CAP: usize = 64;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CvVal {
    pub tag: u32,
    pub num: f64,
    pub str_ptr: *const u8,
    pub str_len: usize,
    pub err_code: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CvMatrix {
    pub rows: usize,
    pub cols: usize,
    pub vals: *mut CvVal,
    pub owner: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CvPort {
    pub kind: u32,
    pub value: CvVal,
    pub rows: CvMatrix,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CvArg {
    pub kind: u32,
    pub value: CvVal,
    pub rows: CvMatrix,
}

#[repr(C)]
pub struct CvError {
    pub code: u32,
    pub unit: i64,
    pub reason_len: u32,
    pub reason: [u8; REASON_CAP],
}

#[repr(C)]
pub struct CvRunStats {
    pub xcalls: u64,
    pub guard_views: u64,
    pub guard_probes: u64,
    pub guard_max_row_minus_limit: i64,
    pub t_fresh_s: f64,
    pub t_run_s: f64,
}

pub type CvXcallFn = unsafe extern "C" fn(*mut c_void, *const CvArg, usize, *mut CvMatrix) -> i32;

#[derive(Clone, Debug, PartialEq)]
enum Val {
    Blank,
    Num(f64),
    Bool(bool),
    Str(String),
    Err(u32),
}

const SHEET_DIMS: [(usize, usize); 2] = [(20, 6), (3, 3)];

pub struct CvRun {
    sheets: Vec<Vec<Vec<Val>>>,
    xcalls: u64,
}

impl CvRun {
    fn set(&mut self, sheet: usize, row: usize, col: usize, value: Val) {
        self.sheets[sheet][row - 1][col - 1] = value;
    }
    fn get(&self, sheet: usize, row: usize, col: usize) -> &Val {
        &self.sheets[sheet][row - 1][col - 1]
    }
}

static META: &str = concat!(
    r#"{"schema": "god383-rs-cv-meta-1", "workbook_sha256": ""#,
    env!("STUB_WORKBOOK_SHA"),
    r#"", "artifact_format": 3, "sheets": ["Calc", "Report"], "port_names": ["amount", "mode", "table"], "#,
    r#""ports": {"amount": [0, 1, 1, 1, 1], "mode": [0, 2, 1, 2, 1], "table": [0, 10, 1, 11, 3]}, "#,
    r#""port_default_kind": ["constant", "constant", "constant"], "#,
    r#""outputs": {"result": [0, 1, 3, 2, 4], "total": [0, 5, 1, 5, 1]}, "#,
    r#""output_shape": {"result": [2, 2], "total": [1, 1]}}"#
);

enum Failure {
    Code(u32, i64, &'static str),
}

fn fail(err: *mut CvError, code: u32, unit: i64, reason: &str) {
    if err.is_null() {
        return;
    }
    // SAFETY: the host passes a valid CvError for the call.
    let err = unsafe { &mut *err };
    err.code = code;
    err.unit = unit;
    let bytes = reason.as_bytes();
    let n = bytes.len().min(REASON_CAP);
    err.reason[..n].copy_from_slice(&bytes[..n]);
    err.reason_len = n as u32;
}

unsafe fn val_in(value: &CvVal) -> Val {
    match value.tag {
        TAG_NUM => Val::Num(value.num),
        TAG_BOOL => Val::Bool(value.num != 0.0),
        TAG_STR => {
            let bytes = if value.str_len == 0 {
                &[][..]
            } else {
                std::slice::from_raw_parts(value.str_ptr, value.str_len)
            };
            Val::Str(String::from_utf8_lossy(bytes).into_owned())
        }
        TAG_ERR => Val::Err(value.err_code),
        _ => Val::Blank,
    }
}

fn val_out(value: &Val) -> CvVal {
    let mut out = CvVal { tag: TAG_BLANK, num: 0.0, str_ptr: std::ptr::null(), str_len: 0, err_code: 0 };
    match value {
        Val::Blank => {}
        Val::Num(n) => {
            out.tag = TAG_NUM;
            out.num = *n;
        }
        Val::Bool(b) => {
            out.tag = TAG_BOOL;
            out.num = if *b { 1.0 } else { 0.0 };
        }
        Val::Str(s) => {
            out.tag = TAG_STR;
            out.str_ptr = s.as_ptr();
            out.str_len = s.len();
        }
        Val::Err(code) => {
            out.tag = TAG_ERR;
            out.err_code = *code;
        }
    }
    out
}

unsafe fn matrix_in(matrix: &CvMatrix) -> Vec<Vec<Val>> {
    let mut rows = Vec::with_capacity(matrix.rows);
    for r in 0..matrix.rows {
        let mut row = Vec::with_capacity(matrix.cols);
        for c in 0..matrix.cols {
            row.push(val_in(&*matrix.vals.add(r * matrix.cols + c)));
        }
        rows.push(row);
    }
    rows
}

fn num(value: &Val) -> f64 {
    match value {
        Val::Num(n) => *n,
        Val::Bool(true) => 1.0,
        _ => 0.0,
    }
}

/// One nested call: `[target, block, output, tail...]`; returns the answer
/// matrix (copied) or the failure the module reports.
unsafe fn nested(
    run: &mut CvRun,
    xcall: Option<CvXcallFn>,
    ctx: *mut c_void,
    block: Option<&Vec<Vec<Val>>>,
    amount: f64,
) -> Result<Vec<Vec<Val>>, Failure> {
    let Some(xcall) = xcall else { return Err(Failure::Code(ERR_XCALL, 1, "xcall")) };
    run.xcalls += 1;
    let target = Val::Str("rates/child".into());
    let output = Val::Str("result".into());
    let tail_name = Val::Str("amount".into());
    let tail_value = Val::Num(amount * 10.0);
    let block_vals: Vec<CvVal> = block.map(|rows| rows.iter().flatten().map(val_out).collect()).unwrap_or_default();
    let blank = CvMatrix { rows: 0, cols: 0, vals: std::ptr::null_mut(), owner: std::ptr::null_mut() };
    let scalar = |v: &Val| CvArg { kind: ARG_SCALAR, value: val_out(v), rows: blank };
    let block_arg = match block {
        Some(rows) => CvArg {
            kind: ARG_ROWS,
            value: val_out(&Val::Blank),
            rows: CvMatrix {
                rows: rows.len(),
                cols: rows.first().map_or(0, Vec::len),
                vals: block_vals.as_ptr() as *mut CvVal,
                owner: std::ptr::null_mut(),
            },
        },
        None => scalar(&Val::Num(0.0)),
    };
    let args = [scalar(&target), block_arg, scalar(&output), scalar(&tail_name), scalar(&tail_value)];
    let mut out = blank;
    let code = xcall(ctx, args.as_ptr(), args.len(), &mut out);
    if code != 0 {
        return Err(Failure::Code(ERR_XCALL, 1, "xcall"));
    }
    if out.rows * out.cols > 0 && out.vals.is_null() {
        return Err(Failure::Code(ERR_XCALL, 1, "xcall_result"));
    }
    Ok(matrix_in(&out))
}

/// What each `mode` does (output `result` is C1:D2):
/// `plain` [[amount, amount*2], [TRUE, "text"]]; `table` [[sum row 1, sum row 2], [table C11, blank]];
/// `nested` calls `rates/child` (block 0, tail amount = amount*10) and writes
/// [[answer[0][0], rows], [cols, blank]]; `nested_rows` the same with the table as block;
/// `twice` calls twice and writes the two answers; `today` [[today, blank], [blank, blank]];
/// `nonfinite` +inf at C1; `err_num` / `err_na` / `err_div` that error at C1;
/// `decline` declines `stub_decline`; `violation` a guard violation at unit 7;
/// `panic` panics inside the entry; `badrect` asks the host nothing and fails CV_ERR_ARGS.
unsafe fn run_mode(
    run: &mut CvRun,
    mode: &str,
    amount: f64,
    table: &Vec<Vec<Val>>,
    today: f64,
    xcall: Option<CvXcallFn>,
    ctx: *mut c_void,
) -> Result<(), Failure> {
    let plain = |run: &mut CvRun| {
        run.set(0, 1, 3, Val::Num(amount));
        run.set(0, 1, 4, Val::Num(amount * 2.0));
        run.set(0, 2, 3, Val::Bool(true));
        run.set(0, 2, 4, Val::Str("text".into()));
    };
    match mode {
        "plain" => plain(run),
        "table" => {
            run.set(0, 1, 3, Val::Num(table[0].iter().map(num).sum()));
            run.set(0, 1, 4, Val::Num(table[1].iter().map(num).sum()));
            run.set(0, 2, 3, table[1][2].clone());
        }
        "nested" | "nested_rows" | "twice" => {
            let block = (mode == "nested_rows").then_some(table);
            let first = nested(run, xcall, ctx, block, amount)?;
            let head = first.first().and_then(|row| row.first()).cloned().unwrap_or(Val::Blank);
            run.set(0, 1, 3, head);
            run.set(0, 1, 4, Val::Num(first.len() as f64));
            run.set(0, 2, 3, Val::Num(first.first().map_or(0, Vec::len) as f64));
            if mode == "twice" {
                let second = nested(run, xcall, ctx, None, amount + 1.0)?;
                let head = second.first().and_then(|row| row.first()).cloned().unwrap_or(Val::Blank);
                run.set(0, 2, 4, head);
            }
        }
        "today" => run.set(0, 1, 3, Val::Num(today)),
        "nonfinite" => {
            plain(run);
            run.set(0, 1, 3, Val::Num(f64::INFINITY));
        }
        "err_num" | "err_na" | "err_div" => {
            plain(run);
            let code = match mode {
                "err_num" => XLERR_NUM,
                "err_na" => XLERR_NA,
                _ => XLERR_DIV0,
            };
            run.set(0, 1, 3, Val::Err(code));
        }
        "decline" => return Err(Failure::Code(ERR_DECLINE, -1, "stub_decline")),
        "violation" => return Err(Failure::Code(ERR_VIOLATION, 7, "")),
        "panic" => panic!("stub panic"),
        "badrect" => return Err(Failure::Code(ERR_ARGS, -1, "")),
        _ => return Err(Failure::Code(ERR_DECLINE, -1, "mode")),
    }
    Ok(())
}

#[no_mangle]
pub extern "C" fn cv_native_abi() -> u32 {
    CV_NATIVE_ABI
}

#[no_mangle]
pub unsafe extern "C" fn cv_meta_json(len: *mut usize) -> *const u8 {
    if !len.is_null() {
        *len = META.len();
    }
    META.as_ptr()
}

#[no_mangle]
pub unsafe extern "C" fn cv_run(
    ports: *const CvPort,
    n_ports: usize,
    today: f64,
    xcall: Option<CvXcallFn>,
    ctx: *mut c_void,
    err: *mut CvError,
) -> *mut CvRun {
    let body = AssertUnwindSafe(|| -> Result<Box<CvRun>, Failure> {
        if ports.is_null() || n_ports != 3 {
            return Err(Failure::Code(ERR_ARGS, -1, ""));
        }
        let ports = std::slice::from_raw_parts(ports, n_ports);
        let mut run = Box::new(CvRun {
            sheets: SHEET_DIMS.iter().map(|&(r, c)| vec![vec![Val::Blank; c]; r]).collect(),
            xcalls: 0,
        });
        let amount = match ports[0].kind {
            PORT_NOT_SUPPLIED => Val::Num(1.0),
            PORT_VALUE => val_in(&ports[0].value),
            _ => return Err(Failure::Code(ERR_DECLINE, -1, "admission")),
        };
        let mode = match ports[1].kind {
            PORT_NOT_SUPPLIED => "plain".to_owned(),
            PORT_VALUE => match val_in(&ports[1].value) {
                Val::Str(text) => text,
                _ => return Err(Failure::Code(ERR_DECLINE, -1, "mode")),
            },
            _ => return Err(Failure::Code(ERR_DECLINE, -1, "admission")),
        };
        let table = match ports[2].kind {
            PORT_NOT_SUPPLIED => {
                vec![(1..=3).map(|n| Val::Num(n as f64)).collect(), (4..=6).map(|n| Val::Num(n as f64)).collect()]
            }
            PORT_ROWS => {
                let rows = &ports[2].rows;
                if rows.rows != 2 || rows.cols != 3 || rows.vals.is_null() {
                    return Err(Failure::Code(ERR_DECLINE, -1, "admission"));
                }
                matrix_in(rows)
            }
            _ => return Err(Failure::Code(ERR_DECLINE, -1, "admission")),
        };
        run.set(0, 1, 1, amount.clone());
        run.set(0, 2, 1, Val::Str(mode.clone()));
        run.set(0, 1, 2, Val::Num(today));
        for (r, row) in table.iter().enumerate() {
            for (c, value) in row.iter().enumerate() {
                run.set(0, 10 + r, 1 + c, value.clone());
            }
        }
        let amount = num(&amount);
        run_mode(&mut run, &mode, amount, &table, today, xcall, ctx)?;
        let total = amount + table.iter().flatten().map(num).sum::<f64>();
        run.set(0, 5, 1, Val::Num(total));
        run.set(1, 1, 1, Val::Str("label".into()));
        run.set(1, 2, 2, Val::Num(amount + 1.0));
        run.set(1, 3, 3, Val::Err(XLERR_NA));
        Ok(run)
    });
    match catch_unwind(body) {
        Ok(Ok(run)) => {
            fail(err, OK, -1, "");
            Box::into_raw(run)
        }
        Ok(Err(Failure::Code(code, unit, reason))) => {
            fail(err, code, unit, reason);
            std::ptr::null_mut()
        }
        Err(_) => {
            fail(err, ERR_PANIC, -1, "");
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn cv_run_read_rect(
    run: *const CvRun,
    sheet_index: u32,
    r1: u32,
    c1: u32,
    r2: u32,
    c2: u32,
    out: *mut CvMatrix,
) -> i32 {
    let body = AssertUnwindSafe(|| -> i32 {
        if run.is_null() || out.is_null() {
            return ERR_ARGS as i32;
        }
        let run = &*run;
        let sheet = sheet_index as usize;
        let Some(&(rows, cols)) = SHEET_DIMS.get(sheet) else { return ERR_ARGS as i32 };
        let (r1, c1, r2, c2) = (r1 as usize, c1 as usize, r2 as usize, c2 as usize);
        if r1 == 0 || c1 == 0 || r1 > r2 || c1 > c2 || r2 > rows || c2 > cols {
            return ERR_ARGS as i32;
        }
        let mut vals: Vec<CvVal> = Vec::with_capacity((r2 - r1 + 1) * (c2 - c1 + 1));
        for r in r1..=r2 {
            for c in c1..=c2 {
                // Strings point into the run store, which outlives the matrix.
                vals.push(val_out(run.get(sheet, r, c)));
            }
        }
        let mut boxed = Box::new(vals);
        *out = CvMatrix {
            rows: r2 - r1 + 1,
            cols: c2 - c1 + 1,
            vals: boxed.as_mut_ptr(),
            owner: Box::into_raw(boxed).cast(),
        };
        OK as i32
    });
    catch_unwind(body).unwrap_or(ERR_PANIC as i32)
}

#[no_mangle]
pub unsafe extern "C" fn cv_run_stats(run: *const CvRun, out: *mut CvRunStats) -> i32 {
    if run.is_null() || out.is_null() {
        return ERR_ARGS as i32;
    }
    *out = CvRunStats {
        xcalls: (*run).xcalls,
        guard_views: 0,
        guard_probes: 0,
        guard_max_row_minus_limit: i64::MIN,
        t_fresh_s: 0.0,
        t_run_s: 0.0,
    };
    OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn cv_matrix_free(matrix: *mut CvMatrix) {
    if matrix.is_null() || (*matrix).owner.is_null() {
        return;
    }
    drop(Box::from_raw((*matrix).owner.cast::<Vec<CvVal>>()));
    (*matrix).owner = std::ptr::null_mut();
    (*matrix).vals = std::ptr::null_mut();
}

#[no_mangle]
pub unsafe extern "C" fn cv_run_free(run: *mut CvRun) {
    if !run.is_null() {
        drop(Box::from_raw(run));
    }
}
