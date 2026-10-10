use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::OnceLock;

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyTuple};

use crate::context::Value;
use crate::errors::TemplateError;
use crate::utils::html_escape;

mod date_format;

pub(crate) use date_format::{
    decimal_separator, invalidate_locale_snapshot, reset_locale_snapshot,
};

pub type FastFilter = fn(&Value, &[Value], bool) -> Option<Value>;

pub struct NativeFilter {
    pub name: &'static str,
    pub fast: FastFilter,
    pub is_safe: bool,
    pub needs_autoescape: bool,
    pub expects_localtime: bool,
}

impl NativeFilter {
    pub fn call(
        &self,
        py: Python<'_>,
        value: &Value,
        args: &[Value],
        autoescape: bool,
        use_tz: Option<bool>,
    ) -> Result<Value, TemplateError> {
        let input = self.localized_input(py, value, use_tz)?;
        let result = match self.fast_result(&input, args, autoescape) {
            Some(result) => result,
            None => call_builtin_filter(
                py,
                self.name,
                &input,
                args,
                self.needs_autoescape.then_some(autoescape),
            )?,
        };
        self.finish(py, &input, result)
    }

    pub fn try_fast(
        &self,
        py: Python<'_>,
        value: &Value,
        args: &[Value],
        autoescape: bool,
        use_tz: Option<bool>,
    ) -> Option<Value> {
        let input = self.localized_input(py, value, use_tz).ok()?;
        let result = self.fast_result(&input, args, autoescape)?;
        self.finish(py, &input, result).ok()
    }

    fn localized_input<'a>(
        &self,
        py: Python<'_>,
        value: &'a Value,
        use_tz: Option<bool>,
    ) -> Result<Cow<'a, Value>, TemplateError> {
        match value {
            Value::PyObject(obj)
                if self.expects_localtime
                    && obj.bind(py).is_instance_of::<pyo3::types::PyDateTime>() =>
            {
                Ok(Cow::Owned(template_localtime(py, value, use_tz)?))
            }
            _ => Ok(Cow::Borrowed(value)),
        }
    }

    #[inline]
    fn fast_result(&self, input: &Value, args: &[Value], autoescape: bool) -> Option<Value> {
        (self.fast)(input, args, self.needs_autoescape && autoescape)
    }

    #[inline]
    fn finish(&self, py: Python<'_>, input: &Value, result: Value) -> Result<Value, TemplateError> {
        if self.is_safe && is_safe_data(py, input) {
            mark_safe_value(py, result)
        } else {
            Ok(result)
        }
    }
}

pub(crate) fn template_localtime(
    py: Python<'_>,
    value: &Value,
    use_tz: Option<bool>,
) -> Result<Value, TemplateError> {
    let dj = crate::python_cache::django(py)?;
    let converted = dj.template_localtime.bind(py).call1((
        value.to_pyobject(py),
        crate::nodes::optional_bool(py, use_tz),
    ))?;
    Ok(Value::from(&converted))
}

pub fn call_builtin_filter(
    py: Python<'_>,
    name: &str,
    value: &Value,
    args: &[Value],
    autoescape: Option<bool>,
) -> Result<Value, TemplateError> {
    let dj = crate::python_cache::django(py)?;
    let func = dj.builtin_filters.bind(py).get_item(name)?;
    let mut call_args: Vec<Py<PyAny>> = Vec::with_capacity(1 + args.len());
    call_args.push(value.to_pyobject(py));
    call_args.extend(args.iter().map(|arg| arg.to_pyobject(py)));
    let call_args = PyTuple::new(py, call_args)?;
    let result = match autoescape {
        Some(flag) => {
            let kwargs = PyDict::new(py);
            kwargs.set_item(pyo3::intern!(py, "autoescape"), flag)?;
            func.call(call_args, Some(&kwargs))
        }
        None => func.call1(call_args),
    };
    invalidate_locale_snapshot();
    Ok(Value::from(&result?))
}

pub fn is_safe_data(py: Python<'_>, value: &Value) -> bool {
    match value {
        Value::SafeString(_) => true,
        Value::PyObject(obj) => crate::python_cache::django(py)
            .ok()
            .and_then(|dj| obj.bind(py).is_instance(dj.safe_data_cls.bind(py)).ok())
            .unwrap_or(false),
        _ => false,
    }
}

pub fn mark_safe_value(py: Python<'_>, value: Value) -> Result<Value, TemplateError> {
    match value {
        Value::SafeString(_) => Ok(value),
        Value::String(s) => Ok(Value::SafeString(s.into())),
        other => {
            let dj = crate::python_cache::django(py)?;
            let marked = dj.mark_safe.bind(py).call1((other.to_pyobject(py),))?;
            Ok(Value::from(&marked))
        }
    }
}

#[inline]
fn text(value: &Value) -> Option<Cow<'_, str>> {
    value.python_str()
}

#[inline]
fn py_isspace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

#[inline]
fn number_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

fn python_split(s: &str) -> Vec<&str> {
    s.split(py_isspace)
        .filter(|word| !word.is_empty())
        .collect()
}

fn python_truthy(value: &Value) -> Option<bool> {
    crate::python_cache::attach(|py| crate::tags::python_truthy(py, value).ok())
}

enum IntConversion {
    Value(i64),
    ValueError,
    TypeError,
    Unknown,
}

fn parse_python_int(s: &str) -> IntConversion {
    if !s.is_ascii() {
        return IntConversion::Unknown;
    }
    let trimmed = s.trim_matches(number_whitespace);
    let (negative, digits) = match trimmed.as_bytes().first() {
        Some(b'-') => (true, &trimmed[1..]),
        Some(b'+') => (false, &trimmed[1..]),
        _ => (false, trimmed),
    };
    let bytes = digits.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_digit() || !bytes[bytes.len() - 1].is_ascii_digit() {
        return IntConversion::ValueError;
    }
    let mut cleaned = String::with_capacity(bytes.len() + 1);
    if negative {
        cleaned.push('-');
    }
    let mut previous_underscore = false;
    for &b in bytes {
        if b == b'_' {
            if previous_underscore {
                return IntConversion::ValueError;
            }
            previous_underscore = true;
        } else if b.is_ascii_digit() {
            previous_underscore = false;
            cleaned.push(b as char);
        } else {
            return IntConversion::ValueError;
        }
    }
    match cleaned.parse::<i64>() {
        Ok(n) => IntConversion::Value(n),
        Err(_) => IntConversion::Unknown,
    }
}

fn python_int(value: &Value) -> IntConversion {
    match value {
        Value::Int(n) => IntConversion::Value(*n),
        Value::Bool(b) => IntConversion::Value(i64::from(*b)),
        Value::Float(f) if f.is_nan() => IntConversion::ValueError,
        Value::Float(f) if f.is_finite() && f.trunc().abs() < 9.0e18 => {
            IntConversion::Value(f.trunc() as i64)
        }
        Value::String(s) => parse_python_int(s),
        Value::SafeString(s) => parse_python_int(s),
        Value::None => IntConversion::TypeError,
        _ => IntConversion::Unknown,
    }
}

fn int_arg(args: &[Value], index: usize) -> Option<i64> {
    match python_int(args.get(index)?) {
        IntConversion::Value(n) => Some(n),
        _ => None,
    }
}

enum FloatConversion {
    Value(f64),
    ValueError,
    TypeError,
    Unknown,
}

fn parse_python_float(s: &str) -> FloatConversion {
    if !s.is_ascii() || s.contains('_') {
        return FloatConversion::Unknown;
    }
    let trimmed = s.trim_matches(number_whitespace);
    let unsigned = trimmed.trim_start_matches(['+', '-']);
    if unsigned.len() + 1 < trimmed.len() {
        return FloatConversion::ValueError;
    }
    let lowered = unsigned.to_ascii_lowercase();
    if matches!(lowered.as_str(), "inf" | "infinity" | "nan") {
        return FloatConversion::Unknown;
    }
    let bytes = unsigned.as_bytes();
    let mut index = 0;
    let mut mantissa_digits = 0;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
        mantissa_digits += 1;
    }
    if index < bytes.len() && bytes[index] == b'.' {
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
            mantissa_digits += 1;
        }
    }
    if mantissa_digits == 0 {
        return FloatConversion::ValueError;
    }
    if index < bytes.len() && (bytes[index] == b'e' || bytes[index] == b'E') {
        index += 1;
        if index < bytes.len() && (bytes[index] == b'+' || bytes[index] == b'-') {
            index += 1;
        }
        let exponent_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if index == exponent_start {
            return FloatConversion::ValueError;
        }
    }
    if index != bytes.len() {
        return FloatConversion::ValueError;
    }
    match trimmed.parse::<f64>() {
        Ok(f) => FloatConversion::Value(f),
        Err(_) => FloatConversion::Unknown,
    }
}

fn python_float(value: &Value) -> FloatConversion {
    match value {
        Value::Int(n) => FloatConversion::Value(*n as f64),
        Value::Bool(b) => FloatConversion::Value(if *b { 1.0 } else { 0.0 }),
        Value::Float(f) => FloatConversion::Value(*f),
        Value::String(s) => parse_python_float(s),
        Value::SafeString(s) => parse_python_float(s),
        Value::None | Value::List(_) | Value::Dict(_) => FloatConversion::TypeError,
        Value::PyObject(_) => FloatConversion::Unknown,
    }
}

enum ExactSequence<'a, 'py> {
    Native(&'a [Value]),
    List(&'a Bound<'py, PyList>),
    Tuple(&'a Bound<'py, PyTuple>),
}

impl ExactSequence<'_, '_> {
    fn len(&self) -> usize {
        match self {
            Self::Native(items) => items.len(),
            Self::List(list) => list.len(),
            Self::Tuple(tuple) => tuple.len(),
        }
    }

    fn get(&self, index: usize) -> Option<Value> {
        match self {
            Self::Native(items) => items.get(index).cloned(),
            Self::List(list) => list.get_item(index).ok().map(|item| Value::from(&item)),
            Self::Tuple(tuple) => tuple.get_item(index).ok().map(|item| Value::from(&item)),
        }
    }

    fn items(&self) -> Vec<Value> {
        match self {
            Self::Native(items) => items.to_vec(),
            Self::List(list) => list.iter().map(|item| Value::from(&item)).collect(),
            Self::Tuple(tuple) => tuple.iter().map(|item| Value::from(&item)).collect(),
        }
    }
}

fn with_exact_sequence<R>(
    value: &Value,
    read: impl FnOnce(ExactSequence<'_, '_>) -> R,
) -> Option<R> {
    match value {
        Value::List(items) => Some(read(ExactSequence::Native(items))),
        Value::PyObject(obj) => crate::python_cache::attach(|py| {
            let bound = obj.bind(py);
            if let Ok(list) = bound.cast_exact::<PyList>() {
                Some(read(ExactSequence::List(list)))
            } else if let Ok(tuple) = bound.cast_exact::<PyTuple>() {
                Some(read(ExactSequence::Tuple(tuple)))
            } else {
                None
            }
        }),
        _ => None,
    }
}

fn exact_sequence_item(value: &Value, from_end: bool) -> Option<Option<Value>> {
    with_exact_sequence(value, |sequence| match sequence.len() {
        0 => None,
        len => sequence.get(if from_end { len - 1 } else { 0 }),
    })
}

fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

fn percent_quote(s: &str, always_safe_extra: &str) -> String {
    let safe: Vec<u8> = always_safe_extra.bytes().filter(u8::is_ascii).collect();
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"_.-~".contains(&b) || safe.contains(&b) {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn filter_addslashes(value: &Value, _args: &[Value], _autoescape: bool) -> Option<Value> {
    let s = text(value)?;
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\'' => out.push_str("\\'"),
            _ => out.push(c),
        }
    }
    Some(Value::String(out))
}

fn filter_capfirst(value: &Value, _args: &[Value], _autoescape: bool) -> Option<Value> {
    let s = text(value)?;
    let mut chars = s.chars();
    match chars.next() {
        None => Some(Value::String(String::new())),
        Some(first) if first.is_ascii() => {
            let mut out = String::with_capacity(s.len());
            out.push(first.to_ascii_uppercase());
            out.push_str(chars.as_str());
            Some(Value::String(out))
        }
        Some(_) => None,
    }
}

fn pad(s: &str, width: i64, left_share: fn(usize) -> usize) -> Option<String> {
    let length = s.chars().count();
    if width <= 0 || length as i64 >= width {
        return Some(s.to_owned());
    }
    let padding = usize::try_from(width).ok()? - length;
    let left = left_share(padding);
    let mut out = String::new();
    out.try_reserve_exact(s.len().checked_add(padding)?).ok()?;
    out.extend(std::iter::repeat_n(' ', left));
    out.push_str(s);
    out.extend(std::iter::repeat_n(' ', padding - left));
    Some(out)
}

fn filter_center(value: &Value, args: &[Value], _autoescape: bool) -> Option<Value> {
    let width = int_arg(args, 0)?;
    let s = text(value)?;
    pad(&s, width, |padding| padding / 2).map(Value::String)
}

fn filter_ljust(value: &Value, args: &[Value], _autoescape: bool) -> Option<Value> {
    let width = int_arg(args, 0)?;
    let s = text(value)?;
    pad(&s, width, |_| 0).map(Value::String)
}

fn filter_rjust(value: &Value, args: &[Value], _autoescape: bool) -> Option<Value> {
    let width = int_arg(args, 0)?;
    let s = text(value)?;
    pad(&s, width, |padding| padding).map(Value::String)
}

fn filter_cut(value: &Value, args: &[Value], _autoescape: bool) -> Option<Value> {
    let to_cut = args.first()?.as_str()?;
    let s = text(value)?;
    let out = s.replace(to_cut, "");
    if matches!(value, Value::SafeString(_)) && to_cut != ";" {
        Some(Value::SafeString(out.into()))
    } else {
        Some(Value::String(out))
    }
}

fn filter_escape(value: &Value, _args: &[Value], _autoescape: bool) -> Option<Value> {
    if matches!(value, Value::SafeString(_)) {
        return Some(value.clone());
    }
    Some(Value::SafeString(html_escape(&text(value)?).into()))
}

fn filter_escapejs(value: &Value, _args: &[Value], _autoescape: bool) -> Option<Value> {
    let s = text(value)?;
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\u005C"),
            '\'' => out.push_str("\\u0027"),
            '"' => out.push_str("\\u0022"),
            '>' => out.push_str("\\u003E"),
            '<' => out.push_str("\\u003C"),
            '&' => out.push_str("\\u0026"),
            '=' => out.push_str("\\u003D"),
            '-' => out.push_str("\\u002D"),
            ';' => out.push_str("\\u003B"),
            '`' => out.push_str("\\u0060"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 32 => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            _ => out.push(c),
        }
    }
    Some(Value::SafeString(out.into()))
}

fn filter_force_escape(value: &Value, _args: &[Value], _autoescape: bool) -> Option<Value> {
    Some(Value::SafeString(html_escape(&text(value)?).into()))
}

fn filter_iriencode(value: &Value, _args: &[Value], _autoescape: bool) -> Option<Value> {
    Some(Value::String(percent_quote(
        &text(value)?,
        "/#%[]=:;$&()+,!?*@'~",
    )))
}

fn filter_linenumbers(value: &Value, _args: &[Value], autoescape: bool) -> Option<Value> {
    let s = text(value)?;
    let lines: Vec<&str> = s.split('\n').collect();
    let width = lines.len().to_string().len();
    let escape_lines = autoescape && !matches!(value, Value::SafeString(_));
    let mut out = String::with_capacity(s.len() + lines.len() * (width + 2));
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let _ = write!(out, "{:0width$}. ", index + 1, width = width);
        if escape_lines {
            crate::utils::html_escape_into(line, &mut out);
        } else {
            out.push_str(line);
        }
    }
    Some(Value::SafeString(out.into()))
}

fn filter_lower(value: &Value, _args: &[Value], _autoescape: bool) -> Option<Value> {
    let s = text(value)?;
    if !s.is_ascii() {
        return None;
    }
    Some(Value::String(s.to_ascii_lowercase()))
}

fn filter_upper(value: &Value, _args: &[Value], _autoescape: bool) -> Option<Value> {
    let s = text(value)?;
    if !s.is_ascii() {
        return None;
    }
    Some(Value::String(s.to_ascii_uppercase()))
}

fn filter_make_list(value: &Value, _args: &[Value], _autoescape: bool) -> Option<Value> {
    let s = text(value)?;
    Some(Value::List(
        s.chars().map(|c| Value::String(c.to_string())).collect(),
    ))
}

fn filter_slugify(value: &Value, _args: &[Value], _autoescape: bool) -> Option<Value> {
    let s = text(value)?;
    if !s.is_ascii() {
        return None;
    }
    let cleaned: String = s
        .to_ascii_lowercase()
        .chars()
        .filter(|&c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || py_isspace(c))
        .collect();
    let mut slug = String::with_capacity(cleaned.len());
    let mut in_separator_run = false;
    for c in cleaned.chars() {
        if c == '-' || py_isspace(c) {
            if !in_separator_run {
                slug.push('-');
                in_separator_run = true;
            }
        } else {
            slug.push(c);
            in_separator_run = false;
        }
    }
    Some(Value::String(
        slug.trim_matches(|c| c == '-' || c == '_').to_owned(),
    ))
}

fn filter_stringformat(value: &Value, args: &[Value], _autoescape: bool) -> Option<Value> {
    match args.first()?.as_str()? {
        "s" => Some(Value::String(text(value)?.into_owned())),
        "d" | "i" => match value {
            Value::Int(n) => Some(Value::String(n.to_string())),
            Value::Bool(b) => Some(Value::String(i64::from(*b).to_string())),
            _ => None,
        },
        _ => None,
    }
}

fn lowercase_matches(s: &str, width: usize, matches: fn(&[u8]) -> bool) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut index = 0;
    while index < bytes.len() {
        if index + width <= bytes.len() && matches(&bytes[index..index + width]) {
            for &b in &bytes[index..index + width] {
                out.push(b.to_ascii_lowercase() as char);
            }
            index += width;
        } else {
            out.push(bytes[index] as char);
            index += 1;
        }
    }
    out
}

fn filter_title(value: &Value, _args: &[Value], _autoescape: bool) -> Option<Value> {
    let s = text(value)?;
    if !s.is_ascii() {
        return None;
    }
    let mut titled = String::with_capacity(s.len());
    let mut previous_cased = false;
    for c in s.chars() {
        titled.push(if previous_cased {
            c.to_ascii_lowercase()
        } else {
            c.to_ascii_uppercase()
        });
        previous_cased = c.is_ascii_alphabetic();
    }
    let titled = lowercase_matches(&titled, 3, |window| {
        window[0].is_ascii_lowercase() && window[1] == b'\'' && window[2].is_ascii_uppercase()
    });
    let titled = lowercase_matches(&titled, 2, |window| {
        window[0].is_ascii_digit() && window[1].is_ascii_uppercase()
    });
    Some(Value::String(titled))
}

fn filter_truncatechars(value: &Value, args: &[Value], _autoescape: bool) -> Option<Value> {
    let length = int_arg(args, 0)?;
    let s = text(value)?;
    if length <= 0 {
        return Some(Value::String(String::new()));
    }
    if !s.is_ascii() || s.len() as i64 > length {
        return None;
    }
    Some(Value::String(s.into_owned()))
}

fn filter_truncatewords(value: &Value, args: &[Value], _autoescape: bool) -> Option<Value> {
    let length = int_arg(args, 0)?;
    let s = text(value)?;
    if length <= 0 {
        return Some(Value::String(String::new()));
    }
    let words = python_split(&s);
    if words.len() as i64 > length {
        let joined = words[..length as usize].join(" ");
        if joined.ends_with(" …") {
            return Some(Value::String(joined));
        }
        return Some(Value::String(format!("{joined} …")));
    }
    Some(Value::String(words.join(" ")))
}

fn filter_urlencode(value: &Value, args: &[Value], _autoescape: bool) -> Option<Value> {
    let safe = match args.first() {
        None => "/",
        Some(arg) => arg.as_str()?,
    };
    Some(Value::String(percent_quote(&text(value)?, safe)))
}

fn filter_wordcount(value: &Value, _args: &[Value], _autoescape: bool) -> Option<Value> {
    Some(Value::Int(python_split(&text(value)?).len() as i64))
}

fn filter_linebreaks(value: &Value, _args: &[Value], autoescape: bool) -> Option<Value> {
    let s = normalize_newlines(&text(value)?);
    let escape = autoescape && !matches!(value, Value::SafeString(_));
    let bytes = s.as_bytes();
    let mut paragraphs: Vec<&str> = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\n' {
            let run_start = index;
            while index < bytes.len() && bytes[index] == b'\n' {
                index += 1;
            }
            if index - run_start >= 2 {
                paragraphs.push(&s[start..run_start]);
                start = index;
            }
        } else {
            index += 1;
        }
    }
    paragraphs.push(&s[start..]);
    let rendered: Vec<String> = paragraphs
        .into_iter()
        .map(|paragraph| {
            let body = if escape {
                html_escape(paragraph)
            } else {
                paragraph.to_owned()
            };
            format!("<p>{}</p>", body.replace('\n', "<br>"))
        })
        .collect();
    Some(Value::SafeString(rendered.join("\n\n").into()))
}

fn filter_linebreaksbr(value: &Value, _args: &[Value], autoescape: bool) -> Option<Value> {
    let s = normalize_newlines(&text(value)?);
    let s = if autoescape && !matches!(value, Value::SafeString(_)) {
        html_escape(&s)
    } else {
        s
    };
    Some(Value::SafeString(s.replace('\n', "<br>").into()))
}

fn filter_safe(value: &Value, _args: &[Value], _autoescape: bool) -> Option<Value> {
    match value {
        Value::SafeString(_) => Some(value.clone()),
        _ => Some(Value::SafeString(text(value)?.into_owned().into())),
    }
}

fn filter_safeseq(value: &Value, _args: &[Value], _autoescape: bool) -> Option<Value> {
    let items: Vec<Value> = match value {
        Value::List(items) => items.clone(),
        Value::String(s) => s.chars().map(|c| Value::String(c.to_string())).collect(),
        Value::SafeString(s) => s.chars().map(|c| Value::String(c.to_string())).collect(),
        _ => return None,
    };
    let mut safe_items = Vec::with_capacity(items.len());
    for item in &items {
        safe_items.push(match item {
            Value::SafeString(_) => item.clone(),
            other => Value::SafeString(text(other)?.into_owned().into()),
        });
    }
    Some(Value::List(safe_items))
}

fn filter_striptags(value: &Value, _args: &[Value], _autoescape: bool) -> Option<Value> {
    let s = text(value)?;
    if s.contains('<') && s.contains('>') {
        return None;
    }
    Some(Value::String(s.into_owned()))
}

fn filter_first(value: &Value, _args: &[Value], _autoescape: bool) -> Option<Value> {
    if let Some(s) = value.as_str() {
        return Some(Value::String(
            s.chars().next().map(String::from).unwrap_or_default(),
        ));
    }
    Some(exact_sequence_item(value, false)?.unwrap_or(Value::String(String::new())))
}

fn filter_last(value: &Value, _args: &[Value], _autoescape: bool) -> Option<Value> {
    if let Some(s) = value.as_str() {
        return Some(Value::String(
            s.chars().next_back().map(String::from).unwrap_or_default(),
        ));
    }
    Some(exact_sequence_item(value, true)?.unwrap_or(Value::String(String::new())))
}

fn filter_join(value: &Value, args: &[Value], autoescape: bool) -> Option<Value> {
    let separator = args.first()?;
    let separator_text = separator.as_str()?;
    let items: Vec<Value> = match value {
        Value::String(s) => s.chars().map(|c| Value::String(c.to_string())).collect(),
        Value::SafeString(s) => s.chars().map(|c| Value::String(c.to_string())).collect(),
        Value::None | Value::Bool(_) | Value::Int(_) | Value::Float(_) => {
            return Some(value.clone());
        }
        _ => with_exact_sequence(value, |sequence| sequence.items())?,
    };
    let mut out = String::new();
    if autoescape {
        let escaped_separator = match separator {
            Value::SafeString(s) => s.to_string(),
            _ => html_escape(separator_text),
        };
        for (index, item) in items.iter().enumerate() {
            if index > 0 {
                out.push_str(&escaped_separator);
            }
            match item {
                Value::SafeString(s) => out.push_str(s),
                other => crate::utils::html_escape_into(&text(other)?, &mut out),
            }
        }
    } else {
        for item in &items {
            match item {
                Value::String(_) | Value::SafeString(_) => {}
                Value::PyObject(_) => return None,
                _ => return Some(value.clone()),
            }
        }
        for (index, item) in items.iter().enumerate() {
            if index > 0 {
                out.push_str(separator_text);
            }
            out.push_str(item.as_str().expect("items checked to be strings"));
        }
    }
    Some(Value::SafeString(out.into()))
}

fn filter_length(value: &Value, _args: &[Value], _autoescape: bool) -> Option<Value> {
    match value {
        Value::String(s) => Some(Value::Int(s.chars().count() as i64)),
        Value::SafeString(s) => Some(Value::Int(s.chars().count() as i64)),
        Value::List(items) => Some(Value::Int(items.len() as i64)),
        Value::Dict(map) => Some(Value::Int(map.len() as i64)),
        Value::None | Value::Bool(_) | Value::Int(_) | Value::Float(_) => Some(Value::Int(0)),
        Value::PyObject(obj) => crate::python_cache::attach(|py| match obj.bind(py).len() {
            Ok(n) => Some(Value::Int(n as i64)),
            Err(err)
                if err.is_instance_of::<pyo3::exceptions::PyTypeError>(py)
                    || err.is_instance_of::<pyo3::exceptions::PyValueError>(py) =>
            {
                Some(Value::Int(0))
            }
            Err(_) => None,
        }),
    }
}

fn python_slice_indices(
    length: i64,
    start: Option<i64>,
    stop: Option<i64>,
    step: i64,
) -> Vec<usize> {
    let (lower, upper) = if step > 0 {
        (0, length)
    } else {
        (-1, length - 1)
    };
    let adjust = |bound: Option<i64>, default: i64| -> i64 {
        match bound {
            None => default,
            Some(b) if b < 0 => (b.saturating_add(length)).max(lower),
            Some(b) => b.min(upper),
        }
    };
    let start = adjust(start, if step < 0 { upper } else { lower });
    let stop = adjust(stop, if step < 0 { lower } else { upper });
    let mut indices = Vec::new();
    let mut index = start;
    while (step > 0 && index < stop) || (step < 0 && index > stop) {
        indices.push(index as usize);
        match index.checked_add(step) {
            Some(next) => index = next,
            None => break,
        }
    }
    indices
}

fn filter_slice(value: &Value, args: &[Value], _autoescape: bool) -> Option<Value> {
    let spec = text(args.first()?)?;
    let mut bits: Vec<Option<i64>> = Vec::new();
    for part in spec.split(':') {
        if part.is_empty() {
            bits.push(None);
            continue;
        }
        match parse_python_int(part) {
            IntConversion::Value(n) => bits.push(Some(n)),
            IntConversion::ValueError | IntConversion::TypeError => return Some(value.clone()),
            IntConversion::Unknown => return None,
        }
    }
    let (start, stop, step) = match bits.as_slice() {
        [stop] => (None, *stop, None),
        [start, stop] => (*start, *stop, None),
        [start, stop, step] => (*start, *stop, *step),
        _ => return Some(value.clone()),
    };
    let step = step.unwrap_or(1);
    if step == 0 {
        return Some(value.clone());
    }
    match value {
        Value::String(_) | Value::SafeString(_) => {
            let chars: Vec<char> = value.as_str().expect("string variant").chars().collect();
            let selected: String = python_slice_indices(chars.len() as i64, start, stop, step)
                .into_iter()
                .map(|i| chars[i])
                .collect();
            Some(Value::String(selected))
        }
        Value::List(items) => Some(Value::List(
            python_slice_indices(items.len() as i64, start, stop, step)
                .into_iter()
                .map(|i| items[i].clone())
                .collect(),
        )),
        _ => None,
    }
}

fn filter_add(value: &Value, args: &[Value], _autoescape: bool) -> Option<Value> {
    let arg = args.first()?;
    match (python_int(value), python_int(arg)) {
        (IntConversion::Value(a), IntConversion::Value(b)) => {
            return a.checked_add(b).map(Value::Int);
        }
        (IntConversion::Unknown, _) | (_, IntConversion::Unknown) => return None,
        _ => {}
    }
    let numeric = |v: &Value| match v {
        Value::Int(n) => Some(*n as f64),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::Float(f) => Some(*f),
        _ => None,
    };
    match (value, arg) {
        (Value::String(_) | Value::SafeString(_), Value::String(_) | Value::SafeString(_)) => {
            let joined = format!(
                "{}{}",
                value.as_str().expect("string"),
                arg.as_str().expect("string")
            );
            if matches!(value, Value::SafeString(_)) && matches!(arg, Value::SafeString(_)) {
                Some(Value::SafeString(joined.into()))
            } else {
                Some(Value::String(joined))
            }
        }
        _ => match (numeric(value), numeric(arg)) {
            (Some(a), Some(b)) => Some(Value::Float(a + b)),
            _ => Some(Value::String(String::new())),
        },
    }
}

fn filter_get_digit(value: &Value, args: &[Value], _autoescape: bool) -> Option<Value> {
    let (Value::Int(number), IntConversion::Value(position)) = (value, python_int(args.first()?))
    else {
        return None;
    };
    if position < 1 {
        return Some(Value::Int(*number));
    }
    let digits = number.to_string();
    if position as usize > digits.len() {
        return Some(Value::Int(0));
    }
    let digit = digits.as_bytes()[digits.len() - position as usize];
    if !digit.is_ascii_digit() {
        return None;
    }
    Some(Value::Int(i64::from(digit - b'0')))
}

fn filter_date(value: &Value, args: &[Value], _autoescape: bool) -> Option<Value> {
    match value {
        Value::None => return Some(Value::String(String::new())),
        Value::String(s) if s.is_empty() => return Some(Value::String(String::new())),
        Value::SafeString(s) if s.is_empty() => return Some(Value::String(String::new())),
        _ => {}
    }
    let Value::PyObject(obj) = value else {
        return None;
    };
    let format_str = match args.first() {
        None => "DATE_FORMAT",
        Some(arg) => match arg.as_str()? {
            "" => "DATE_FORMAT",
            format_str => format_str,
        },
    };
    crate::python_cache::attach(|py| date_format::try_format(py, obj.bind(py), format_str))
        .map(Value::String)
}

fn filter_default(value: &Value, args: &[Value], _autoescape: bool) -> Option<Value> {
    if python_truthy(value)? {
        Some(value.clone())
    } else {
        Some(args.first()?.clone())
    }
}

fn filter_default_if_none(value: &Value, args: &[Value], _autoescape: bool) -> Option<Value> {
    match value {
        Value::None => Some(args.first()?.clone()),
        _ => Some(value.clone()),
    }
}

fn filter_divisibleby(value: &Value, args: &[Value], _autoescape: bool) -> Option<Value> {
    let (IntConversion::Value(a), IntConversion::Value(b)) =
        (python_int(value), python_int(args.first()?))
    else {
        return None;
    };
    Some(Value::Bool(a.checked_rem(b)? == 0))
}

fn filter_pluralize(value: &Value, args: &[Value], _autoescape: bool) -> Option<Value> {
    let suffix = match args.first() {
        None => "s",
        Some(arg) => arg.as_str()?,
    };
    let spec = if suffix.contains(',') {
        Cow::Borrowed(suffix)
    } else {
        Cow::Owned(format!(",{suffix}"))
    };
    let bits: Vec<&str> = spec.split(',').collect();
    if bits.len() > 2 {
        return Some(Value::String(String::new()));
    }
    let (singular, plural) = (bits[0], bits[1]);
    let choose = |is_one: bool| Value::String(if is_one { singular } else { plural }.to_owned());
    match python_float(value) {
        FloatConversion::Value(f) => Some(choose(f == 1.0)),
        FloatConversion::ValueError => Some(Value::String(String::new())),
        FloatConversion::Unknown => {
            with_exact_sequence(value, |sequence| choose(sequence.len() == 1))
        }
        FloatConversion::TypeError => match value {
            Value::List(items) => Some(choose(items.len() == 1)),
            Value::Dict(map) => Some(choose(map.len() == 1)),
            _ => Some(Value::String(String::new())),
        },
    }
}

fn filter_phone2numeric(value: &Value, _args: &[Value], _autoescape: bool) -> Option<Value> {
    let s = value.as_str()?;
    if !s.is_ascii() {
        return None;
    }
    let out: String = s
        .chars()
        .map(|c| match c.to_ascii_lowercase() {
            'a' | 'b' | 'c' => '2',
            'd' | 'e' | 'f' => '3',
            'g' | 'h' | 'i' => '4',
            'j' | 'k' | 'l' => '5',
            'm' | 'n' | 'o' => '6',
            'p' | 'q' | 'r' | 's' => '7',
            't' | 'u' | 'v' => '8',
            'w' | 'x' | 'y' | 'z' => '9',
            lowered => lowered,
        })
        .collect();
    Some(Value::String(out))
}

fn filter_yesno(value: &Value, args: &[Value], _autoescape: bool) -> Option<Value> {
    let mapping = args.first()?.as_str()?;
    let bits: Vec<&str> = mapping.split(',').collect();
    if bits.len() < 2 {
        return Some(value.clone());
    }
    let (yes, no, maybe) = if bits.len() == 3 {
        (bits[0], bits[1], bits[2])
    } else {
        (bits[0], bits[1], bits[1])
    };
    if matches!(value, Value::None) {
        return Some(Value::String(maybe.to_owned()));
    }
    Some(Value::String(
        if python_truthy(value)? { yes } else { no }.to_owned(),
    ))
}

fn delegate(_value: &Value, _args: &[Value], _autoescape: bool) -> Option<Value> {
    None
}

pub fn native_for(
    py: Python<'_>,
    name: &str,
    func: &Bound<'_, PyAny>,
    provided_args: usize,
) -> Option<&'static NativeFilter> {
    let filter = get_default_filters().get(name)?;
    let dj = crate::python_cache::django(py).ok()?;
    let builtin = dj.builtin_filters.bind(py).get_item(name).ok()?;
    if !func.is(&builtin) {
        return None;
    }
    let capacity = native_arg_capacity(py, filter, &builtin)?;
    (provided_args <= capacity).then_some(filter)
}

fn native_arg_capacity(
    py: Python<'_>,
    filter: &'static NativeFilter,
    builtin: &Bound<'_, PyAny>,
) -> Option<usize> {
    static CAPACITY: std::sync::LazyLock<std::sync::Mutex<HashMap<&'static str, Option<usize>>>> =
        std::sync::LazyLock::new(Default::default);
    if let Some(capacity) = CAPACITY
        .lock()
        .ok()
        .and_then(|cache| cache.get(filter.name).copied())
    {
        return capacity;
    }
    let flag = |attribute: &str| {
        builtin
            .getattr(attribute)
            .ok()
            .and_then(|value| value.is_truthy().ok())
            .unwrap_or(false)
    };
    let flags_match = flag("is_safe") == filter.is_safe
        && flag("needs_autoescape") == filter.needs_autoescape
        && flag("expects_localtime") == filter.expects_localtime;
    let mut target = builtin.clone();
    while let Ok(wrapped) = target.getattr(pyo3::intern!(py, "__wrapped__")) {
        target = wrapped;
    }
    let argcount = target
        .getattr(pyo3::intern!(py, "__code__"))
        .and_then(|code| code.getattr(pyo3::intern!(py, "co_argcount")))
        .and_then(|count| count.extract::<usize>())
        .ok();
    let capacity = match argcount {
        Some(count) if flags_match => count.checked_sub(1 + usize::from(filter.needs_autoescape)),
        _ => None,
    };
    if let Ok(mut cache) = CAPACITY.lock() {
        cache.insert(filter.name, capacity);
    }
    capacity
}

pub fn check_filter_args(
    py: Python<'_>,
    name: &str,
    func: &Bound<'_, PyAny>,
    provided_args: usize,
) -> Result<(), TemplateError> {
    type ArgsCache = HashMap<(usize, usize), (Py<PyAny>, Option<String>)>;
    static CHECKED: std::sync::LazyLock<std::sync::Mutex<ArgsCache>> =
        std::sync::LazyLock::new(Default::default);
    let key = (func.as_ptr() as usize, provided_args);
    let cached = CHECKED
        .lock()
        .ok()
        .and_then(|cache| cache.get(&key).map(|(_, error)| error.clone()));
    let error = match cached {
        Some(error) => error,
        None => {
            let dj = crate::python_cache::django(py)?;
            let provided = PyList::new(py, std::iter::repeat_n(py.None(), provided_args))?;
            let error = match dj
                .template_base
                .bind(py)
                .getattr(pyo3::intern!(py, "FilterExpression"))?
                .call_method1(pyo3::intern!(py, "args_check"), (name, func, provided))
            {
                Ok(_) => None,
                Err(error) if error.is_instance(py, dj.template_syntax_error_cls.bind(py)) => {
                    Some(error.value(py).str()?.to_string())
                }
                Err(error) => return Err(error.into()),
            };
            if let Ok(mut cache) = CHECKED.lock() {
                cache.insert(key, (func.clone().unbind(), error.clone()));
            }
            error
        }
    };
    match error {
        Some(message) => Err(TemplateError::TemplateSyntaxError(message)),
        None => Ok(()),
    }
}

pub fn get_default_filters() -> &'static HashMap<String, NativeFilter> {
    static FILTERS: OnceLock<HashMap<String, NativeFilter>> = OnceLock::new();
    FILTERS.get_or_init(build_default_filters)
}

fn build_default_filters() -> HashMap<String, NativeFilter> {
    let mut filters = HashMap::new();
    let mut register = |name: &'static str,
                        fast: FastFilter,
                        is_safe: bool,
                        needs_autoescape: bool,
                        expects_localtime: bool| {
        filters.insert(
            name.to_owned(),
            NativeFilter {
                name,
                fast,
                is_safe,
                needs_autoescape,
                expects_localtime,
            },
        );
    };

    register("addslashes", filter_addslashes, true, false, false);
    register("capfirst", filter_capfirst, true, false, false);
    register("escapejs", filter_escapejs, false, false, false);
    register("json_script", delegate, true, false, false);
    register("floatformat", delegate, true, false, false);
    register("iriencode", filter_iriencode, true, false, false);
    register("linenumbers", filter_linenumbers, true, true, false);
    register("lower", filter_lower, true, false, false);
    register("make_list", filter_make_list, false, false, false);
    register("slugify", filter_slugify, true, false, false);
    register("stringformat", filter_stringformat, true, false, false);
    register("title", filter_title, true, false, false);
    register("truncatechars", filter_truncatechars, true, false, false);
    register("truncatechars_html", delegate, true, false, false);
    register("truncatewords", filter_truncatewords, true, false, false);
    register("truncatewords_html", delegate, true, false, false);
    register("upper", filter_upper, false, false, false);
    register("urlencode", filter_urlencode, false, false, false);
    register("urlize", delegate, true, true, false);
    register("urlizetrunc", delegate, true, true, false);
    register("wordcount", filter_wordcount, false, false, false);
    register("wordwrap", delegate, true, false, false);
    register("ljust", filter_ljust, true, false, false);
    register("rjust", filter_rjust, true, false, false);
    register("center", filter_center, true, false, false);
    register("cut", filter_cut, false, false, false);
    register("escape", filter_escape, true, false, false);
    register("escapeseq", delegate, true, false, false);
    register("force_escape", filter_force_escape, true, false, false);
    register("linebreaks", filter_linebreaks, true, true, false);
    register("linebreaksbr", filter_linebreaksbr, true, true, false);
    register("safe", filter_safe, true, false, false);
    register("safeseq", filter_safeseq, true, false, false);
    register("striptags", filter_striptags, true, false, false);
    register("dictsort", delegate, false, false, false);
    register("dictsortreversed", delegate, false, false, false);
    register("first", filter_first, false, false, false);
    register("join", filter_join, true, true, false);
    register("last", filter_last, true, false, false);
    register("length", filter_length, false, false, false);
    register("random", delegate, true, false, false);
    register("slice", filter_slice, true, false, false);
    register("unordered_list", delegate, true, true, false);
    register("add", filter_add, false, false, false);
    register("get_digit", filter_get_digit, false, false, false);
    register("date", filter_date, false, false, true);
    register("time", delegate, false, false, true);
    register("timesince", delegate, false, false, false);
    register("timeuntil", delegate, false, false, false);
    register("default", filter_default, false, false, false);
    register(
        "default_if_none",
        filter_default_if_none,
        false,
        false,
        false,
    );
    register("divisibleby", filter_divisibleby, false, false, false);
    register("yesno", filter_yesno, false, false, false);
    register("filesizeformat", delegate, true, false, false);
    register("pluralize", filter_pluralize, false, false, false);
    register("phone2numeric", filter_phone2numeric, true, false, false);
    register("pprint", delegate, true, false, false);

    filters
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(val: &str) -> Value {
        Value::String(val.to_owned())
    }

    fn safe(val: &str) -> Value {
        Value::SafeString(val.to_owned().into())
    }

    fn int(n: i64) -> Value {
        Value::Int(n)
    }

    #[test]
    fn registry_matches_django_6_builtins() {
        let filters = get_default_filters();
        assert_eq!(filters.len(), 57);
        assert!(!filters.contains_key("length_is"));
    }

    #[test]
    fn python_int_parsing_follows_int_constructor() {
        assert!(matches!(parse_python_int(" 12 "), IntConversion::Value(12)));
        assert!(matches!(
            parse_python_int("1_000"),
            IntConversion::Value(1000)
        ));
        assert!(matches!(parse_python_int("-7"), IntConversion::Value(-7)));
        assert!(matches!(
            parse_python_int("1__0"),
            IntConversion::ValueError
        ));
        assert!(matches!(parse_python_int("_1"), IntConversion::ValueError));
        assert!(matches!(parse_python_int("1.5"), IntConversion::ValueError));
        assert!(matches!(parse_python_int(""), IntConversion::ValueError));
        assert!(matches!(parse_python_int("٣"), IntConversion::Unknown));
        assert!(matches!(
            parse_python_int("99999999999999999999"),
            IntConversion::Unknown
        ));
    }

    #[test]
    fn python_float_parsing_follows_float_constructor() {
        assert!(matches!(parse_python_float(" 1.0 "), FloatConversion::Value(f) if f == 1.0));
        assert!(matches!(parse_python_float("1e0"), FloatConversion::Value(f) if f == 1.0));
        assert!(matches!(parse_python_float(".5"), FloatConversion::Value(f) if f == 0.5));
        assert!(matches!(
            parse_python_float("abc"),
            FloatConversion::ValueError
        ));
        assert!(matches!(
            parse_python_float("1e"),
            FloatConversion::ValueError
        ));
        assert!(matches!(
            parse_python_float("--1"),
            FloatConversion::ValueError
        ));
        assert!(matches!(
            parse_python_float("inf"),
            FloatConversion::Unknown
        ));
    }

    #[test]
    fn python_slice_indices_match_python() {
        assert_eq!(python_slice_indices(5, None, Some(2), 1), vec![0, 1]);
        assert_eq!(python_slice_indices(5, None, Some(-1), 1), vec![0, 1, 2, 3]);
        assert_eq!(python_slice_indices(5, None, None, -1), vec![4, 3, 2, 1, 0]);
        assert_eq!(python_slice_indices(5, Some(-2), None, 1), vec![3, 4]);
        assert_eq!(python_slice_indices(5, None, None, 2), vec![0, 2, 4]);
        assert_eq!(python_slice_indices(3, Some(10), None, -1), vec![2, 1, 0]);
    }

    #[test]
    fn title_applies_django_fixups() {
        assert_eq!(
            filter_title(&s("it's 1st o'NEIL"), &[], false),
            Some(s("It's 1st O'Neil"))
        );
        assert_eq!(filter_title(&s("straße"), &[], false), None);
    }

    #[test]
    fn linebreaks_keeps_empty_paragraphs() {
        assert_eq!(filter_linebreaks(&s(""), &[], true), Some(safe("<p></p>")));
        assert_eq!(
            filter_linebreaks(&s("a\n\n\nb\nc"), &[], true),
            Some(safe("<p>a</p>\n\n<p>b<br>c</p>"))
        );
    }

    #[test]
    fn pluralize_follows_float_and_len_rules() {
        assert_eq!(filter_pluralize(&int(1), &[], false), Some(s("")));
        assert_eq!(filter_pluralize(&int(2), &[], false), Some(s("s")));
        assert_eq!(
            filter_pluralize(&s("1"), &[s("y,ies")], false),
            Some(s("y"))
        );
        assert_eq!(filter_pluralize(&s("abc"), &[], false), Some(s("")));
        assert_eq!(filter_pluralize(&Value::None, &[], false), Some(s("")));
        assert_eq!(
            filter_pluralize(&Value::Bool(true), &[s("y,ies")], false),
            Some(s("y"))
        );
        assert_eq!(filter_pluralize(&int(2), &[s("a,b,c")], false), Some(s("")));
    }

    #[test]
    fn slice_uses_python_slice_semantics() {
        assert_eq!(
            filter_slice(&s("abcdef"), &[s("-1")], false),
            Some(s("abcde"))
        );
        assert_eq!(filter_slice(&s("abcdef"), &[s("2")], false), Some(s("ab")));
        assert_eq!(
            filter_slice(&s("abcdef"), &[s("::2")], false),
            Some(s("ace"))
        );
        assert_eq!(filter_slice(&s("abc"), &[s("x")], false), Some(s("abc")));
        assert_eq!(filter_slice(&s("abc"), &[s("::0")], false), Some(s("abc")));
    }

    #[test]
    fn add_follows_int_then_concat_rules() {
        assert_eq!(filter_add(&int(4), &[int(2)], false), Some(int(6)));
        assert_eq!(filter_add(&s("4"), &[s("2")], false), Some(int(6)));
        assert_eq!(filter_add(&s("a"), &[s("b")], false), Some(s("ab")));
        assert_eq!(filter_add(&s("a"), &[int(1)], false), Some(s("")));
        assert_eq!(
            filter_add(&Value::Float(1.9), &[int(1)], false),
            Some(int(2))
        );
        assert_eq!(filter_add(&safe("a"), &[s("b")], false), Some(s("ab")));
        assert_eq!(filter_add(&int(i64::MAX), &[int(1)], false), None);
    }

    #[test]
    fn escapejs_matches_django_table() {
        assert_eq!(
            filter_escapejs(&s("`a`\n"), &[], false),
            Some(safe("\\u0060a\\u0060\\u000A"))
        );
    }

    #[test]
    fn non_ascii_case_changes_go_to_django() {
        assert_eq!(filter_upper(&s("straße"), &[], false), None);
        assert_eq!(filter_lower(&s("ÇA"), &[], false), None);
        assert_eq!(filter_capfirst(&s("ßa"), &[], false), None);
    }
}
