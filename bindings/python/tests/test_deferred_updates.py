import formualizer as fz
import pytest


def workbook():
    model = fz.Workbook()
    model.set_value("S", 1, 1, 1)
    model.set_formula("S", 1, 2, "=A1*2")
    model.set_formula("S", 1, 3, "=B1+1")
    model.evaluate_all()
    return model


def test_ordered_updates_preserve_reads_and_propagate_once_scope_ends():
    model = workbook()

    def update():
        model.set_value("S", 1, 1, 5)
        assert model.get_value("S", 1, 1) == 5
        model.set_value("S", 1, 2, "text")
        assert not model.get_formula("S", 1, 2)
        assert model.get_value("S", 1, 2) == "text"
        model.set_value("S", 1, 2, 12)
        return "completed"

    assert model.with_deferred_updates(update) == "completed"
    model.evaluate_all()
    assert model.get_value("S", 1, 3) == 13


def test_callback_error_flushes_dirty_state_without_retrying_writes():
    model = workbook()
    calls = []

    def update():
        calls.append(1)
        model.set_value("S", 1, 1, 7)
        raise ValueError("stop updates")

    with pytest.raises(ValueError, match="stop updates"):
        model.with_deferred_updates(update)
    assert calls == [1]
    model.evaluate_all()
    assert model.get_value("S", 1, 3) == 15
    model.set_value("S", 1, 1, 9)
    model.evaluate_all()
    assert model.get_value("S", 1, 3) == 19
