"""Excel-token surface for engine error kinds (GOD-230).

Drivers that compare engine output against a real workbook need one
authoritative kind -> token table. Guessing a token, or defaulting an unknown
kind to something plausible, silently corrupts a comparison; every assertion
here exists to make that impossible.
"""

import pytest

import formualizer as fz

# The full table, spelled out. Keep in sync with the Rust-side EXPECTED in
# bindings/python/src/value.rs and the table test in
# crates/formualizer-common/src/error.rs.
EXPECTED_TOKENS = {
    "Null": "#NULL!",
    "Ref": "#REF!",
    "Name": "#NAME?",
    "Value": "#VALUE!",
    "Div": "#DIV/0!",
    "Na": "#N/A",
    "Num": "#NUM!",
    "Error": None,
    "NImpl": None,
    "Spill": "#SPILL!",
    "Calc": "#CALC!",
    "Circ": None,
    "Cancelled": None,
}


def test_excel_error_tokens_table_is_exact():
    assert fz.EXCEL_ERROR_TOKENS == EXPECTED_TOKENS
    assert len(fz.EXCEL_ERROR_TOKENS) == 13
    tokens = [v for v in fz.EXCEL_ERROR_TOKENS.values() if v is not None]
    assert len(tokens) == 9
    assert len(tokens) == len(set(tokens)), "tokens must be unique"
    assert sum(1 for v in fz.EXCEL_ERROR_TOKENS.values() if v is None) == 4


def test_excel_error_tokens_is_reexported_from_the_native_module():
    import formualizer.formualizer_py as native

    assert fz.EXCEL_ERROR_TOKENS == native.EXCEL_ERROR_TOKENS


@pytest.mark.parametrize(("kind", "token"), sorted(EXPECTED_TOKENS.items()))
def test_excel_token_for_kind_matches_the_table(kind, token):
    assert fz.excel_token_for_kind(kind) == token


def test_excel_token_for_kind_is_case_insensitive():
    assert fz.excel_token_for_kind("div") == "#DIV/0!"
    assert fz.excel_token_for_kind("DIV") == "#DIV/0!"
    assert fz.excel_token_for_kind("nImPl") is None


def test_excel_token_for_kind_accepts_the_div0_alias():
    # LiteralValue.error() has always accepted "Div0" on input; the token
    # lookup accepts it too so a driver need not normalise first.
    assert fz.excel_token_for_kind("Div0") == "#DIV/0!"
    assert fz.excel_token_for_kind("div0") == "#DIV/0!"
    # And the "NA" spelling that LiteralValue.error() also accepts.
    assert fz.excel_token_for_kind("NA") == "#N/A"


def test_excel_token_for_kind_raises_on_an_unknown_string():
    # A driver typo must fail loudly, not resolve to a default token.
    with pytest.raises(ValueError):
        fz.excel_token_for_kind("Divide")
    with pytest.raises(ValueError):
        fz.excel_token_for_kind("")
    # It takes kind names, not tokens.
    with pytest.raises(ValueError):
        fz.excel_token_for_kind("#DIV/0!")


def test_literal_value_excel_token_on_a_real_division_by_zero():
    wb = fz.Workbook(config=fz.qualified_config())
    wb.add_sheet("S")
    wb.set_formula("S", 1, 1, "=1/0")
    wb.evaluate_all()

    # `Workbook.get_value` hands back the engine's plain-Python error dict,
    # never a LiteralValue, so round-trip it through `LiteralValue.from_object`
    # to reach the `excel_token` property.
    raw = wb.get_value("S", 1, 1)
    assert raw["type"] == "Error"
    assert raw["kind"] == "Div"

    value = fz.LiteralValue.from_object(raw)
    assert value.is_error
    assert value.excel_token == "#DIV/0!"

    # The table lookup is the path a driver takes straight off the dict.
    assert fz.excel_token_for_kind(raw["kind"]) == "#DIV/0!"
    assert fz.EXCEL_ERROR_TOKENS[raw["kind"]] == "#DIV/0!"


def test_literal_value_excel_token_is_none_for_a_non_error():
    assert fz.LiteralValue.number(1.5).excel_token is None
    assert fz.LiteralValue.text("hello").excel_token is None
    assert fz.LiteralValue.empty().excel_token is None


def test_literal_value_excel_token_is_none_for_a_tokenless_error_kind():
    # None means "no Excel token", which is NOT the same as "not an error".
    # `is_error` is what tells them apart.
    circ = fz.LiteralValue.error("Circ", None)
    assert circ.is_error
    assert circ.excel_token is None

    number = fz.LiteralValue.number(1.0)
    assert not number.is_error
    assert number.excel_token is None
