//! Per-request calculation context (`runtime.CalculationContext`).
//!
//! The Python dataclass reads its feature flags from the environment when a
//! field is not passed (`mdl_env`, new `MDL_*` name then old `WORKBOOK_*`
//! name; see docs/modelcall_contract.md). The Rust context never reads the
//! environment: the binding passes resolved values, so a request's behaviour
//! is fixed by its context alone.

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

use crate::ModelCallError;

/// `CalculationContext.operation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Operation {
    #[default]
    Client,
    Report,
    Diagnostic,
}

/// Feature flags, resolved by the caller. Defaults are today's defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CalculationFlags {
    /// Per-request call memo (GOD-379). `MDL_CALL_MEMO` / `WORKBOOK_XCALL_MEMO`;
    /// on unless the variable is `0`.
    pub call_memo: bool,
    /// Sibling prefetch (GOD-383). `MDL_PREFETCH` / `WORKBOOK_XCALL_PREFETCH`;
    /// on only when `1`. Needs the memo.
    pub prefetch: bool,
    /// Concurrent sibling flights. `MDL_PREFETCH_MAX` /
    /// `WORKBOOK_XCALL_PREFETCH_MAX`; default 1, minimum 1.
    pub prefetch_max: u32,
    /// Admit finished error values from a sibling. `MDL_PREFETCH_ERRORS` /
    /// `WORKBOOK_XCALL_PREFETCH_ERRORS`; on only when `1`.
    pub prefetch_errors: bool,
    /// Consult the compiled-child hook first. `MDL_COMPILED` /
    /// `WORKBOOK_XCALL_COMPILED`; on only when `1`.
    pub compiled: bool,
    /// CL-097 skip-unchanged-writes. `WORKBOOK_SKIP_UNCHANGED_WRITES` (not
    /// renamed); on only when `1`.
    pub skip_unchanged_writes: bool,
}

impl Default for CalculationFlags {
    fn default() -> Self {
        Self {
            call_memo: true,
            prefetch: false,
            prefetch_max: 1,
            prefetch_errors: false,
            compiled: false,
            skip_unchanged_writes: false,
        }
    }
}

/// The wire form of the context (what the Python binding passes).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalculationContextSpec {
    /// Timezone-aware RFC 3339 clock (`CalculationContext.now`).
    pub now: String,
    #[serde(default)]
    pub operation: Operation,
    #[serde(default = "default_seed")]
    pub random_seed: u64,
    /// Seconds from construction until the deadline; `None` = no deadline.
    /// (Python holds an absolute monotonic time; the binding converts.)
    #[serde(default)]
    pub deadline_seconds: Option<f64>,
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,
    #[serde(default)]
    pub flags: CalculationFlags,
}

fn default_seed() -> u64 {
    147
}

fn default_max_depth() -> u32 {
    32
}

/// A validated request context.
#[derive(Debug, Clone)]
pub struct CalculationContext {
    /// Deterministic clock handed to every workbook (`set_deterministic_clock`).
    pub now: DateTime<FixedOffset>,
    pub operation: Operation,
    pub random_seed: u64,
    /// Absolute monotonic deadline.
    pub deadline: Option<Instant>,
    pub max_depth: u32,
    pub flags: CalculationFlags,
}

impl CalculationContext {
    /// Validate like `CalculationContext.__post_init__`.
    pub fn new(
        now: DateTime<FixedOffset>,
        operation: Operation,
        random_seed: u64,
        deadline: Option<Instant>,
        max_depth: u32,
        flags: CalculationFlags,
    ) -> Result<Self, ModelCallError> {
        if max_depth < 1 {
            return Err(ModelCallError::infrastructure("ValueError", "max_depth must be positive"));
        }
        Ok(Self { now, operation, random_seed, deadline, max_depth, flags })
    }

    /// From the wire form; the clock must carry an offset (timezone-aware).
    pub fn from_spec(spec: &CalculationContextSpec) -> Result<Self, ModelCallError> {
        let now = DateTime::parse_from_rfc3339(&spec.now).map_err(|_| {
            ModelCallError::infrastructure("ValueError", "calculation clock must be timezone-aware")
        })?;
        let deadline = match spec.deadline_seconds {
            None => None,
            Some(seconds) if seconds.is_finite() && seconds >= 0.0 => {
                Some(Instant::now() + Duration::from_secs_f64(seconds))
            }
            Some(_) => Some(Instant::now()),
        };
        Self::new(now, spec.operation, spec.random_seed, deadline, spec.max_depth, spec.flags.clone())
    }

    /// `check_deadline`: `TimeoutError: calculation deadline exceeded`.
    pub fn check_deadline(&self) -> Result<(), ModelCallError> {
        match self.deadline {
            Some(deadline) if Instant::now() >= deadline => Err(ModelCallError::deadline()),
            _ => Ok(()),
        }
    }

    /// Whether the prefetch may run: flag on and the memo on (it holds results).
    pub fn prefetch_enabled(&self) -> bool {
        self.flags.prefetch && self.flags.call_memo
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_spec_defaults_match_python() {
        let spec: CalculationContextSpec =
            serde_json::from_str(r#"{"now": "2026-09-28T00:00:00+00:00"}"#).unwrap();
        let context = CalculationContext::from_spec(&spec).unwrap();
        assert_eq!(context.random_seed, 147);
        assert_eq!(context.max_depth, 32);
        assert_eq!(context.operation, Operation::Client);
        assert!(context.flags.call_memo && !context.flags.prefetch && !context.flags.compiled);
        assert_eq!(context.flags.prefetch_max, 1);
        assert!(context.check_deadline().is_ok());
    }

    #[test]
    fn naive_clock_and_zero_depth_are_refused() {
        let naive = CalculationContextSpec {
            now: "2026-09-28T00:00:00".into(),
            operation: Operation::Report,
            random_seed: 1,
            deadline_seconds: None,
            max_depth: 32,
            flags: CalculationFlags::default(),
        };
        assert!(CalculationContext::from_spec(&naive).is_err());
        let zero = CalculationContextSpec { now: "2026-09-28T00:00:00Z".into(), max_depth: 0, ..naive };
        assert!(CalculationContext::from_spec(&zero).is_err());
    }

    #[test]
    fn expired_deadline_is_a_timeout() {
        let spec = CalculationContextSpec {
            now: "2026-09-28T00:00:00Z".into(),
            operation: Operation::Client,
            random_seed: 147,
            deadline_seconds: Some(0.0),
            max_depth: 32,
            flags: CalculationFlags::default(),
        };
        let context = CalculationContext::from_spec(&spec).unwrap();
        assert_eq!(context.check_deadline(), Err(ModelCallError::deadline()));
    }
}
