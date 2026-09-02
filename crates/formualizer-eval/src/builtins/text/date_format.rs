//! Bounded Excel date/time format renderer for `TEXT`.
//!
//! This deliberately implements only the measured oracle surface (GOD-227 /
//! CL-066 / ES-045, receipt
//! `artifacts/private/god227/probe/excel-text-date-probe.json`, Excel for Mac
//! 16.105.3, `en_US`, 2026-09-02). Syntax outside that surface returns
//! [`Fallback::Unsupported`] so `TEXT` keeps its previous legacy rendering
//! byte-for-byte; [`Fallback::Invalid`] is a measured Excel `#VALUE!`.
//!
//! Measured surface: token *runs* of the same letter, matched
//! case-insensitively — `m`/`mm`/`mmm`/`mmmm`/`mmmmm`, `d`/`dd`/`ddd`/`dddd`,
//! `yy`/`yyyy`, `h`/`hh`, `s`/`ss` — plus the `AM/PM` marker (either spelling,
//! any case mix; the receipt shows the output is always fixed-case `AM`/`PM`)
//! and elapsed hours `[h]`. Every other character is a verbatim literal. `m`
//! is minutes when its run is adjacent to an hour token before it or an `s`
//! token after it (allowing intervening non-letter literals), month otherwise.
//!
//! Month and day names are hard-coded **en_US**. That is the locale receipt
//! banked with the oracle (`locale.macos_locale = "en_US"`,
//! `macos_languages = ["en-US"]`, `month_name_full = "August"`,
//! `day_name_full = "Thursday"`); no other locale was measured, so no other
//! locale is implemented.
//!
//! Serial → calendar parts routes through
//! `formualizer_common::try_serial_to_display_date_parts_for`, preserving the
//! deliberate Excel-1900 phantom-leap-day handling (serial 59 → 1900-02-28,
//! serial 60 → 1900-02-29), which the receipt confirms Excel agrees with.

use formualizer_common::try_serial_to_display_date_parts_for;

/// Why the bounded renderer declined to produce text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Fallback {
    /// Syntax outside the measured surface; `TEXT` keeps its legacy rendering.
    Unsupported,
    /// A measured Excel `#VALUE!` result.
    Invalid,
}

const MONTHS_FULL: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

const MONTHS_ABBREV: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Indexed by `serial mod 7`, where Excel's serial 1 (1900-01-01) is a Sunday
/// and therefore serial 0 is a Saturday.
const DAYS_FULL: [&str; 7] = [
    "Saturday",
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
];

const DAYS_ABBREV: [&str; 7] = ["Sat", "Sun", "Mon", "Tue", "Wed", "Thu", "Fri"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    /// Unresolved `m` run: month or minute, decided by adjacency.
    MonthOrMinute(usize),
    Month(usize),
    Minute(usize),
    Day(usize),
    Year(usize),
    Hour(usize),
    Second(usize),
    ElapsedHours,
    AmPm,
    Literal(char),
}

impl Token {
    /// A letter-bearing token stops adjacency scanning; punctuation and
    /// spaces do not.
    fn is_scan_barrier(&self) -> bool {
        match self {
            Token::Literal(c) => c.is_alphabetic(),
            _ => true,
        }
    }
}

pub(super) fn format_date(
    system: crate::engine::DateSystem,
    value: f64,
    code: &str,
) -> Result<String, Fallback> {
    let tokens = tokenize(code)?;
    render(system, value, &tokens)
}

/// Characters that always take the code outside the measured surface: quoted
/// and escaped literals, width reservation, fill, sections, fraction and
/// numeric placeholders, and the text placeholder.
fn is_unsupported_char(c: char) -> bool {
    matches!(c, '"' | '\\' | '_' | '*' | ';' | '?' | '0' | '#' | '@')
}

fn tokenize(code: &str) -> Result<Vec<Token>, Fallback> {
    let chars: Vec<char> = code.chars().collect();
    let mut tokens: Vec<Token> = Vec::with_capacity(chars.len());
    let mut i = 0usize;
    let mut saw_field = false;

    while i < chars.len() {
        let c = chars[i];
        if is_unsupported_char(c) {
            return Err(Fallback::Unsupported);
        }
        if c == '[' {
            // Only `[h]` / `[H]` is measured. `[m]`, `[s]`, `[$-409]`, colour
            // and condition brackets are all outside the surface.
            let is_elapsed_hours = chars.get(i + 1).is_some_and(|h| *h == 'h' || *h == 'H')
                && chars.get(i + 2) == Some(&']');
            if !is_elapsed_hours {
                return Err(Fallback::Unsupported);
            }
            tokens.push(Token::ElapsedHours);
            saw_field = true;
            i += 3;
            continue;
        }
        if matches_ampm(&chars, i) {
            tokens.push(Token::AmPm);
            saw_field = true;
            i += 5;
            continue;
        }
        if c.is_alphabetic() {
            let lower = c.to_ascii_lowercase();
            let mut run = 0usize;
            while i + run < chars.len() && chars[i + run].to_ascii_lowercase() == lower {
                run += 1;
            }
            let token = match lower {
                'm' if (1..=5).contains(&run) => Some(Token::MonthOrMinute(run)),
                'd' if (1..=4).contains(&run) => Some(Token::Day(run)),
                'y' if run == 2 || run == 4 => Some(Token::Year(run)),
                'h' if (1..=2).contains(&run) => Some(Token::Hour(run)),
                's' if (1..=2).contains(&run) => Some(Token::Second(run)),
                'm' | 'd' | 'y' | 'h' | 's' => {
                    // A run width the receipt never measured.
                    return Err(Fallback::Unsupported);
                }
                _ => None,
            };
            match token {
                Some(t) => {
                    tokens.push(t);
                    saw_field = true;
                }
                None => {
                    for offset in 0..run {
                        tokens.push(Token::Literal(chars[i + offset]));
                    }
                }
            }
            i += run;
            continue;
        }
        tokens.push(Token::Literal(c));
        i += 1;
    }

    if !saw_field {
        return Err(Fallback::Unsupported);
    }
    resolve_month_or_minute(&mut tokens);
    Ok(tokens)
}

/// `AM/PM` and `am/pm` (any case mix) are the only measured markers.
fn matches_ampm(chars: &[char], i: usize) -> bool {
    const MARKER: [char; 5] = ['a', 'm', '/', 'p', 'm'];
    chars.len() >= i + 5 && (0..5).all(|k| chars[i + k].to_ascii_lowercase() == MARKER[k])
}

/// `m` is minutes when it sits directly after an hour token or directly before
/// a seconds token, ignoring intervening non-letter literals. Adjacency, not
/// position.
fn resolve_month_or_minute(tokens: &mut [Token]) {
    for index in 0..tokens.len() {
        let width = match tokens[index] {
            Token::MonthOrMinute(width) => width,
            _ => continue,
        };
        let preceded_by_hour = tokens[..index]
            .iter()
            .rev()
            .find(|t| t.is_scan_barrier())
            .is_some_and(|t| matches!(t, Token::Hour(_) | Token::ElapsedHours));
        let followed_by_second = tokens[index + 1..]
            .iter()
            .find(|t| t.is_scan_barrier())
            .is_some_and(|t| matches!(t, Token::Second(_)));
        tokens[index] = if preceded_by_hour || followed_by_second {
            Token::Minute(width)
        } else {
            Token::Month(width)
        };
    }
}

fn render(
    system: crate::engine::DateSystem,
    value: f64,
    tokens: &[Token],
) -> Result<String, Fallback> {
    let has_ampm = tokens.iter().any(|t| matches!(t, Token::AmPm));
    let has_seconds = tokens.iter().any(|t| matches!(t, Token::Second(_)));
    let has_clock = tokens
        .iter()
        .any(|t| matches!(t, Token::Hour(_) | Token::Minute(_) | Token::Second(_)));

    // Rounding to the displayed resolution can carry into the next calendar
    // day; the displayed date must follow it (pinned pre-GOD-227 behaviour).
    let (display_serial, total_seconds) = if !has_clock {
        (value, 0i64)
    } else if has_seconds {
        let total = (value.fract() * 86_400.0).round() as i64;
        if total == 86_400 {
            (value.trunc() + 1.0, 0)
        } else {
            (value, total)
        }
    } else {
        let minutes = (value.fract() * 1_440.0).round() as i64;
        if minutes == 1_440 {
            (value.trunc() + 1.0, 0)
        } else {
            (value, minutes * 60)
        }
    };

    let parts = try_serial_to_display_date_parts_for(system, display_serial)
        .map_err(|_| Fallback::Invalid)?;

    let hour24 = total_seconds / 3_600;
    let minute = (total_seconds / 60) % 60;
    let second = total_seconds % 60;
    let hour_display = if has_ampm {
        match hour24 % 12 {
            0 => 12,
            other => other,
        }
    } else {
        hour24
    };
    let weekday = (((display_serial.trunc() as i64) % 7) + 7) % 7;
    let month_index = (parts.month.clamp(1, 12) - 1) as usize;

    let mut out = String::with_capacity(tokens.len() * 2);
    for token in tokens {
        match *token {
            Token::Literal(c) => out.push(c),
            Token::Month(1) => out.push_str(&parts.month.to_string()),
            Token::Month(2) => out.push_str(&format!("{:02}", parts.month)),
            Token::Month(3) => out.push_str(MONTHS_ABBREV[month_index]),
            Token::Month(4) => out.push_str(MONTHS_FULL[month_index]),
            Token::Month(_) => out.push(
                MONTHS_FULL[month_index]
                    .chars()
                    .next()
                    .expect("month name is never empty"),
            ),
            Token::Day(1) => out.push_str(&parts.day.to_string()),
            Token::Day(2) => out.push_str(&format!("{:02}", parts.day)),
            Token::Day(3) => out.push_str(DAYS_ABBREV[weekday as usize]),
            Token::Day(_) => out.push_str(DAYS_FULL[weekday as usize]),
            Token::Year(2) => out.push_str(&format!("{:02}", parts.year.rem_euclid(100))),
            Token::Year(_) => out.push_str(&format!("{:04}", parts.year)),
            Token::Hour(1) => out.push_str(&hour_display.to_string()),
            Token::Hour(_) => out.push_str(&format!("{hour_display:02}")),
            Token::Minute(1) => out.push_str(&minute.to_string()),
            Token::Minute(_) => out.push_str(&format!("{minute:02}")),
            Token::Second(1) => out.push_str(&second.to_string()),
            Token::Second(_) => out.push_str(&format!("{second:02}")),
            Token::ElapsedHours => {
                // Elapsed hours are the whole serial in hours, unaffected by
                // any AM/PM marker.
                out.push_str(&format!("{}", (value * 24.0).floor() as i64));
            }
            Token::AmPm => out.push_str(if hour24 < 12 { "AM" } else { "PM" }),
            Token::MonthOrMinute(_) => unreachable!("resolved during tokenization"),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::DateSystem;

    fn render_1900(code: &str, serial: f64) -> Result<String, Fallback> {
        format_date(DateSystem::Excel1900, serial, code)
    }

    /// GOD-227 oracle group `a_corpus`, receipt
    /// `artifacts/private/god227/probe/excel-text-date-probe.json`
    /// (Excel for Mac 16.105.3, `en_US`, 2026-09-02): every date format string
    /// that appears in the graded corpus, in its corpus spelling, across six
    /// serials including the 1900 phantom leap day.
    #[test]
    fn god227_a_corpus_formats_by_serial() {
        let cases = [
            ("MM/DD/YYYY", 46247.0, "08/13/2026"),
            ("MM/DD/YYYY", 46086.0, "03/05/2026"),
            ("MM/DD/YYYY", 46351.0, "11/25/2026"),
            ("MM/DD/YYYY", 45351.0, "02/29/2024"),
            ("MM/DD/YYYY", 59.0, "02/28/1900"),
            ("MM/DD/YYYY", 60.0, "02/29/1900"),
            ("M/D/YYYY", 46247.0, "8/13/2026"),
            ("M/D/YYYY", 46086.0, "3/5/2026"),
            ("M/D/YYYY", 46351.0, "11/25/2026"),
            ("M/D/YYYY", 45351.0, "2/29/2024"),
            ("M/D/YYYY", 59.0, "2/28/1900"),
            ("M/D/YYYY", 60.0, "2/29/1900"),
            ("m/dd/yyyy", 46247.0, "8/13/2026"),
            ("m/dd/yyyy", 46086.0, "3/05/2026"),
            ("m/dd/yyyy", 46351.0, "11/25/2026"),
            ("m/dd/yyyy", 45351.0, "2/29/2024"),
            ("m/dd/yyyy", 59.0, "2/28/1900"),
            ("m/dd/yyyy", 60.0, "2/29/1900"),
            ("mm/dd/yyyy", 46247.0, "08/13/2026"),
            ("mm/dd/yyyy", 46086.0, "03/05/2026"),
            ("mm/dd/yyyy", 46351.0, "11/25/2026"),
            ("mm/dd/yyyy", 45351.0, "02/29/2024"),
            ("mm/dd/yyyy", 59.0, "02/28/1900"),
            ("mm/dd/yyyy", 60.0, "02/29/1900"),
            ("MMMM DD, YYYY", 46247.0, "August 13, 2026"),
            ("MMMM DD, YYYY", 46086.0, "March 05, 2026"),
            ("MMMM DD, YYYY", 46351.0, "November 25, 2026"),
            ("MMMM DD, YYYY", 45351.0, "February 29, 2024"),
            ("MMMM DD, YYYY", 59.0, "February 28, 1900"),
            ("MMMM DD, YYYY", 60.0, "February 29, 1900"),
            ("mm/dd/yy", 46247.0, "08/13/26"),
            ("mm/dd/yy", 46086.0, "03/05/26"),
            ("mm/dd/yy", 46351.0, "11/25/26"),
            ("mm/dd/yy", 45351.0, "02/29/24"),
            ("mm/dd/yy", 59.0, "02/28/00"),
            ("mm/dd/yy", 60.0, "02/29/00"),
        ];
        for (code, serial, expected) in cases {
            assert_eq!(
                render_1900(code, serial),
                Ok(expected.into()),
                "{code}@{serial}"
            );
        }
    }

    /// GOD-227 oracle group `b_grammar`, same receipt (Excel for Mac 16.105.3,
    /// `en_US`, 2026-09-02): one token per row at serial 46247.5
    /// (2026-08-13 12:00, a Thursday), plus the composite codes that
    /// discriminate month-versus-minute adjacency and elapsed hours.
    #[test]
    fn god227_b_grammar_tokens() {
        let cases = [
            ("m", 46247.5, "8"),
            ("mm", 46247.5, "08"),
            ("mmm", 46247.5, "Aug"),
            ("mmmm", 46247.5, "August"),
            ("mmmmm", 46247.5, "A"),
            ("d", 46247.5, "13"),
            ("dd", 46247.5, "13"),
            ("ddd", 46247.5, "Thu"),
            ("dddd", 46247.5, "Thursday"),
            ("yy", 46247.5, "26"),
            ("yyyy", 46247.5, "2026"),
            ("h:mm", 46247.5, "12:00"),
            ("hh:mm:ss", 46247.5, "12:00:00"),
            ("h:mm AM/PM", 46247.5, "12:00 PM"),
            ("h:mm am/pm", 46247.5, "12:00 PM"),
            ("m/d/yyyy h:mm", 46247.5, "8/13/2026 12:00"),
            ("yyyy-mm-dd", 46247.5, "2026-08-13"),
            ("dd-mmm-yyyy", 46247.5, "13-Aug-2026"),
            ("[h]:mm", 46247.5, "1109940:00"),
            ("mmm d", 46247.5, "Aug 13"),
            ("d mmmm yyyy", 46247.5, "13 August 2026"),
        ];
        for (code, serial, expected) in cases {
            assert_eq!(
                render_1900(code, serial),
                Ok(expected.into()),
                "{code}@{serial}"
            );
        }
    }

    /// GOD-227 oracle group `c_case`, same receipt (Excel for Mac 16.105.3,
    /// `en_US`, 2026-09-02): OOXML preserves the author's token case and Excel
    /// ignores it. The pre-fix renderer echoed these format strings verbatim.
    #[test]
    fn god227_c_case_insensitivity() {
        let cases = [
            ("Mm/Dd/Yyyy", 46247.0, "08/13/2026"),
            ("MMM D", 46247.0, "Aug 13"),
        ];
        for (code, serial, expected) in cases {
            assert_eq!(
                render_1900(code, serial),
                Ok(expected.into()),
                "{code}@{serial}"
            );
        }
    }

    /// GOD-227 oracle row `b_ot076_A23`, same receipt: `TEXT(TIME(13,5,0),
    /// "h:mm AM/PM")` is `1:05 PM`. `TIME(13,5,0)` is serial 13/24 + 5/1440.
    #[test]
    fn god227_b_ot076_residual_am_pm() {
        let serial = 13.0 / 24.0 + 5.0 / 1_440.0;
        assert_eq!(render_1900("h:mm AM/PM", serial), Ok("1:05 PM".into()));
    }

    /// Everything the receipt does not measure stays outside the surface so
    /// `TEXT` falls back to its legacy rendering unchanged.
    #[test]
    fn god227_unmeasured_syntax_is_unsupported() {
        for code in [
            r#""Q"yyyy"#,
            r"\myyyy",
            "_(mm/dd/yyyy_)",
            "* mm/dd/yyyy",
            "mm/dd/yyyy;@",
            "# ?/?",
            "0.00",
            "@",
            "[m]:ss",
            "[s]",
            "[$-409]mm/dd/yyyy",
            "[Red]mm/dd/yyyy",
            "General",
            "",
            "yyy",
            "mmmmmm",
            "hhh",
        ] {
            assert_eq!(
                render_1900(code, 46247.0),
                Err(Fallback::Unsupported),
                "{code}"
            );
        }
    }

    /// Serials Excel cannot display are the measured `#VALUE!` wall; the
    /// legacy renderer produced the same error from the same call.
    #[test]
    fn god227_out_of_range_serials_are_value_errors() {
        for serial in [-1.0_f64, 2_958_466.0, 1.0e20, f64::INFINITY] {
            assert_eq!(
                render_1900("yyyy-mm-dd", serial),
                Err(Fallback::Invalid),
                "{serial}"
            );
        }
    }
}
