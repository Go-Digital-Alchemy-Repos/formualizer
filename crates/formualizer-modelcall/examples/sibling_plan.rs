//! `cargo run -p formualizer-modelcall --example sibling_plan -- plan <parent.xlsx> [--full]`
//!
//! The Rust twin of `python -m workbook_runtime.prefetch plan <parent.xlsx>`:
//! prints the parent's sibling-plan counts (no target, name or value), so the
//! two plans compare key for key (GOD-383 CP2 item 3). `--full` adds every
//! group's target, output, cells and per-member position values; that output
//! can carry workbook constants and belongs only where client data may live.

use formualizer_common::LiteralValue;
use formualizer_modelcall::prefetch::{PlanValue, SiblingPlan, sibling_plan_for_path};
use serde_json::{Value, json};
use std::path::Path;
use std::process::ExitCode;

fn typed(value: &PlanValue) -> Value {
    match value {
        PlanValue::Dynamic => json!({"type": "dynamic"}),
        PlanValue::Absent => json!({"type": "absent"}),
        PlanValue::Constant(LiteralValue::Boolean(flag)) => json!({"type": "bool", "value": flag}),
        PlanValue::Constant(LiteralValue::Int(number)) => json!({"type": "int", "value": number}),
        PlanValue::Constant(LiteralValue::Number(number)) => {
            json!({"type": "float", "bits": format!("{:016x}", number.to_bits())})
        }
        PlanValue::Constant(LiteralValue::Text(text)) => json!({"type": "str", "value": text}),
        PlanValue::Constant(other) => json!({"type": "other", "value": format!("{other:?}")}),
    }
}

fn full(plan: &SiblingPlan) -> Value {
    Value::Array(
        plan.groups
            .iter()
            .map(|group| {
                json!({
                    "target": group.target,
                    "output": group.output,
                    "cells": group.cells,
                    "positions": group.positions.iter().map(|position| json!({
                        "name": position.name,
                        "kind": position.kind.as_str(),
                        "values": position.values.iter().map(typed).collect::<Vec<_>>(),
                    })).collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let with_full = args.iter().any(|arg| arg == "--full");
    let positional: Vec<&String> = args.iter().filter(|arg| !arg.starts_with("--")).collect();
    if positional.len() != 2 || positional[0] != "plan" {
        eprintln!("usage: sibling_plan plan <parent.xlsx> [--full]");
        return ExitCode::from(2);
    }
    match sibling_plan_for_path(Path::new(positional[1])) {
        Ok(plan) => {
            let mut out = json!({"counts": plan.counts()});
            if with_full {
                out["groups"] = full(&plan);
            }
            println!("{out}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(1)
        }
    }
}
