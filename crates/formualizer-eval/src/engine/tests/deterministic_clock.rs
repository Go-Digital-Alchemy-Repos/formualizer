use crate::engine::{DeterministicMode, Engine, EvalConfig};
use crate::test_workbook::TestWorkbook;
use crate::timezone::TimeZoneSpec;
use chrono::TimeZone;
use formualizer_common::LiteralValue;
use formualizer_parse::parser::{ASTNode, ASTNodeType};

#[test]
fn now_and_today_use_injected_fixed_clock() {
    let wb = TestWorkbook::new();
    let fixed = chrono::Utc
        .with_ymd_and_hms(2025, 1, 15, 10, 0, 0)
        .single()
        .expect("valid fixed timestamp");

    let cfg = EvalConfig {
        temporal_egress: crate::engine::TemporalEgress::Serial,
        deterministic_mode: DeterministicMode::Enabled {
            timestamp_utc: fixed,
            timezone: TimeZoneSpec::Utc,
        },
        ..Default::default()
    };
    let mut engine = Engine::new(wb, cfg);

    // A1 = NOW(), A2 = TODAY()
    engine
        .set_cell_formula(
            "Sheet1",
            1,
            1,
            ASTNode {
                node_type: ASTNodeType::Function {
                    name: "NOW".into(),
                    args: vec![],
                },
                source_token: None,
                contains_volatile: true,
            },
        )
        .unwrap();
    engine
        .set_cell_formula(
            "Sheet1",
            2,
            1,
            ASTNode {
                node_type: ASTNodeType::Function {
                    name: "TODAY".into(),
                    args: vec![],
                },
                source_token: None,
                contains_volatile: true,
            },
        )
        .unwrap();

    engine.evaluate_all().unwrap();

    let now_serial = match engine.get_cell_value("Sheet1", 1, 1).unwrap() {
        LiteralValue::Number(n) => n,
        v => panic!("Expected number, got {v:?}"),
    };
    let today_serial = match engine.get_cell_value("Sheet1", 2, 1).unwrap() {
        LiteralValue::Number(n) => n,
        v => panic!("Expected number, got {v:?}"),
    };

    let expected_now =
        formualizer_common::datetime_to_serial_for(engine.config.date_system, &fixed.naive_utc());
    let expected_today =
        formualizer_common::date_to_serial_for(engine.config.date_system, &fixed.date_naive());
    assert_eq!(now_serial, expected_now);
    assert_eq!(today_serial, expected_today);

    // Re-evaluating should remain stable (clock is fixed).
    engine.evaluate_all().unwrap();
    let now_serial_2 = match engine.get_cell_value("Sheet1", 1, 1).unwrap() {
        LiteralValue::Number(n) => n,
        _ => unreachable!(),
    };
    let today_serial_2 = match engine.get_cell_value("Sheet1", 2, 1).unwrap() {
        LiteralValue::Number(n) => n,
        _ => unreachable!(),
    };
    assert_eq!(now_serial, now_serial_2);
    assert_eq!(today_serial, today_serial_2);
}

#[test]
fn deterministic_mode_rejects_local_timezone() {
    let wb = TestWorkbook::new();
    let mut engine = Engine::new(wb, EvalConfig::default());

    let fixed = chrono::Utc
        .with_ymd_and_hms(2025, 1, 15, 10, 0, 0)
        .single()
        .unwrap();

    let res = engine.set_deterministic_mode(DeterministicMode::Enabled {
        timestamp_utc: fixed,
        timezone: TimeZoneSpec::Local,
    });
    assert!(res.is_err());
}

/// Build `=<ref> + 1` over a single-cell reference.
#[cfg(test)]
fn plus_one(row: u32, col: u32, original: &str) -> ASTNode {
    use formualizer_parse::parser::ReferenceType;
    ASTNode {
        node_type: ASTNodeType::BinaryOp {
            op: "+".into(),
            left: Box::new(ASTNode {
                node_type: ASTNodeType::Reference {
                    original: original.to_string(),
                    reference: ReferenceType::cell(None, row, col),
                },
                source_token: None,
                contains_volatile: false,
            }),
            right: Box::new(ASTNode {
                node_type: ASTNodeType::Literal(LiteralValue::Int(1)),
                source_token: None,
                contains_volatile: false,
            }),
        },
        source_token: None,
        contains_volatile: false,
    }
}

#[cfg(test)]
fn clock_fn(name: &str) -> ASTNode {
    ASTNode {
        node_type: ASTNodeType::Function {
            name: name.into(),
            args: vec![],
        },
        source_token: None,
        contains_volatile: false,
    }
}

/// A frozen deterministic clock makes `NOW()`/`TODAY()` constants, so a recalc
/// with nothing dirty must evaluate nothing at all — not the clock cells, and
/// above all not their dependent cone. Moving the frozen clock to a new
/// timestamp must re-evaluate exactly that cone and leave the rest clean.
///
/// Measured on the 18k-formula Avocet parent workbook before this behaviour
/// existed: 14 `TODAY()` cells re-dirtied 105,778 vertices on every pass and a
/// no-op `evaluate_all` cost ~0.94 s (release).
#[test]
fn frozen_clock_leaves_clock_volatiles_and_their_cone_clean() {
    const PURE_FORMULAS: u32 = 15_000;
    const CLOCK_DEPENDENTS: u32 = 5_000;
    const CLOCK_CELLS: usize = 3;

    let first = chrono::Utc
        .with_ymd_and_hms(2025, 1, 15, 10, 0, 0)
        .single()
        .expect("valid fixed timestamp");
    let cfg = EvalConfig {
        temporal_egress: crate::engine::TemporalEgress::Serial,
        deterministic_mode: DeterministicMode::Enabled {
            timestamp_utc: first,
            timezone: TimeZoneSpec::Utc,
        },
        ..Default::default()
    };
    let mut engine = Engine::new(TestWorkbook::new(), cfg);

    // A1 literal seed; A2..A4 clock volatiles.
    engine
        .set_cell_value("Sheet1", 1, 1, LiteralValue::Int(10))
        .unwrap();
    for row in 2..=(1 + CLOCK_CELLS as u32) {
        engine
            .set_cell_formula("Sheet1", row, 1, clock_fn("TODAY"))
            .unwrap();
    }

    // Column B: pure dependents of the literal. Column C: dependents of A2.
    for row in 1..=PURE_FORMULAS {
        engine
            .set_cell_formula("Sheet1", row, 2, plus_one(1, 1, "A1"))
            .unwrap();
    }
    for row in 1..=CLOCK_DEPENDENTS {
        engine
            .set_cell_formula("Sheet1", row, 3, plus_one(2, 1, "A2"))
            .unwrap();
    }

    let total_formulas = PURE_FORMULAS + CLOCK_DEPENDENTS + CLOCK_CELLS as u32;
    let first_pass = engine.evaluate_all().unwrap();
    assert!(
        first_pass.computed_vertices >= total_formulas as usize,
        "first pass should evaluate every formula, got {}",
        first_pass.computed_vertices
    );

    let today_before = engine.get_cell_value("Sheet1", 2, 1).unwrap();

    for pass in 0..3 {
        let clean = engine.evaluate_all().unwrap();
        assert_eq!(
            clean.computed_vertices, 0,
            "clean pass {pass} under a frozen clock must evaluate nothing"
        );
    }
    assert_eq!(engine.get_cell_value("Sheet1", 2, 1).unwrap(), today_before);

    // Move the frozen clock: only the clock cells and their cone recompute.
    let second = chrono::Utc
        .with_ymd_and_hms(2025, 3, 20, 10, 0, 0)
        .single()
        .expect("valid fixed timestamp");
    engine
        .set_deterministic_mode(DeterministicMode::Enabled {
            timestamp_utc: second,
            timezone: TimeZoneSpec::Utc,
        })
        .unwrap();

    let moved = engine.evaluate_all().unwrap();
    assert_eq!(
        moved.computed_vertices,
        CLOCK_CELLS + CLOCK_DEPENDENTS as usize,
        "only the clock cells and their dependents should recompute"
    );
    assert_ne!(
        engine.get_cell_value("Sheet1", 2, 1).unwrap(),
        today_before,
        "TODAY() must observe the new deterministic timestamp"
    );

    // And the workbook settles again.
    assert_eq!(engine.evaluate_all().unwrap().computed_vertices, 0);
}

/// The frozen-clock skip is clock-specific: a volatile that does not come from
/// the clock (here `RAND()`) keeps re-evaluating every recalc even while the
/// deterministic clock is pinned.
#[test]
fn frozen_clock_does_not_park_non_clock_volatiles() {
    let fixed = chrono::Utc
        .with_ymd_and_hms(2025, 1, 15, 10, 0, 0)
        .single()
        .expect("valid fixed timestamp");
    let cfg = EvalConfig {
        temporal_egress: crate::engine::TemporalEgress::Serial,
        deterministic_mode: DeterministicMode::Enabled {
            timestamp_utc: fixed,
            timezone: TimeZoneSpec::Utc,
        },
        ..Default::default()
    };
    let mut engine = Engine::new(TestWorkbook::new(), cfg);

    engine
        .set_cell_formula("Sheet1", 1, 1, clock_fn("RAND"))
        .unwrap();
    engine
        .set_cell_formula("Sheet1", 2, 1, plus_one(1, 1, "A1"))
        .unwrap();

    engine.evaluate_all().unwrap();
    let after_first = engine.evaluate_all().unwrap();
    assert_eq!(
        after_first.computed_vertices, 2,
        "RAND() and its dependent must re-evaluate under a frozen clock"
    );
}

/// Re-pinning the *same* deterministic clock is idempotent: it must not wake
/// the clock-only volatiles. The session runtime calls
/// `set_deterministic_clock` with the same timestamp on every acquire of a
/// retained workbook, so a re-dirty here would reinstate the whole clean-pass
/// floor on every request (measured: 0.83 s per repeat request on the Avocet
/// parent). Re-pinning a *different* timestamp must still wake exactly the
/// clock cone.
#[test]
fn repinning_the_same_deterministic_clock_evaluates_nothing() {
    const CLOCK_DEPENDENTS: u32 = 500;

    let fixed = chrono::Utc
        .with_ymd_and_hms(2025, 1, 15, 10, 0, 0)
        .single()
        .expect("valid fixed timestamp");
    let same = DeterministicMode::Enabled {
        timestamp_utc: fixed,
        timezone: TimeZoneSpec::Utc,
    };
    let cfg = EvalConfig {
        temporal_egress: crate::engine::TemporalEgress::Serial,
        deterministic_mode: same.clone(),
        ..Default::default()
    };
    let mut engine = Engine::new(TestWorkbook::new(), cfg);

    engine
        .set_cell_formula("Sheet1", 1, 1, clock_fn("TODAY"))
        .unwrap();
    for row in 1..=CLOCK_DEPENDENTS {
        engine
            .set_cell_formula("Sheet1", row, 2, plus_one(1, 1, "A1"))
            .unwrap();
    }
    engine.evaluate_all().unwrap();
    assert_eq!(engine.evaluate_all().unwrap().computed_vertices, 0);

    // Identical mode, timestamp and timezone: nothing to recompute.
    engine.set_deterministic_mode(same.clone()).unwrap();
    assert_eq!(
        engine.evaluate_all().unwrap().computed_vertices,
        0,
        "re-pinning the same deterministic clock must not dirty anything"
    );

    // Repeated re-pins stay idempotent.
    for _ in 0..3 {
        engine.set_deterministic_mode(same.clone()).unwrap();
        assert_eq!(engine.evaluate_all().unwrap().computed_vertices, 0);
    }

    // A different timestamp still wakes the clock cell and its cone.
    let moved = chrono::Utc
        .with_ymd_and_hms(2025, 6, 2, 10, 0, 0)
        .single()
        .expect("valid fixed timestamp");
    engine
        .set_deterministic_mode(DeterministicMode::Enabled {
            timestamp_utc: moved,
            timezone: TimeZoneSpec::Utc,
        })
        .unwrap();
    assert_eq!(
        engine.evaluate_all().unwrap().computed_vertices,
        1 + CLOCK_DEPENDENTS as usize,
        "moving the deterministic clock must recompute the clock cone"
    );
    assert_eq!(engine.evaluate_all().unwrap().computed_vertices, 0);

    // Same timestamp, different timezone is also a move.
    engine
        .set_deterministic_mode(DeterministicMode::Enabled {
            timestamp_utc: moved,
            timezone: TimeZoneSpec::FixedOffsetSeconds(3600),
        })
        .unwrap();
    assert_eq!(
        engine.evaluate_all().unwrap().computed_vertices,
        1 + CLOCK_DEPENDENTS as usize,
        "changing the deterministic timezone must recompute the clock cone"
    );
}
