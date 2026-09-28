//! Call errors and their spreadsheet error values.
//!
//! Mirrors `callbacks.py`: a `ChildRoutingError` becomes `#REF!` carrying the
//! message; any other exception is an infrastructure fault that becomes
//! `#CALC!` with the fixed message `child infrastructure failure`, is recorded
//! in the run's faults, cancels the parent and fails the run afterwards.

use formualizer_common::{ExcelError, ExcelErrorKind};

/// The `#CALC!` message every infrastructure fault returns to the cell.
pub const INFRASTRUCTURE_ERROR_MESSAGE: &str = "child infrastructure failure";

/// The `#CALC!` message a call gets when no run is bound to the workbook.
pub const UNBOUND_ERROR_MESSAGE: &str = "child router unbound";

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ModelCallError {
    /// A call the pinned package cannot route: unknown target, cycle, depth,
    /// bad input block, undeclared output, inputs the child's ports refuse.
    /// Returned to the cell as `#REF!` with this message (`ChildRoutingError`).
    #[error("{0}")]
    Routing(String),
    /// Anything else: engine failure, deadline, child Pending, a fault in a
    /// compiled child. `kind` is the Python exception type name the receipt
    /// records (`TimeoutError`, `RuntimeError`, ...). Returned as `#CALC!`.
    #[error("{kind}: {message}")]
    Infrastructure { kind: String, message: String },
    /// A Lane 0 placeholder that its owning lane has not implemented yet.
    /// Treated as an infrastructure fault.
    #[error("NotImplementedError: {0}")]
    NotImplemented(&'static str),
}

impl ModelCallError {
    pub fn routing(message: impl Into<String>) -> Self {
        Self::Routing(message.into())
    }

    pub fn infrastructure(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Infrastructure { kind: kind.into(), message: message.into() }
    }

    /// `TimeoutError: calculation deadline exceeded` (runtime.check_deadline).
    pub fn deadline() -> Self {
        Self::infrastructure("TimeoutError", "calculation deadline exceeded")
    }

    pub fn is_routing(&self) -> bool {
        matches!(self, Self::Routing(_))
    }

    /// The event's `error` text: the bare message for a routing error,
    /// `Type: message` for an infrastructure fault (callbacks.py).
    pub fn event_error(&self) -> String {
        self.to_string()
    }

    /// The spreadsheet error value the call returns to its cell.
    pub fn to_excel_error(&self) -> ExcelError {
        match self {
            Self::Routing(message) => ExcelError::new(ExcelErrorKind::Ref).with_message(message.clone()),
            Self::Infrastructure { .. } | Self::NotImplemented(_) => {
                ExcelError::new(ExcelErrorKind::Calc).with_message(INFRASTRUCTURE_ERROR_MESSAGE)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_error_maps_to_ref_with_message() {
        let error = ModelCallError::routing("maximum child depth exceeded");
        let value = error.to_excel_error();
        assert_eq!(value.kind, ExcelErrorKind::Ref);
        assert_eq!(value.message.as_deref(), Some("maximum child depth exceeded"));
        assert_eq!(error.event_error(), "maximum child depth exceeded");
    }

    #[test]
    fn infrastructure_error_maps_to_calc_with_fixed_message() {
        let error = ModelCallError::deadline();
        let value = error.to_excel_error();
        assert_eq!(value.kind, ExcelErrorKind::Calc);
        assert_eq!(value.message.as_deref(), Some(INFRASTRUCTURE_ERROR_MESSAGE));
        assert_eq!(error.event_error(), "TimeoutError: calculation deadline exceeded");
        assert_eq!(ModelCallError::NotImplemented("x").to_excel_error().kind, ExcelErrorKind::Calc);
    }
}
