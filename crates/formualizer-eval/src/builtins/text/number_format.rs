//! Bounded Excel numeric format renderer for `TEXT`.
//!
//! This deliberately implements only the OT-076 oracle surface. Unsupported
//! syntax returns `None` so `TEXT` can preserve its previous behavior.

use crate::builtins::utils::round_to_precision;

const MAX_PLACEHOLDERS: usize = 30;

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

pub(super) fn format_number(value: f64, code: &str) -> Option<String> {
    if !value.is_finite() {
        return None;
    }
    let raw_sections = split_sections(code)?;
    if raw_sections.is_empty() || raw_sections.len() > 3 {
        return None;
    }
    let sections = raw_sections
        .into_iter()
        .map(parse_section)
        .collect::<Option<Vec<_>>>()?;

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
    render_section(&sections[index], value.abs(), automatic_minus)
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

fn parse_section(code: &str) -> Option<Section> {
    let mut units = Vec::with_capacity(code.len());
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
                    return None;
                }
            }
            '\\' => units.push(Unit::Literal(chars.next()?)),
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
                    return None;
                }
            }
            '0' | '#' => units.push(Unit::Placeholder(ch)),
            '.' => units.push(Unit::Decimal),
            ',' => units.push(Unit::Comma),
            '%' => units.push(Unit::Percent),
            '?' | '@' | '*' | '_' => return None,
            other if other.is_ascii_alphabetic() => return None,
            other => units.push(Unit::Literal(other)),
        }
    }

    let placeholder_indices = units
        .iter()
        .enumerate()
        .filter_map(|(index, unit)| matches!(unit, Unit::Placeholder(_)).then_some(index))
        .collect::<Vec<_>>();
    if placeholder_indices.is_empty() {
        if units.iter().any(|unit| !matches!(unit, Unit::Literal(_))) {
            return None;
        }
        return Some(Section {
            prefix: String::new(),
            suffix: String::new(),
            min_integer_digits: 0,
            min_fraction_digits: 0,
            max_fraction_digits: 0,
            grouping: false,
            percent_count: 0,
            comma_scale_count: 0,
            literal_only: Some(literal_text(&units)?),
        });
    }
    if placeholder_indices.len() > MAX_PLACEHOLDERS {
        return None;
    }

    let first = placeholder_indices[0];
    let last = *placeholder_indices.last().unwrap();
    let decimal_indices = units[first..=last]
        .iter()
        .enumerate()
        .filter_map(|(offset, unit)| matches!(unit, Unit::Decimal).then_some(first + offset))
        .collect::<Vec<_>>();
    if decimal_indices.len() > 1 {
        return None;
    }
    let decimal = decimal_indices.first().copied();
    if units[first..=last]
        .iter()
        .any(|unit| !matches!(unit, Unit::Placeholder(_) | Unit::Decimal | Unit::Comma))
    {
        return None;
    }

    let integer_end = decimal.unwrap_or(last + 1);
    let integer = &units[first..integer_end];
    let fraction = decimal.map_or(&[][..], |index| &units[index + 1..=last]);
    if fraction.iter().any(|unit| matches!(unit, Unit::Comma)) {
        return None;
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

    let prefix = rendered_affix(&units[..first])?;
    let suffix = rendered_affix(&units[last + 1..])?;
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
        return None;
    }

    Some(Section {
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

fn render_section(section: &Section, value: f64, automatic_minus: bool) -> Option<String> {
    if let Some(literal) = &section.literal_only {
        return Some(literal.clone());
    }
    let percent_scale = 100_f64.powi(section.percent_count.try_into().ok()?);
    let comma_scale = 1_000_f64.powi(section.comma_scale_count.try_into().ok()?);
    let scaled = value * percent_scale / comma_scale;
    if !scaled.is_finite() {
        return None;
    }
    let rounded = round_to_precision(scaled, section.max_fraction_digits as i32);
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
    Some(format!(
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
        assert_eq!(format_number(7.0, r#"0";units""#), Some("7;units".into()));
    }

    #[test]
    fn unsupported_syntax_falls_back() {
        for code in ["[>=1]0", "0.00E+00", "# ?/?", "h:mm AM/PM", "_(* #,##0_)"] {
            assert_eq!(format_number(1.0, code), None, "{code}");
        }
    }
}
