//! Profile repeated no-dirty `evaluate_all` passes on a workbook.
//!
//! Path comes from `FZ_PROFILE_XLSX`; no data is committed with the example.
//! Run with `--release --features xlsx-recalc --example clean_recalc_profile`.
#[cfg(all(feature = "calamine", not(target_arch = "wasm32")))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use formualizer_eval::engine::{CycleDetection, DeterministicMode, EvalConfig};
    use formualizer_workbook::backends::CalamineAdapter;
    use formualizer_workbook::traits::SpreadsheetReader;
    use formualizer_workbook::{LoadStrategy, Workbook, WorkbookConfig};
    use std::time::Instant;

    let path = std::env::var("FZ_PROFILE_XLSX")?;
    let passes: usize = std::env::var("FZ_PROFILE_PASSES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);

    let mut eval = EvalConfig::default();
    eval.enable_parallel = std::env::var("FZ_PARALLEL").is_ok();
    if let Some(n) = std::env::var("FZ_THREADS").ok().and_then(|v| v.parse().ok()) {
        eval.max_threads = Some(n);
    }
    eval.enable_virtual_dep_telemetry = std::env::var("FZ_VDEP_TELEMETRY").is_ok();
    eval.cycle.detection = CycleDetection::Runtime;
    eval.workbook_seed = 147;
    let mut cfg = WorkbookConfig::interactive();
    cfg.eval = EvalConfig {
        defer_graph_building: cfg.eval.defer_graph_building,
        formula_parse_policy: cfg.eval.formula_parse_policy,
        ..eval
    };

    let t = Instant::now();
    let adapter = <CalamineAdapter as SpreadsheetReader>::open_path(std::path::Path::new(&path))?;
    let mut wb = Workbook::from_reader(adapter, LoadStrategy::EagerAll, cfg)?;
    println!("load        {:?}", t.elapsed());

    wb.set_deterministic_mode(DeterministicMode::Enabled {
        timestamp_utc: "2026-01-01T12:00:00Z".parse()?,
        timezone: formualizer_eval::timezone::TimeZoneSpec::Utc,
    })?;

    let t = Instant::now();
    wb.prepare_graph_all()?;
    println!("prepare     {:?}", t.elapsed());

    let t = Instant::now();
    let r = wb.evaluate_all()?;
    println!(
        "first  wall={:?} computed={} cycles={}",
        t.elapsed(),
        r.computed_vertices,
        r.cycle_errors
    );
    for i in 0..passes {
        let t = Instant::now();
        let r = wb.evaluate_all()?;
        println!(
            "clean{i} wall={:?} computed={} cycles={}",
            t.elapsed(),
            r.computed_vertices,
            r.cycle_errors
        );
    }

    // Changed-scenario mode: rewrite ONE numeric input cell and recalc, the
    // shape of a scenario change through the session runtime. The cell is
    // named by `FZ_SCENARIO_CELL` as `Sheet!row,col`; the value alternates
    // between what the cell already holds and that times 1.1, so every pass
    // is a real change (an unchanged literal write is a documented no-op).
    // Values are never printed.
    let Ok(spec) = std::env::var("FZ_SCENARIO_CELL") else {
        return Ok(());
    };
    let (sheet, rowcol) = spec.rsplit_once('!').ok_or("FZ_SCENARIO_CELL=Sheet!row,col")?;
    let (row, col) = rowcol.split_once(',').ok_or("FZ_SCENARIO_CELL=Sheet!row,col")?;
    let (row, col): (u32, u32) = (row.trim().parse()?, col.trim().parse()?);
    let eval_telemetry = std::env::var("FZ_VDEP_TELEMETRY").is_ok();
    let base = match wb.get_value(sheet, row, col) {
        Some(formualizer_common::LiteralValue::Number(n)) => n,
        Some(formualizer_common::LiteralValue::Int(n)) => n as f64,
        other => return Err(format!("scenario cell is not numeric: {:?}", other.map(|_| ())).into()),
    };
    let scenario_passes: usize = std::env::var("FZ_SCENARIO_PASSES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    for i in 0..scenario_passes {
        let next = if i % 2 == 0 { base * 1.1 } else { base };
        let t = Instant::now();
        wb.set_value(sheet, row, col, formualizer_common::LiteralValue::Number(next))?;
        let write = t.elapsed();
        let t = Instant::now();
        let r = wb.evaluate_all()?;
        println!(
            "scenario{i} write={:?} eval={:?} computed={} cycles={}",
            write,
            t.elapsed(),
            r.computed_vertices,
            r.cycle_errors
        );
        if eval_telemetry {
            println!("  vdep {:?}", wb.engine().last_virtual_dep_telemetry());
        }
    }
    Ok(())
}
#[cfg(not(all(feature = "calamine", not(target_arch = "wasm32"))))]
fn main() {
    eprintln!("this example requires the calamine backend");
}
