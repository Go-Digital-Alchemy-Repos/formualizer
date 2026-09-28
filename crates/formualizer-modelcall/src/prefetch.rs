//! Lane B: sibling prefetch (`workbook_runtime/prefetch.py`).
//!
//! A parent can hold a *group* of call cells that ask the same child for the
//! same output with the same argument layout and differ only in a few
//! argument values that are constants of the pinned bytes. When the first
//! call of such a group misses the request memo, the other members' input
//! vectors are known: the observed vector with those constants substituted.
//! The [`Prefetcher`] evaluates up to `prefetch_max` such sibling vectors
//! through [`ChildEvaluator`] (each flight on its own thread, or all of one
//! dispatch handed to a [`ChildBatchEvaluator`], which runs them as up to
//! `prefetch_max` parallel chunks, one loaded child per chunk) while the
//! in-line call runs, joins them after
//! it and adopts each result into the memo under the sibling's own exact key.
//!
//! The sibling plan is static: read once per parent `workbook_sha256` from
//! the parent's formulas with `formualizer-parse`'s tokenizer (the openpyxl
//! tokenizer's token model) over cells read by `formualizer-workbook`'s
//! calamine reader, and cached. The call names come from the import-boundary
//! table ([`crate::import_boundary::is_call_function`]).
//!
//! Differences from the Python, all on the side of planning less:
//! * a formula the tokenizer refuses contributes no call (openpyxl would
//!   raise and the plan would fail);
//! * a decimal integer literal beyond `i64` is not a literal;
//! * a cell constant is what calamine reads (an empty string reads as blank,
//!   an error cell as an error, both `Dynamic`; openpyxl gives `''` / text);
//! * pre-warmed prefetch slots (`xcall_prefetch_slots`) are not ported: a
//!   batch evaluator is the Rust way to reuse one loaded child.

use formualizer_common::LiteralValue;
use formualizer_parse::{Token, TokenSubType, TokenType, Tokenizer};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Instant;

use crate::evaluator::{CancelToken, ChildEvaluator, ChildMatrix, ChildOutcome, ChildRequest};
use crate::event::{CallStatus, ModelCallEvent};
use crate::import_boundary::{call_function_names, is_call_function};
use crate::key::{InputPairs, MemoKey, MemoToken, casefold, matrix_is_finished, matrix_is_memoisable, memo_token};
use crate::memo::ModelCallMemo;
use crate::receipt::{PrefetchReport, TimingValue, Timings, timing_keys};
use crate::spec::{CellRange, ModelPackage, ModelSpec};
use crate::{CalculationContext, ModelCallError};

// -- static plan ---------------------------------------------------------------

/// One argument value of one group member at a differing position.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanValue {
    /// A constant of the pinned bytes (Boolean, Int, finite Number or Text).
    Constant(LiteralValue),
    /// Known only at evaluation (`DYNAMIC`).
    Dynamic,
    /// The member passes no tail pair of this name (`ABSENT`).
    Absent,
}

/// `override` / `partial` / `extra` (see [`SiblingGroup`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionKind {
    Override,
    Partial,
    Extra,
}

impl PositionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Override => "override",
            Self::Partial => "partial",
            Self::Extra => "extra",
        }
    }
}

/// A value position whose argument differs between members.
#[derive(Debug, Clone, PartialEq)]
pub struct Position {
    /// Input name, casefolded.
    pub name: String,
    pub kind: PositionKind,
    /// Per member, in `SiblingGroup::cells` order.
    pub values: Vec<PlanValue>,
}

/// Call cells asking one child for one output with aligned arguments.
///
/// `override`: every member's value there is a constant; `partial`: some
/// members' argument is dynamic (all of those share one identical argument);
/// `extra`: a tail name only some members pass (each with a constant, the
/// others `Absent`).
///
/// Lane B changed this struct's fields from the Lane 0 placeholder
/// (`members: Vec<InputPairs>`) to the Python dataclass's
/// (`cells`, `positions`); nothing outside this module used them.
#[derive(Debug, Clone, PartialEq)]
pub struct SiblingGroup {
    pub target: String,
    pub output: String,
    /// `Sheet!A1` per member, in scan order.
    pub cells: Vec<String>,
    pub positions: Vec<Position>,
}

impl SiblingGroup {
    fn count(&self, kind: PositionKind) -> usize {
        self.positions.iter().filter(|position| position.kind == kind).count()
    }
    pub fn override_positions(&self) -> usize {
        self.count(PositionKind::Override)
    }
    pub fn partial_positions(&self) -> usize {
        self.count(PositionKind::Partial)
    }
    pub fn extra_positions(&self) -> usize {
        self.count(PositionKind::Extra)
    }
}

/// `SiblingPlan`: every group in the parent workbook, plus scan counts.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SiblingPlan {
    pub groups: Vec<SiblingGroup>,
    /// Formula cells holding at least one readable call (`xcall_cells`).
    pub call_cells: u64,
    pub unresolved_calls: u64,
    pub rejected_groups: u64,
}

impl SiblingPlan {
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// `SiblingPlan.counts()`: counts only (no target, name or value), with
    /// the Python key spellings so the two CLIs compare directly.
    pub fn counts(&self) -> serde_json::Value {
        let per = |f: fn(&SiblingGroup) -> usize| self.groups.iter().map(f).collect::<Vec<_>>();
        serde_json::json!({
            "xcall_cells": self.call_cells,
            "unresolved_calls": self.unresolved_calls,
            "groups": self.groups.len(),
            "rejected_groups": self.rejected_groups,
            "group_cells": per(|group| group.cells.len()),
            "override_positions": per(SiblingGroup::override_positions),
            "partial_positions": per(SiblingGroup::partial_positions),
            "extra_positions": per(SiblingGroup::extra_positions),
        })
    }
}

/// One cell of the parent as the plan reads it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlanCell {
    /// Formula text, with its leading `=`.
    pub formula: Option<String>,
    pub value: Option<LiteralValue>,
}

/// The parent's cells, sheet by sheet in workbook order, and the rectangles
/// a request or a solve may write (`_declared_rectangles`; `None` when a
/// declaration is unreadable, so no cell is trusted as a constant).
/// One sheet's cells by 1-based `(row, col)`, row-major.
pub type PlanSheet = BTreeMap<(u32, u32), PlanCell>;

#[derive(Debug, Clone, Default)]
pub struct PlanBook {
    pub sheets: Vec<(String, PlanSheet)>,
    pub rectangles: Option<Vec<CellRange>>,
}

impl PlanBook {
    fn sheet(&self, name: &str) -> Option<&PlanSheet> {
        self.sheets.iter().find(|(sheet, _)| sheet == name).map(|(_, cells)| cells)
    }

    /// Read a workbook file (formulas and literal values, no evaluation).
    pub fn from_path(path: &Path) -> Result<Self, ModelCallError> {
        use formualizer_workbook::traits::{DefinedNameDefinition, SpreadsheetReader};
        use formualizer_workbook::CalamineAdapter;
        let io = |error: &dyn std::fmt::Display| ModelCallError::infrastructure("OSError", error.to_string());
        let mut reader = CalamineAdapter::open_path(path).map_err(|error| io(&error))?;
        let names = reader.sheet_names().map_err(|error| io(&error))?;
        let mut sheets = Vec::with_capacity(names.len());
        for name in names {
            let data = reader.read_sheet(&name).map_err(|error| io(&error))?;
            let cells = data
                .cells
                .into_iter()
                .map(|(position, cell)| (position, PlanCell { formula: cell.formula, value: cell.value }))
                .collect();
            sheets.push((name, cells));
        }
        let mut rectangles = Some(Vec::new());
        for defined in reader.defined_names().map_err(|error| io(&error))? {
            let folded = casefold(&defined.name);
            if !(folded.starts_with("xinput") || folded.starts_with("xsolve")) {
                continue;
            }
            match (&defined.definition, rectangles.as_mut()) {
                (DefinedNameDefinition::Range { address }, Some(list)) => list.push(CellRange {
                    sheet: address.sheet.clone(),
                    start_row: address.start_row,
                    start_col: address.start_col,
                    end_row: address.end_row,
                    end_col: address.end_col,
                }),
                (DefinedNameDefinition::Literal { .. }, _) => rectangles = None,
                _ => {}
            }
        }
        Ok(Self { sheets, rectangles })
    }
}

fn is_whitespace_token(token: &Token) -> bool {
    token.token_type == TokenType::Whitespace
        || (token.token_type == TokenType::OpInfix && !token.value.is_empty() && token.value.trim().is_empty())
}

/// `_is_xcall`: a function-open token naming the call function.
fn is_call_token(token: &Token) -> bool {
    let name = token.value.strip_suffix('(').unwrap_or(&token.value);
    let folded = casefold(name);
    let name = folded.strip_prefix("_xlfn.").unwrap_or(&folded);
    is_call_function(name)
}

fn opens(token: &Token) -> bool {
    matches!(token.token_type, TokenType::Func | TokenType::Paren | TokenType::Array)
        && token.subtype == TokenSubType::Open
}

fn closes(token: &Token) -> bool {
    matches!(token.token_type, TokenType::Func | TokenType::Paren | TokenType::Array)
        && token.subtype == TokenSubType::Close
}

type Argument = Vec<Token>;

/// `_xcall_arguments`: every call occurrence's arguments, each a list of
/// non-whitespace tokens. An unterminated call is skipped.
pub fn call_arguments(formula: &str) -> Vec<Vec<Vec<Token>>> {
    let Ok(tokenizer) = Tokenizer::new(formula) else { return Vec::new() };
    let tokens: Vec<Token> = tokenizer.items.into_iter().filter(|token| !is_whitespace_token(token)).collect();
    let mut calls = Vec::new();
    for (start, token) in tokens.iter().enumerate() {
        if !(token.token_type == TokenType::Func && token.subtype == TokenSubType::Open && is_call_token(token)) {
            continue;
        }
        let mut arguments: Vec<Argument> = Vec::new();
        let mut current: Argument = Vec::new();
        let mut depth = 1usize;
        let mut terminated = false;
        for inner in &tokens[start + 1..] {
            if opens(inner) {
                depth += 1;
            } else if closes(inner) {
                depth -= 1;
                if depth == 0 {
                    arguments.push(std::mem::take(&mut current));
                    terminated = true;
                    break;
                }
            } else if inner.token_type == TokenType::Sep && inner.subtype == TokenSubType::Arg && depth == 1 {
                arguments.push(std::mem::take(&mut current));
                continue;
            }
            current.push(inner.clone());
        }
        if terminated {
            calls.push(arguments);
        }
    }
    calls
}

fn text(tokens: &[Token]) -> String {
    tokens.iter().map(|token| token.value.as_str()).collect()
}

/// `_literal`: `Some(value)` for a literal argument.
pub fn literal(tokens: &[Token]) -> Option<LiteralValue> {
    if tokens.len() == 2
        && tokens[0].token_type == TokenType::OpPrefix
        && tokens[0].value == "-"
        && tokens[1].token_type == TokenType::Operand
        && tokens[1].subtype == TokenSubType::Number
    {
        return match literal(&tokens[1..])? {
            LiteralValue::Int(value) => value.checked_neg().map(LiteralValue::Int),
            LiteralValue::Number(value) => Some(LiteralValue::Number(-value)),
            _ => None,
        };
    }
    let [token] = tokens else { return None };
    if token.token_type != TokenType::Operand {
        return None;
    }
    match token.subtype {
        TokenSubType::Text => {
            let inner = token.value.get(1..token.value.len().saturating_sub(1)).unwrap_or("");
            Some(LiteralValue::Text(inner.replace("\"\"", "\"")))
        }
        TokenSubType::Number => {
            if !token.value.is_empty() && token.value.bytes().all(|byte| byte.is_ascii_digit()) {
                return token.value.parse::<i64>().ok().map(LiteralValue::Int);
            }
            let value = token.value.parse::<f64>().ok()?;
            value.is_finite().then_some(LiteralValue::Number(value))
        }
        TokenSubType::Logical => Some(LiteralValue::Boolean(token.value.eq_ignore_ascii_case("TRUE"))),
        _ => None,
    }
}

fn literal_text(tokens: &[Token]) -> Option<String> {
    match literal(tokens) {
        Some(LiteralValue::Text(value)) => Some(value),
        _ => None,
    }
}

/// `_CELL_REF`: `[sheet!]$A$1` -> (sheet if given, column letters, row).
fn cell_reference(value: &str) -> Option<(Option<String>, String, u32)> {
    let (sheet, address) = if let Some(rest) = value.strip_prefix('\'') {
        // Quoted sheet: '' is an escaped quote; the closing quote precedes '!'.
        let bytes = rest.as_bytes();
        let mut index = 0;
        let mut name = String::new();
        loop {
            let next = rest[index..].find('\'')? + index;
            name.push_str(&rest[index..next]);
            if bytes.get(next + 1) == Some(&b'\'') {
                name.push('\'');
                index = next + 2;
                continue;
            }
            if name.is_empty() {
                return None;
            }
            let after = rest.get(next + 1..)?;
            let address = after.strip_prefix('!')?;
            break (Some(name), address);
        }
    } else if let Some((sheet, address)) = value.split_once('!') {
        if sheet.is_empty() || sheet.contains('\'') {
            return None;
        }
        (Some(sheet.to_owned()), address)
    } else {
        (None, value)
    };
    let rest = address.strip_prefix('$').unwrap_or(address);
    let letters = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
    if !(1..=3).contains(&letters) {
        return None;
    }
    let (column, rest) = rest.split_at(letters);
    let rest = rest.strip_prefix('$').unwrap_or(rest);
    if rest.is_empty() || !rest.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((sheet, column.to_ascii_uppercase(), rest.parse().ok()?))
}

fn column_index(letters: &str) -> u32 {
    letters.bytes().fold(0u32, |total, byte| total * 26 + u32::from(byte - b'A' + 1))
}

/// `get_column_letter`.
fn column_letters(mut col: u32) -> String {
    let mut letters = Vec::new();
    while col > 0 {
        let rem = (col - 1) % 26;
        letters.push(b'A' + u8::try_from(rem).unwrap_or(0));
        col = (col - 1) / 26;
    }
    letters.reverse();
    String::from_utf8(letters).unwrap_or_default()
}

/// `_constant`: the value a constant argument always evaluates to.
fn constant(tokens: &[Token], sheet: &str, book: &PlanBook) -> PlanValue {
    if let Some(value) = literal(tokens) {
        return PlanValue::Constant(value);
    }
    let [token] = tokens else { return PlanValue::Dynamic };
    if token.token_type != TokenType::Operand || token.subtype != TokenSubType::Range {
        return PlanValue::Dynamic;
    }
    let Some((named_sheet, column, row)) = cell_reference(&token.value) else { return PlanValue::Dynamic };
    let sheet = named_sheet.unwrap_or_else(|| sheet.to_owned());
    let (Some(cells), Some(rectangles)) = (book.sheet(&sheet), book.rectangles.as_ref()) else {
        return PlanValue::Dynamic;
    };
    if row == 0 {
        return PlanValue::Dynamic;
    }
    let col = column_index(&column);
    let covered = rectangles.iter().any(|rect| {
        rect.sheet == sheet && rect.start_row <= row && row <= rect.end_row && rect.start_col <= col && col <= rect.end_col
    });
    if covered {
        return PlanValue::Dynamic;
    }
    let Some(cell) = cells.get(&(row, col)) else { return PlanValue::Dynamic };
    if cell.formula.is_some() {
        return PlanValue::Dynamic;
    }
    match &cell.value {
        Some(value @ (LiteralValue::Boolean(_) | LiteralValue::Int(_) | LiteralValue::Text(_))) => {
            PlanValue::Constant(value.clone())
        }
        // openpyxl reads a number cell as `int` when its XML text has no
        // point or exponent, which is how Excel writes integral values below
        // 1e15; calamine reads every number as a float.
        Some(LiteralValue::Number(number)) if number.fract() == 0.0 && number.abs() < 1e15 => {
            PlanValue::Constant(LiteralValue::Int(*number as i64))
        }
        Some(LiteralValue::Number(number)) if number.is_finite() => PlanValue::Constant(LiteralValue::Number(*number)),
        _ => PlanValue::Dynamic,
    }
}

/// `repr(value)` equality for constants (type and exact value).
fn repr_token(value: &LiteralValue) -> Option<MemoToken> {
    memo_token(value).ok()
}

type Tail = Vec<(Argument, Argument)>;

fn tail_pairs(arguments: &[Argument]) -> Option<Tail> {
    let tail = arguments.get(3..).unwrap_or(&[]);
    if !tail.len().is_multiple_of(2) {
        return None;
    }
    Some(tail.chunks_exact(2).map(|pair| (pair[0].clone(), pair[1].clone())).collect())
}

struct Member {
    sheet: String,
    cell: String,
    arguments: Vec<Argument>,
}

/// `_group_plan`: a group, or `None` when the members are not aligned.
fn group_plan(target: &str, output: &str, members: &[Member], book: &PlanBook) -> Option<SiblingGroup> {
    let blocks: HashSet<String> = members.iter().map(|member| text(&member.arguments[1])).collect();
    if blocks.len() != 1 {
        return None; // a differing input block cannot be predicted
    }
    let tails: Vec<Tail> = members.iter().map(|member| tail_pairs(&member.arguments)).collect::<Option<_>>()?;
    let common = tails.iter().map(Vec::len).min().unwrap_or(0);
    let mut positions = Vec::new();
    // `None` stands for a name that is not a text literal.
    let mut common_names: Vec<Option<String>> = Vec::new();
    for index in 0..common {
        let names: HashSet<String> = tails.iter().map(|tail| text(&tail[index].0)).collect();
        if names.len() != 1 {
            return None; // tail names not aligned
        }
        let name = literal_text(&tails[0][index].0);
        let folded = name.as_deref().map(casefold);
        if !common_names.contains(&folded) {
            common_names.push(folded.clone());
        }
        let texts: Vec<String> = tails.iter().map(|tail| text(&tail[index].1)).collect();
        if texts.iter().all(|value| *value == texts[0]) {
            continue; // identical argument: the observed value carries over
        }
        let folded = folded?;
        let values: Vec<PlanValue> =
            members.iter().zip(&tails).map(|(member, tail)| constant(&tail[index].1, &member.sheet, book)).collect();
        let dynamic: HashSet<&String> =
            texts.iter().zip(&values).filter(|(_, value)| **value == PlanValue::Dynamic).map(|(text, _)| text).collect();
        if dynamic.len() > 1 {
            return None; // two different dynamic arguments: not predictable
        }
        let kind = if dynamic.is_empty() { PositionKind::Override } else { PositionKind::Partial };
        positions.push(Position { name: folded, kind, values });
    }
    let mut extras: Vec<(String, Vec<PlanValue>)> = Vec::new();
    for (member_index, (member, tail)) in members.iter().zip(&tails).enumerate() {
        if tail.len() > common && common_names.contains(&None) {
            return None; // an unreadable common name could collide with an extra one
        }
        let mut own: HashSet<String> = HashSet::new();
        for (name_tokens, value_tokens) in &tail[common..] {
            let name = casefold(&literal_text(name_tokens)?);
            if common_names.contains(&Some(name.clone())) || own.contains(&name) {
                return None; // a duplicate tail name: the call itself is refused
            }
            own.insert(name.clone());
            let value = constant(value_tokens, &member.sheet, book);
            if value == PlanValue::Dynamic {
                return None; // an extra pair must be a constant of the pinned bytes
            }
            let slot = match extras.iter().position(|(existing, _)| *existing == name) {
                Some(slot) => slot,
                None => {
                    extras.push((name, vec![PlanValue::Absent; members.len()]));
                    extras.len() - 1
                }
            };
            extras[slot].1[member_index] = value;
        }
    }
    for (name, values) in extras {
        if !values.contains(&PlanValue::Absent) {
            let reprs: Vec<Option<MemoToken>> = values
                .iter()
                .map(|value| match value {
                    PlanValue::Constant(constant) => repr_token(constant),
                    _ => None,
                })
                .collect();
            if reprs.iter().all(|token| token.is_some() && *token == reprs[0]) {
                continue; // every member passes the same constant: nothing differs
            }
        }
        positions.push(Position { name, kind: PositionKind::Extra, values });
    }
    if !positions.iter().any(|position| matches!(position.kind, PositionKind::Override | PositionKind::Extra)) {
        // Only a position where the members pass differing constants drives a
        // prediction; a differing formula-valued argument alone does not.
        return None;
    }
    Some(SiblingGroup {
        target: target.to_owned(),
        output: output.to_owned(),
        cells: members.iter().map(|member| format!("{}!{}", member.sheet, member.cell)).collect(),
        positions,
    })
}

/// `sibling_plan_for_path` over cells already read.
pub fn sibling_plan_for_book(book: &PlanBook) -> SiblingPlan {
    let needles: Vec<String> = call_function_names().map(str::to_ascii_uppercase).collect();
    let mut grouped: Vec<((String, String), Vec<Member>)> = Vec::new();
    let mut plan = SiblingPlan::default();
    for (sheet, cells) in &book.sheets {
        for (&(row, col), cell) in cells {
            let Some(formula) = cell.formula.as_deref().filter(|formula| formula.starts_with('=')) else { continue };
            let upper = formula.to_ascii_uppercase();
            if !needles.iter().any(|needle| upper.contains(needle.as_str())) {
                continue;
            }
            let calls = call_arguments(formula);
            if !calls.is_empty() {
                plan.call_cells += 1;
            }
            for arguments in calls {
                let resolved = if arguments.len() >= 3 {
                    literal_text(&arguments[0]).zip(literal_text(&arguments[2]))
                } else {
                    None
                };
                let Some(key) = resolved else {
                    plan.unresolved_calls += 1;
                    continue;
                };
                let member = Member { sheet: sheet.clone(), cell: format!("{}{row}", column_letters(col)), arguments };
                match grouped.iter_mut().find(|(existing, _)| *existing == key) {
                    Some((_, members)) => members.push(member),
                    None => grouped.push((key, vec![member])),
                }
            }
        }
    }
    for ((target, output), members) in grouped {
        if members.len() < 2 {
            continue;
        }
        match group_plan(&target, &output, &members, book) {
            Some(group) => plan.groups.push(group),
            None => plan.rejected_groups += 1,
        }
    }
    plan
}

/// Read the parent's call formulas and derive its sibling groups.
pub fn sibling_plan_for_path(path: &Path) -> Result<SiblingPlan, ModelCallError> {
    Ok(sibling_plan_for_book(&PlanBook::from_path(path)?))
}

fn plan_cache() -> &'static Mutex<HashMap<String, Arc<SiblingPlan>>> {
    static PLANS: OnceLock<Mutex<HashMap<String, Arc<SiblingPlan>>>> = OnceLock::new();
    PLANS.get_or_init(Mutex::default)
}

/// `sibling_plan(spec)`: `Ok(None)` when the parent has no sibling group.
/// Cached by the pinned `workbook_sha256`.
pub fn sibling_plan(spec: &ModelSpec) -> Result<Option<SiblingPlan>, ModelCallError> {
    let key = spec.workbook_sha256.clone();
    let cached = plan_cache().lock().ok().and_then(|plans| plans.get(&key).cloned());
    let plan = match cached {
        Some(plan) => plan,
        None => {
            let plan = Arc::new(sibling_plan_for_path(Path::new(&spec.workbook_path))?);
            match plan_cache().lock() {
                Ok(mut plans) => plans.entry(key).or_insert(plan).clone(),
                Err(_) => plan,
            }
        }
    };
    Ok((!plan.is_empty()).then(|| (*plan).clone()))
}

// -- runtime derivation --------------------------------------------------------

/// Floats that convert to `i64` exactly when integral.
const I64_SPAN: std::ops::Range<f64> = -9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NumberKind {
    Int,
    Float,
}

fn number_kind(value: &LiteralValue) -> Option<NumberKind> {
    match value {
        LiteralValue::Int(_) => Some(NumberKind::Int),
        LiteralValue::Number(_) => Some(NumberKind::Float),
        _ => None,
    }
}

/// `_harmonise`: the constant spelled as the engine spelled the observed
/// number (int vs float); `style` decides where nothing numeric was observed.
fn harmonise(constant: &LiteralValue, observed: Option<&LiteralValue>, style: Option<NumberKind>) -> LiteralValue {
    if number_kind(constant).is_none() {
        return constant.clone();
    }
    let kind = observed.and_then(number_kind).or(style);
    match (kind, constant) {
        (Some(NumberKind::Float), LiteralValue::Int(value)) => LiteralValue::Number(*value as f64),
        (Some(NumberKind::Int), LiteralValue::Number(value))
            if value.fract() == 0.0 && I64_SPAN.contains(value) =>
        {
            LiteralValue::Int(*value as i64)
        }
        _ => constant.clone(),
    }
}

/// `_same`: the observed value is this constant, spelled the engine's way.
fn same(value: &LiteralValue, constant: &LiteralValue) -> bool {
    match (memo_token(value), memo_token(&harmonise(constant, Some(value), None))) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn get<'a>(inputs: &'a [(String, LiteralValue)], name: &str) -> Option<&'a LiteralValue> {
    inputs.iter().find(|(existing, _)| existing == name).map(|(_, value)| value)
}

fn constant_of(value: &PlanValue) -> Option<&LiteralValue> {
    match value {
        PlanValue::Constant(constant) => Some(constant),
        _ => None,
    }
}

/// `_number_style`: the numeric type the engine passed for the source's
/// numeric constants.
fn number_style(group: &SiblingGroup, source: usize, inputs: &[(String, LiteralValue)]) -> Option<NumberKind> {
    group.positions.iter().find_map(|position| {
        constant_of(&position.values[source]).and_then(number_kind)?;
        get(inputs, &position.name).and_then(number_kind)
    })
}

/// `_consistent`: whether the observed call can have come from this member.
fn consistent(group: &SiblingGroup, member: usize, inputs: &[(String, LiteralValue)]) -> bool {
    group.positions.iter().all(|position| match &position.values[member] {
        PlanValue::Absent => true,
        value => match get(inputs, &position.name) {
            None => false,
            Some(observed) => constant_of(value).is_none_or(|constant| same(observed, constant)),
        },
    })
}

/// Python `==` between two argument values (numbers across int/float/bool,
/// NaN equal to itself as the identity shortcut makes it in a dict compare).
fn py_equal(left: &LiteralValue, right: &LiteralValue) -> bool {
    fn numeric(value: &LiteralValue) -> Option<(Option<i64>, f64)> {
        match value {
            LiteralValue::Int(number) => Some((Some(*number), *number as f64)),
            LiteralValue::Boolean(flag) => Some((Some(i64::from(*flag)), f64::from(u8::from(*flag)))),
            LiteralValue::Number(number) => Some((None, *number)),
            _ => None,
        }
    }
    match (left, right) {
        (LiteralValue::Number(a), LiteralValue::Number(b)) => a == b || a.to_bits() == b.to_bits(),
        (LiteralValue::Array(a), LiteralValue::Array(b)) => {
            a.len() == b.len()
                && a.iter().zip(b).all(|(x, y)| x.len() == y.len() && x.iter().zip(y).all(|(p, q)| py_equal(p, q)))
        }
        _ => match (numeric(left), numeric(right)) {
            (Some((Some(a), _)), Some((Some(b), _))) => a == b,
            (Some((Some(integer), _)), Some((None, float))) | (Some((None, float)), Some((Some(integer), _))) => {
                float.fract() == 0.0 && I64_SPAN.contains(&float) && float as i64 == integer
            }
            (Some(_), _) | (_, Some(_)) => false,
            _ => left == right,
        },
    }
}

/// Python dict equality (order-free, values by `==`).
fn same_vector(left: &[(String, LiteralValue)], right: &[(String, LiteralValue)]) -> bool {
    left.len() == right.len()
        && left.iter().all(|(name, value)| get(right, name).is_some_and(|other| py_equal(value, other)))
}

fn set_input(vector: &mut InputPairs, name: &str, value: LiteralValue) {
    match vector.iter_mut().find(|(existing, _)| existing == name) {
        Some(slot) => slot.1 = value,
        None => vector.push((name.to_owned(), value)),
    }
}

/// `sibling_vectors(plan, target, output, inputs)`: candidate input vectors
/// for the observed call's siblings, in plan order, harmonised to the
/// observed call's value styles.
pub fn sibling_vectors(
    plan: &SiblingPlan,
    target: &str,
    output: &str,
    inputs: &[(String, LiteralValue)],
) -> Vec<InputPairs> {
    let mut vectors: Vec<InputPairs> = Vec::new();
    for group in &plan.groups {
        if group.target != target || group.output != output {
            continue;
        }
        let members = 0..group.cells.len();
        let sources: Vec<usize> = members.clone().filter(|&member| consistent(group, member, inputs)).collect();
        // A source that passes no pair of a name the observed call holds is
        // only a guess (the block may hold that name); an exact source wins.
        let exact: Vec<usize> = sources
            .iter()
            .copied()
            .filter(|&member| {
                !group.positions.iter().any(|position| {
                    position.values[member] == PlanValue::Absent && get(inputs, &position.name).is_some()
                })
            })
            .collect();
        let sources = if exact.is_empty() { sources } else { exact };
        for source in sources {
            let style = number_style(group, source, inputs);
            'sibling: for sibling in members.clone() {
                if sibling == source {
                    continue;
                }
                let mut vector: InputPairs = inputs.to_vec();
                for position in &group.positions {
                    let (theirs, ours) = (&position.values[sibling], &position.values[source]);
                    match theirs {
                        PlanValue::Dynamic => {
                            if *ours != PlanValue::Dynamic {
                                continue 'sibling;
                            }
                        }
                        PlanValue::Absent => {
                            if *ours != PlanValue::Absent {
                                continue 'sibling;
                            }
                        }
                        PlanValue::Constant(constant) => {
                            let value = harmonise(constant, get(inputs, &position.name), style);
                            set_input(&mut vector, &position.name, value);
                        }
                    }
                }
                if !vectors.iter().any(|existing| same_vector(existing, &vector)) {
                    vectors.push(vector);
                }
            }
        }
    }
    vectors
}

// -- request-scoped prefetcher --------------------------------------------------

/// The in-line call a dispatch is made for.
#[derive(Debug, Clone, Copy)]
pub struct ObservedCall<'a> {
    pub stack: &'a [String],
    pub target: &'a str,
    pub output: &'a LiteralValue,
    /// Child `identity:sha256`.
    pub identity: &'a str,
    /// Key into `package.children`.
    pub child_version: &'a str,
    pub child: &'a ModelSpec,
    pub inputs: &'a [(String, LiteralValue)],
    pub observed_key: &'a MemoKey,
}

/// The memo operations the prefetcher uses (`peek_key`, `contains`,
/// `adopt`). Implemented by [`ModelCallMemo`]; the `*_on` methods of
/// [`Prefetcher`] take any implementor (tests use a fake).
pub trait PrefetchMemo {
    fn peek_key(
        &self,
        stack: &[String],
        identity: &str,
        output: &LiteralValue,
        inputs: &[(String, LiteralValue)],
        spec: Option<&ModelSpec>,
    ) -> Option<MemoKey>;
    fn contains(&self, key: &MemoKey) -> bool;
    fn adopt(&mut self, key: MemoKey, source_index: usize, matrix: &ChildMatrix, allow_errors: bool) -> bool;
}

impl PrefetchMemo for ModelCallMemo {
    fn peek_key(
        &self,
        stack: &[String],
        identity: &str,
        output: &LiteralValue,
        inputs: &[(String, LiteralValue)],
        spec: Option<&ModelSpec>,
    ) -> Option<MemoKey> {
        ModelCallMemo::peek_key(self, stack, identity, output, inputs, spec)
    }
    fn contains(&self, key: &MemoKey) -> bool {
        ModelCallMemo::contains(self, key)
    }
    fn adopt(&mut self, key: MemoKey, source_index: usize, matrix: &ChildMatrix, allow_errors: bool) -> bool {
        ModelCallMemo::adopt(self, key, source_index, matrix, allow_errors)
    }
}

/// Several sibling sub-requests evaluated on ONE loaded child (the batch
/// variant): every request of one dispatch has the same child version,
/// stack and output and differs only in inputs. Returns one outcome per
/// request, in order (a missing outcome counts as a failed flight). The
/// implementation loads (or reuses) one separate child `Workbook` and
/// evaluates each scenario on it, restoring the child's baseline between
/// scenarios (e.g. SheetPort's `BatchExecutor`); `requests[i].cancel` is one
/// shared token.
pub trait ChildBatchEvaluator: Send + Sync {
    fn evaluate_children(&self, requests: &[ChildRequest]) -> Vec<ChildOutcome>;
}

/// Any [`ChildEvaluator`] as a batch evaluator: the scenarios run one after
/// another on one thread (one load each). The reference behaviour a real
/// one-load batch evaluator must match.
pub struct SequentialBatch(pub Arc<dyn ChildEvaluator>);

impl ChildBatchEvaluator for SequentialBatch {
    fn evaluate_children(&self, requests: &[ChildRequest]) -> Vec<ChildOutcome> {
        requests.iter().map(|request| self.0.evaluate_child(request)).collect()
    }
}

/// What a runner thread hands back: per request an outcome (`None` when the
/// evaluator panicked or returned too few), and the thread's wall seconds.
struct RunnerOutput {
    outcomes: Vec<Option<ChildOutcome>>,
    seconds: f64,
}

struct Runner {
    handle: Option<JoinHandle<RunnerOutput>>,
    cancel: CancelToken,
    output: Option<RunnerOutput>,
    seconds_counted: bool,
}

impl Runner {
    fn join(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.output = handle.join().ok();
        }
    }

    fn take_seconds(&mut self) -> f64 {
        if self.seconds_counted {
            return 0.0;
        }
        self.seconds_counted = true;
        self.output.as_ref().map_or(0.0, |output| output.seconds)
    }
}

struct Flight {
    key: MemoKey,
    event: ModelCallEvent,
    runner: usize,
    slot: usize,
}

fn run_guarded(work: impl FnOnce() -> Vec<ChildOutcome>, count: usize) -> RunnerOutput {
    let started = Instant::now();
    let outcomes = match catch_unwind(AssertUnwindSafe(work)) {
        Ok(outcomes) => {
            let mut outcomes: Vec<Option<ChildOutcome>> = outcomes.into_iter().map(Some).collect();
            outcomes.resize_with(count, || None);
            outcomes
        }
        Err(_) => (0..count).map(|_| None).collect(),
    };
    RunnerOutput { outcomes, seconds: started.elapsed().as_secs_f64() }
}

pub struct Prefetcher {
    plan: SiblingPlan,
    max_flights: u32,
    store_errors: bool,
    evaluator: Arc<dyn ChildEvaluator>,
    package: Arc<ModelPackage>,
    context: CalculationContext,
    batch: Option<Arc<dyn ChildBatchEvaluator>>,
    flights: Vec<Flight>,
    runners: Vec<Runner>,
    attempted: HashSet<MemoKey>,
    stored: HashSet<MemoKey>,
    counts: PrefetchReport,
}

impl Prefetcher {
    pub fn new(
        plan: SiblingPlan,
        package: Arc<ModelPackage>,
        context: CalculationContext,
        evaluator: Arc<dyn ChildEvaluator>,
    ) -> Self {
        let max_flights = context.flags.prefetch_max.max(1);
        let store_errors = context.flags.prefetch_errors;
        Self {
            plan,
            max_flights,
            store_errors,
            evaluator,
            package,
            context,
            batch: None,
            flights: Vec::new(),
            runners: Vec::new(),
            attempted: HashSet::new(),
            stored: HashSet::new(),
            counts: PrefetchReport::default(),
        }
    }

    /// Batch variant: every dispatch's sibling scenarios run on one thread
    /// through `batch` (one loaded child) instead of one thread and one load
    /// per flight. Receipt, memo and counters are the same either way.
    pub fn with_batch_evaluator(mut self, batch: Arc<dyn ChildBatchEvaluator>) -> Self {
        self.batch = Some(batch);
        self
    }

    pub fn plan(&self) -> &SiblingPlan {
        &self.plan
    }

    /// Start sibling flights for a parent-level miss; true if any started.
    /// Never fails. Records prefetch timings on first dispatch.
    pub fn dispatch(&mut self, memo: &ModelCallMemo, call: ObservedCall<'_>, timings: &mut Timings) -> bool {
        self.dispatch_on(memo, call, timings)
    }

    /// [`Prefetcher::dispatch`] over any [`PrefetchMemo`].
    pub fn dispatch_on(&mut self, memo: &dyn PrefetchMemo, call: ObservedCall<'_>, timings: &mut Timings) -> bool {
        if !self.flights.is_empty() || call.stack.len() != 1 {
            return false;
        }
        let LiteralValue::Text(output) = call.output else { return false };
        // The in-line call computes this key itself: never prefetch it later.
        self.attempted.insert(call.observed_key.clone());
        let mut context = self.context.clone();
        context.flags.prefetch = false;
        let mut stack = call.stack.to_vec();
        stack.push(call.identity.to_owned());
        let mut planned: Vec<(MemoKey, ModelCallEvent, ChildRequest)> = Vec::new();
        for vector in sibling_vectors(&self.plan, call.target, output, call.inputs) {
            if planned.len() >= self.max_flights as usize {
                break;
            }
            let Some(key) = memo.peek_key(call.stack, call.identity, call.output, &vector, Some(call.child)) else {
                continue;
            };
            if key == *call.observed_key || self.attempted.contains(&key) || memo.contains(&key) {
                continue;
            }
            self.attempted.insert(key.clone());
            let mut event =
                ModelCallEvent::started(0, call.stack, LiteralValue::Text(call.target.to_owned()), call.output.clone());
            event.child = Some(call.identity.to_owned());
            event.inputs = Some(vector.clone());
            event.prefetch = true;
            let request = ChildRequest {
                package: self.package.clone(),
                child_version: call.child_version.to_owned(),
                inputs: vector,
                output: output.clone(),
                stack: stack.clone(),
                context: context.clone(),
                cancel: CancelToken::new(),
            };
            planned.push((key, event, request));
        }
        if planned.is_empty() {
            return false;
        }
        match self.batch.clone() {
            Some(batch) => {
                let cancel = CancelToken::new();
                let requests: Vec<ChildRequest> = planned
                    .iter()
                    .map(|(_, _, request)| ChildRequest { cancel: cancel.clone(), ..request.clone() })
                    .collect();
                let count = requests.len();
                let spawned = std::thread::Builder::new()
                    .name("model-call-prefetch".into())
                    .spawn(move || run_guarded(|| batch.evaluate_children(&requests), count));
                if let Ok(handle) = spawned {
                    let runner = self.runners.len();
                    self.runners.push(Runner { handle: Some(handle), cancel, output: None, seconds_counted: false });
                    for (slot, (key, event, _)) in planned.into_iter().enumerate() {
                        self.flights.push(Flight { key, event, runner, slot });
                        self.note_dispatched(timings);
                    }
                }
            }
            None => {
                for (key, event, request) in planned {
                    let evaluator = self.evaluator.clone();
                    let cancel = request.cancel.clone();
                    let spawned = std::thread::Builder::new()
                        .name("model-call-prefetch".into())
                        .spawn(move || run_guarded(|| vec![evaluator.evaluate_child(&request)], 1));
                    if let Ok(handle) = spawned {
                        let runner = self.runners.len();
                        self.runners.push(Runner { handle: Some(handle), cancel, output: None, seconds_counted: false });
                        self.flights.push(Flight { key, event, runner, slot: 0 });
                        self.note_dispatched(timings);
                    }
                }
            }
        }
        !self.flights.is_empty()
    }

    fn note_dispatched(&mut self, timings: &mut Timings) {
        self.counts.dispatched += 1;
        timings.set(timing_keys::PREFETCH_COUNT, TimingValue::Count(self.counts.dispatched));
        timings.set_default(timing_keys::PREFETCH_SECONDS, TimingValue::Seconds(0.0));
        timings.set_default(timing_keys::PREFETCH_WAIT_SECONDS, TimingValue::Seconds(0.0));
        timings.set_default(timing_keys::PREFETCH_HITS, TimingValue::Count(0));
    }

    /// The in-line call hit a key a flight stored.
    pub fn note_hit(&mut self, key: &MemoKey, timings: &mut Timings) {
        if self.stored.contains(key) {
            self.counts.hits += 1;
            timings.set(timing_keys::PREFETCH_HITS, TimingValue::Count(self.counts.hits));
        }
    }

    fn storable(&self, matrix: &ChildMatrix) -> bool {
        matrix_is_memoisable(matrix) || (self.store_errors && matrix_is_finished(matrix))
    }

    /// Join flights in dispatch order, merge their events (offset indices,
    /// `prefetch: true`) and adopt their results into the memo. Never fails.
    pub fn join_into(&mut self, invocations: &mut Vec<ModelCallEvent>, memo: &mut ModelCallMemo, timings: &mut Timings) {
        self.join_into_on(invocations, memo, timings);
    }

    /// [`Prefetcher::join_into`] over any [`PrefetchMemo`].
    ///
    /// Event numbering: the flight's own event lands at `offset`
    /// (`len(invocations)` before the merge) and a sub-request event with
    /// index `i` in `ChildOutcome::invocations` (numbered from 0, flight
    /// event excluded) at `offset + 1 + i`, `memo_of` likewise: the Python
    /// sub-session's list, which holds the flight event at 0.
    pub fn join_into_on(
        &mut self,
        invocations: &mut Vec<ModelCallEvent>,
        memo: &mut dyn PrefetchMemo,
        timings: &mut Timings,
    ) {
        let flights = std::mem::take(&mut self.flights);
        let mut runners = std::mem::take(&mut self.runners);
        for flight in flights {
            let waited = Instant::now();
            let runner = &mut runners[flight.runner];
            runner.join();
            timings.add_seconds(timing_keys::PREFETCH_WAIT_SECONDS, waited.elapsed().as_secs_f64());
            let seconds = runner.take_seconds();
            timings.add_seconds(timing_keys::PREFETCH_SECONDS, seconds);
            let outcome = runner.output.as_mut().and_then(|output| output.outcomes.get_mut(flight.slot)?.take());
            let matrix = match outcome {
                Some(ChildOutcome { result: Ok(matrix), faults, invocations: events, .. }) if faults.is_empty() => {
                    Some((matrix, events))
                }
                _ => None,
            };
            let Some((matrix, events)) = matrix.filter(|(matrix, _)| self.storable(matrix)) else {
                self.counts.not_stored += 1;
                continue;
            };
            let offset = invocations.len();
            let mut event = flight.event;
            event.index = offset;
            event.status = CallStatus::Completed;
            event.matrix = Some(matrix.clone());
            event.prefetch = true;
            invocations.push(event);
            for mut event in events {
                event.prefetch = true;
                event.index += offset + 1;
                if let Some(source) = event.memo_of.as_mut() {
                    *source += offset + 1;
                }
                invocations.push(event);
            }
            if !memo.adopt(flight.key.clone(), offset, &matrix, self.store_errors) {
                self.counts.not_stored += 1;
                continue;
            }
            self.stored.insert(flight.key);
            self.counts.stored += 1;
        }
    }

    /// Cancel and join every flight; nothing is stored. Never fails.
    pub fn abandon(&mut self, timings: &mut Timings) {
        let flights = std::mem::take(&mut self.flights);
        let mut runners = std::mem::take(&mut self.runners);
        for runner in &runners {
            runner.cancel.cancel();
        }
        for flight in flights {
            let runner = &mut runners[flight.runner];
            runner.join();
            timings.add_seconds(timing_keys::PREFETCH_SECONDS, runner.take_seconds());
            self.counts.not_stored += 1;
        }
    }

    /// `None` until something was dispatched.
    pub fn report(&self) -> Option<PrefetchReport> {
        (self.counts.dispatched > 0).then(|| self.counts.clone())
    }
}

impl Drop for Prefetcher {
    /// Flights never joined (a dropped request) are cancelled, not waited for.
    fn drop(&mut self) {
        for runner in &self.runners {
            runner.cancel.cancel();
        }
    }
}
