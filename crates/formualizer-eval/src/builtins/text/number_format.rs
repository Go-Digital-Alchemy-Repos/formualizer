//! Bounded Excel numeric format renderer for `TEXT`.
//!
//! This deliberately implements only the measured oracle surface (OT-076
//! ES-035 plus the OT-080 and OT-081 defect probes). Syntax outside that
//! surface returns [`Fallback::Unsupported`] so `TEXT` can preserve its
//! previous behavior; [`Fallback::Invalid`] is a measured Excel `#VALUE!`.

use crate::builtins::math::numeric::excel_round;
use formualizer_common::numfmt::{FormatClass, NumberFormat};

/// `TEXT` returns `#VALUE!` once its rendered result grows past this length.
/// OT-081 pinned the boundary: a 255-character result renders and a
/// 256-character result returns `#VALUE!`.
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
    // Placeholder count has no separate cap on the measured surface. OT-081
    // T4m renders forty zero placeholders, while T4l's three hundred
    // placeholders return #VALUE! because the result exceeds 255 characters.
    // Parse every placeholder here and let the final rendered-length check
    // decide validity, so an overlong result is Invalid rather than Unsupported.

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
            // A period after the percent suffix is punctuation, not another
            // decimal separator (GOD-308 captured report format 0.00%.).
            Unit::Decimal if out.ends_with('%') => out.push('.'),
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
    let (raw_integer, raw_fraction) = decimal_view_parts(rounded, section.max_fraction_digits)?;

    let mut integer = raw_integer;
    if integer == "0" && section.min_integer_digits == 0 {
        integer.clear();
    }
    if integer.len() < section.min_integer_digits {
        integer = "0".repeat(section.min_integer_digits - integer.len()) + &integer;
    }
    if section.grouping && !integer.is_empty() {
        integer = group_thousands(&integer);
    }

    let mut fraction = raw_fraction;
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

fn decimal_view_parts(value: f64, fraction_digits: usize) -> Result<(String, String), Fallback> {
    let rendered = value.to_string();
    let (mantissa, exponent) = if let Some((mantissa, exponent)) = rendered.split_once(['e', 'E']) {
        (
            mantissa,
            exponent.parse::<i64>().map_err(|_| Fallback::Unsupported)?,
        )
    } else {
        (rendered.as_str(), 0_i64)
    };
    let integer_digits = mantissa
        .split_once('.')
        .map_or(mantissa.len(), |(integer, _)| integer.len()) as i64;
    let digits: String = mantissa.chars().filter(|ch| ch.is_ascii_digit()).collect();
    let decimal_position = integer_digits + exponent;

    let integer = if decimal_position <= 0 {
        "0".into()
    } else {
        let decimal_position = decimal_position as usize;
        if decimal_position < digits.len() {
            digits[..decimal_position].into()
        } else {
            digits.clone() + &"0".repeat(decimal_position - digits.len())
        }
    };

    let mut fraction = String::with_capacity(fraction_digits);
    for offset in 0..fraction_digits {
        let position = decimal_position + offset as i64;
        if position < 0 {
            fraction.push('0');
        } else {
            fraction.push(
                digits
                    .as_bytes()
                    .get(position as usize)
                    .copied()
                    .unwrap_or(b'0') as char,
            );
        }
    }

    let rendered_end = decimal_position + fraction_digits as i64;
    if rendered_end < digits.len() as i64
        && digits[rendered_end.max(0) as usize..]
            .bytes()
            .any(|digit| digit != b'0')
    {
        return Err(Fallback::Unsupported);
    }

    Ok((integer, fraction))
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

    // GOD-308: captured DirectGrowth UAT report footnote is 0.10%.
    #[test]
    fn percent_suffix_preserves_trailing_period() {
        assert_eq!(format_number(0.001, "0.00%."), Ok("0.10%.".into()));
        assert_eq!(format_number(0.001, "0.00%"), Ok("0.10%".into()));
    }

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

    /// OT-081 Excel oracle: the decimal-view result may run to 255 characters
    /// and returns `#VALUE!` at 256.
    #[test]
    fn ot081_rendered_length_wall_is_255_characters() {
        let at_wall = format_number(1e254, "0").expect("255 digits render");
        assert_eq!(at_wall.len(), 255);
        assert_eq!(at_wall, decimal_power(254));
        assert_eq!(format_number(1e255, "0"), Err(Fallback::Invalid));
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

    /// OT-081 Excel oracle (Excel for Mac 16.105.3, saved-XML readback,
    /// 2026-09-01): integer rendering expands the 15-significant-digit
    /// decimal view, not the exact binary64 value.
    fn decimal_power(exponent: usize) -> String {
        "1".to_string() + &"0".repeat(exponent)
    }

    #[test]
    fn ot081_t4c_expands_the_decimal_view_at_1e100() {
        assert_eq!(format_number(1e100, "0"), Ok(decimal_power(100)));
    }

    #[test]
    fn ot081_t4d_expands_the_decimal_view_at_1e200() {
        assert_eq!(format_number(1e200, "0"), Ok(decimal_power(200)));
    }

    #[test]
    fn ot081_fractional_formats_expand_the_large_decimal_view() {
        let expected = decimal_power(100) + ".00";
        assert_eq!(format_number(1e100, "0.00"), Ok(expected.clone()));
        assert_eq!(
            format_number(-1e100, "0.00"),
            Ok("-".to_string() + &expected)
        );
    }

    #[test]
    fn ot081_t4h_allows_a_255_character_decimal_view() {
        assert_eq!(format_number(1e254, "0"), Ok(decimal_power(254)));
    }

    #[test]
    fn ot081_t4i_rejects_a_256_character_decimal_view() {
        assert_eq!(format_number(1e255, "0"), Err(Fallback::Invalid));
    }

    #[test]
    fn ot081_sub_1e15_exact_integer_control_is_unchanged() {
        assert_eq!(
            format_number(999_999_999_999_999_f64, "0"),
            Ok("999999999999999".into())
        );
    }

    #[test]
    fn ot081_negative_decimal_view_mirrors_the_positive_path() {
        assert_eq!(
            format_number(-1e100, "0"),
            Ok("-".to_string() + &decimal_power(100))
        );
    }

    #[test]
    fn ot081_negative_exact_integer_control_is_unchanged() {
        assert_eq!(
            format_number(-999_999_999_999_999_f64, "0"),
            Ok("-999999999999999".into())
        );
    }

    /// OT-081 Excel oracle: placeholder count has no independent 30-character
    /// cap. The rendered result succeeds through 255 characters and returns
    /// `#VALUE!` above that wall.
    #[test]
    fn ot081_t4m_allows_40_zero_placeholders() {
        assert_eq!(
            format_number(1.0, &"0".repeat(40)),
            Ok("0".repeat(39) + "1")
        );
    }

    #[test]
    fn ot081_t4l_rejects_a_300_character_render() {
        assert_eq!(format_number(1.0, &"0".repeat(300)), Err(Fallback::Invalid));
    }
}
