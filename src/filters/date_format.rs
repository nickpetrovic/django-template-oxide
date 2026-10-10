use std::collections::HashMap;
use std::fmt::Write;
use std::sync::{Arc, Mutex, OnceLock};

use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDate, PyDateAccess, PyDateTime, PyString, PyTimeAccess};

const FORMAT_CHARS: &str = "aAbcdDeEfFgGhHiIjlLmMnNoOPrsStTUuwWyYzZ";
const TIME_FORMAT_CHARS: &str = "aAefgGhHiOPsTuZ";
const RUST_FORMAT_CHARS: &str = "aAbdDEfFgGhHijlLmMnNPsStuwyYz";
const NAMED_FORMAT_CHARS: &str = "aAbDEFlMNP";

enum Piece {
    Literal(String),
    Format(char),
}

fn parse_format(format_str: &str) -> Vec<Piece> {
    let chars: Vec<char> = format_str.chars().collect();
    let mut pieces = Vec::new();
    let mut literal: Vec<char> = Vec::new();
    for (index, &c) in chars.iter().enumerate() {
        let escaped = index > 0 && chars[index - 1] == '\\';
        if FORMAT_CHARS.contains(c) && !escaped {
            if !literal.is_empty() {
                pieces.push(Piece::Literal(unescape(&literal)));
                literal.clear();
            }
            pieces.push(Piece::Format(c));
        } else {
            literal.push(c);
        }
    }
    if !literal.is_empty() {
        pieces.push(Piece::Literal(unescape(&literal)));
    }
    pieces
}

fn unescape(chars: &[char]) -> String {
    let mut out = String::with_capacity(chars.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '\\' && index + 1 < chars.len() && chars[index + 1] != '\n' {
            out.push(chars[index + 1]);
            index += 2;
        } else {
            out.push(chars[index]);
            index += 1;
        }
    }
    out
}

struct LocaleNames {
    months: Vec<String>,
    months_3: Vec<String>,
    months_3_title: Vec<String>,
    months_ap: Vec<String>,
    months_alt: Vec<String>,
    weekdays: Vec<String>,
    weekdays_abbr: Vec<String>,
    am: String,
    pm: String,
    am_upper: String,
    pm_upper: String,
    midnight: String,
    noon: String,
}

#[derive(Default)]
struct RenderLocale {
    epoch: u64,
    activation: Option<Py<PyAny>>,
    language: Option<Py<PyAny>>,
    names: Option<Arc<LocaleNames>>,
    formats: HashMap<String, Option<String>>,
    decimal_separator: Option<Option<Arc<str>>>,
}

thread_local! {
    static LOCALE_EPOCH: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
    static RENDER_LOCALE: std::cell::RefCell<RenderLocale> =
        std::cell::RefCell::new(RenderLocale::default());
}

pub fn invalidate_locale_snapshot() {
    LOCALE_EPOCH.with(|epoch| epoch.set(epoch.get().wrapping_add(1)));
}

pub fn reset_locale_snapshot() {
    RENDER_LOCALE.with(|cell| *cell.borrow_mut() = RenderLocale::default());
}

fn activation_variable<'py>(active: &Bound<'py, PyAny>) -> Option<Bound<'py, PyAny>> {
    let py = active.py();
    let variable = active
        .getattr(pyo3::intern!(py, "_storage"))
        .and_then(|storage| storage.getattr(pyo3::intern!(py, "_data")))
        .ok()?;
    let context_var_type = py.import("contextvars").ok()?.getattr("ContextVar").ok()?;
    variable
        .is_exact_instance(&context_var_type)
        .then_some(variable)
}

type CachedActivationVariable = Option<(Py<PyAny>, Option<Py<PyAny>>)>;

fn module_global_ptr(module: &Bound<'_, PyAny>, name: &Bound<'_, PyString>) -> *mut ffi::PyObject {
    let found = unsafe {
        let dict = ffi::PyModule_GetDict(module.as_ptr());
        if dict.is_null() {
            std::ptr::null_mut()
        } else {
            ffi::PyDict_GetItemWithError(dict, name.as_ptr())
        }
    };
    if found.is_null() {
        let _ = PyErr::take(module.py());
    }
    found
}

fn context_var_value<'py>(py: Python<'py>, variable: &Py<PyAny>) -> Option<Bound<'py, PyAny>> {
    let mut value: *mut ffi::PyObject = std::ptr::null_mut();
    let status =
        unsafe { ffi::PyContextVar_Get(variable.as_ptr(), std::ptr::null_mut(), &mut value) };
    if status < 0 {
        let _ = PyErr::take(py);
        return None;
    }
    if value.is_null() {
        return Some(py.None().into_bound(py));
    }
    Some(unsafe { Bound::from_owned_ptr(py, value) })
}

fn current_activation(py: Python<'_>) -> Option<Bound<'_, PyAny>> {
    thread_local! {
        static ACTIVATION_VARIABLE: std::cell::RefCell<CachedActivationVariable> =
            const { std::cell::RefCell::new(None) };
    }
    let trans_real = crate::python_cache::django(py).ok()?.trans_real.bind(py);
    let active_ptr = module_global_ptr(trans_real, pyo3::intern!(py, "_active"));
    let cached = ACTIVATION_VARIABLE.with(|cell| {
        let cell = cell.borrow();
        let (cached_active, variable) = cell.as_ref()?;
        (!active_ptr.is_null() && std::ptr::eq(cached_active.as_ptr(), active_ptr))
            .then(|| variable.as_ref().and_then(|v| context_var_value(py, v)))
    });
    if let Some(value) = cached {
        return value;
    }
    let active = trans_real.getattr(pyo3::intern!(py, "_active")).ok()?;
    let variable = activation_variable(&active).map(Bound::unbind);
    let value = variable.as_ref().and_then(|v| context_var_value(py, v));
    ACTIVATION_VARIABLE.with(|cell| *cell.borrow_mut() = Some((active.unbind(), variable)));
    value
}

fn revalidate_render_locale(py: Python<'_>) -> PyResult<()> {
    let epoch = LOCALE_EPOCH.with(std::cell::Cell::get);
    if RENDER_LOCALE.with(|cell| cell.borrow().epoch == epoch) {
        return Ok(());
    }
    let activation = current_activation(py);
    let same_activation = RENDER_LOCALE.with(|cell| {
        let snapshot = cell.borrow();
        snapshot.language.is_some()
            && match (&snapshot.activation, &activation) {
                (Some(stored), Some(current)) => std::ptr::eq(stored.as_ptr(), current.as_ptr()),
                _ => false,
            }
    });
    if same_activation {
        RENDER_LOCALE.with(|cell| cell.borrow_mut().epoch = epoch);
        return Ok(());
    }
    let stored_language =
        RENDER_LOCALE.with(|cell| cell.borrow().language.as_ref().map(|l| l.clone_ref(py)));
    let language = crate::python_cache::django(py)?
        .get_language
        .bind(py)
        .call0()?;
    let same_language = match &stored_language {
        Some(stored) => {
            let stored = stored.bind(py);
            stored.is(&language) || stored.eq(&language)?
        }
        None => false,
    };
    let activation = activation.map(Bound::unbind);
    RENDER_LOCALE.with(|cell| {
        let mut snapshot = cell.borrow_mut();
        if same_language {
            snapshot.epoch = epoch;
            snapshot.activation = activation;
        } else {
            *snapshot = RenderLocale {
                epoch,
                activation,
                language: Some(language.unbind()),
                ..RenderLocale::default()
            };
        }
    });
    Ok(())
}

fn current_render_locale<R>(py: Python<'_>, read: impl FnOnce(&RenderLocale) -> R) -> PyResult<R> {
    revalidate_render_locale(py)?;
    Ok(RENDER_LOCALE.with(|cell| read(&cell.borrow())))
}

fn update_render_locale(update: impl FnOnce(&mut RenderLocale)) {
    let epoch = LOCALE_EPOCH.with(std::cell::Cell::get);
    RENDER_LOCALE.with(|cell| {
        let mut snapshot = cell.borrow_mut();
        if snapshot.epoch == epoch {
            update(&mut snapshot);
        }
    });
}

pub fn decimal_separator(py: Python<'_>) -> PyResult<Option<Arc<str>>> {
    if let Some(separator) =
        current_render_locale(py, |snapshot| snapshot.decimal_separator.clone())?
    {
        return Ok(separator);
    }
    let format = crate::python_cache::django(py)?
        .get_format
        .bind(py)
        .call1((pyo3::intern!(py, "DECIMAL_SEPARATOR"),))?;
    let separator = match format.cast_exact::<pyo3::types::PyString>() {
        Ok(text) => Some(Arc::<str>::from(text.to_cow()?.as_ref())),
        Err(_) => None,
    };
    update_render_locale(|snapshot| snapshot.decimal_separator = Some(separator.clone()));
    Ok(separator)
}

struct LocaleCache {
    translations: Option<Py<PyAny>>,
    by_language: HashMap<String, Arc<LocaleNames>>,
}

fn locale_names(py: Python<'_>) -> PyResult<Arc<LocaleNames>> {
    if let Some(names) = current_render_locale(py, |snapshot| snapshot.names.clone())? {
        return Ok(names);
    }
    let language =
        RENDER_LOCALE.with(|cell| cell.borrow().language.as_ref().map(|l| l.clone_ref(py)));
    let names = shared_locale_names(py, language)?;
    update_render_locale(|snapshot| snapshot.names = Some(Arc::clone(&names)));
    Ok(names)
}

fn shared_locale_names(py: Python<'_>, language: Option<Py<PyAny>>) -> PyResult<Arc<LocaleNames>> {
    static CACHE: OnceLock<Mutex<LocaleCache>> = OnceLock::new();
    let dj = crate::python_cache::django(py)?;
    let language = match language {
        Some(language) => language.into_bound(py),
        None => dj.get_language.bind(py).call0()?,
    };
    let key = if language.is_none() {
        String::new()
    } else {
        language.str()?.to_string()
    };
    let translations = dj
        .trans_real
        .bind(py)
        .getattr(pyo3::intern!(py, "_translations"))
        .ok();
    let cache = CACHE.get_or_init(|| {
        Mutex::new(LocaleCache {
            translations: None,
            by_language: HashMap::new(),
        })
    });
    let cached = cache.lock().ok().and_then(|mut guard| {
        let current = translations.as_ref().map(|t| t.as_ptr());
        let stored = guard.translations.as_ref().map(|t| t.as_ptr());
        if current != stored {
            guard.by_language.clear();
            guard.translations = translations.as_ref().map(|t| t.clone().unbind());
        }
        guard.by_language.get(&key).cloned()
    });
    if let Some(names) = cached {
        return Ok(names);
    }
    let names = build_locale_names(py, dj)?;
    if let Ok(mut guard) = cache.lock() {
        guard.by_language.insert(key, Arc::clone(&names));
    }
    Ok(names)
}

fn is_python_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn resolve_format(py: Python<'_>, format_str: &str) -> Option<String> {
    if !is_python_identifier(format_str) {
        return Some(format_str.to_owned());
    }
    if let Some(resolved) =
        current_render_locale(py, |snapshot| snapshot.formats.get(format_str).cloned()).ok()?
    {
        return resolved;
    }
    let resolved = crate::python_cache::django(py)
        .ok()
        .and_then(|dj| dj.get_format.bind(py).call1((format_str,)).ok())
        .and_then(|format| {
            format
                .cast::<pyo3::types::PyString>()
                .ok()
                .map(|s| s.to_string())
        });
    update_render_locale(|snapshot| {
        snapshot
            .formats
            .insert(format_str.to_owned(), resolved.clone());
    });
    resolved
}

fn build_locale_names(
    py: Python<'_>,
    dj: &crate::python_cache::DjangoModules,
) -> PyResult<Arc<LocaleNames>> {
    let dates = dj.dates.bind(py);
    let table = |name: &str, keys: std::ops::RangeInclusive<i64>| -> PyResult<Vec<String>> {
        let mapping = dates.getattr(name)?;
        keys.map(|k| Ok(mapping.get_item(k)?.str()?.to_string()))
            .collect()
    };
    let titled_table = |name: &str| -> PyResult<Vec<String>> {
        let mapping = dates.getattr(name)?;
        (1..=12)
            .map(|k| {
                Ok(mapping
                    .get_item(k)?
                    .str()?
                    .call_method0("title")?
                    .str()?
                    .to_string())
            })
            .collect()
    };
    let gettext = |msgid: &str| -> PyResult<String> {
        Ok(dj.gettext.bind(py).call1((msgid,))?.str()?.to_string())
    };
    Ok(Arc::new(LocaleNames {
        months: table("MONTHS", 1..=12)?,
        months_3: table("MONTHS_3", 1..=12)?,
        months_3_title: titled_table("MONTHS_3")?,
        months_ap: table("MONTHS_AP", 1..=12)?,
        months_alt: table("MONTHS_ALT", 1..=12)?,
        weekdays: table("WEEKDAYS", 0..=6)?,
        weekdays_abbr: table("WEEKDAYS_ABBR", 0..=6)?,
        am: gettext("a.m.")?,
        pm: gettext("p.m.")?,
        am_upper: gettext("AM")?,
        pm_upper: gettext("PM")?,
        midnight: gettext("midnight")?,
        noon: gettext("noon")?,
    }))
}

struct Components {
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    microsecond: u32,
}

fn components(value: &Bound<'_, PyAny>, is_datetime: bool) -> PyResult<Components> {
    if is_datetime {
        let dt = value.cast_exact::<PyDateTime>()?;
        return Ok(Components {
            year: dt.get_year(),
            month: u32::from(dt.get_month()),
            day: u32::from(dt.get_day()),
            hour: u32::from(dt.get_hour()),
            minute: u32::from(dt.get_minute()),
            second: u32::from(dt.get_second()),
            microsecond: dt.get_microsecond(),
        });
    }
    let date = value.cast_exact::<PyDate>()?;
    Ok(Components {
        year: date.get_year(),
        month: u32::from(date.get_month()),
        day: u32::from(date.get_day()),
        hour: 0,
        minute: 0,
        second: 0,
        microsecond: 0,
    })
}

pub fn try_format(py: Python<'_>, value: &Bound<'_, PyAny>, format_str: &str) -> Option<String> {
    let is_datetime = value.is_exact_instance_of::<PyDateTime>();
    let is_date = !is_datetime && value.is_exact_instance_of::<PyDate>();
    if !is_datetime && !is_date {
        return None;
    }
    let format_str = resolve_format(py, format_str)?;
    let format_str = format_str.as_str();
    let pieces = parse_format(format_str);
    for piece in &pieces {
        if let Piece::Format(c) = piece {
            if !RUST_FORMAT_CHARS.contains(*c) {
                return None;
            }
            if is_date && TIME_FORMAT_CHARS.contains(*c) {
                return None;
            }
        }
    }
    let parts = components(value, is_datetime).ok()?;
    let needs_names = pieces
        .iter()
        .any(|piece| matches!(piece, Piece::Format(c) if NAMED_FORMAT_CHARS.contains(*c)));
    let names = if needs_names {
        Some(locale_names(py).ok()?)
    } else {
        None
    };
    let names = names.as_deref();
    let weekday_monday0 = (weekday_from_ymd(parts.year, parts.month, parts.day) + 6) % 7;
    let hour_12 = if parts.hour % 12 == 0 {
        12
    } else {
        parts.hour % 12
    };
    let month_index = (parts.month - 1) as usize;
    let is_pm = parts.hour > 11;
    fn am_pm(names: &LocaleNames, is_pm: bool) -> &str {
        if is_pm { &names.pm } else { &names.am }
    }
    let twelve_hour_minutes = || {
        if parts.minute == 0 {
            hour_12.to_string()
        } else {
            format!("{}:{:02}", hour_12, parts.minute)
        }
    };

    let mut out = String::with_capacity(format_str.len() + 16);
    for piece in pieces {
        match piece {
            Piece::Literal(text) => out.push_str(&text),
            Piece::Format(c) => match c {
                'a' => out.push_str(am_pm(names?, is_pm)),
                'A' => out.push_str(if parts.hour > 11 {
                    &names?.pm_upper
                } else {
                    &names?.am_upper
                }),
                'b' => out.push_str(&names?.months_3[month_index]),
                'd' => write!(out, "{:02}", parts.day).ok()?,
                'D' => out.push_str(&names?.weekdays_abbr[weekday_monday0 as usize]),
                'E' => out.push_str(&names?.months_alt[month_index]),
                'f' => out.push_str(&twelve_hour_minutes()),
                'F' => out.push_str(&names?.months[month_index]),
                'g' => write!(out, "{hour_12}").ok()?,
                'G' => write!(out, "{}", parts.hour).ok()?,
                'h' => write!(out, "{hour_12:02}").ok()?,
                'H' => write!(out, "{:02}", parts.hour).ok()?,
                'i' => write!(out, "{:02}", parts.minute).ok()?,
                'j' => write!(out, "{}", parts.day).ok()?,
                'l' => out.push_str(&names?.weekdays[weekday_monday0 as usize]),
                'L' => out.push_str(if is_leap_year(parts.year) {
                    "True"
                } else {
                    "False"
                }),
                'm' => write!(out, "{:02}", parts.month).ok()?,
                'M' => out.push_str(&names?.months_3_title[month_index]),
                'n' => write!(out, "{}", parts.month).ok()?,
                'N' => out.push_str(&names?.months_ap[month_index]),
                'P' => {
                    let names = names?;
                    if parts.minute == 0 && parts.hour == 0 {
                        out.push_str(&names.midnight);
                    } else if parts.minute == 0 && parts.hour == 12 {
                        out.push_str(&names.noon);
                    } else {
                        write!(out, "{} {}", twelve_hour_minutes(), am_pm(names, is_pm)).ok()?;
                    }
                }
                's' => write!(out, "{:02}", parts.second).ok()?,
                'S' => out.push_str(ordinal_suffix(parts.day)),
                't' => write!(out, "{}", days_in_month(parts.year, parts.month)).ok()?,
                'u' => write!(out, "{:06}", parts.microsecond).ok()?,
                'w' => write!(
                    out,
                    "{}",
                    weekday_from_ymd(parts.year, parts.month, parts.day)
                )
                .ok()?,
                'y' => write!(out, "{:02}", parts.year.rem_euclid(100)).ok()?,
                'Y' => write!(out, "{:04}", parts.year).ok()?,
                'z' => write!(out, "{}", day_of_year(parts.year, parts.month, parts.day)).ok()?,
                _ => return None,
            },
        }
    }
    Some(out)
}

#[inline]
fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

#[inline]
fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

fn day_of_year(year: i32, month: u32, day: u32) -> u32 {
    (1..month).map(|m| days_in_month(year, m)).sum::<u32>() + day
}

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
fn ordinal_suffix(day: u32) -> &'static str {
    if (11..=13).contains(&day) {
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

    fn render(format_str: &str) -> Vec<String> {
        parse_format(format_str)
            .into_iter()
            .map(|piece| match piece {
                Piece::Literal(text) => format!("L:{text}"),
                Piece::Format(c) => format!("F:{c}"),
            })
            .collect()
    }

    #[test]
    fn leap_years() {
        assert!(is_leap_year(2000));
        assert!(is_leap_year(2024));
        assert!(!is_leap_year(2100));
        assert!(!is_leap_year(2023));
    }

    #[test]
    fn weekday_known_dates() {
        assert_eq!(weekday_from_ymd(2026, 5, 26), 2);
        assert_eq!(weekday_from_ymd(2000, 1, 1), 6);
        assert_eq!(weekday_from_ymd(2024, 2, 29), 4);
    }

    #[test]
    fn day_of_year_counts_from_one() {
        assert_eq!(day_of_year(2024, 1, 1), 1);
        assert_eq!(day_of_year(2024, 3, 1), 61);
        assert_eq!(day_of_year(2023, 3, 1), 60);
    }

    #[test]
    fn ordinal_suffixes() {
        assert_eq!(ordinal_suffix(1), "st");
        assert_eq!(ordinal_suffix(11), "th");
        assert_eq!(ordinal_suffix(22), "nd");
        assert_eq!(ordinal_suffix(23), "rd");
    }

    #[test]
    fn escaped_format_chars_are_literal() {
        assert_eq!(render("\\Y Y"), vec!["L:Y ", "F:Y"]);
        assert_eq!(render("\\\\d"), vec!["L:\\d"]);
        assert_eq!(render("a\\"), vec!["F:a", "L:\\"]);
    }
}
