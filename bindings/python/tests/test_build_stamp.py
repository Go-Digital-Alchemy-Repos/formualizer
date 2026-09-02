"""Wheel provenance stamp (GOD-230).

A qualification round has to be able to say which source tree produced the
wheel it ran. `build.rs` bakes that in at compile time; these tests pin the
shape and the honesty rules, not any particular commit (which changes with
every build).
"""

import re

import formualizer as fz
import formualizer.formualizer_py as native

SHA_RE = re.compile(r"\A[0-9a-f]{40}\Z")


def test_build_has_the_expected_keys():
    assert isinstance(fz.__build__, dict)
    assert {"commit", "dirty"} <= set(fz.__build__)


def test_commit_is_none_or_a_full_sha():
    commit = fz.__build__["commit"]
    assert commit is None or (isinstance(commit, str) and SHA_RE.match(commit)), commit


def test_dirty_is_a_bool_or_none():
    dirty = fz.__build__["dirty"]
    assert dirty is None or isinstance(dirty, bool), dirty


def test_dirty_is_unknown_whenever_the_commit_is():
    # `dirty` must never claim "clean" for a build that could not read git at
    # all: an empty `git status` from a failed invocation would be a lie.
    if fz.__build__["commit"] is None:
        assert fz.__build__["dirty"] is None


def test_build_carries_no_timestamp():
    # An embedded build time would make two builds of the same source differ,
    # and wheel reproducibility is a measured gate.
    for key in fz.__build__:
        assert "time" not in key.lower()
        assert "date" not in key.lower()


def test_package_reexports_the_native_stamp():
    assert fz.__build__ == native.__build__


def test_build_is_not_in_all():
    # A dunder does not belong in a star-import surface.
    assert "__build__" not in native.__all__
    assert "__build__" not in fz.__all__
