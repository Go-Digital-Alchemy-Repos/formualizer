//! Bounded Excel numeric format renderer for `TEXT`.
//!
//! This deliberately implements only the measured oracle surface (OT-076
//! ES-035 plus the OT-080 defect probes). Syntax outside that surface returns
//! [`Fallback::Unsupported`] so `TEXT` can preserve its previous behavior;
//! [`Fallback::Invalid`] is a measured Excel `#VALUE!`.

use crate::builtins::math::numeric::excel_round;
use formualizer_common::numfmt::{FormatClass, NumberFormat};

const MAX_PLACEHOLDERS: usize = 30;

/// `TEXT` returns `#VALUE!` once its rendered result grows past this length.
/// Every OT-080 huge-magnitude probe (1.7E+306 and up, 307+ digits) measured
/// `#VALUE!` in Excel; the exact cut-over between 255 characters and those
/// magnitudes is not oracle-pinned, so 255 (Excel's string-result limit for
/// the function) is the inferred wall.
const MAX_RENDERED_CHARS: usize = 255;

/// Why the bounded renderer declined to produce text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Fallback {
    /// Syntax outside the measured surface; `TEXT` keeps its legacy rendering.
    Unsupported,
    /// A measured Excel `#VALUE!` result.
    Invalid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unit {
    Placeholder(char),
    Decimal,
    Comma,
    Percent,
    Literal(char),
}

#[derive(Debug, PartialEq, Eq)]
struct Section {
    prefix: String,
    suffix: String,
    min_integer_digits: usize,
    min_fraction_digits: usize,
    max_fraction_digits: usize,
    grouping: bool,
    percent_count: usize,
    comma_scale_count: usize,
    literal_only: Option<String>,
}

pub(super) fn format_number(value: f64, code: &str) -> Result<String, Fallback> {
    if !value.is_finite() {
        return Err(Fallback::Unsupported);
    }
    if !matches!(
        NumberFormat::parse(code).class(),
        FormatClass::Number { .. } | FormatClass::Percent { .. }
    ) {
        return Err(Fallback::Unsupported);
    }
    let raw_sections = split_sections(code).ok_or(Fallback::Unsupported)?;
    if raw_sections.is_empty() || raw_sections.len() > 3 {
        return Err(Fallback::Unsupported);
    }
    let sections = raw_sections
        .into_iter()
        .map(parse_section)
        .collect::<Result<Vec<_>, _>>()?;

    let (index, automatic_minus) = if value < 0.0 {
        if sections.len() >= 2 {
            (1, false)
        } else {
            (0, true)
        }
    } else if value == 0.0 && sections.len() >= 3 {
        (2, false)
    } else {
        (0, false)
    };
    let rendered = render_section(&sections[index], value.abs(), automatic_minus)?;
    if rendered.chars().count() > MAX_RENDERED_CHARS {
        return Err(Fallback::Invalid);
    }
    Ok(rendered)
}

fn split_sections(code: &str) -> Option<Vec<&str>> {
    let mut sections = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    let mut bracketed = false;
    for (index, ch) in code.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quoted {
            if ch == '"' {
                quoted = false;
            }
            continue;
        }
        if bracketed {
            if ch == ']' {
                bracketed = false;
            }
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => quoted = true,
            '[' => bracketed = true,
            ';' => {
                sections.push(&code[start..index]);
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    if quoted || escaped || bracketed {
        return None;
    }
    sections.push(&code[start..]);
    Some(sections)
}

fn parse_section(code: &str) -> Result<Section, Fallback> {
    let mut units = Vec::with_capacity(code.len());
    // An unquoted, unescaped `m` is the month/minute code. Excel returns
    // `#VALUE!` when it sits beside numeric placeholders (OT-080 oracle:
    // `TEXT(1,"0m")`, `TEXT(45000,"0.00m")`, `TEXT(5,"0 mm")`).
    let mut temporal_code = false;
    let mut chars = code.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                let mut closed = false;
                for literal in chars.by_ref() {
                    if literal == '"' {
                        closed = true;
                        break;
                    }
                    units.push(Unit::Literal(literal));
                }
                if !closed {
                    return Err(Fallback::Unsupported);
                }
            }
            '\\' => units.push(Unit::Literal(chars.next().ok_or(Fallback::Unsupported)?)),
            '[' => {
                let mut tag = String::new();
                let mut closed = false;
                for tagged in chars.by_ref() {
                    if tagged == ']' {
                        closed = true;
                        break;
                    }
                    tag.push(tagged);
                }
                if !closed || !is_supported_color(&tag) {
                    return Err(Fallback::Unsupported);
                }
            }
            '0' | '#' => units.push(Unit::Placeholder(ch)),
            '.' => units.push(Unit::Decimal),
            ',' => units.push(Unit::Comma),
            '%' => units.push(Unit::Percent),
            '?' | '@' | '*' | '_' => return Err(Fallback::Unsupported),
            'm' | 'M' => {
                temporal_code = true;
                units.push(Unit::Literal(ch));
            }
            other => units.push(Unit::Literal(other)),
        }
    }

    let placeholder_indices = units
        .iter()
        .enumerate()
        .filter_map(|(index, unit)| matches!(unit, Unit::Placeholder(_)).then_some(index))
        .collect::<Vec<_>>();
    if placeholder_indices.is_empty() {
        if temporal_code || units.iter().any(|unit| !matches!(unit, Unit::Literal(_))) {
            return Err(Fallback::Unsupported);
        }
        return Ok(Section {
            prefix: String::new(),
            suffix: String::new(),
            min_integer_digits: 0,
            min_fraction_digits: 0,
            max_fraction_digits: 0,
            grouping: false,
            percent_count: 0,
            comma_scale_count: 0,
            literal_only: Some(literal_text(&units).ok_or(Fallback::Unsupported)?),
        });
    }
    if temporal_code {
        return Err(Fallback::Invalid);
    }
    if placeholder_indices.len() > MAX_PLACEHOLDERS {
        return Err(Fallback::Unsupported);
    }

    let first = placeholder_indices[0];
    let last = *placeholder_indices.last().unwrap();
    let decimal_indices = units[first..=last]
        .iter()
        .enumerate()
        .filter_map(|(offset, unit)| matches!(unit, Unit::Decimal).then_some(first + offset))
        .collect::<Vec<_>>();
    if decimal_indices.len() > 1 {
        return Err(Fallback::Unsupported);
    }
    let decimal = decimal_indices.first().copied();
    if units[first..=last]
        .iter()
        .any(|unit| !matches!(unit, Unit::Placeholder(_) | Unit::Decimal | Unit::Comma))
    {
        return Err(Fallback::Unsupported);
    }

    let integer_end = decimal.unwrap_or(last + 1);
    let integer = &units[first..integer_end];
    let fraction = decimal.map_or(&[][..], |index| &units[index + 1..=last]);
    if fraction.iter().any(|unit| matches!(unit, Unit::Comma)) {
        return Err(Fallback::Unsupported);
    }
    let min_integer_digits = integer
        .iter()
        .filter(|unit| matches!(unit, Unit::Placeholder('0')))
        .count();
    let min_fraction_digits = fraction
        .iter()
        .filter(|unit| matches!(unit, Unit::Placeholder('0')))
        .count();
    let max_fraction_digits = fraction
        .iter()
        .filter(|unit| matches!(unit, Unit::Placeholder(_)))
        .count();
    let grouping = integer.iter().any(|unit| matches!(unit, Unit::Comma));

    let prefix = rendered_affix(&units[..first]).ok_or(Fallback::Unsupported)?;
    let suffix = rendered_affix(&units[last + 1..]).ok_or(Fallback::Unsupported)?;
    let percent_count = units
        .iter()
        .filter(|unit| matches!(unit, Unit::Percent))
        .count();
    let comma_scale_count = units[last + 1..]
        .iter()
        .filter(|unit| matches!(unit, Unit::Comma))
        .count();
    if units[..first]
        .iter()
        .any(|unit| matches!(unit, Unit::Comma))
    {
        return Err(Fallback::Unsupported);
    }

    Ok(Section {
        prefix,
        suffix,
        min_integer_digits,
        min_fraction_digits,
        max_fraction_digits,
        grouping,
        percent_count,
        comma_scale_count,
        literal_only: None,
    })
}

fn is_supported_color(tag: &str) -> bool {
    matches!(
        tag.to_ascii_lowercase().as_str(),
        "black" | "blue" | "cyan" | "green" | "magenta" | "red" | "white" | "yellow"
    )
}

fn literal_text(units: &[Unit]) -> Option<String> {
    units
        .iter()
        .map(|unit| match unit {
            Unit::Literal(ch) => Some(*ch),
            _ => None,
        })
        .collect()
}

fn rendered_affix(units: &[Unit]) -> Option<String> {
    let mut out = String::new();
    for unit in units {
        match unit {
            Unit::Literal(ch) => out.push(*ch),
            Unit::Percent => out.push('%'),
            Unit::Comma => {}
            _ => return None,
        }
    }
    Some(out)
}

fn render_section(
    section: &Section,
    value: f64,
    automatic_minus: bool,
) -> Result<String, Fallback> {
    if let Some(literal) = &section.literal_only {
        return Ok(literal.clone());
    }
    let percent_scale = 100_f64.powi(
        section
            .percent_count
            .try_into()
            .map_err(|_| Fallback::Unsupported)?,
    );
    let comma_scale = 1_000_f64.powi(
        section
            .comma_scale_count
            .try_into()
            .map_err(|_| Fallback::Unsupported)?,
    );
    let scaled = value * percent_scale / comma_scale;
    if !scaled.is_finite() {
        // Only a magnitude already past the rendered-length wall can overflow
        // the percent scaling; Excel measured `#VALUE!` there (T1g).
        return Err(Fallback::Invalid);
    }
    // Display rounding acts on the 15-significant-digit decimal view, half
    // away from zero, exactly like ROUND (OT-080 oracle T3a-T3g), and never
    // scales by a binary `10^digits`, so no magnitude overflows to `inf`.
    let rounded = excel_round(scaled, section.max_fraction_digits as i32);
    if !rounded.is_finite() {
        // The 15-digit decimal view of a value at the top of the double range
        // rounds up past `f64::MAX`; it is far beyond the measured `#VALUE!`
        // region anyway.
        return Err(Fallback::Invalid);
    }
    let fixed = format!("{:.*}", section.max_fraction_digits, rounded);
    let (raw_integer, raw_fraction) = fixed.split_once('.').unwrap_or((&fixed, ""));

    let mut integer = raw_integer.to_string();
    if integer == "0" && section.min_integer_digits == 0 {
        integer.clear();
    }
    if integer.len() < section.min_integer_digits {
        integer = "0".repeat(section.min_integer_digits - integer.len()) + &integer;
    }
    if section.grouping && !integer.is_empty() {
        integer = group_thousands(&integer);
    }

    let mut fraction = raw_fraction.to_string();
    while fraction.len() > section.min_fraction_digits && fraction.ends_with('0') {
        fraction.pop();
    }
    let decimal = if fraction.is_empty() {
        String::new()
    } else {
        format!(".{fraction}")
    };
    Ok(format!(
        "{}{}{}{}{}",
        if automatic_minus { "-" } else { "" },
        section.prefix,
        integer,
        decimal,
        section.suffix
    ))
}

fn group_thousands(integer: &str) -> String {
    let mut reversed = String::with_capacity(integer.len() + integer.len() / 3);
    for (index, ch) in integer.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            reversed.push(',');
        }
        reversed.push(ch);
    }
    reversed.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_semicolons_do_not_split_sections() {
        assert_eq!(format_number(7.0, r#"0";units""#), Ok("7;units".into()));
    }

    #[test]
    fn unsupported_syntax_falls_back() {
        for code in ["[>=1]0", "0.00E+00", "# ?/?", "h:mm AM/PM", "_(* #,##0_)"] {
            assert_eq!(
                format_number(1.0, code),
                Err(Fallback::Unsupported),
                "{code}"
            );
        }
    }

    /// OT-080 oracle tier 1 (Excel for Mac 16.105.3, 2026-09-01): every
    /// huge-magnitude probe is `#VALUE!`, never `inf` and never a 300-digit
    /// expansion. The previous renderer scaled by a binary `10^digits` and
    /// overflowed to infinity.
    #[test]
    fn ot080_huge_magnitudes_are_value_errors() {
        let cases = [
            (1e307_f64, "0.00"),
            (-1e307_f64, "0.00"),
            (1e307_f64, "#,##0.00"),
            (1e307_f64, "0"),
            (9.9e306_f64, "0.00"),
            (1.7e306_f64, "0.00"),
            (1e307_f64, "0.00%"),
            (f64::MAX, "0.00"),
        ];
        for (value, code) in cases {
            assert_eq!(
                format_number(value, code),
                Err(Fallback::Invalid),
                "format_number({value:e}, {code:?})"
            );
        }
    }

    /// Engine wall, INFERRED (not oracle-pinned): the rendered result may run
    /// to 255 characters and errors at 256. The measured `#VALUE!` region
    /// starts at 1.7E+306 (307+ digits); the follow-up probes that would pin
    /// the cut-over are `=TEXT(1E+255,"0")` (the double below 1E+255 has 255
    /// digits) and `=TEXT(1E+256,"0")` (256 digits).
    #[test]
    fn ot080_rendered_length_wall_is_255_characters() {
        let at_wall = format_number(1e255, "0").expect("255 digits render");
        assert_eq!(at_wall.len(), 255);
        assert!(at_wall.chars().all(|ch| ch.is_ascii_digit()), "{at_wall}");
        assert_eq!(format_number(1e256, "0"), Err(Fallback::Invalid));
        assert_eq!(
            format_number(1e20, "0.00"),
            Ok("100000000000000000000.00".into())
        );
    }

    /// OT-080 oracle tier 2: an unquoted `m` beside numeric placeholders is a
    /// temporal code and Excel returns `#VALUE!`; quoted and backslash-escaped
    /// `m` stay literal. A bare `m`/`mm` is a month code and belongs to the
    /// date path, so the numeric renderer declines it.
    #[test]
    fn ot080_month_code_beside_placeholders_is_a_value_error() {
        for code in ["0m", "0.00m", "0 mm", "0.00 m", "m0", "0mm", "0.00M"] {
            assert_eq!(format_number(1.0, code), Err(Fallback::Invalid), "{code}");
            assert_eq!(
                format_number(45000.0, code),
                Err(Fallback::Invalid),
                "{code}"
            );
        }
        assert_eq!(format_number(1.0, r#"0"m""#), Ok("1m".into()));
        assert_eq!(format_number(1.0, r"0\m"), Ok("1m".into()));
        assert_eq!(format_number(45000.0, "m"), Err(Fallback::Unsupported));
        assert_eq!(format_number(45000.0, "mm"), Err(Fallback::Unsupported));
    }

    /// OT-080 oracle tier 3: display rounding uses the 15-significant-digit
    /// decimal view, half away from zero. `1.005` and `0.145` are below the
    /// midpoint as binary64 but round up in Excel.
    #[test]
    fn ot080_midpoints_round_the_decimal_view_half_away_from_zero() {
        let cases = [
            (1.005_f64, "0.00", "1.01"),
            (0.145_f64, "0.00", "0.15"),
            (2.675_f64, "0.00", "2.68"),
            (44821.875_f64, "0.00", "44821.88"),
            (8.835_f64, "0.00", "8.84"),
            (1.5_f64, "0", "2"),
            (2.5_f64, "0", "3"),
        ];
        for (value, code, expected) in cases {
            assert_eq!(
                format_number(value, code),
                Ok(expected.into()),
                "format_number({value}, {code:?})"
            );
        }
    }
}
