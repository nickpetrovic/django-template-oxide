//! Rust fast path for Django's `dateformat`. Common 90% of `|date`
//! invocations; unknown chars return `None` so the caller falls back
//! to `django.utils.dateformat.format`.
//!
//! Not supported (delegated to Django): `c`/`r`/`U` composites,
//! `O`/`T`/`Z`/`e` timezone, `o`/`W` ISO week, localised names.

use pyo3::prelude::*;
use pyo3::types::PyAny;
use std::fmt::Write;

/// `Some(rendered)` when every format char is supported; `None`
/// otherwise. Component getattrs are lazy (only referenced chars).
pub fn try_format(py: Python<'_>, dt: &Bound<'_, PyAny>, format_str: &str) -> Option<String> {
    if !is_supported(format_str) {
        return None;
    }

    let mut cache = ComponentCache::new(dt);

    let mut out = String::with_capacity(format_str.len() + 16);
    let mut chars = format_str.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            // Backslash: emit the next char literally.
            '\\' => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            'd' => write!(out, "{:02}", cache.day(py)?).ok()?,
            'j' => write!(out, "{}", cache.day(py)?).ok()?,
            'D' => out.push_str(DAY_NAMES_SHORT[cache.weekday_0sun(py)? as usize]),
            'l' => out.push_str(DAY_NAMES_LONG[cache.weekday_0sun(py)? as usize]),
            'N' => out.push_str(MONTH_AP_STYLE[(cache.month(py)? - 1) as usize]),
            'S' => out.push_str(ordinal_suffix(cache.day(py)?)),
            'w' => write!(out, "{}", cache.weekday_0sun(py)?).ok()?,
            'z' => write!(out, "{}", cache.day_of_year(py)?).ok()?,

            'm' => write!(out, "{:02}", cache.month(py)?).ok()?,
            'n' => write!(out, "{}", cache.month(py)?).ok()?,
            'M' => out.push_str(MONTHS_SHORT[(cache.month(py)? - 1) as usize]),
            'F' => out.push_str(MONTHS_LONG[(cache.month(py)? - 1) as usize]),
            't' => write!(out, "{}", days_in_month(cache.year(py)?, cache.month(py)?)).ok()?,
            'L' => out.push_str(if is_leap_year(cache.year(py)?) { "True" } else { "False" }),

            'Y' => write!(out, "{}", cache.year(py)?).ok()?,
            'y' => write!(out, "{:02}", cache.year(py)?.rem_euclid(100)).ok()?,

            'H' => write!(out, "{:02}", cache.hour(py)?).ok()?,
            'G' => write!(out, "{}", cache.hour(py)?).ok()?,
            'h' => write!(out, "{:02}", hour_12(cache.hour(py)?)).ok()?,
            'g' => write!(out, "{}", hour_12(cache.hour(py)?)).ok()?,
            'i' => write!(out, "{:02}", cache.minute(py)?).ok()?,
            's' => write!(out, "{:02}", cache.second(py)?).ok()?,
            'u' => write!(out, "{:06}", cache.microsecond(py)?).ok()?,

            'a' => out.push_str(if cache.hour(py)? < 12 { "a.m." } else { "p.m." }),
            'A' => out.push_str(if cache.hour(py)? < 12 { "AM" } else { "PM" }),
            'P' => {
                // 12-hour with "a.m./p.m." and noon/midnight specials.
                let h = cache.hour(py)?;
                let m = cache.minute(py)?;
                if h == 0 && m == 0 {
                    out.push_str("midnight");
                } else if h == 12 && m == 0 {
                    out.push_str("noon");
                } else {
                    let h12 = hour_12(h);
                    if m == 0 {
                        write!(out, "{} {}", h12, if h < 12 { "a.m." } else { "p.m." }).ok()?;
                    } else {
                        write!(out, "{}:{:02} {}", h12, m, if h < 12 { "a.m." } else { "p.m." }).ok()?;
                    }
                }
            }

            // Literal (already filtered by is_supported).
            _ => out.push(c),
        }
    }

    Some(out)
}

fn is_supported(format_str: &str) -> bool {
    let mut chars = format_str.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            chars.next();
            continue;
        }
        if is_format_char(c) || is_literal_char(c) {
            continue;
        }
        return false;
    }
    true
}

/// Format chars we implement.
#[inline]
fn is_format_char(c: char) -> bool {
    matches!(
        c,
        'd' | 'j' | 'D' | 'l' | 'N' | 'S' | 'w' | 'z'
            | 'm' | 'n' | 'M' | 'F' | 't' | 'L'
            | 'Y' | 'y'
            | 'H' | 'G' | 'h' | 'g' | 'i' | 's' | 'u'
            | 'a' | 'A' | 'P'
    )
}

/// Chars that pass through as literals (no format meaning in Django's
/// spec). Includes punctuation, whitespace, and any non-format-char ASCII
/// letter that Django would also emit as-is.
///
/// Django's dateformat actually treats *any* unknown char as a literal.
/// We're more conservative - non-ASCII letters pass through, but we
/// refuse format strings containing Django-format-meaningful chars we
/// don't yet implement (`c`, `r`, `U`, `O`, `T`, `Z`, `e`, `o`, `W`) so
/// the caller falls back to Django's Python impl for correctness.
#[inline]
fn is_literal_char(c: char) -> bool {
    !matches!(
        c,
        // Format chars we don't yet implement - keep the format-string
        // scanner conservative.
        'c' | 'r' | 'U' | 'O' | 'T' | 'Z' | 'e' | 'o' | 'W'
    )
}

/// Lazy date/time components. One `getattr` per referenced component.
struct ComponentCache<'a, 'py> {
    dt: &'a Bound<'py, PyAny>,
    year: Option<i32>,
    month: Option<u32>,
    day: Option<u32>,
    hour: Option<u32>,
    minute: Option<u32>,
    second: Option<u32>,
    microsecond: Option<u32>,
    weekday_0sun: Option<u32>,
    day_of_year: Option<u32>,
}

impl<'a, 'py> ComponentCache<'a, 'py> {
    fn new(dt: &'a Bound<'py, PyAny>) -> Self {
        Self {
            dt,
            year: None,
            month: None,
            day: None,
            hour: None,
            minute: None,
            second: None,
            microsecond: None,
            weekday_0sun: None,
            day_of_year: None,
        }
    }

    fn year(&mut self, _py: Python<'_>) -> Option<i32> {
        if let Some(v) = self.year {
            return Some(v);
        }
        let v: i32 = self.dt.getattr("year").ok()?.extract().ok()?;
        self.year = Some(v);
        Some(v)
    }
    fn month(&mut self, _py: Python<'_>) -> Option<u32> {
        if let Some(v) = self.month {
            return Some(v);
        }
        let v: u32 = self.dt.getattr("month").ok()?.extract().ok()?;
        self.month = Some(v);
        Some(v)
    }
    fn day(&mut self, _py: Python<'_>) -> Option<u32> {
        if let Some(v) = self.day {
            return Some(v);
        }
        let v: u32 = self.dt.getattr("day").ok()?.extract().ok()?;
        self.day = Some(v);
        Some(v)
    }
    // Plain `date` objects lack hour/minute/etc. The None propagates
    // out of `try_format` so the Python fallback raises Django's
    // proper "format for date objects may not contain time-related
    // format specifiers" error.
    fn hour(&mut self, _py: Python<'_>) -> Option<u32> {
        if let Some(v) = self.hour {
            return Some(v);
        }
        let v: u32 = self.dt.getattr("hour").ok()?.extract().ok()?;
        self.hour = Some(v);
        Some(v)
    }
    fn minute(&mut self, _py: Python<'_>) -> Option<u32> {
        if let Some(v) = self.minute {
            return Some(v);
        }
        let v: u32 = self.dt.getattr("minute").ok()?.extract().ok()?;
        self.minute = Some(v);
        Some(v)
    }
    fn second(&mut self, _py: Python<'_>) -> Option<u32> {
        if let Some(v) = self.second {
            return Some(v);
        }
        let v: u32 = self.dt.getattr("second").ok()?.extract().ok()?;
        self.second = Some(v);
        Some(v)
    }
    fn microsecond(&mut self, _py: Python<'_>) -> Option<u32> {
        if let Some(v) = self.microsecond {
            return Some(v);
        }
        let v: u32 = self.dt.getattr("microsecond").ok()?.extract().ok()?;
        self.microsecond = Some(v);
        Some(v)
    }
    /// 0=Sunday..6=Saturday (Django's `w` convention).
    fn weekday_0sun(&mut self, py: Python<'_>) -> Option<u32> {
        if let Some(v) = self.weekday_0sun {
            return Some(v);
        }
        let v = weekday_from_ymd(self.year(py)?, self.month(py)?, self.day(py)?);
        self.weekday_0sun = Some(v);
        Some(v)
    }
    fn day_of_year(&mut self, py: Python<'_>) -> Option<u32> {
        if let Some(v) = self.day_of_year {
            return Some(v);
        }
        let v = day_of_year(self.year(py)?, self.month(py)?, self.day(py)?);
        self.day_of_year = Some(v);
        Some(v)
    }
}

// Static tables matching Django's exact strings.

const MONTHS_SHORT: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun",
    "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

const MONTHS_LONG: [&str; 12] = [
    "January", "February", "March", "April", "May", "June",
    "July", "August", "September", "October", "November", "December",
];

/// AP-style abbreviations (Django's `N`).
const MONTH_AP_STYLE: [&str; 12] = [
    "Jan.", "Feb.", "March", "April", "May", "June",
    "July", "Aug.", "Sept.", "Oct.", "Nov.", "Dec.",
];

/// Indexed by `w` (0=Sun..6=Sat).
const DAY_NAMES_SHORT: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const DAY_NAMES_LONG: [&str; 7] = [
    "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday",
];

#[inline]
fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

#[inline]
fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => if is_leap_year(year) { 29 } else { 28 },
        _ => 0,
    }
}

/// 1-indexed day of year (Feb 1 -> 32).
fn day_of_year(year: i32, month: u32, day: u32) -> u32 {
    let mut total: u32 = 0;
    for m in 1..month {
        total += days_in_month(year, m);
    }
    total + day
}

/// 0=Sun..6=Sat via Sakamoto's algorithm.
fn weekday_from_ymd(year: i32, month: u32, day: u32) -> u32 {
    static T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let mut y = year;
    if month < 3 {
        y -= 1;
    }
    let m = month as i32;
    let d = day as i32;
    ((y + y / 4 - y / 100 + y / 400 + T[(m - 1) as usize] + d).rem_euclid(7)) as u32
}

#[inline]
fn hour_12(h: u32) -> u32 {
    match h {
        0 => 12,
        h if h > 12 => h - 12,
        h => h,
    }
}

#[inline]
fn ordinal_suffix(day: u32) -> &'static str {
    if day >= 11 && day <= 13 {
        return "th";
    }
    match day % 10 {
        1 => "st",
        2 => "nd",
        3 => "rd",
        _ => "th",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leap_years() {
        assert!(is_leap_year(2000));
        assert!(is_leap_year(2024));
        assert!(!is_leap_year(2100));
        assert!(!is_leap_year(2023));
    }

    #[test]
    fn days_in_month_basics() {
        assert_eq!(days_in_month(2023, 1), 31);
        assert_eq!(days_in_month(2023, 2), 28);
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2023, 4), 30);
    }

    #[test]
    fn weekday_known_dates() {
        // 2026-05-26 Tue (w=2).
        assert_eq!(weekday_from_ymd(2026, 5, 26), 2);
        // 2000-01-01 Sat (w=6).
        assert_eq!(weekday_from_ymd(2000, 1, 1), 6);
        // 2024-02-29 Thu (w=4).
        assert_eq!(weekday_from_ymd(2024, 2, 29), 4);
    }

    #[test]
    fn ordinal_suffixes() {
        assert_eq!(ordinal_suffix(1), "st");
        assert_eq!(ordinal_suffix(2), "nd");
        assert_eq!(ordinal_suffix(3), "rd");
        assert_eq!(ordinal_suffix(4), "th");
        assert_eq!(ordinal_suffix(11), "th");
        assert_eq!(ordinal_suffix(12), "th");
        assert_eq!(ordinal_suffix(13), "th");
        assert_eq!(ordinal_suffix(21), "st");
        assert_eq!(ordinal_suffix(22), "nd");
        assert_eq!(ordinal_suffix(31), "st");
    }

    #[test]
    fn hour_12_known() {
        assert_eq!(hour_12(0), 12);
        assert_eq!(hour_12(1), 1);
        assert_eq!(hour_12(11), 11);
        assert_eq!(hour_12(12), 12);
        assert_eq!(hour_12(13), 1);
        assert_eq!(hour_12(23), 11);
    }

    #[test]
    fn unsupported_format_chars_bail() {
        assert!(!is_supported("c"));
        assert!(!is_supported("M d, Y r"));
        assert!(!is_supported("U"));
    }

    #[test]
    fn supported_format_chars_pass() {
        assert!(is_supported("M d, Y"));
        assert!(is_supported("H:i:s"));
        assert!(is_supported("D, j N Y"));
        // Backslash escape preceding a non-format char.
        assert!(is_supported("\\Y is Y"));
    }
}
