#[cfg(not(target_os = "emscripten"))]
use pyo3_stub_gen::Result;
#[cfg(not(target_os = "emscripten"))]
use std::fs;
#[cfg(not(target_os = "emscripten"))]
use std::path::PathBuf;

#[cfg(target_os = "emscripten")]
fn main() {}

#[cfg(not(target_os = "emscripten"))]
fn main() -> Result<()> {
    // `stub_info` is defined in `src/lib.rs` by `define_stub_info_gatherer!`.
    let stub = formualizer_py::stub_info()?;
    stub.generate()?;

    // pyo3-stub-gen 0.23 follows maturin's private extension module path and
    // writes `formualizer/formualizer_py/__init__.pyi`. The public Python package
    // re-exports that module, so publish the declarations as a companion
    // `formualizer_py.pyi`, emit a public re-export facade at the package root,
    // and remove the intermediate package-shaped directory.
    let generated_stub_path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "formualizer",
        "formualizer_py",
        "__init__.pyi",
    ]
    .iter()
    .collect();
    let generated_stub_dir = generated_stub_path
        .parent()
        .expect("generated stub path has a parent");
    let private_stub_path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "formualizer",
        "formualizer_py.pyi",
    ]
    .iter()
    .collect();
    let public_stub_path: PathBuf = [env!("CARGO_MANIFEST_DIR"), "formualizer", "__init__.pyi"]
        .iter()
        .collect();

    let mut contents = fs::read_to_string(&generated_stub_path)?;
    fs::remove_dir_all(generated_stub_dir)?;

    for (generated, corrected) in [
        (
            "def __eq__(self, other: typing.Any)",
            "def __eq__(self, other: typing.Any, /)",
        ),
        (
            "def __ne__(self, other: typing.Any)",
            "def __ne__(self, other: typing.Any, /)",
        ),
        (
            "def __contains__(self, address: builtins.str)",
            "def __contains__(self, address: builtins.str, /)",
        ),
        (
            "def __getitem__(self, index: builtins.int)",
            "def __getitem__(self, index: builtins.int, /)",
        ),
        (
            "def __getitem__(self, name: builtins.str)",
            "def __getitem__(self, name: builtins.str, /)",
        ),
        (
            "def __getitem__(self, address: builtins.str)",
            "def __getitem__(self, address: builtins.str, /)",
        ),
    ] {
        assert!(
            contents.contains(generated),
            "generated stub is missing the expected magic-method signature: {generated}"
        );
        contents = contents.replace(generated, corrected);
    }

    let additional_public_exports = r#"    "EXCEL_ERROR_TOKENS",
    "DependencyStateUnavailableError",
    "ExcelEvaluationError",
    "FormualizerHostError",
    "InspectionError",
    "InspectionResourceExhaustedError",
    "InspectionRevisionMismatchError",
    "InvalidInspectionAddressError",
    "InvalidInspectionOptionsError",
    "ParserError",
    "SheetNotFoundError",
    "PyFormulaDialect",
    "PyRefWalker",
    "PyToken",
    "PyTokenSubType",
    "PyTokenType",
    "PyTokenizer",
    "PyTokenizerIter",
    "SheetPortConstraintError",
    "SheetPortError",
    "SheetPortManifestError",
    "SheetPortWorkbookError",
    "TokenizerError",
"#;
    let all_end = "]\n\n@typing.final";
    assert!(
        contents.contains(all_end),
        "generated stub is missing the expected __all__ terminator"
    );
    contents = contents.replacen(all_end, &format!("{additional_public_exports}{all_end}"), 1);

    contents.push_str(
        r#"

# Exceptions created with pyo3::create_exception! are public runtime types but
# are not currently discovered by pyo3-stub-gen's inventory.
class TokenizerError(Exception): ...
class ParserError(Exception): ...
class FormualizerHostError(Exception): ...
class ExcelEvaluationError(Exception): ...
class InspectionError(Exception): ...
class SheetNotFoundError(InspectionError): ...
class InvalidInspectionAddressError(InspectionError): ...
class InvalidInspectionOptionsError(InspectionError): ...
class DependencyStateUnavailableError(InspectionError): ...
class InspectionRevisionMismatchError(InspectionError):
    expected: StateStamp
    actual: StateStamp
class InspectionResourceExhaustedError(InspectionError): ...
class SheetPortError(Exception): ...
class SheetPortManifestError(SheetPortError): ...
class SheetPortConstraintError(SheetPortError): ...
class SheetPortWorkbookError(SheetPortError): ...

# GOD-230: module members registered at runtime (`m.add` / `m.setattr`), which
# pyo3-stub-gen's inventory does not see.
#
# `__build__` is the wheel's provenance stamp: `commit` is the 40-hex fork
# commit it was built from (None when the build could not read a checkout) and
# `dirty` says whether that tree had uncommitted changes (None when unknown).
__build__: dict[str, typing.Any]
# Every engine error kind mapped to its Excel cell token, or None for the
# engine-internal kinds Excel has no token for.
EXCEL_ERROR_TOKENS: dict[builtins.str, builtins.str | None]

# Backwards compatible Py* aliases
#
# Historically this package exported most symbols with a `Py...` prefix.
# Keep these aliases so older code continues to type-check.
PyToken = Token
PyTokenizer = Tokenizer
PyTokenizerIter = TokenizerIter
PyRefWalker = RefWalker
PyTokenType = TokenType
PyTokenSubType = TokenSubType
PyFormulaDialect = FormulaDialect

# Private type-check sentinels make a deleted generated member a mypy error,
# while stubtest still rejects invented members that are absent at runtime.
if typing.TYPE_CHECKING:
    _inspection_enum_members = (
        Staleness.Current, Staleness.Dirty, Staleness.NeverEvaluated, Staleness.Unknown,
        Provenance.Declared, Provenance.Observed, Provenance.Unknown,
        LinkDisposition.Expanded, LinkDisposition.Convergent, LinkDisposition.Cycle, LinkDisposition.Elided, LinkDisposition.Unknown,
        TraceDirection.Precedents, TraceDirection.Dependents,
        OmittedCountKind.Exact, OmittedCountKind.AtLeast, OmittedCountKind.Unknown,
        SpillRoleKind.Anchor, SpillRoleKind.Member, SpillRoleKind.Unknown,
        ReferenceKind.Cell, ReferenceKind.Range, ReferenceKind.Name, ReferenceKind.Table, ReferenceKind.External, ReferenceKind.ThreeDimensional, ReferenceKind.Unsupported, ReferenceKind.Unknown,
        NameResolutionKind.Cell, NameResolutionKind.Range, NameResolutionKind.Literal, NameResolutionKind.Formula, NameResolutionKind.Unresolved, NameResolutionKind.Unknown,
        TraceLinkKindType.Formula, TraceLinkKindType.SpillAnchor, TraceLinkKindType.SpillReader, TraceLinkKindType.Unknown,
    )
"#,
    );
    let all_start = contents
        .find("__all__ = [")
        .expect("generated stub is missing __all__");
    let all_close = all_start
        + contents[all_start..]
            .find("]\n\n@typing.final")
            .expect("generated stub is missing the end of __all__");
    let mut public_all = contents[all_start..=all_close].to_owned();
    public_all.insert_str(
        public_all.len() - 1,
        concat!(
            "    \"ReferenceLike\",\n",
            "    \"visitor\",\n",
            // GOD-230 qualification helpers, defined in formualizer/__init__.py.
            "    \"QUALIFIED_WORKBOOK_SEED\",\n",
            "    \"qualified_config\",\n",
            "    \"qualified_eval_config\",\n",
            "    \"set_qualified_clock\",\n",
        ),
    );

    fs::write(&private_stub_path, contents)?;
    fs::write(
        &public_stub_path,
        format!(
            r#"# This file is automatically generated by pyo3_stub_gen
# ruff: noqa: E501, F401, F403, F405

import builtins
import typing

from . import visitor as visitor
from ._types import ReferenceLike as ReferenceLike
from .formualizer_py import *
from .formualizer_py import EXCEL_ERROR_TOKENS as EXCEL_ERROR_TOKENS
from .formualizer_py import EvaluationConfig, WorkbookConfig
from .formualizer_py import excel_token_for_kind as excel_token_for_kind

{public_all}

# Provenance of the compiled extension (GOD-230): `commit` is the 40-hex fork
# commit, or None; `dirty` is a bool, or None. Never fabricated, and carries no
# timestamp so builds stay reproducible.
__build__: dict[builtins.str, typing.Any]

# GOD-230 qualification helpers, implemented in formualizer/__init__.py.
QUALIFIED_WORKBOOK_SEED: builtins.int

def qualified_eval_config(
    *,
    workbook_seed: builtins.int = ...,
    cycle_detection: builtins.str = ...,
) -> EvaluationConfig:
    ...

def qualified_config(
    *,
    workbook_seed: builtins.int = ...,
    cycle_detection: builtins.str = ...,
    eval_config: EvaluationConfig | None = ...,
) -> WorkbookConfig:
    ...

def set_qualified_clock(
    target: typing.Any,
    clock_seconds: builtins.float,
    utc_offset_seconds: builtins.int,
) -> None:
    ...
"#
        ),
    )?;

    Ok(())
}
