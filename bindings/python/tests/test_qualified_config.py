"""The qualification-harness config factory (GOD-230).

Every driver in the round has been building its own config by hand, which is
how the seed and the cycle mode drift between drivers. These factories are the
single place that decision is made; the tests pin both the knobs and the
observable behaviour they produce.
"""

import datetime

import pytest

import formualizer as fz

# A statically circular but live-acyclic region: the IF never takes the branch
# that closes the loop. Static detection stamps it #CIRC; runtime detection
# only rules on cycles it actually witnesses, so it evaluates to 7.
PHANTOM_A1 = "=IF(TRUE,7,B1)"
PHANTOM_B1 = "=A1"

CIRC = {"type": "Error", "kind": "Circ"}


def _phantom_workbook(config):
    wb = fz.Workbook(config=config)
    wb.add_sheet("S")
    wb.set_formula("S", 1, 1, PHANTOM_A1)
    wb.set_formula("S", 1, 2, PHANTOM_B1)
    wb.evaluate_all()
    return wb


# --------------------------------------------------------------------------
# The binding default
# --------------------------------------------------------------------------


def test_binding_default_is_runtime_detection():
    # GOD-230 (CL-004 / CL-067). This is a *binding* default; the engine's own
    # EvalConfig::default() is still static.
    assert fz.EvaluationConfig().cycle_detection == "runtime"


def test_a_bare_default_config_no_longer_stamps_a_phantom_scc():
    # MEASURED: 7.0 under the new default; {"type":"Error","kind":"Circ"} before.
    wb = _phantom_workbook(fz.WorkbookConfig(eval_config=fz.EvaluationConfig()))
    assert wb.get_value("S", 1, 1) == 7.0
    assert wb.get_value("S", 1, 2) == 7.0


def test_static_detection_still_stamps_a_phantom_scc():
    cfg = fz.EvaluationConfig()
    cfg.cycle_detection = "static"
    wb = _phantom_workbook(fz.WorkbookConfig(eval_config=cfg))
    assert wb.get_value("S", 1, 1) == CIRC


# --------------------------------------------------------------------------
# qualified_eval_config
# --------------------------------------------------------------------------


def test_qualified_seed_constant():
    assert fz.QUALIFIED_WORKBOOK_SEED == 147


def test_qualified_eval_config_defaults():
    cfg = fz.qualified_eval_config()
    assert cfg.workbook_seed == fz.QUALIFIED_WORKBOOK_SEED == 147
    assert cfg.cycle_detection == "runtime"
    assert cfg.cycle_policy == "error"


def test_qualified_eval_config_overrides():
    cfg = fz.qualified_eval_config(workbook_seed=999, cycle_detection="static")
    assert cfg.workbook_seed == 999
    assert cfg.cycle_detection == "static"


def test_qualified_eval_config_rejects_a_bad_cycle_detection():
    with pytest.raises(ValueError):
        fz.qualified_eval_config(cycle_detection="bogus")
    with pytest.raises(ValueError):
        fz.qualified_eval_config(cycle_detection="Runtime")  # case matters


def test_qualified_eval_config_returns_a_fresh_object_each_call():
    first = fz.qualified_eval_config()
    first.workbook_seed = 1
    assert fz.qualified_eval_config().workbook_seed == fz.QUALIFIED_WORKBOOK_SEED


# --------------------------------------------------------------------------
# qualified_config
# --------------------------------------------------------------------------


def test_qualified_config_returns_a_workbook_config():
    assert isinstance(fz.qualified_config(), fz.WorkbookConfig)


def test_qualified_config_evaluates_a_phantom_scc_as_runtime_does():
    wb = _phantom_workbook(fz.qualified_config())
    assert wb.get_value("S", 1, 1) == 7.0


def test_qualified_config_rejects_a_bad_cycle_detection():
    with pytest.raises(ValueError):
        fz.qualified_config(cycle_detection="bogus")


def test_qualified_config_honours_an_explicit_eval_config():
    # The explicit config is used verbatim: asking for static detection through
    # it must bring back the #CIRC stamp on the phantom SCC, and the
    # cycle_detection keyword must be ignored rather than fighting it.
    explicit = fz.EvaluationConfig()
    explicit.cycle_detection = "static"
    explicit.workbook_seed = 4242

    wb = _phantom_workbook(
        fz.qualified_config(eval_config=explicit, cycle_detection="runtime")
    )
    assert wb.get_value("S", 1, 1) == CIRC


def test_qualified_config_with_a_genuine_cycle_stamps_circ():
    # MEASURED. Runtime detection is not iteration: with the default
    # cycle_policy of "error", a cycle that IS witnessed still gets #CIRC.
    # Runtime detection changes which regions are judged circular, not the
    # verdict for the ones that really are.
    wb = fz.Workbook(config=fz.qualified_config())
    wb.add_sheet("S")
    wb.set_formula("S", 1, 1, "=B1+1")
    wb.set_formula("S", 1, 2, "=A1+1")
    wb.evaluate_all()

    assert wb.get_value("S", 1, 1) == CIRC
    assert wb.get_value("S", 1, 2) == CIRC

    # And nothing was iterated, because the policy is "error".
    telemetry = wb.last_cycle_telemetry()
    assert telemetry.iterated_sccs == 0
    assert telemetry.converged_sccs == 0
    assert telemetry.capped_sccs == 0


# --------------------------------------------------------------------------
# set_qualified_clock
# --------------------------------------------------------------------------


def test_set_qualified_clock_pins_today():
    clock_seconds = 1_600_000_000  # 2020-09-13T12:26:40Z
    wb = fz.Workbook(config=fz.qualified_config())
    wb.add_sheet("S")
    fz.set_qualified_clock(wb, clock_seconds, 0)
    wb.set_formula("S", 1, 1, "=TEXT(TODAY(),\"yyyy-mm-dd\")")
    wb.evaluate_all()

    expected = datetime.datetime.fromtimestamp(
        clock_seconds, tz=datetime.timezone.utc
    ).strftime("%Y-%m-%d")
    assert wb.get_value("S", 1, 1) == expected


def test_set_qualified_clock_is_per_workbook():
    # The clock is workbook state, not config state: a second workbook built
    # from the same config is unaffected until it is pinned itself.
    config = fz.qualified_config()
    pinned = fz.Workbook(config=config)
    pinned.add_sheet("S")
    fz.set_qualified_clock(pinned, 1_600_000_000, 0)

    other = fz.Workbook(config=config)
    other.add_sheet("S")
    for wb in (pinned, other):
        wb.set_formula("S", 1, 1, "=TEXT(TODAY(),\"yyyy-mm-dd\")")
        wb.evaluate_all()

    assert pinned.get_value("S", 1, 1) == "2020-09-13"
    assert other.get_value("S", 1, 1) != "2020-09-13"


def test_set_qualified_clock_honours_a_utc_offset():
    # 2020-09-13T00:30:00Z is still 2020-09-12 at UTC-2.
    clock_seconds = datetime.datetime(
        2020, 9, 13, 0, 30, tzinfo=datetime.timezone.utc
    ).timestamp()

    wb = fz.Workbook(config=fz.qualified_config())
    wb.add_sheet("S")
    fz.set_qualified_clock(wb, clock_seconds, -2 * 3600)
    wb.set_formula("S", 1, 1, "=TEXT(TODAY(),\"yyyy-mm-dd\")")
    wb.evaluate_all()

    assert wb.get_value("S", 1, 1) == "2020-09-12"
