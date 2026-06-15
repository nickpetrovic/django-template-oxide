//! Django's built-in template filters, implemented in pure Rust.
//! Signature: `fn(value: &Value, args: &[Value], autoescape: bool) -> Value`.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::OnceLock;

use crate::context::Value;
use crate::utils::html_escape;

mod date_format;

/// A Rust-native template filter.
pub struct NativeFilter {
    pub name: &'static str,
    pub func: fn(&Value, &[Value], bool) -> Value,
    /// Safe input produces safe output when the filter doesn't alter text.
    pub is_safe: bool,
    /// `autoescape` carries real information and must be forwarded.
    pub needs_autoescape: bool,
    /// Apply `template_localtime` to input before calling.
    pub expects_localtime: bool,
}

/// Coerce a `Value` to its string representation.
fn value_to_string(v: &Value) -> String {
    match v {
        // Matches Django's `str(None)` and `@stringfilter` coercion.
        Value::None => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Int(n) => n.to_string(),
        Value::Float(n) => format_float(*n),
        Value::String(s) => s.clone(),
        Value::SafeString(s) => s.to_string(),
        Value::List(_) | Value::Dict(_) => format!("{v}"),
        Value::PyObject(obj) => Python::attach(|py| {
            obj.bind(py)
                .str()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        }),
    }
}

/// Borrow as `&str` for String/SafeString, else fall back to a copy.
#[inline]
fn value_as_str_cow(v: &Value) -> Cow<'_, str> {
    match v.as_str() {
        Some(s) => Cow::Borrowed(s),
        None => Cow::Owned(value_to_string(v)),
    }
}

/// Format a float the way Python does (no trailing zeros, but at least one
/// decimal if it's a whole number).
fn format_float(n: f64) -> String {
    if n.fract() == 0.0 && n.is_finite() {
        format!("{n:.1}")
    } else {
        format!("{n}")
    }
}

/// Django's truthiness. PyObject defers to Python `bool()`.
fn is_truthy(v: &Value) -> bool {
    match v {
        Value::None => false,
        Value::Bool(b) => *b,
        Value::Int(n) => *n != 0,
        Value::Float(n) => *n != 0.0,
        Value::String(s) => !s.is_empty(),
        Value::SafeString(s) => !s.is_empty(),
        Value::List(items) => !items.is_empty(),
        Value::Dict(map) => !map.is_empty(),
        Value::PyObject(obj) => pyo3::Python::attach(|py| obj.bind(py).is_truthy().unwrap_or(true)),
    }
}

/// Check if a `Value` is a safe string.
fn is_safe_value(v: &Value) -> bool {
    matches!(v, Value::SafeString(_))
}

/// Wrap the output string with the same safe/unsafe status as the input.
fn preserve_safety(input: &Value, output: String) -> Value {
    if is_safe_value(input) {
        Value::SafeString(std::sync::Arc::from(output))
    } else {
        Value::String(output)
    }
}

/// Get the first filter argument as a string, or return a default.
fn arg_as_string(args: &[Value], default: &str) -> String {
    args.first()
        .map(value_to_string)
        .unwrap_or_else(|| default.to_owned())
}

/// Get the first filter argument as an i64, or return a default.
fn arg_as_i64(args: &[Value], default: i64) -> i64 {
    args.first()
        .and_then(coerce_to_i64)
        .unwrap_or(default)
}

/// Try to coerce a `Value` to i64.
fn coerce_to_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Int(n) => Some(*n),
        Value::Float(f) => Some(*f as i64),
        Value::Bool(b) => Some(if *b { 1 } else { 0 }),
        _ => v.as_str().and_then(|s| s.trim().parse::<i64>().ok()),
    }
}

/// Try to coerce a `Value` to f64.
fn coerce_to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int(n) => Some(*n as f64),
        Value::Float(f) => Some(*f),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => v.as_str().and_then(|s| s.trim().parse::<f64>().ok()),
    }
}

/// Get the length of a value.
fn value_length(v: &Value) -> usize {
    if let Some(s) = v.as_str() {
        return s.chars().count();
    }
    match v {
        Value::List(items) => items.len(),
        Value::Dict(map) => map.len(),
        Value::PyObject(obj) => {
            // Delegate to Python's `len()` for lazy collections (lists,
            // tuples, dicts, sets, custom containers).
            Python::attach(|py| obj.bind(py).len().unwrap_or(0))
        }
        _ => 0,
    }
}

/// `Value::List` or iterable PyObject -> `Vec<Value>`. `None` if not
/// iterable. Used by filters that need to walk items (random, slice,
/// dictsort, etc.).
fn value_to_list(v: &Value) -> Option<Vec<Value>> {
    match v {
        Value::List(items) => Some(items.clone()),
        Value::PyObject(obj) => Python::attach(|py| {
            let bound = obj.bind(py);
            let iter = bound.try_iter().ok()?;
            let mut out = Vec::new();
            for v in iter.flatten() {
                out.push(Value::from(&v));
            }
            Some(out)
        }),
        _ => None,
    }
}

// String filters

/// `addslashes`: backslashes before `\`, `'`, `"`.
fn filter_addslashes(value: &Value, _args: &[Value], _autoescape: bool) -> Value {
    // Single-pass copy; avoids three chained `String::replace` allocs.
    let cow = value_as_str_cow(value);
    let s: &str = &cow;
    let bytes = s.as_bytes();
    let extra = bytes
        .iter()
        .filter(|&&b| matches!(b, b'\\' | b'\'' | b'"'))
        .count();
    if extra == 0 {
        return preserve_safety(value, cow.into_owned());
    }
    let mut out = String::with_capacity(bytes.len() + extra);
    let mut last = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        let replacement: &str = match b {
            b'\\' => "\\\\",
            b'\'' => "\\'",
            b'"' => "\\\"",
            _ => continue,
        };
        // Boundary is on an ASCII byte, so &s[last..i] is valid UTF-8.
        out.push_str(&s[last..i]);
        out.push_str(replacement);
        last = i + 1;
    }
    out.push_str(&s[last..]);
    preserve_safety(value, out)
}

/// `capfirst`: uppercase the first character.
fn filter_capfirst(value: &Value, _args: &[Value], _autoescape: bool) -> Value {
    let cow = value_as_str_cow(value);
    let s: &str = &cow;
    if s.is_empty() {
        return preserve_safety(value, String::new());
    }
    // ASCII fast path.
    let first_byte = s.as_bytes()[0];
    if first_byte < 0x80 {
        if !first_byte.is_ascii_lowercase() {
            return preserve_safety(value, s.to_owned());
        }
        let mut out = String::with_capacity(s.len());
        out.push((first_byte - b'a' + b'A') as char);
        out.push_str(&s[1..]);
        return preserve_safety(value, out);
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    let mut out = String::with_capacity(s.len());
    for uc in first.to_uppercase() {
        out.push(uc);
    }
    out.push_str(chars.as_str());
    preserve_safety(value, out)
}

/// `center`: center in a field of given width.
fn filter_center(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    let s = value_to_string(value);
    let width = arg_as_i64(args, 0) as usize;
    let char_count = s.chars().count();
    if char_count >= width {
        return preserve_safety(value, s);
    }
    let total_pad = width - char_count;
    let left_pad = total_pad / 2;
    let right_pad = total_pad - left_pad;
    let mut out = String::with_capacity(width);
    for _ in 0..left_pad {
        out.push(' ');
    }
    out.push_str(&s);
    for _ in 0..right_pad {
        out.push(' ');
    }
    preserve_safety(value, out)
}

/// `cut`: remove occurrences of `arg`. Output stays safe unless `arg`
/// is `;` (removing semicolons can break HTML entities).
fn filter_cut(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    let s = value_to_string(value);
    let to_cut = arg_as_string(args, "");
    let out = s.replace(&to_cut, "");
    let input_is_safe = is_safe_value(value);
    if input_is_safe && to_cut != ";" {
        Value::SafeString(out.into())
    } else {
        Value::String(out)
    }
}

/// `escape`: HTML-escape, marking output safe. No-op on SafeData input.
fn filter_escape(value: &Value, _args: &[Value], _autoescape: bool) -> Value {
    if is_safe_value(value) {
        return value.clone();
    }
    let cow = value_as_str_cow(value);
    Value::SafeString(html_escape(&cow).into())
}

/// `escapejs`: escape for use in JavaScript.
fn filter_escapejs(value: &Value, _args: &[Value], _autoescape: bool) -> Value {
    use std::fmt::Write as _;
    let cow = value_as_str_cow(value);
    let s: &str = &cow;
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
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 32 => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            _ => out.push(c),
        }
    }
    Value::SafeString(out.into())
}

/// `force_escape`: HTML-escape, even if already safe.
fn filter_force_escape(value: &Value, _args: &[Value], _autoescape: bool) -> Value {
    let cow = value_as_str_cow(value);
    Value::SafeString(html_escape(&cow).into())
}

/// `ljust`: left-align in a field of given width.
fn filter_ljust(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    let s = value_to_string(value);
    let width = arg_as_i64(args, 0) as usize;
    let char_count = s.chars().count();
    if char_count >= width {
        return preserve_safety(value, s);
    }
    let mut out = s;
    for _ in 0..(width - char_count) {
        out.push(' ');
    }
    preserve_safety(value, out)
}

/// `lower`: convert to lowercase.
fn filter_lower(value: &Value, _args: &[Value], _autoescape: bool) -> Value {
    let out = match value {
        Value::String(s) => s.to_lowercase(),
        Value::SafeString(s) => s.to_lowercase(),
        _ => value_to_string(value).to_lowercase(),
    };
    preserve_safety(value, out)
}

/// `rjust`: right-align in a field of given width.
fn filter_rjust(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    let s = value_to_string(value);
    let width = arg_as_i64(args, 0) as usize;
    let char_count = s.chars().count();
    if char_count >= width {
        return preserve_safety(value, s);
    }
    let mut out = String::with_capacity(width);
    for _ in 0..(width - char_count) {
        out.push(' ');
    }
    out.push_str(&s);
    preserve_safety(value, out)
}

/// `slugify`: URL-friendly slug matching Django's `django.utils.text.slugify`.
/// NFKD-normalizes, strips non-ASCII, lowercases, removes non-word chars
/// (except hyphens), collapses whitespace/hyphens, strips leading/trailing
/// hyphens and underscores.
fn filter_slugify(value: &Value, _args: &[Value], _autoescape: bool) -> Value {
    use unicode_normalization::UnicodeNormalization;

    let s = value_to_string(value);
    // Step 1-2: NFKD normalize, then strip non-ASCII (matching Python's
    // unicodedata.normalize('NFKD').encode('ascii', 'ignore').decode('ascii'))
    let ascii: String = s.nfkd().filter(|c| c.is_ascii()).collect();
    // Step 3: lowercase, remove anything that isn't alphanumeric, whitespace,
    // underscore, or hyphen (matches re.sub(r'[^\w\s-]', '', ...))
    let lower = ascii.to_lowercase();
    let cleaned: String = lower
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace() || *c == '-' || *c == '_')
        .collect();
    // Step 4: collapse whitespace/hyphens into single hyphens
    // (matches re.sub(r'[-\s]+', '-', ...))
    let mut slug = String::with_capacity(cleaned.len());
    let mut prev_was_sep = false;
    for c in cleaned.chars() {
        if c == '-' || c.is_whitespace() {
            if !prev_was_sep {
                slug.push('-');
                prev_was_sep = true;
            }
        } else {
            slug.push(c);
            prev_was_sep = false;
        }
    }
    // Step 5: strip leading/trailing hyphens and underscores
    // (matches .strip('-_'))
    let slug = slug.trim_matches(|c: char| c == '-' || c == '_');
    Value::SafeString(slug.into())
}

/// `striptags`: drop every `<...>` span. Mirrors `strip_tags`, no
/// whitespace insertion or collapsing at tag boundaries.
fn filter_striptags(value: &Value, _args: &[Value], _autoescape: bool) -> Value {
    let s = value_to_string(value);
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    Value::String(out)
}

/// `title`: titlecase, with Django's apostrophe fixup so `"it's"` ->
/// `"It's"` rather than `"It'S"`.
fn filter_title(value: &Value, _args: &[Value], _autoescape: bool) -> Value {
    let s = value_to_string(value);
    let mut out = String::with_capacity(s.len());
    let mut capitalize_next = true;
    for c in s.chars() {
        if c.is_whitespace() || !c.is_alphanumeric() {
            capitalize_next = true;
            out.push(c);
        } else if capitalize_next {
            for uc in c.to_uppercase() {
                out.push(uc);
            }
            capitalize_next = false;
        } else {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
        }
    }

    // Apostrophe fixup: lowercase the letter after `'` if both sides are
    // ASCII letters. Pattern: `([a-z])'([A-Z])` -> lowercase group 2.
    let chars: Vec<char> = out.chars().collect();
    let mut fixed = String::with_capacity(out.len());
    let len = chars.len();
    let mut i = 0;
    while i < len {
        if i + 2 < len
            && chars[i].is_ascii_lowercase()
            && chars[i + 1] == '\''
            && chars[i + 2].is_ascii_uppercase()
        {
            fixed.push(chars[i]);
            fixed.push('\'');
            for lc in chars[i + 2].to_lowercase() {
                fixed.push(lc);
            }
            i += 3;
        } else {
            fixed.push(chars[i]);
            i += 1;
        }
    }

    preserve_safety(value, fixed)
}

/// `truncatechars`: truncate to N chars, appending U+2026 ellipsis.
/// Truncation length includes the ellipsis (`truncatechars:3` on
/// "Testing" gives "Te...").
fn filter_truncatechars(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    let s = value_to_string(value);
    let max_len = match args.first().and_then(coerce_to_i64) {
        Some(n) => n,
        None => return preserve_safety(value, s),
    };
    if max_len <= 0 {
        return Value::String("\u{2026}".to_owned());
    }
    let max_len = max_len as usize;
    let char_count = s.chars().count();
    if char_count <= max_len {
        return preserve_safety(value, s);
    }
    if max_len == 1 {
        return Value::String("\u{2026}".to_owned());
    }
    let truncated: String = s.chars().take(max_len - 1).collect();
    let out = format!("{truncated}\u{2026}");
    preserve_safety(value, out)
}

/// `truncatewords`: truncate to N words, appending " ...".
fn filter_truncatewords(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    let s = value_to_string(value);
    let max_words = arg_as_i64(args, 0);
    if max_words <= 0 {
        return Value::String(String::new());
    }
    let max_words = max_words as usize;
    let words: Vec<&str> = s.split_whitespace().collect();
    if words.len() <= max_words {
        return preserve_safety(value, s);
    }
    let truncated = words[..max_words].join(" ");
    let out = format!("{truncated} \u{2026}");
    preserve_safety(value, out)
}

/// `upper`: uppercase. `is_safe=False` in Django because uppercasing
/// can break HTML entities (`&amp;` -> `&AMP;`).
fn filter_upper(value: &Value, _args: &[Value], _autoescape: bool) -> Value {
    match value {
        Value::String(s) => Value::String(s.to_uppercase()),
        Value::SafeString(s) => Value::String(s.to_uppercase()),
        _ => Value::String(value_to_string(value).to_uppercase()),
    }
}

/// `wordcount`: count whitespace-separated words.
fn filter_wordcount(value: &Value, _args: &[Value], _autoescape: bool) -> Value {
    let s = value_to_string(value);
    Value::Int(s.split_whitespace().count() as i64)
}

/// `wordwrap`: wrap text at given width.
fn filter_wordwrap(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    let s = value_to_string(value);
    let width = arg_as_i64(args, 0) as usize;
    if width == 0 {
        return preserve_safety(value, s);
    }
    let mut out = String::with_capacity(s.len() + s.len() / width);
    let mut col = 0;
    for word in s.split(' ') {
        let word_len = word.chars().count();
        if col > 0 && col + 1 + word_len > width {
            out.push('\n');
            col = 0;
        } else if col > 0 {
            out.push(' ');
            col += 1;
        }
        out.push_str(word);
        col += word_len;
    }
    preserve_safety(value, out)
}

// HTML filters

/// `linebreaks`: newlines to `<p>` and `<br>`.
fn filter_linebreaks(value: &Value, _args: &[Value], autoescape: bool) -> Value {
    let s = value_to_string(value);
    let s = if autoescape && !is_safe_value(value) {
        html_escape(&s)
    } else {
        s
    };
    let s = s.replace("\r\n", "\n").replace('\r', "\n");
    let paragraphs: Vec<&str> = s.split("\n\n").collect();
    let mut out = String::new();
    for para in &paragraphs {
        let trimmed = para.trim();
        if trimmed.is_empty() {
            continue;
        }
        let with_br = trimmed.replace('\n', "<br>");
        out.push_str(&format!("<p>{with_br}</p>\n\n"));
    }
    // Remove trailing whitespace.
    let out = out.trim_end().to_owned();
    Value::SafeString(out.into())
}

/// `linebreaksbr` - converts newlines to `<br>` tags.
fn filter_linebreaksbr(value: &Value, _args: &[Value], autoescape: bool) -> Value {
    let s = value_to_string(value);
    let s = if autoescape && !is_safe_value(value) {
        html_escape(&s)
    } else {
        s
    };
    let s = s.replace("\r\n", "\n").replace('\r', "\n");
    let out = s.replace('\n', "<br>");
    Value::SafeString(out.into())
}

/// `linenumbers` - prepends line numbers to each line.
///
/// Mirrors Django's implementation exactly: split on `\n` (NOT
/// `str::lines`, which discards the trailing empty element for `""`),
/// zero-pad the index to the width of the line count, and escape each
/// line content unless autoescape is off or the input is safe.
fn filter_linenumbers(value: &Value, _args: &[Value], autoescape: bool) -> Value {
    let s = value_to_string(value);
    // `split('\n').collect()` matches Python's `"".split("\n") == [""]`
    // semantics: empty input produces a single empty line, so we still
    // emit `"1. "`. Rust's `str::lines()` returns an empty iterator
    // for empty input - a different, incompatible behaviour.
    let lines: Vec<&str> = s.split('\n').collect();
    let width = lines.len().to_string().len();
    let escape_input = autoescape && !is_safe_value(value);
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        // Django uses zero-padded format (`"%0Nd"`), not space-padded.
        let escaped: String;
        let rendered_line = if escape_input {
            escaped = html_escape(line);
            escaped.as_str()
        } else {
            *line
        };
        out.push_str(&format!(
            "{:0width$}. {rendered_line}",
            i + 1,
            width = width
        ));
    }
    Value::SafeString(out.into())
}

/// `safe`: mark value safe (no auto-escaping).
fn filter_safe(value: &Value, _args: &[Value], _autoescape: bool) -> Value {
    match value {
        Value::String(s) => Value::SafeString(s.clone().into()),
        Value::SafeString(s) => Value::SafeString(s.clone()),
        _ => Value::SafeString(value_to_string(value).into()),
    }
}

/// `safeseq`: mark each item in a sequence safe.
fn filter_safeseq(value: &Value, _args: &[Value], _autoescape: bool) -> Value {
    if let Some(items) = value_to_list(value) {
        let safe_items: Vec<Value> = items
            .into_iter()
            .map(|item| Value::SafeString(value_to_string(&item).into()))
            .collect();
        return Value::List(safe_items);
    }
    Value::SafeString(value_to_string(value).into())
}

/// `urlize`: delegates to Django (URL detection too complex to port).
fn filter_urlize(value: &Value, args: &[Value], autoescape: bool) -> Value {
    call_django_urlize("urlize", value, args, autoescape)
}

/// `urlizetrunc`: urlize with link text truncated to N chars.
fn filter_urlizetrunc(value: &Value, args: &[Value], autoescape: bool) -> Value {
    call_django_urlize("urlizetrunc", value, args, autoescape)
}

fn call_django_urlize(filter_name: &str, value: &Value, args: &[Value], autoescape: bool) -> Value {
    Python::attach(|py| {
        let module = match py.import(pyo3::types::PyString::new(
            py,
            "django.template.defaultfilters",
        )) {
            Ok(m) => m,
            Err(_) => return value.clone(),
        };
        let func = match module.getattr(filter_name) {
            Ok(f) => f,
            Err(_) => return value.clone(),
        };

        let py_value = value.to_pyobject(py);

        let result = if filter_name == "urlizetrunc" && !args.is_empty() {
            let py_arg = args[0].to_pyobject(py);
            let kwargs = pyo3::types::PyDict::new(py);
            kwargs.set_item("autoescape", autoescape).ok();
            func.call((py_value, py_arg), Some(&kwargs))
        } else {
            let kwargs = pyo3::types::PyDict::new(py);
            kwargs.set_item("autoescape", autoescape).ok();
            func.call((py_value,), Some(&kwargs))
        };

        match result {
            Ok(r) => {
                if let Ok(s) = r.extract::<String>() {
                    Value::SafeString(s.into())
                } else {
                    Value::from(&r)
                }
            }
            Err(_) => value.clone(),
        }
    })
}

// List filters

/// `first`: first item of a list or first char of a string.
fn filter_first(value: &Value, _args: &[Value], _autoescape: bool) -> Value {
    if let Some(s) = value.as_str() {
        return s.chars().next().map_or(Value::String(String::new()), |c| {
            Value::String(c.to_string())
        });
    }
    match value {
        Value::List(items) => items
            .first()
            .cloned()
            .unwrap_or(Value::String(String::new())),
        Value::PyObject(obj) => Python::attach(|py| {
            obj.bind(py)
                .get_item(0)
                .ok()
                .map(|v| Value::from(&v))
                .unwrap_or_else(|| Value::String(String::new()))
        }),
        _ => Value::String(String::new()),
    }
}

/// `join`: list with separator. Items AND separator are autoescaped
/// when autoescape is on and they're not safe.
fn filter_join(value: &Value, args: &[Value], autoescape: bool) -> Value {
    let sep_value = args.first();
    let sep_raw = sep_value.map(value_to_string).unwrap_or_default();
    let sep_is_safe = sep_value.is_none_or(is_safe_value);
    let sep = if autoescape && !sep_is_safe {
        html_escape(&sep_raw)
    } else {
        sep_raw
    };

    let items: Option<Vec<Value>> = match value {
        Value::List(items) => Some(items.clone()),
        // Django joins any iterable; iterating a str yields its plain
        // (escapable) chars.
        Value::String(s) => Some(s.chars().map(|c| Value::String(c.to_string())).collect()),
        Value::SafeString(s) => {
            Some(s.chars().map(|c| Value::String(c.to_string())).collect())
        }
        Value::PyObject(obj) => Python::attach(|py| {
            let bound = obj.bind(py);
            if let Ok(iter) = bound.try_iter() {
                let mut result = Vec::new();
                for v in iter.flatten() {
                    result.push(Value::from(&v));
                }
                Some(result)
            } else {
                None
            }
        }),
        _ => None,
    };

    match items {
        Some(items) => {
            let parts: Vec<String> = items
                .iter()
                .map(|item| {
                    let s = value_to_string(item);
                    if autoescape && !is_safe_value(item) {
                        html_escape(&s)
                    } else {
                        s
                    }
                })
                .collect();
            let out = parts.join(&sep);
            Value::SafeString(out.into())
        }
        None => preserve_safety(value, value_to_string(value)),
    }
}

/// `last`: last item of a list.
fn filter_last(value: &Value, _args: &[Value], _autoescape: bool) -> Value {
    if let Some(s) = value.as_str() {
        return s
            .chars()
            .next_back()
            .map_or(Value::String(String::new()), |c| {
                Value::String(c.to_string())
            });
    }
    match value {
        Value::List(items) => items
            .last()
            .cloned()
            .unwrap_or(Value::String(String::new())),
        Value::PyObject(obj) => Python::attach(|py| {
            let bound = obj.bind(py);
            let len = match bound.len() {
                Ok(n) if n > 0 => n,
                _ => return Value::String(String::new()),
            };
            bound
                .get_item(len - 1)
                .ok()
                .map(|v| Value::from(&v))
                .unwrap_or_else(|| Value::String(String::new()))
        }),
        _ => Value::String(String::new()),
    }
}

/// `length`: length of list or string.
fn filter_length(value: &Value, _args: &[Value], _autoescape: bool) -> Value {
    Value::Int(value_length(value) as i64)
}

/// `length_is`: whether length equals arg.
fn filter_length_is(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    let target = arg_as_i64(args, 0);
    Value::Bool(value_length(value) as i64 == target)
}

/// `random`: pick a random element from the list. Delegates to Python's
/// `random.choice` so that `random.seed()` in tests is respected.
fn filter_random(value: &Value, _args: &[Value], _autoescape: bool) -> Value {
    match value_to_list(value) {
        Some(items) if !items.is_empty() => {
            let idx = Python::attach(|py| -> usize {
                let random = py.import("random").expect("random module");
                let len = items.len();
                random
                    .call_method1("randrange", (len,))
                    .and_then(|v| v.extract::<usize>())
                    .unwrap_or(0)
            });
            items[idx.min(items.len() - 1)].clone()
        }
        _ => Value::String(String::new()),
    }
}

/// `slice`: Python-style `[start:stop:step]`.
fn filter_slice(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    let slice_str = arg_as_string(args, ":");
    let items: Vec<Value> = if let Some(s) = value.as_str() {
        s.chars().map(|c| Value::String(c.to_string())).collect()
    } else {
        match value {
            Value::List(items) => items.clone(),
            Value::PyObject(_) => match value_to_list(value) {
                Some(items) => items,
                None => return value.clone(),
            },
            _ => return value.clone(),
        }
    };

    let len = items.len() as i64;
    let parts: Vec<&str> = slice_str.splitn(3, ':').collect();

    let parse_idx = |s: &str| -> Option<i64> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            trimmed.parse::<i64>().ok()
        }
    };

    let explicit_start = parts.first().and_then(|s| parse_idx(s));
    let explicit_stop = parts.get(1).and_then(|s| parse_idx(s));
    let explicit_step = parts.get(2).and_then(|s| parse_idx(s));

    let step = explicit_step.unwrap_or(1);
    let step = if step == 0 { 1 } else { step };

    let clamp_pos = |v: i64| -> i64 { if v < 0 { (len + v).max(0) } else { v.min(len) } };
    let clamp_neg = |v: i64| -> i64 {
        // Negative step: indices walk downward to -1 as terminator.
        let v = if v < 0 { len + v } else { v };
        v.clamp(-1, len - 1)
    };

    // Python slice defaults: positive step starts 0/len, negative step
    // starts len-1/-1 (so index 0 is included).
    let (start, stop) = if step > 0 {
        (
            explicit_start.map_or(0, clamp_pos),
            explicit_stop.map_or(len, clamp_pos),
        )
    } else {
        (
            explicit_start.map_or(len - 1, clamp_neg),
            explicit_stop.map_or(-1, clamp_neg),
        )
    };

    let mut result = Vec::new();
    if step > 0 {
        let mut i = start;
        while i < stop {
            if (i as usize) < items.len() {
                result.push(items[i as usize].clone());
            }
            i += step;
        }
    } else {
        let mut i = start;
        while i > stop {
            if i >= 0 && (i as usize) < items.len() {
                result.push(items[i as usize].clone());
            }
            i += step;
        }
    }

    if value.as_str().is_some() {
        let s: String = result.iter().map(value_to_string).collect();
        return preserve_safety(value, s);
    }
    Value::List(result)
}

/// `dictsort`: delegates to Django's `dictsort` filter for correct
/// dotted-key resolution on Python objects.
fn filter_dictsort(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    _dictsort_via_python(value, args, false)
}

/// `dictsortreversed`: delegates to Django's `dictsortreversed` filter.
fn filter_dictsortreversed(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    _dictsort_via_python(value, args, true)
}

fn _dictsort_via_python(value: &Value, args: &[Value], reversed: bool) -> Value {
    Python::attach(|py| {
        let filters = py
            .import("django.template.defaultfilters")
            .expect("defaultfilters");
        let func_name = if reversed {
            "dictsortreversed"
        } else {
            "dictsort"
        };
        let py_val = value.to_pyobject(py);
        let py_arg = if args.is_empty() {
            "".into_pyobject(py).expect("str").into_any().unbind()
        } else {
            args[0].to_pyobject(py)
        };
        match filters.call_method1(func_name, (py_val.bind(py), py_arg.bind(py))) {
            Ok(result) => Value::from(&result),
            Err(_) => value.clone(),
        }
    })
}

/// `unordered_list`: nested list -> HTML `<li>` items (no outer `<ul>`).
/// Each level indented with tabs.
fn filter_unordered_list(value: &Value, _args: &[Value], autoescape: bool) -> Value {
    /// Sublist iff `v` is a non-empty list / iterable PyObject.
    fn as_sublist(v: &Value) -> Option<Vec<Value>> {
        match v {
            Value::List(sub) if !sub.is_empty() => Some(sub.clone()),
            Value::PyObject(_) => {
                let sub = value_to_list(v)?;
                if sub.is_empty() { None } else { Some(sub) }
            }
            _ => None,
        }
    }

    fn render_items(items: &[Value], indent: usize, autoescape: bool) -> String {
        let tabs = "\t".repeat(indent);
        let mut lines: Vec<String> = Vec::new();
        let mut i = 0;
        while i < items.len() {
            let item = &items[i];
            if let Some(sub) = as_sublist(item) {
                let child = render_items(&sub, indent + 1, autoescape);
                lines.push(format!(
                    "{tabs}<li>\n{tabs}<ul>\n{child}\n{tabs}</ul>\n{tabs}</li>"
                ));
                i += 1;
            } else {
                let s = value_to_string(item);
                let s = if autoescape && !is_safe_value(item) {
                    html_escape(&s)
                } else {
                    s
                };

                if i + 1 < items.len()
                    && let Some(sub) = as_sublist(&items[i + 1]) {
                        let child = render_items(&sub, indent + 1, autoescape);
                        lines.push(format!(
                            "{tabs}<li>{s}\n{tabs}<ul>\n{child}\n{tabs}</ul>\n{tabs}</li>"
                        ));
                        i += 2;
                        continue;
                    }

                lines.push(format!("{tabs}<li>{s}</li>"));
                i += 1;
            }
        }
        lines.join("\n")
    }

    match value_to_list(value) {
        Some(items) => {
            let out = render_items(&items, 1, autoescape);
            Value::SafeString(out.into())
        }
        None => Value::SafeString(String::new().into()),
    }
}

// Integer / Math filters

/// `add`: numeric, then string, then list concat. PyObject falls back
/// to Python's `+` for tuples, dates, etc.
fn filter_add(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    let arg = args.first().cloned().unwrap_or(Value::None);

    if let (Some(a), Some(b)) = (coerce_to_i64(value), coerce_to_i64(&arg)) {
        return Value::Int(a + b);
    }
    if let (Some(a), Some(b)) = (coerce_to_f64(value), coerce_to_f64(&arg)) {
        return Value::Float(a + b);
    }

    match (value, &arg) {
        (Value::String(_) | Value::SafeString(_), Value::String(_) | Value::SafeString(_)) => {
            let a = value_to_string(value);
            let b = value_to_string(&arg);
            return preserve_safety(value, format!("{a}{b}"));
        }
        (Value::List(a), Value::List(b)) => {
            let mut result = a.clone();
            result.extend(b.iter().cloned());
            return Value::List(result);
        }
        _ => {}
    }

    // PyObject (tuples, dates+timedelta, str+lazy_string) via Python `+`.
    let has_pyobj = matches!(value, Value::PyObject(_)) || matches!(&arg, Value::PyObject(_));
    if has_pyobj {
        let result = Python::attach(|py| {
            let py_val = value.to_pyobject(py);
            let py_arg = arg.to_pyobject(py);
            match py_val.bind(py).add(py_arg.bind(py)) {
                Ok(result) => Some(Value::from(&result)),
                Err(_) => None,
            }
        });
        if let Some(v) = result {
            return v;
        }
    }

    // Django's `add` returns "" when neither `int(value) + int(arg)` nor
    // `value + arg` (str/list/tuple concat, date+timedelta, ...) succeeds.
    Value::String(String::new())
}

/// `divisibleby`: value divisible by arg.
fn filter_divisibleby(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    let v = coerce_to_i64(value).unwrap_or(0);
    let d = arg_as_i64(args, 1);
    if d == 0 {
        return Value::Bool(false);
    }
    Value::Bool(v % d == 0)
}

/// `filesizeformat`: bytes -> human-readable size.
fn filter_filesizeformat(value: &Value, _args: &[Value], _autoescape: bool) -> Value {
    let bytes = coerce_to_f64(value).unwrap_or(0.0).abs();

    let (size, unit) = if bytes < 1024.0 {
        (bytes, "bytes")
    } else if bytes < 1024.0 * 1024.0 {
        (bytes / 1024.0, "KB")
    } else if bytes < 1024.0 * 1024.0 * 1024.0 {
        (bytes / (1024.0 * 1024.0), "MB")
    } else if bytes < 1024.0 * 1024.0 * 1024.0 * 1024.0 {
        (bytes / (1024.0 * 1024.0 * 1024.0), "GB")
    } else if bytes < 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0 {
        (bytes / (1024.0 * 1024.0 * 1024.0 * 1024.0), "TB")
    } else {
        (bytes / (1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0), "PB")
    };

    let formatted = if unit == "bytes" {
        format!("{}\u{a0}{unit}", size as u64)
    } else {
        format!("{size:.1}\u{a0}{unit}")
    };
    Value::String(formatted)
}

/// `floatformat`: format a float to N decimal places.
/// - No arg / `-1`: one decimal for non-whole, integer for whole.
/// - Positive N: always N decimals.
/// - Negative N: N decimals for non-whole, integer for whole.
/// - Suffix `g`: thousand separators in integer part.
/// - Suffix `u`: skip localisation (no-op here; just stripped).
fn filter_floatformat(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    let n = match coerce_to_f64(value) {
        Some(v) => v,
        None => return Value::String(String::new()),
    };

    let raw = arg_as_string(args, "-1");

    let (mut arg_body, group, _unlocalize): (&str, bool, bool) = {
        let a = raw.as_str();
        let (a, group) = if a.ends_with('g') && !a.starts_with('g') {
            (&a[..a.len() - 1], true)
        } else {
            (a, false)
        };
        let (a, unloc) = if a.ends_with('u') && !a.starts_with('u') {
            (&a[..a.len() - 1], true)
        } else {
            (a, false)
        };
        (a, group, unloc)
    };
    if arg_body.is_empty() {
        arg_body = "-1";
    }

    let d: i64 = arg_body.parse().unwrap_or(-1);
    let abs_d = d.unsigned_abs() as usize;
    let is_whole = n.fract() == 0.0;

    let formatted = if d < 0 && is_whole {
        format!("{:.0}", n)
    } else {
        format!("{n:.prec$}", prec = abs_d)
    };

    let formatted = if group {
        insert_thousand_separators(&formatted)
    } else {
        formatted
    };

    Value::String(formatted)
}

/// Insert `,` every 3 digits in the integer portion.
fn insert_thousand_separators(s: &str) -> String {
    let (int_part, dec_part) = match s.find('.') {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    };
    let (sign, digits) = if let Some(rest) = int_part.strip_prefix('-') {
        ("-", rest)
    } else {
        ("", int_part)
    };
    if digits.len() <= 3 {
        return format!("{sign}{digits}{dec_part}");
    }
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 2);
    let head = digits.len() % 3;
    if head > 0 {
        out.push_str(&digits[..head]);
        if digits.len() > head {
            out.push(',');
        }
    }
    for (i, chunk) in digits.as_bytes()[head..].chunks(3).enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(std::str::from_utf8(chunk).unwrap());
    }
    format!("{sign}{out}{dec_part}")
}

/// `get_digit`: Nth digit from the right (1-indexed).
fn filter_get_digit(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    let v = coerce_to_i64(value);
    let digit_pos = arg_as_i64(args, 0);

    match v {
        Some(n) if digit_pos > 0 => {
            let s = n.abs().to_string();
            let pos = digit_pos as usize;
            if pos > s.len() {
                return Value::Int(0);
            }
            let idx = s.len() - pos;
            let ch = s.chars().nth(idx).unwrap_or('0');
            Value::Int(ch.to_digit(10).unwrap_or(0) as i64)
        }
        _ => value.clone(),
    }
}

/// `pluralize`: plural suffix. `count|pluralize`,
/// `count|pluralize:"es"`, `count|pluralize:"y,ies"`.
fn filter_pluralize(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    let arg = arg_as_string(args, "s");
    let (singular, plural) = if let Some((s, p)) = arg.split_once(',') {
        (s.to_owned(), p.to_owned())
    } else {
        (String::new(), arg)
    };

    let count = match value {
        Value::Int(n) => *n,
        Value::Float(f) => *f as i64,
        Value::List(items) => items.len() as i64,
        Value::String(s) => s.parse::<i64>().unwrap_or(0),
        Value::SafeString(s) => s.parse::<i64>().unwrap_or(0),
        // Sequences pluralize by `len(value)`; without this,
        // `{{ qs|pluralize }}` always returns plural.
        Value::PyObject(obj) => {
            pyo3::Python::attach(|py| obj.bind(py).len().ok().map(|n| n as i64).unwrap_or(0))
        }
        _ => 0,
    };

    if count == 1 {
        Value::String(singular)
    } else {
        Value::String(plural)
    }
}

// Logic filters

/// `default`: arg if value is falsy.
fn filter_default(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    if is_truthy(value) {
        value.clone()
    } else {
        args.first()
            .cloned()
            .unwrap_or(Value::String(String::new()))
    }
}

/// `default_if_none`: arg if value is None.
fn filter_default_if_none(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    match value {
        Value::None => args
            .first()
            .cloned()
            .unwrap_or(Value::String(String::new())),
        _ => value.clone(),
    }
}

/// `yesno` - maps True/False/None to custom strings.
///
/// Argument: `"yes,no,maybe"` (the third is optional).
fn filter_yesno(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    let mapping = arg_as_string(args, "yes,no,maybe");
    let parts: Vec<&str> = mapping.split(',').collect();
    let (yes, no, maybe) = match parts.len() {
        1 => (parts[0], "no", "maybe"),
        2 => (parts[0], parts[1], parts[1]), // maybe defaults to no
        _ => (parts[0], parts[1], parts[2]),
    };

    match value {
        Value::None => Value::String(maybe.to_owned()),
        _ if is_truthy(value) => Value::String(yes.to_owned()),
        _ => Value::String(no.to_owned()),
    }
}

// Date/time filters (delegate to Python at runtime)

use pyo3::prelude::*;

/// Invoke a Django filter via Python; falls back to the input on error.
fn call_django_filter(
    module_path: &'static str,
    func_name: &'static str,
    value: &Value,
    args: &[Value],
) -> Value {
    Python::attach(|py| {
        let func = match cached_django_callable(py, module_path, func_name) {
            Some(f) => f,
            None => return value.clone(),
        };

        let py_value = value.to_pyobject(py);

        let result = if args.is_empty() {
            func.call1(py, (py_value,))
        } else {
            let py_arg = args[0].to_pyobject(py);
            func.call1(py, (py_value, py_arg))
        };

        match result {
            Ok(r) => {
                let r = r.bind(py);
                if let Ok(s) = r.extract::<String>() {
                    Value::SafeString(s.into())
                } else {
                    Value::from(r)
                }
            }
            Err(_) => value.clone(),
        }
    })
}

/// Cache `<module>.<func>` callables, keyed by literal pointer addresses.
fn cached_django_callable(
    py: Python<'_>,
    module_path: &'static str,
    func_name: &'static str,
) -> Option<Py<PyAny>> {
    use once_cell::sync::Lazy;
    use std::collections::HashMap;
    use std::sync::Mutex;

    type Cache = Mutex<HashMap<(usize, usize), Py<PyAny>>>;
    static CACHE: Lazy<Cache> = Lazy::new(|| Mutex::new(HashMap::new()));

    let key = (module_path.as_ptr() as usize, func_name.as_ptr() as usize);

    if let Ok(guard) = CACHE.lock()
        && let Some(f) = guard.get(&key) {
            return Some(f.clone_ref(py));
        }

    let module = py.import(module_path).ok()?;
    let func = module.getattr(func_name).ok()?;
    let unbound = func.clone().unbind();

    if let Ok(mut guard) = CACHE.lock() {
        guard.insert(key, unbound.clone_ref(py));
    }
    Some(unbound)
}

/// `date`: format a date. Mirrors `defaultfilters.date`.
fn filter_date(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    // `if value in (None, ""): return ""` without crossing into Python.
    if matches!(value, Value::None) {
        return Value::SafeString(String::new().into());
    }
    if let Some(s) = value.as_str()
        && s.is_empty() {
            return Value::SafeString(String::new().into());
        }

    Python::attach(|py| {
        // No-arg form needs DATE_FORMAT from settings; defer to Django.
        let format_str = match args.first() {
            Some(v) if v.as_str().is_some() => Cow::Borrowed(v.as_str().unwrap()),
            Some(other) => Cow::Owned(other.to_string()),
            None => {
                return call_django_filter("django.template.defaultfilters", "date", value, args);
            }
        };

        // PyObject only: non-Py values aren't valid date input.
        let py_value = match value {
            Value::PyObject(obj) => obj.bind(py).clone(),
            _ => return Value::SafeString(String::new().into()),
        };

        // Rust fast path for common format chars (~10x speedup).
        if let Some(rendered) = date_format::try_format(py, &py_value, &format_str) {
            return Value::SafeString(rendered.into());
        }

        // Slow path: Django's `dateformat.format` for full coverage
        // (timezone specs, ISO week, etc.).
        let format_fn = match cached_django_callable(py, "django.utils.dateformat", "format") {
            Some(f) => f,
            None => return value.clone(),
        };
        let fmt_py = pyo3::types::PyString::new(py, &format_str);
        match format_fn.call1(py, (py_value, fmt_py)) {
            Ok(r) => {
                let r = r.bind(py);
                if let Ok(s) = r.extract::<String>() {
                    Value::SafeString(s.into())
                } else {
                    Value::from(r)
                }
            }
            // AttributeError: value lacks the needed `__format__`.
            // Django's wrapper returns "".
            Err(_) => Value::SafeString(String::new().into()),
        }
    })
}

/// `time`: delegates to `defaultfilters.time`.
fn filter_time(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    call_django_filter("django.template.defaultfilters", "time", value, args)
}

/// `timesince`: delegates to `defaultfilters.timesince_filter`.
fn filter_timesince(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    call_django_filter(
        "django.template.defaultfilters",
        "timesince_filter",
        value,
        args,
    )
}

/// `timeuntil`: delegates to `defaultfilters.timeuntil_filter`.
fn filter_timeuntil(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    call_django_filter(
        "django.template.defaultfilters",
        "timeuntil_filter",
        value,
        args,
    )
}

// Encoding filters

/// `iriencode`: IRI-encodes (percent-encodes non-ASCII).
fn filter_iriencode(value: &Value, _args: &[Value], _autoescape: bool) -> Value {
    let s = value_to_string(value);
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() && (c.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(c)) {
            out.push(c);
        } else {
            let mut buf = [0u8; 4];
            let encoded = c.encode_utf8(&mut buf);
            for byte in encoded.bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    preserve_safety(value, out)
}

/// `urlencode`: URL-encode. By default encodes everything except `/`.
fn filter_urlencode(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    let s = value_to_string(value);
    let safe_chars = arg_as_string(args, "/");
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || "-_.~".contains(c) || safe_chars.contains(c) {
            out.push(c);
        } else {
            let mut buf = [0u8; 4];
            let encoded = c.encode_utf8(&mut buf);
            for byte in encoded.bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    Value::SafeString(out.into())
}

// Other filters

/// `json_script`: wrap value in `<script type="application/json">`.
fn filter_json_script(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    let element_id = args.first().map(value_to_string);
    let json_str = value_to_json(value);
    // Escape `</`, `<!--`, `-->` for safe HTML embedding.
    let safe_json = json_str
        .replace('<', "\\u003C")
        .replace('>', "\\u003E")
        .replace('&', "\\u0026");

    let id_attr = match &element_id {
        Some(id) if !id.is_empty() => format!(" id=\"{}\"", html_escape(id)),
        _ => String::new(),
    };
    let out = format!("<script{id_attr} type=\"application/json\">{safe_json}</script>");
    Value::SafeString(out.into())
}

/// Minimal JSON serialization.
fn value_to_json(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => {
                    out.push_str(&format!("\\u{:04x}", c as u32));
                }
                _ => out.push(c),
            }
        }
        out.push('"');
        return out;
    }
    match v {
        Value::None => "null".to_owned(),
        Value::Bool(true) => "true".to_owned(),
        Value::Bool(false) => "false".to_owned(),
        Value::Int(n) => n.to_string(),
        Value::Float(n) => {
            if n.is_finite() {
                format!("{n}")
            } else {
                "null".to_owned()
            }
        }
        Value::List(items) => {
            let mut out = String::from("[");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&value_to_json(item));
            }
            out.push(']');
            out
        }
        Value::Dict(map) => {
            let mut out = String::from("{");
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&format!(
                    "\"{}\"",
                    k.replace('\\', "\\\\").replace('"', "\\\"")
                ));
                out.push_str(": ");
                out.push_str(&value_to_json(v));
            }
            out.push('}');
            out
        }
        // PyObject: serialise dicts/lists/tuples; everything else -> null.
        Value::PyObject(obj) => Python::attach(|py| {
            use pyo3::types::{PyDict, PyList, PyTuple};
            let bound = obj.bind(py);
            if let Ok(d) = bound.cast::<PyDict>() {
                let mut out = String::from("{");
                let mut first = true;
                for (k, v) in d.iter() {
                    if !first {
                        out.push_str(", ");
                    }
                    first = false;
                    let key_str = k.extract::<String>().unwrap_or_default();
                    out.push_str(&format!(
                        "\"{}\"",
                        key_str.replace('\\', "\\\\").replace('"', "\\\"")
                    ));
                    out.push_str(": ");
                    let v_val = Value::from(&v);
                    out.push_str(&value_to_json(&v_val));
                }
                out.push('}');
                out
            } else if let Ok(list) = bound.cast::<PyList>() {
                let mut out = String::from("[");
                for (i, item) in list.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    let item_val = Value::from(&item);
                    out.push_str(&value_to_json(&item_val));
                }
                out.push(']');
                out
            } else if let Ok(tup) = bound.cast::<PyTuple>() {
                let mut out = String::from("[");
                for (i, item) in tup.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    let item_val = Value::from(&item);
                    out.push_str(&value_to_json(&item_val));
                }
                out.push(']');
                out
            } else {
                // Django uses DjangoJSONEncoder; we approximate with null.
                "null".to_owned()
            }
        }),
        Value::String(_) | Value::SafeString(_) => unreachable!("strings handled above"),
    }
}

/// `make_list`: string -> chars, integer -> digits.
fn filter_make_list(value: &Value, _args: &[Value], _autoescape: bool) -> Value {
    // Django `defaultfilters.make_list`: `list(str(value))`.
    let s = value_to_string(value);
    Value::List(s.chars().map(|c| Value::String(c.to_string())).collect())
}

/// `phone2numeric`: letter -> digit on a phone keypad.
fn filter_phone2numeric(value: &Value, _args: &[Value], _autoescape: bool) -> Value {
    let s = value_to_string(value);
    let out: String = s
        .chars()
        .map(|c| match c.to_ascii_uppercase() {
            'A' | 'B' | 'C' => '2',
            'D' | 'E' | 'F' => '3',
            'G' | 'H' | 'I' => '4',
            'J' | 'K' | 'L' => '5',
            'M' | 'N' | 'O' => '6',
            'P' | 'Q' | 'R' | 'S' => '7',
            'T' | 'U' | 'V' => '8',
            'W' | 'X' | 'Y' | 'Z' => '9',
            _ => c,
        })
        .collect();
    preserve_safety(value, out)
}

/// `pprint`: delegates to Python's `pprint.pformat` for correct output.
fn filter_pprint(value: &Value, _args: &[Value], _autoescape: bool) -> Value {
    let out = Python::attach(|py| {
        let pprint = py.import("pprint").expect("pprint module");
        let py_val = value.to_pyobject(py);
        pprint
            .call_method1("pformat", (py_val.bind(py),))
            .and_then(|r| r.extract::<String>())
            .unwrap_or_else(|_| format!("{value:#?}"))
    });
    Value::String(out)
}

/// `stringformat`: Python `%`-style formatting. Supports `s`, `d`, `i`,
/// `f`, `e`, `g`, with width/precision.
fn filter_stringformat(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    let fmt = arg_as_string(args, "s");
    let last_char = fmt.chars().last().unwrap_or('s');

    let out = match last_char {
        's' => {
            let s = value_to_string(value);
            // Parse width from format string (everything before 's').
            let width_spec = &fmt[..fmt.len() - 1];
            if width_spec.is_empty() {
                s
            } else if let Ok(width) = width_spec.parse::<usize>() {
                format!("{s:>width$}")
            } else if let Some(neg) = width_spec.strip_prefix('-') {
                if let Ok(width) = neg.parse::<usize>() {
                    format!("{s:<width$}")
                } else {
                    s
                }
            } else {
                s
            }
        }
        'd' | 'i' => {
            let n = coerce_to_i64(value).unwrap_or(0);
            // Parse width from format string (everything before 'd'/'i').
            let width_spec = &fmt[..fmt.len() - 1];
            if width_spec.is_empty() {
                format!("{n}")
            } else if let Ok(width) = width_spec.trim_start_matches('0').parse::<usize>() {
                if width_spec.starts_with('0') {
                    format!("{n:0>width$}")
                } else {
                    format!("{n:>width$}")
                }
            } else {
                format!("{n}")
            }
        }
        'f' => {
            let n = coerce_to_f64(value).unwrap_or(0.0);
            // Parse precision: ".2f" -> 2.
            let spec = &fmt[..fmt.len() - 1];
            if let Some(dot_pos) = spec.rfind('.') {
                let prec: usize = spec[dot_pos + 1..].parse().unwrap_or(6);
                format!("{n:.prec$}")
            } else {
                format!("{n:.6}")
            }
        }
        'e' => {
            let n = coerce_to_f64(value).unwrap_or(0.0);
            format!("{n:e}")
        }
        _ => value_to_string(value),
    };
    Value::String(out)
}

/// `truncatechars_html`: delegates to Django for HTML tag balancing.
fn filter_truncatechars_html(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    _truncate_html_via_python(value, args, "truncatechars_html")
}

/// `truncatewords_html`: delegates to Django for HTML tag balancing.
fn filter_truncatewords_html(value: &Value, args: &[Value], _autoescape: bool) -> Value {
    _truncate_html_via_python(value, args, "truncatewords_html")
}

fn _truncate_html_via_python(value: &Value, args: &[Value], func_name: &str) -> Value {
    Python::attach(|py| {
        let filters = py
            .import("django.template.defaultfilters")
            .expect("defaultfilters");
        let py_val = value.to_pyobject(py);
        let py_arg = if args.is_empty() {
            0i64.into_pyobject(py).expect("int").into_any().unbind()
        } else {
            args[0].to_pyobject(py)
        };
        match filters.call_method1(func_name, (py_val.bind(py), py_arg.bind(py))) {
            Ok(result) => Value::from(&result),
            Err(_) => value.clone(),
        }
    })
}

/// Built-in filter id for direct-match dispatch. LLVM can inline match
/// arms (unlike `fn` pointers in NativeFilter), letting it constant-fold
/// `autoescape` and inline trivial filters like `default`. Filters not
/// listed here use `External`, falling back to the registry pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterId {
    AddSlashes,
    CapFirst,
    Center,
    Cut,
    Default,
    DefaultIfNone,
    Escape,
    ForceEscape,
    First,
    Join,
    Last,
    Length,
    LengthIs,
    Linebreaks,
    Linebreaksbr,
    Linenumbers,
    Ljust,
    Lower,
    MakeList,
    Pluralize,
    Random,
    Rjust,
    Safe,
    Safeseq,
    Slice,
    Slugify,
    StringFormat,
    Striptags,
    Title,
    Truncatechars,
    Truncatewords,
    UnorderedList,
    Upper,
    Wordcount,
    Wordwrap,
    Yesno,
    /// Catch-all; dispatches via the registry's fn pointer.
    External,
}

impl FilterId {
    pub fn from_name(name: &str) -> Self {
        match name {
            "addslashes" => Self::AddSlashes,
            "capfirst" => Self::CapFirst,
            "center" => Self::Center,
            "cut" => Self::Cut,
            "default" => Self::Default,
            "default_if_none" => Self::DefaultIfNone,
            "escape" => Self::Escape,
            "force_escape" => Self::ForceEscape,
            "first" => Self::First,
            "join" => Self::Join,
            "last" => Self::Last,
            "length" => Self::Length,
            "length_is" => Self::LengthIs,
            "linebreaks" => Self::Linebreaks,
            "linebreaksbr" => Self::Linebreaksbr,
            "linenumbers" => Self::Linenumbers,
            "ljust" => Self::Ljust,
            "lower" => Self::Lower,
            "make_list" => Self::MakeList,
            "pluralize" => Self::Pluralize,
            "random" => Self::Random,
            "rjust" => Self::Rjust,
            "safe" => Self::Safe,
            "safeseq" => Self::Safeseq,
            "slice" => Self::Slice,
            "slugify" => Self::Slugify,
            "stringformat" => Self::StringFormat,
            "striptags" => Self::Striptags,
            "title" => Self::Title,
            "truncatechars" => Self::Truncatechars,
            "truncatewords" => Self::Truncatewords,
            "unordered_list" => Self::UnorderedList,
            "upper" => Self::Upper,
            "wordcount" => Self::Wordcount,
            "wordwrap" => Self::Wordwrap,
            "yesno" => Self::Yesno,
            _ => Self::External,
        }
    }

    /// Direct dispatch for known filters; `External` falls through to
    /// `external_fn`. `external_fn` is a parameter so the registry
    /// HashMap doesn't import onto every call site.
    #[inline]
    #[allow(clippy::type_complexity)]
    pub fn dispatch(
        self,
        value: &Value,
        args: &[Value],
        autoescape: bool,
        external_fn: Option<fn(&Value, &[Value], bool) -> Value>,
    ) -> Value {
        match self {
            Self::AddSlashes => filter_addslashes(value, args, autoescape),
            Self::CapFirst => filter_capfirst(value, args, autoescape),
            Self::Center => filter_center(value, args, autoescape),
            Self::Cut => filter_cut(value, args, autoescape),
            Self::Default => filter_default(value, args, autoescape),
            Self::DefaultIfNone => filter_default_if_none(value, args, autoescape),
            Self::Escape => filter_escape(value, args, autoescape),
            Self::ForceEscape => filter_force_escape(value, args, autoescape),
            Self::First => filter_first(value, args, autoescape),
            Self::Join => filter_join(value, args, autoescape),
            Self::Last => filter_last(value, args, autoescape),
            Self::Length => filter_length(value, args, autoescape),
            Self::LengthIs => filter_length_is(value, args, autoescape),
            Self::Linebreaks => filter_linebreaks(value, args, autoescape),
            Self::Linebreaksbr => filter_linebreaksbr(value, args, autoescape),
            Self::Linenumbers => filter_linenumbers(value, args, autoescape),
            Self::Ljust => filter_ljust(value, args, autoescape),
            Self::Lower => filter_lower(value, args, autoescape),
            Self::MakeList => filter_make_list(value, args, autoescape),
            Self::Pluralize => filter_pluralize(value, args, autoescape),
            Self::Random => filter_random(value, args, autoescape),
            Self::Rjust => filter_rjust(value, args, autoescape),
            Self::Safe => filter_safe(value, args, autoescape),
            Self::Safeseq => filter_safeseq(value, args, autoescape),
            Self::Slice => filter_slice(value, args, autoescape),
            Self::Slugify => filter_slugify(value, args, autoescape),
            Self::StringFormat => filter_stringformat(value, args, autoescape),
            Self::Striptags => filter_striptags(value, args, autoescape),
            Self::Title => filter_title(value, args, autoescape),
            Self::Truncatechars => filter_truncatechars(value, args, autoescape),
            Self::Truncatewords => filter_truncatewords(value, args, autoescape),
            Self::UnorderedList => filter_unordered_list(value, args, autoescape),
            Self::Upper => filter_upper(value, args, autoescape),
            Self::Wordcount => filter_wordcount(value, args, autoescape),
            Self::Wordwrap => filter_wordwrap(value, args, autoescape),
            Self::Yesno => filter_yesno(value, args, autoescape),
            Self::External => match external_fn {
                Some(f) => f(value, args, autoescape),
                None => Value::String(String::new()),
            },
        }
    }
}

/// Global default filters registry, initialised lazily.
pub fn get_default_filters() -> &'static HashMap<String, NativeFilter> {
    static FILTERS: OnceLock<HashMap<String, NativeFilter>> = OnceLock::new();
    FILTERS.get_or_init(build_default_filters)
}

fn build_default_filters() -> HashMap<String, NativeFilter> {
    let mut m = HashMap::new();

    macro_rules! register {
        ($name:expr, $func:expr, safe=$safe:expr, autoescape=$ae:expr, localtime=$lt:expr) => {
            m.insert(
                $name.to_owned(),
                NativeFilter {
                    name: $name,
                    func: $func,
                    is_safe: $safe,
                    needs_autoescape: $ae,
                    expects_localtime: $lt,
                },
            );
        };
        // Short form: safe=false, autoescape=false, localtime=false.
        ($name:expr, $func:expr) => {
            register!(
                $name,
                $func,
                safe = false,
                autoescape = false,
                localtime = false
            );
        };
    }

    // is_safe flags match django.template.defaultfilters exactly.
    register!(
        "addslashes",
        filter_addslashes,
        safe = true,
        autoescape = false,
        localtime = false
    );
    register!(
        "capfirst",
        filter_capfirst,
        safe = true,
        autoescape = false,
        localtime = false
    );
    register!(
        "center",
        filter_center,
        safe = true,
        autoescape = false,
        localtime = false
    );
    register!("cut", filter_cut);
    register!(
        "escape",
        filter_escape,
        safe = true,
        autoescape = false,
        localtime = false
    );
    register!(
        "escapejs",
        filter_escapejs,
        safe = true,
        autoescape = false,
        localtime = false
    );
    register!(
        "force_escape",
        filter_force_escape,
        safe = true,
        autoescape = false,
        localtime = false
    );
    register!(
        "ljust",
        filter_ljust,
        safe = true,
        autoescape = false,
        localtime = false
    );
    register!(
        "lower",
        filter_lower,
        safe = true,
        autoescape = false,
        localtime = false
    );
    register!(
        "rjust",
        filter_rjust,
        safe = true,
        autoescape = false,
        localtime = false
    );
    register!(
        "slugify",
        filter_slugify,
        safe = true,
        autoescape = false,
        localtime = false
    );
    register!(
        "striptags",
        filter_striptags,
        safe = true,
        autoescape = false,
        localtime = false
    );
    register!(
        "title",
        filter_title,
        safe = true,
        autoescape = false,
        localtime = false
    );
    register!(
        "truncatechars",
        filter_truncatechars,
        safe = true,
        autoescape = false,
        localtime = false
    );
    register!(
        "truncatewords",
        filter_truncatewords,
        safe = true,
        autoescape = false,
        localtime = false
    );
    register!(
        "upper",
        filter_upper,
        safe = false,
        autoescape = false,
        localtime = false
    );
    register!(
        "wordcount",
        filter_wordcount,
        safe = false,
        autoescape = false,
        localtime = false
    );
    register!(
        "wordwrap",
        filter_wordwrap,
        safe = true,
        autoescape = false,
        localtime = false
    );

    register!(
        "linebreaks",
        filter_linebreaks,
        safe = true,
        autoescape = true,
        localtime = false
    );
    register!(
        "linebreaksbr",
        filter_linebreaksbr,
        safe = true,
        autoescape = true,
        localtime = false
    );
    register!(
        "linenumbers",
        filter_linenumbers,
        safe = true,
        autoescape = true,
        localtime = false
    );
    register!(
        "safe",
        filter_safe,
        safe = true,
        autoescape = false,
        localtime = false
    );
    register!(
        "safeseq",
        filter_safeseq,
        safe = true,
        autoescape = false,
        localtime = false
    );
    register!(
        "urlize",
        filter_urlize,
        safe = true,
        autoescape = true,
        localtime = false
    );
    register!(
        "urlizetrunc",
        filter_urlizetrunc,
        safe = true,
        autoescape = true,
        localtime = false
    );

    register!(
        "first",
        filter_first,
        safe = false,
        autoescape = false,
        localtime = false
    );
    register!(
        "join",
        filter_join,
        safe = true,
        autoescape = true,
        localtime = false
    );
    register!(
        "last",
        filter_last,
        safe = true,
        autoescape = false,
        localtime = false
    );
    register!(
        "length",
        filter_length,
        safe = false,
        autoescape = false,
        localtime = false
    );
    register!(
        "length_is",
        filter_length_is,
        safe = false,
        autoescape = false,
        localtime = false
    );
    register!(
        "random",
        filter_random,
        safe = false,
        autoescape = false,
        localtime = false
    );
    register!(
        "slice",
        filter_slice,
        safe = true,
        autoescape = false,
        localtime = false
    );
    register!(
        "dictsort",
        filter_dictsort,
        safe = false,
        autoescape = false,
        localtime = false
    );
    register!(
        "dictsortreversed",
        filter_dictsortreversed,
        safe = false,
        autoescape = false,
        localtime = false
    );
    register!(
        "unordered_list",
        filter_unordered_list,
        safe = true,
        autoescape = true,
        localtime = false
    );

    register!(
        "add",
        filter_add,
        safe = false,
        autoescape = false,
        localtime = false
    );
    register!(
        "divisibleby",
        filter_divisibleby,
        safe = false,
        autoescape = false,
        localtime = false
    );
    register!(
        "filesizeformat",
        filter_filesizeformat,
        safe = true,
        autoescape = false,
        localtime = false
    );
    register!(
        "floatformat",
        filter_floatformat,
        safe = true,
        autoescape = false,
        localtime = false
    );
    register!(
        "get_digit",
        filter_get_digit,
        safe = false,
        autoescape = false,
        localtime = false
    );
    register!(
        "pluralize",
        filter_pluralize,
        safe = false,
        autoescape = false,
        localtime = false
    );

    register!(
        "default",
        filter_default,
        safe = false,
        autoescape = false,
        localtime = false
    );
    register!(
        "default_if_none",
        filter_default_if_none,
        safe = false,
        autoescape = false,
        localtime = false
    );
    register!(
        "yesno",
        filter_yesno,
        safe = false,
        autoescape = false,
        localtime = false
    );

    register!(
        "date",
        filter_date,
        safe = true,
        autoescape = false,
        localtime = true
    );
    register!(
        "time",
        filter_time,
        safe = true,
        autoescape = false,
        localtime = true
    );
    register!(
        "timesince",
        filter_timesince,
        safe = false,
        autoescape = false,
        localtime = false
    );
    register!(
        "timeuntil",
        filter_timeuntil,
        safe = false,
        autoescape = false,
        localtime = false
    );

    register!(
        "iriencode",
        filter_iriencode,
        safe = true,
        autoescape = false,
        localtime = false
    );
    register!(
        "urlencode",
        filter_urlencode,
        safe = false,
        autoescape = false,
        localtime = false
    );

    register!(
        "json_script",
        filter_json_script,
        safe = true,
        autoescape = false,
        localtime = false
    );
    register!(
        "make_list",
        filter_make_list,
        safe = false,
        autoescape = false,
        localtime = false
    );
    register!(
        "phone2numeric",
        filter_phone2numeric,
        safe = true,
        autoescape = false,
        localtime = false
    );
    register!(
        "pprint",
        filter_pprint,
        safe = true,
        autoescape = false,
        localtime = false
    );
    register!(
        "stringformat",
        filter_stringformat,
        safe = true,
        autoescape = false,
        localtime = false
    );
    register!(
        "truncatechars_html",
        filter_truncatechars_html,
        safe = true,
        autoescape = false,
        localtime = false
    );
    register!(
        "truncatewords_html",
        filter_truncatewords_html,
        safe = true,
        autoescape = false,
        localtime = false
    );

    m
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

    fn float(n: f64) -> Value {
        Value::Float(n)
    }

    fn list(items: Vec<Value>) -> Value {
        Value::List(items)
    }

    #[test]
    fn test_registry_has_all_filters() {
        let filters = get_default_filters();
        let expected = [
            "addslashes",
            "capfirst",
            "center",
            "cut",
            "escape",
            "escapejs",
            "force_escape",
            "ljust",
            "lower",
            "rjust",
            "slugify",
            "striptags",
            "title",
            "truncatechars",
            "truncatewords",
            "upper",
            "wordcount",
            "wordwrap",
            "linebreaks",
            "linebreaksbr",
            "linenumbers",
            "safe",
            "safeseq",
            "urlize",
            "urlizetrunc",
            "first",
            "join",
            "last",
            "length",
            "length_is",
            "random",
            "slice",
            "dictsort",
            "dictsortreversed",
            "unordered_list",
            "add",
            "divisibleby",
            "filesizeformat",
            "floatformat",
            "get_digit",
            "pluralize",
            "default",
            "default_if_none",
            "yesno",
            "date",
            "time",
            "timesince",
            "timeuntil",
            "iriencode",
            "urlencode",
            "json_script",
            "make_list",
            "phone2numeric",
            "pprint",
            "stringformat",
            "truncatechars_html",
            "truncatewords_html",
        ];
        for name in &expected {
            assert!(filters.contains_key(*name), "missing filter: {name}");
        }
    }

    #[test]
    fn test_addslashes() {
        assert_eq!(
            filter_addslashes(&s("it's a \"test\""), &[], false),
            s("it\\'s a \\\"test\\\"")
        );
    }

    #[test]
    fn test_capfirst() {
        assert_eq!(filter_capfirst(&s("hello"), &[], false), s("Hello"));
        assert_eq!(filter_capfirst(&s(""), &[], false), s(""));
    }

    #[test]
    fn test_center() {
        let result = filter_center(&s("hi"), &[int(10)], false);
        assert_eq!(result, s("    hi    "));
    }

    #[test]
    fn test_cut() {
        assert_eq!(
            filter_cut(&s("hello world"), &[s("o")], false),
            s("hell wrld")
        );
    }

    #[test]
    fn test_escape() {
        assert_eq!(
            filter_escape(&s("<b>bold</b>"), &[], false),
            safe("&lt;b&gt;bold&lt;/b&gt;")
        );
    }

    #[test]
    fn test_lower() {
        assert_eq!(
            filter_lower(&s("Hello World"), &[], false),
            s("hello world")
        );
    }

    #[test]
    fn test_upper() {
        assert_eq!(filter_upper(&s("hello"), &[], false), s("HELLO"));
    }

    #[test]
    fn test_slugify() {
        assert_eq!(
            filter_slugify(&s("Hello World!"), &[], false),
            safe("hello-world")
        );
        assert_eq!(
            filter_slugify(&s("  Lots   of   spaces  "), &[], false),
            safe("lots-of-spaces")
        );
    }

    #[test]
    fn test_title() {
        assert_eq!(
            filter_title(&s("hello world"), &[], false),
            s("Hello World")
        );
    }

    #[test]
    fn test_truncatechars() {
        assert_eq!(
            filter_truncatechars(&s("hello world"), &[int(8)], false),
            s("hello w\u{2026}")
        );
        // No truncation needed.
        assert_eq!(filter_truncatechars(&s("hi"), &[int(10)], false), s("hi"));
    }

    #[test]
    fn test_truncatewords() {
        assert_eq!(
            filter_truncatewords(&s("one two three four"), &[int(2)], false),
            s("one two \u{2026}")
        );
    }

    #[test]
    fn test_wordcount() {
        assert_eq!(filter_wordcount(&s("hello world"), &[], false), int(2));
        assert_eq!(filter_wordcount(&s(""), &[], false), int(0));
    }

    #[test]
    fn test_striptags() {
        assert_eq!(
            filter_striptags(&s("<p>Hello <b>world</b></p>"), &[], false),
            s("Hello world")
        );
    }

    #[test]
    fn test_linebreaksbr() {
        assert_eq!(
            filter_linebreaksbr(&safe("line1\nline2"), &[], false),
            safe("line1<br>line2")
        );
    }

    #[test]
    fn test_safe() {
        assert_eq!(filter_safe(&s("hello"), &[], false), safe("hello"));
    }

    #[test]
    fn test_first() {
        assert_eq!(
            filter_first(&list(vec![int(1), int(2), int(3)]), &[], false),
            int(1)
        );
        assert_eq!(filter_first(&list(vec![]), &[], false), s(""));
    }

    #[test]
    fn test_last() {
        assert_eq!(
            filter_last(&list(vec![int(1), int(2), int(3)]), &[], false),
            int(3)
        );
    }

    #[test]
    fn test_length() {
        assert_eq!(filter_length(&s("hello"), &[], false), int(5));
        assert_eq!(
            filter_length(&list(vec![int(1), int(2)]), &[], false),
            int(2)
        );
    }

    #[test]
    fn test_join() {
        assert_eq!(
            filter_join(&list(vec![s("a"), s("b"), s("c")]), &[s(", ")], false),
            safe("a, b, c")
        );
    }

    #[test]
    fn test_slice() {
        let input = list(vec![int(1), int(2), int(3), int(4), int(5)]);
        assert_eq!(
            filter_slice(&input, &[s("1:3")], false),
            list(vec![int(2), int(3)])
        );
    }

    #[test]
    fn test_add_integers() {
        assert_eq!(filter_add(&int(4), &[int(2)], false), int(6));
    }

    #[test]
    fn test_add_strings() {
        assert_eq!(
            filter_add(&s("hello "), &[s("world")], false),
            s("hello world")
        );
    }

    #[test]
    fn test_divisibleby() {
        assert_eq!(
            filter_divisibleby(&int(10), &[int(5)], false),
            Value::Bool(true)
        );
        assert_eq!(
            filter_divisibleby(&int(10), &[int(3)], false),
            Value::Bool(false)
        );
    }

    #[test]
    fn test_filesizeformat() {
        assert_eq!(
            filter_filesizeformat(&int(0), &[], false),
            s("0\u{a0}bytes")
        );
        assert_eq!(
            filter_filesizeformat(&int(1024), &[], false),
            s("1.0\u{a0}KB")
        );
        assert_eq!(
            filter_filesizeformat(&int(1048576), &[], false),
            s("1.0\u{a0}MB")
        );
    }

    #[test]
    fn test_floatformat_default() {
        // -1 (default): strip trailing zeros.
        assert_eq!(filter_floatformat(&float(1.0), &[], false), s("1"));
        assert_eq!(filter_floatformat(&float(1.5), &[], false), s("1.5"));
    }

    #[test]
    fn test_floatformat_positive() {
        assert_eq!(filter_floatformat(&float(1.0), &[int(2)], false), s("1.00"));
        assert_eq!(filter_floatformat(&float(1.5), &[int(2)], false), s("1.50"));
    }

    #[test]
    fn test_pluralize() {
        assert_eq!(filter_pluralize(&int(1), &[], false), s(""));
        assert_eq!(filter_pluralize(&int(2), &[], false), s("s"));
        assert_eq!(filter_pluralize(&int(2), &[s("es")], false), s("es"));
        assert_eq!(filter_pluralize(&int(1), &[s("y,ies")], false), s("y"));
        assert_eq!(filter_pluralize(&int(2), &[s("y,ies")], false), s("ies"));
    }

    #[test]
    fn test_get_digit() {
        assert_eq!(filter_get_digit(&int(12345), &[int(1)], false), int(5));
        assert_eq!(filter_get_digit(&int(12345), &[int(3)], false), int(3));
    }

    #[test]
    fn test_default() {
        assert_eq!(
            filter_default(&s(""), &[s("fallback")], false),
            s("fallback")
        );
        assert_eq!(
            filter_default(&s("value"), &[s("fallback")], false),
            s("value")
        );
        assert_eq!(
            filter_default(&Value::None, &[s("fallback")], false),
            s("fallback")
        );
    }

    #[test]
    fn test_default_if_none() {
        assert_eq!(
            filter_default_if_none(&Value::None, &[s("fallback")], false),
            s("fallback")
        );
        assert_eq!(
            filter_default_if_none(&s(""), &[s("fallback")], false),
            s("")
        );
    }

    #[test]
    fn test_yesno() {
        assert_eq!(
            filter_yesno(&Value::Bool(true), &[s("yeah,nope,maybe")], false),
            s("yeah")
        );
        assert_eq!(
            filter_yesno(&Value::Bool(false), &[s("yeah,nope,maybe")], false),
            s("nope")
        );
        assert_eq!(
            filter_yesno(&Value::None, &[s("yeah,nope,maybe")], false),
            s("maybe")
        );
    }

    #[test]
    fn test_urlencode() {
        assert_eq!(
            filter_urlencode(&s("hello world"), &[], false),
            safe("hello%20world")
        );
        assert_eq!(
            filter_urlencode(&s("/path/to/file"), &[], false),
            safe("/path/to/file")
        );
    }

    #[test]
    fn test_make_list() {
        assert_eq!(
            filter_make_list(&s("abc"), &[], false),
            list(vec![s("a"), s("b"), s("c")])
        );
        assert_eq!(
            filter_make_list(&int(123), &[], false),
            list(vec![s("1"), s("2"), s("3")])
        );
    }

    #[test]
    fn test_phone2numeric() {
        assert_eq!(
            filter_phone2numeric(&s("1-800-COLLECT"), &[], false),
            s("1-800-2655328")
        );
    }

    #[test]
    fn test_json_script() {
        let result = filter_json_script(&s("hello"), &[s("my-data")], false);
        match result {
            Value::SafeString(html) => {
                assert!(html.contains("application/json"));
                assert!(html.contains("id=\"my-data\""));
                assert!(html.contains("\"hello\""));
            }
            _ => panic!("expected SafeString"),
        }
    }

    #[test]
    fn test_escapejs() {
        let result = filter_escapejs(&s("hello\nworld"), &[], false);
        match result {
            Value::SafeString(js) => {
                assert!(js.contains("\\u000A"), "expected newline escape, got: {js}");
                assert!(!js.contains('\n'));
            }
            _ => panic!("expected SafeString"),
        }
    }

    #[test]
    fn test_safe_input_preserved_by_safe_filter() {
        // Filters with is_safe=true should preserve SafeString status.
        // Use `lower` which has is_safe=true (unlike `upper` which is false).
        let input = safe("<b>bold</b>");
        let result = filter_lower(&input, &[], false);
        assert!(matches!(result, Value::SafeString(_)));
    }

    #[test]
    fn test_unsafe_input_stays_unsafe() {
        let input = s("hello");
        let result = filter_upper(&input, &[], false);
        assert!(matches!(result, Value::String(_)));
    }
}
