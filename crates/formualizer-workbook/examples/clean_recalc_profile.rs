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
    eval.enable_parallel = false;
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
    Ok(())
}
#[cfg(not(all(feature = "calamine", not(target_arch = "wasm32"))))]
fn main() {
    eprintln!("this example requires the calamine backend");
}
