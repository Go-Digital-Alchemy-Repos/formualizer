//! The one table of imported (Coherent) call-function names.
//!
//! Client workbooks carry the child-model call under its Coherent spelling.
//! Those names are accepted here, at the import boundary, and nowhere else in
//! Rust: registration (`register_import_aliases` in the Python binding) and the
//! sibling-plan formula scan both read this table. Our own name is
//! [`CALL_MODEL_FUNCTION`] (research/NAMING_STANDARD.md).

/// Our registered name for the child-model call.
pub const CALL_MODEL_FUNCTION: &str = "MDL.CALLMODEL";

/// Imported spellings of the same function, bound to the same handler.
/// Mirrors the parity repository's `workbook_runtime/import_boundary.py`
/// `IMPORTED_CALL_NAMES`.
pub const IMPORTED_CALL_NAMES: &[&str] = &["_xldudf_CS_SPARK_XCALL", "CS.SPARK.XCALL"];

/// Defined-name prefix of an imported goal-seek block (`package.py`,
/// `engine_adapter.XSOLVE_PREFIX`). Goal-seek code reads blocks by this prefix
/// and reports them by the suffix (`GoalSeekSpec::suffix`).
pub const GOAL_SEEK_BLOCK_PREFIX: &str = "Xsolve_";

/// Every name the call handler is registered under: ours first, then the
/// imported aliases, in table order.
pub fn call_function_names() -> impl Iterator<Item = &'static str> {
    std::iter::once(CALL_MODEL_FUNCTION).chain(IMPORTED_CALL_NAMES.iter().copied())
}

/// Whether `name` (as written in a formula, any case) is the call function.
pub fn is_call_function(name: &str) -> bool {
    call_function_names().any(|known| known.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_function_names_lists_ours_then_imported() {
        let names: Vec<_> = call_function_names().collect();
        assert_eq!(names, ["MDL.CALLMODEL", "_xldudf_CS_SPARK_XCALL", "CS.SPARK.XCALL"]);
    }

    #[test]
    fn is_call_function_ignores_ascii_case() {
        assert!(is_call_function("mdl.callmodel"));
        assert!(is_call_function("cs.spark.xcall"));
        assert!(!is_call_function("MDL.CALL"));
    }
}
