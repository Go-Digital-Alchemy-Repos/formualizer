"""Formualizer for Python.

This package exposes high-performance Excel-formula parsing and evaluation via Rust (PyO3).

Most of the public API lives in the native extension module ``formualizer.formualizer_py``
and is re-exported here for convenience.

See ``bindings/python/README.md`` in the repository for longer, runnable examples.
"""

import datetime as _datetime
from typing import Any

from . import formualizer_py as _py
from . import visitor
from ._types import ReferenceLike
from .formualizer_py import *  # noqa: F403
from .formualizer_py import EvaluationConfig, WorkbookConfig

#: Provenance of the compiled extension: ``{"commit": str | None, "dirty": bool | None}``.
#:
#: ``commit`` is the 40-hex fork commit the wheel was built from, or ``None``
#: when the build could not read a git checkout. ``dirty`` says whether that
#: tree had uncommitted changes, or ``None`` when it could not be determined.
#: Neither is ever fabricated. There is deliberately no build timestamp: it
#: would break wheel reproducibility.
__build__ = _py.__build__

# GOD-383 Trial A: present only in a wheel built with the `profiling` feature.
if hasattr(_py, "_profile_start"):
    _profile_start = _py._profile_start
    _profile_stop = _py._profile_stop

#: Workbook seed pinned by the qualification harness. Every random-dependent
#: builtin (RAND, RANDBETWEEN, RANDARRAY) derives from it, so two runs that
#: share this seed produce identical values.
QUALIFIED_WORKBOOK_SEED = 147


def qualified_eval_config(
    *,
    workbook_seed: int = QUALIFIED_WORKBOOK_SEED,
    cycle_detection: str = "runtime",
) -> EvaluationConfig:
    """Build the evaluation config the qualification harness runs under.

    This pins the two knobs that are *per-config*: the workbook seed and the
    cycle-detection mode. Everything else stays at the binding defaults.

    ``cycle_detection`` defaults to ``"runtime"``, which is also the binding
    default as of GOD-230 (CL-004 / CL-067): only cycles actually witnessed at
    evaluation time get the policy verdict, so a statically circular but
    live-acyclic region evaluates to a value instead of ``#CIRC``. Pass
    ``"static"`` only to reproduce the old behaviour deliberately.

    Two things this function cannot do for you:

    * **Locale is the caller's responsibility.** It is process-wide, not
      per-config and not per-workbook: set it yourself before evaluating, with
      ``locale.setlocale(...)`` and/or the ``LC_ALL`` / ``LANG`` environment
      variables. Nothing on ``EvaluationConfig`` controls it.
    * **The clock is per-workbook, not per-config.** Pin it after the workbook
      exists with :func:`set_qualified_clock`.

    Raises:
        ValueError: if ``cycle_detection`` is not ``"static"`` or ``"runtime"``.
    """
    config = EvaluationConfig()
    config.workbook_seed = workbook_seed
    config.cycle_detection = cycle_detection
    return config


def qualified_config(
    *,
    workbook_seed: int = QUALIFIED_WORKBOOK_SEED,
    cycle_detection: str = "runtime",
    eval_config: EvaluationConfig | None = None,
) -> WorkbookConfig:
    """Build the ``WorkbookConfig`` the qualification harness runs under.

    Wraps :func:`qualified_eval_config` unless an ``eval_config`` is supplied,
    in which case that config is used verbatim and ``workbook_seed`` /
    ``cycle_detection`` are ignored — the caller has already decided.

    The same two caveats apply as for :func:`qualified_eval_config`: **locale
    is the caller's responsibility** (process-wide, via ``locale.setlocale`` or
    the environment), and **the clock is per-workbook** — pin it with
    :func:`set_qualified_clock` once the workbook is built.

    Raises:
        ValueError: if ``cycle_detection`` is not ``"static"`` or ``"runtime"``.
    """
    if eval_config is None:
        eval_config = qualified_eval_config(
            workbook_seed=workbook_seed,
            cycle_detection=cycle_detection,
        )
    return WorkbookConfig(eval_config=eval_config)


def set_qualified_clock(
    target: Any,
    clock_seconds: float,
    utc_offset_seconds: int,
) -> None:
    """Pin ``target``'s evaluation clock so TODAY/NOW are deterministic.

    ``target`` is anything exposing ``set_deterministic_clock`` — today that is
    :class:`Workbook`. The clock is **per-workbook state, not config**: it must
    be set on every workbook you evaluate, and a config object cannot carry it.

    Args:
        target: the workbook to pin.
        clock_seconds: the instant, as a POSIX timestamp in UTC.
        utc_offset_seconds: the fixed offset from UTC, in seconds, that
            date/time builtins should render in. ``0`` means UTC.

    Locale is unrelated to the clock and is **the caller's responsibility**: it
    is process-wide, set through ``locale.setlocale(...)`` or ``LC_ALL`` /
    ``LANG``.
    """
    instant = _datetime.datetime.fromtimestamp(
        clock_seconds, tz=_datetime.timezone.utc
    )
    target.set_deterministic_clock(instant, utc_offset_seconds)


#: Every engine error kind mapped to its Excel cell token, or ``None`` for the
#: engine-internal kinds Excel has no token for (``Error``, ``NImpl``, ``Circ``,
#: ``Cancelled``). Built from the engine's own variant list, so it cannot drift.
EXCEL_ERROR_TOKENS = _py.EXCEL_ERROR_TOKENS
excel_token_for_kind = _py.excel_token_for_kind

_QUALIFIED_EXPORTS = [
    "EXCEL_ERROR_TOKENS",
    "QUALIFIED_WORKBOOK_SEED",
    "excel_token_for_kind",
    "qualified_config",
    "qualified_eval_config",
    "set_qualified_clock",
]

__all__ = [
    *_py.__all__,
    "ReferenceLike",
    "visitor",
    *(name for name in _QUALIFIED_EXPORTS if name not in _py.__all__),
]

# ---------------------------------------------------------------------------
# Backwards compatible aliases
# ---------------------------------------------------------------------------
#
# Earlier versions exposed most symbols with a `Py...` prefix.
# Keep these aliases so older code keeps working.
PyToken = _py.Token
PyTokenizer = _py.Tokenizer
PyTokenizerIter = _py.TokenizerIter
PyRefWalker = _py.RefWalker
PyTokenType = _py.TokenType
PyTokenSubType = _py.TokenSubType
PyFormulaDialect = _py.FormulaDialect
