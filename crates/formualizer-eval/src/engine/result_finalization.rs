use crate::engine::{AuthoredFormulaKind, FormulaAuthorship, FormulaFence};
use crate::reference::CellRef;
use crate::traits::CalcValue;
use formualizer_common::{ExcelError, ExcelErrorKind, LiteralValue};

fn calc_top_left(value: CalcValue<'_>) -> LiteralValue {
    match value {
        CalcValue::Scalar(LiteralValue::Array(rows))
        | CalcValue::AnnotatedScalar(LiteralValue::Array(rows), _) => rows
            .first()
            .and_then(|row| row.first())
            .cloned()
            .unwrap_or_else(|| LiteralValue::Error(ExcelError::new(ExcelErrorKind::Value))),
        CalcValue::Scalar(value) | CalcValue::AnnotatedScalar(value, _) => value,
        CalcValue::Range(range) => {
            if range.is_empty() {
                LiteralValue::Error(ExcelError::new(ExcelErrorKind::Value))
            } else {
                range.get_cell(0, 0)
            }
        }
        CalcValue::Callable(_) => LiteralValue::Error(
            ExcelError::new(ExcelErrorKind::Calc).with_message("LAMBDA value must be invoked"),
        ),
    }
}

fn finalize_cse_result(value: CalcValue<'_>, fence: FormulaFence) -> LiteralValue {
    if fence.is_single_cell() {
        return calc_top_left(value);
    }

    match value.into_literal() {
        LiteralValue::Array(mut rows) => {
            rows.truncate(fence.rows());
            for row in &mut rows {
                row.truncate(fence.cols());
            }
            LiteralValue::Array(rows)
        }
        scalar => scalar,
    }
}

pub(crate) fn finalize_authored_formula_result(
    value: CalcValue<'_>,
    authorship: FormulaAuthorship,
    current_cell: CellRef,
) -> LiteralValue {
    let value = match authorship.kind {
        AuthoredFormulaKind::LegacyScalar => {
            crate::interpreter::implicit_intersection_calc_at(value, Some(current_cell))
        }
        AuthoredFormulaKind::CseArray => {
            let fence = authorship.cse_fence.unwrap_or_else(|| {
                FormulaFence::new(
                    current_cell.coord.row() + 1,
                    current_cell.coord.col() + 1,
                    current_cell.coord.row() + 1,
                    current_cell.coord.col() + 1,
                )
            });
            finalize_cse_result(value, fence)
        }
        AuthoredFormulaKind::DynamicArray => value.into_literal(),
    };
    finalize_formula_result(value)
}

/// Finalize a formula result immediately before it is published to the grid.
///
/// Excel exposes a blank-cell passthrough as numeric zero once it becomes a
/// formula cell's result. `Number(0.0)` matches the evaluator's existing
/// coercion results; stored blank cells remain `Empty` because only formula
/// publication calls this function.
pub(crate) fn finalize_formula_result(value: LiteralValue) -> LiteralValue {
    match value {
        LiteralValue::Empty => LiteralValue::Number(0.0),
        LiteralValue::Array(rows) => LiteralValue::Array(
            rows.into_iter()
                .map(|row| row.into_iter().map(finalize_formula_result).collect())
                .collect(),
        ),
        other => other,
    }
}
