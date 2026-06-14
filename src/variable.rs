//! Variable / filter-expression parsing and resolution. Parsing happens
//! at construction; resolution crosses into Python. Mirrors
//! `django.template.base.Variable` / `FilterExpression`.

use once_cell::sync::Lazy;
use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyString, PyTuple};
use regex::Regex;

use crate::errors::TemplateError;

/// Django's `VARIABLE_ATTRIBUTE_SEPARATOR`.
const VARIABLE_ATTRIBUTE_SEPARATOR: char = '.';

/// Constant-string sub-pattern: `_("...")` / `_('...')` translatable
/// strings, or `"..."` / `'...'` literals.
const CONSTANT_STRING: &str = concat!(
    r#"(?:_\("[^"\\]*(?:\\.[^"\\]*)*"\)"#,
    r"|",
    r"_\('[^'\\]*(?:\\.[^'\\]*)*'\)",
    r"|",
    r#""[^"\\]*(?:\\.[^"\\]*)*""#,
    r"|",
    r"'[^'\\]*(?:\\.[^'\\]*)*')",
);

/// Three alternatives: leading constant, leading variable, or
/// `|filter:arg`. (Rust's regex crate has no `re.VERBOSE` equivalent.)
fn build_filter_pattern() -> String {
    format!(
        r"^(?P<constant>{constant})|^(?P<var>[\w.\+-]+)|(?:\s*\|\s*(?P<filter_name>\w+)(?::(?:(?P<constant_arg>{constant})|(?P<var_arg>[\w.\+-]+)))?)",
        constant = CONSTANT_STRING,
    )
}

static FILTER_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(&build_filter_pattern()).expect("filter regex must compile"));

/// Parsed `Variable`: literal (number/string) or dotted lookup chain.
#[derive(Debug, Clone)]
enum VariableValue {
    Int(i64),
    Float(f64),
    /// String literal (unescaped); will be mark_safe'd by Django.
    StringLiteral(String),
    /// Dotted path `"article.section"` -> `["article", "section"]`.
    Lookups(Vec<String>),
}

/// Mirrors `django.template.base.Variable`. Parsing at construction;
/// resolution crosses into Python.
#[derive(Debug)]
pub struct Variable {
    pub var: String,
    value: VariableValue,
    /// `parts[1..].join(".")`. Key into `ForBatchPlan::path_to_slot`
    /// for the for-loop fast path; `None` for literals and 1-segment
    /// variables. Pre-joined at parse time so render-side probes don't
    /// allocate.
    lookups_after_first: Option<String>,
    /// Direct slot index into a parent for-loop's `ForBatchPlan`. Set
    /// by `ForBatchPlan::compute`. `AtomicU32` (not `Cell<Option<u16>>`)
    /// keeps Variable `Send + Sync` for cross-thread GC paths.
    /// `u32::MAX` = unset; real values fit in u16.
    batch_slot: std::sync::atomic::AtomicU32,
    pub translate: bool,
    /// `pgettext_lazy` context.
    pub message_context: Option<String>,
}

impl Variable {
    /// Mirrors `Variable.__init__`. Errors on leading `_`, `._`, or
    /// `+`/`-` in non-numeric tokens.
    pub fn new(var: &str) -> Result<Self, TemplateError> {
        let mut translate = false;

        if let Some(val) = try_parse_number(var) {
            return Ok(Variable {
                var: var.to_owned(),
                value: val,
                translate: false,
                message_context: None,
                lookups_after_first: None,
                batch_slot: std::sync::atomic::AtomicU32::new(u32::MAX),
            });
        }

        // Check translation wrapper.
        let working = if var.starts_with("_(") && var.ends_with(')') {
            translate = true;
            &var[2..var.len() - 1]
        } else {
            var
        };

        if let Some(unescaped) = unescape_string_literal(working) {
            return Ok(Variable {
                var: var.to_owned(),
                value: VariableValue::StringLiteral(unescaped),
                translate,
                message_context: None,
                lookups_after_first: None,
                batch_slot: std::sync::atomic::AtomicU32::new(u32::MAX),
            });
        }

        // Lookup path: reject leading `_` and `._`.
        let sep_underscore = format!("{}_{}", VARIABLE_ATTRIBUTE_SEPARATOR, "");
        if working.contains(&sep_underscore) || working.starts_with('_') {
            return Err(TemplateError::TemplateSyntaxError(format!(
                "Variables and attributes may not begin with underscores: '{}'",
                var
            )));
        }

        // `+`/`-` are valid only in numbers.
        for c in ['+', '-'] {
            if working.contains(c) {
                return Err(TemplateError::TemplateSyntaxError(format!(
                    "Invalid character ('{}') in variable name: '{}'",
                    c, var
                )));
            }
        }

        let lookups: Vec<String> = working
            .split(VARIABLE_ATTRIBUTE_SEPARATOR)
            .map(|s| s.to_owned())
            .collect();

        // Pre-join `parts[1..]` for ForBatchPlan lookups.
        let lookups_after_first = if lookups.len() >= 2 {
            let sep_str = VARIABLE_ATTRIBUTE_SEPARATOR.to_string();
            Some(lookups[1..].join(sep_str.as_str()))
        } else {
            None
        };

        Ok(Variable {
            var: var.to_owned(),
            value: VariableValue::Lookups(lookups),
            translate,
            message_context: None,
            lookups_after_first,
            batch_slot: std::sync::atomic::AtomicU32::new(u32::MAX),
        })
    }

    /// Mirrors `Variable.resolve(context)`.
    pub fn resolve<'py>(
        &self,
        py: Python<'py>,
        context: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let value = match &self.value {
            VariableValue::Int(n) => n.into_pyobject(py)?.into_any(),
            VariableValue::Float(f) => f.into_pyobject(py)?.into_any(),
            VariableValue::StringLiteral(s) => {
                // mark_safe per Django.
                let mark_safe = py.import("django.utils.safestring")?.getattr("mark_safe")?;
                mark_safe.call1((s.as_str(),))?
            }
            VariableValue::Lookups(_) => self.resolve_lookup(py, context)?,
        };

        if self.translate {
            self.apply_translation(py, &value)
        } else {
            Ok(value)
        }
    }

    /// Apply Django's translation logic to the resolved value.
    fn apply_translation<'py>(
        &self,
        py: Python<'py>,
        value: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let dj = crate::python_cache::django(py)?;
        let is_safe = value.is_instance(dj.safe_data_cls.bind(py))?;

        // Replace "%" with "%%" to avoid accidental formatting.
        let value_str = value.str()?;
        let msgid_str: String = value_str.extract::<String>()?.replace('%', "%%");
        let msgid: Bound<'py, PyAny> = if is_safe {
            dj.mark_safe.bind(py).call1((msgid_str.as_str(),))?
        } else {
            msgid_str.into_pyobject(py)?.into_any()
        };

        if let Some(ref msg_ctx) = self.message_context {
            dj.pgettext_lazy.bind(py).call1((msg_ctx.as_str(), msgid))
        } else {
            dj.gettext_lazy.bind(py).call1((msgid,))
        }
    }

    /// The core lookup algorithm - mirrors `Variable._resolve_lookup` exactly.
    ///
    /// This is the **hottest** code path. Each lookup step crosses the
    /// Python/Rust boundary via PyO3 for `__getitem__`, `getattr`, `callable`,
    /// and `inspect.signature` checks.
    fn resolve_lookup<'py>(
        &self,
        py: Python<'py>,
        context: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let lookups = match &self.value {
            VariableValue::Lookups(l) => l,
            _ => unreachable!("resolve_lookup called on non-lookup variable"),
        };

        let base_context_cls = py
            .import("django.template.context")?
            .getattr("BaseContext")?;

        // `current` starts as the context itself (which is typically a
        // `Context` object with dict-stack semantics).
        let mut current: Bound<'py, PyAny> = context.clone();

        // Outer try/except: catch-all for silent_variable_failure.
        let result: PyResult<Bound<'py, PyAny>> = (|| {
            for bit in lookups {
                let bit_py = PyString::new(py, bit);

                // --- Step 1: dictionary lookup (`current[bit]`) ---
                let dict_result: Result<Bound<'py, PyAny>, ()> = (|| {
                    // Only attempt if type(current) has __getitem__.
                    let current_type = current.get_type();
                    if !current_type.hasattr("__getitem__").unwrap_or(false) {
                        return Err(());
                    }
                    current.get_item(&bit_py).map_err(|e| {
                        // Swallow TypeError, AttributeError, KeyError,
                        // ValueError, IndexError - fall through.
                        let _ = e;
                    })
                })();

                current = match dict_result {
                    Ok(val) => val,
                    Err(()) => {
                        // --- Step 2: attribute lookup (`getattr(current, bit)`) ---
                        let attr_result: Result<Bound<'py, PyAny>, PyErr> = (|| {
                            // Guard: don't return class attributes if current
                            // is a BaseContext.
                            if current.is_instance(&base_context_cls)?
                                && current.get_type().getattr(bit_py.clone()).is_ok()
                            {
                                return Err(PyTypeError::new_err("skip"));
                            }
                            current.getattr(bit_py.clone())})(
                        );

                        match attr_result {
                            Ok(val) => val,
                            Err(attr_err) => {
                                // @property that raised: attribute is in
                                // dir() but raised -> re-raise.
                                let is_base_ctx =
                                    current.is_instance(&base_context_cls).unwrap_or(false);
                                if !is_base_ctx {
                                    let dir_list = current.dir()?;
                                    if dir_contains(&dir_list, bit) {
                                        return Err(attr_err);
                                    }
                                }

                                // Step 3: int-index lookup.
                                match bit.parse::<i64>() {
                                    Ok(idx) => match current.get_item(idx) {
                                        Ok(val) => val,
                                        Err(_) => {
                                            return Err(TemplateError::VariableDoesNotExist {
                                                msg: "Failed lookup for key [%s] in %r".to_owned(),
                                                params: vec![bit.clone(), repr_py(&current)],
                                            }
                                            .into());
                                        }
                                    },
                                    Err(_) => {
                                        return Err(TemplateError::VariableDoesNotExist {
                                            msg: "Failed lookup for key [%s] in %r".to_owned(),
                                            params: vec![bit.clone(), repr_py(&current)],
                                        }
                                        .into());
                                    }
                                }
                            }
                        }
                    }
                };

                // Callable check after each step.
                if current.is_callable() {
                    let do_not_call = current
                        .getattr("do_not_call_in_templates")
                        .ok()
                        .and_then(|v| v.is_truthy().ok())
                        .unwrap_or(false);

                    if do_not_call {
                        // leave current as-is
                    } else {
                        let alters_data = current
                            .getattr("alters_data")
                            .ok()
                            .and_then(|v| v.is_truthy().ok())
                            .unwrap_or(false);

                        if alters_data {
                            current = get_string_if_invalid(py, context)?;
                        } else {
                            match current.call0() {
                                Ok(val) => {
                                    current = val;
                                }
                                Err(call_err) => {
                                    if call_err.is_instance_of::<PyTypeError>(py) {
                                        // inspect.signature check: were
                                        // args actually required?
                                        current = handle_callable_type_error(
                                            py, &current, context, call_err,
                                        )?;
                                    } else {
                                        return Err(call_err);
                                    }
                                }
                            }
                        }
                    }
                }
            }

            Ok(current)
        })();

        match result {
            Ok(val) => Ok(val),
            Err(e) => {
                let silent = e
                    .value(py)
                    .getattr("silent_variable_failure")
                    .ok()
                    .and_then(|v| v.is_truthy().ok())
                    .unwrap_or(false);

                if silent {
                    get_string_if_invalid(py, context)
                } else {
                    Err(e)
                }
            }
        }
    }

    pub fn is_lookup(&self) -> bool {
        matches!(self.value, VariableValue::Lookups(_))
    }

    pub fn is_literal(&self) -> bool {
        !self.is_lookup()
    }

    pub fn as_string_literal(&self) -> Option<&str> {
        match &self.value {
            VariableValue::StringLiteral(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_int_literal(&self) -> Option<i64> {
        match &self.value {
            VariableValue::Int(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_float_literal(&self) -> Option<f64> {
        match &self.value {
            VariableValue::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// `parts[1..].join(".")`, or `None` for ≤1-segment / literal vars.
    #[inline]
    pub fn lookup_rest(&self) -> Option<&str> {
        self.lookups_after_first.as_deref()
    }

    /// Pre-resolved ForBatchPlan slot, set per Variable whose path
    /// starts with the loopvar.
    #[inline]
    pub fn batch_slot(&self) -> Option<u16> {
        let raw = self.batch_slot.load(std::sync::atomic::Ordering::Relaxed);
        if raw == u32::MAX {
            None
        } else {
            Some(raw as u16)
        }
    }

    /// Install slot if currently unset. Idempotent (outer loop wins);
    /// nested loops with shadowing loopvars are skipped elsewhere.
    pub fn set_batch_slot(&self, slot: u16) {
        let _ = self.batch_slot.compare_exchange(
            u32::MAX,
            slot as u32,
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    pub fn lookups(&self) -> Option<&[String]> {
        match &self.value {
            VariableValue::Lookups(l) => Some(l.as_slice()),
            _ => None,
        }
    }

    /// Resolve a literal against an empty dict. Used by
    /// `FilterExpression.__init__` for constant args.
    pub fn resolve_literal<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let empty_dict = PyDict::new(py);
        self.resolve(py, empty_dict.as_any())
    }
}

// Manual Clone: AtomicU32 doesn't derive Clone. Both source and clone
// independently track the stamped slot.
impl Clone for Variable {
    fn clone(&self) -> Self {
        Self {
            var: self.var.clone(),
            value: self.value.clone(),
            translate: self.translate,
            message_context: self.message_context.clone(),
            lookups_after_first: self.lookups_after_first.clone(),
            batch_slot: std::sync::atomic::AtomicU32::new(
                self.batch_slot.load(std::sync::atomic::Ordering::Relaxed),
            ),
        }
    }
}

impl std::fmt::Display for Variable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.var)
    }
}

fn get_string_if_invalid<'py>(
    _py: Python<'py>,
    context: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    context
        .getattr("template")?
        .getattr("engine")?
        .getattr("string_if_invalid")
}

/// `inspect.signature` / `sig.bind` test: no signature or bind fails ->
/// `string_if_invalid`; bind succeeds -> re-raise the original TypeError.
fn handle_callable_type_error<'py>(
    py: Python<'py>,
    callable: &Bound<'py, PyAny>,
    context: &Bound<'py, PyAny>,
    original_err: PyErr,
) -> PyResult<Bound<'py, PyAny>> {
    let inspect = py.import("inspect")?;
    let signature_fn = inspect.getattr("signature")?;

    match signature_fn.call1((callable,)) {
        Err(_) => get_string_if_invalid(py, context),
        Ok(sig) => match sig.call_method0("bind") {
            Err(_) => get_string_if_invalid(py, context),
            Ok(_) => Err(original_err),
        },
    }
}

fn dir_contains(dir_list: &Bound<'_, PyAny>, name: &str) -> bool {
    if let Ok(list) = dir_list.cast::<PyList>() {
        for item in list.iter() {
            if let Ok(s) = item.extract::<String>()
                && s == name {
                    return true;
                }
        }
    }
    false
}

/// Get Python `repr()` of an object, falling back to `"<repr failed>"`.
fn repr_py(obj: &Bound<'_, PyAny>) -> String {
    obj.repr()
        .map(|r| r.to_string())
        .unwrap_or_else(|_| "<repr failed>".to_owned())
}

/// Parse `var` per Django: float if it contains `.` or `e`/`E` (reject
/// trailing dot), else int. `None` on failure.
fn try_parse_number(var: &str) -> Option<VariableValue> {
    if var.is_empty() {
        return None;
    }

    let lower = var.to_ascii_lowercase();
    if var.contains('.') || lower.contains('e') {
        if var.ends_with('.') {
            return None;
        }
        var.parse::<f64>().ok().map(VariableValue::Float)
    } else {
        var.parse::<i64>().ok().map(VariableValue::Int)
    }
}

/// `django.utils.text.unescape_string_literal`. Returns `None` if `s`
/// isn't a quoted string literal.
fn unescape_string_literal(s: &str) -> Option<String> {
    if s.len() < 2 {
        return None;
    }
    let first = s.as_bytes()[0];
    let last = s.as_bytes()[s.len() - 1];

    if (first != b'"' && first != b'\'') || last != first {
        return None;
    }

    let quote = first as char;
    let inner = &s[1..s.len() - 1];
    let escaped_quote = format!("\\{}", quote);
    let result = inner
        .replace(&escaped_quote, &quote.to_string())
        .replace("\\\\", "\\");
    Some(result)
}

/// A filter arg: lookup `Variable` or constant string.
#[derive(Debug, Clone)]
pub struct FilterArg {
    pub is_lookup: bool,
    pub variable: Option<Variable>,
    pub constant: Option<String>,
    /// Lazy `Value::SafeString` cache for constant args.
    pub constant_value: std::sync::OnceLock<crate::context::Value>,
}

impl FilterArg {
    /// Cached `Value::SafeString` for constant args; `None` for lookups
    /// and translated literals.
    #[inline]
    pub fn cached_constant(&self) -> Option<&crate::context::Value> {
        if self.is_lookup || self.variable.is_some() {
            return None;
        }
        Some(self.constant_value.get_or_init(|| match &self.constant {
            Some(s) => crate::context::Value::SafeString(std::sync::Arc::from(s.as_str())),
            None => crate::context::Value::None,
        }))
    }
}

/// Parsed filter: name + args. Python function is resolved via
/// `parser.find_filter(name)` and stored on `FilterExpression`.
#[derive(Debug, Clone)]
pub struct ParsedFilter {
    pub name: String,
    pub args: Vec<FilterArg>,
}

/// Mirrors `django.template.base.FilterExpression`. Parses
/// `"var|filter1:arg|filter2"` at construction.
#[derive(Debug, Clone)]
pub struct FilterExpression {
    pub token: String,
    pub var: FilterExpressionVar,
    pub is_var: bool,
    pub filters: Vec<ParsedFilter>,
    /// Python callables, parallel to `filters`. Wrapped in `Arc` so
    /// clones don't need the GIL. Tag arg paths that don't carry their
    /// own slice (`{% with %}`, `{% if %}` args, etc.) read from here.
    pub filter_funcs: std::sync::Arc<Vec<pyo3::Py<pyo3::PyAny>>>,
}

/// Head of a `FilterExpression`.
#[derive(Debug, Clone)]
pub enum FilterExpressionVar {
    /// Resolved per-render.
    Var(Variable),
    /// Parse-time-resolved string constant.
    Constant(Option<String>),
}

impl FilterExpression {
    /// Mirrors `FilterExpression.__init__`. `find_filter` resolves
    /// names to Python functions and validates `args_check`.
    pub fn parse<F>(token: &str, mut find_filter: F) -> Result<Self, TemplateError>
    where
        F: FnMut(&str) -> Result<ParsedFilter, TemplateError>,
    {
        let mut var_obj: Option<FilterExpressionVar> = None;
        let mut is_var = false;
        let mut filters: Vec<ParsedFilter> = Vec::new();
        let mut upto: usize = 0;

        for mat in FILTER_RE.find_iter(token) {
            let start = mat.start();
            if upto != start {
                return Err(TemplateError::TemplateSyntaxError(format!(
                    "Could not parse some characters: {}|{}|{}",
                    &token[..upto],
                    &token[upto..start],
                    &token[start..],
                )));
            }

            let caps = FILTER_RE
                .captures(&token[start..])
                .expect("regex matched but captures failed");

            if var_obj.is_none() {
                // First match: variable or constant.
                if let Some(constant) = caps.name("constant") {
                    let constant_str = constant.as_str();
                    match Variable::new(constant_str) {
                        Ok(v) => {
                            match &v.value {
                                VariableValue::StringLiteral(s) => {
                                    if v.translate {
                                        // Keep the Variable so gettext fires
                                        // at render time.
                                        var_obj = Some(FilterExpressionVar::Var(v));
                                        is_var = false;
                                    } else {
                                        var_obj =
                                            Some(FilterExpressionVar::Constant(Some(s.clone())));
                                        is_var = false;
                                    }
                                }
                                VariableValue::Int(_) | VariableValue::Float(_) => {
                                    var_obj = Some(FilterExpressionVar::Var(v));
                                    is_var = true;
                                }
                                VariableValue::Lookups(_) => {
                                    // Constant pattern parsing as a lookup
                                    // would raise VariableDoesNotExist.
                                    var_obj = Some(FilterExpressionVar::Constant(None));
                                    is_var = false;
                                }
                            }
                        }
                        Err(_) => {
                            var_obj = Some(FilterExpressionVar::Constant(None));
                            is_var = false;
                        }
                    }
                } else if let Some(var_match) = caps.name("var") {
                    let var_str = var_match.as_str();
                    let v = Variable::new(var_str)?;
                    is_var = matches!(v.value, VariableValue::Lookups(_));
                    var_obj = Some(FilterExpressionVar::Var(v));
                } else {
                    return Err(TemplateError::TemplateSyntaxError(format!(
                        "Could not find variable at start of {}.",
                        token
                    )));
                }
            } else {
                let filter_name = caps
                    .name("filter_name")
                    .expect("filter match without filter_name")
                    .as_str();

                let mut parsed = find_filter(filter_name)?;

                if let Some(constant_arg) = caps.name("constant_arg") {
                    let arg_var = Variable::new(constant_arg.as_str())?;
                    if arg_var.translate {
                        // `_("Password")`: keep as Variable so gettext
                        // fires at render time (lazy translation).
                        parsed.args.push(FilterArg {
                            is_lookup: false,
                            variable: Some(arg_var),
                            constant: None,
                            constant_value: std::sync::OnceLock::new(),
                        });
                    } else {
                        let constant_val = match &arg_var.value {
                            VariableValue::StringLiteral(s) => s.clone(),
                            _ => constant_arg.as_str().to_owned(),
                        };
                        parsed.args.push(FilterArg {
                            is_lookup: false,
                            variable: None,
                            constant: Some(constant_val),
                            constant_value: std::sync::OnceLock::new(),
                        });
                    }
                } else if let Some(var_arg) = caps.name("var_arg") {
                    let arg_var = Variable::new(var_arg.as_str())?;
                    parsed.args.push(FilterArg {
                        is_lookup: true,
                        variable: Some(arg_var),
                        constant: None,
                        constant_value: std::sync::OnceLock::new(),
                    });
                }

                filters.push(parsed);
            }

            upto = start + mat.len();
        }

        if upto != token.len() {
            return Err(TemplateError::TemplateSyntaxError(format!(
                "Could not parse the remainder: '{}' from '{}'",
                &token[upto..],
                token,
            )));
        }

        let var_obj = var_obj.unwrap_or(FilterExpressionVar::Constant(None));

        Ok(FilterExpression {
            token: token.to_owned(),
            var: var_obj,
            is_var,
            filters,
            // Populated by `Parser::compile_filter`; left empty here so
            // `parse` stays Python-free for tests.
            filter_funcs: std::sync::Arc::new(Vec::new()),
        })
    }

    /// Mirrors `FilterExpression.resolve(context, ignore_failures=False)`.
    pub fn resolve<'py>(
        &self,
        py: Python<'py>,
        context: &Bound<'py, PyAny>,
        filter_funcs: &[Bound<'py, PyAny>],
        ignore_failures: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let dj = crate::python_cache::django(py)?;
        let mark_safe_fn = dj.mark_safe.bind(py);
        let safe_data_cls = dj.safe_data_cls.bind(py);

        let mut obj: Bound<'py, PyAny> = match &self.var {
            FilterExpressionVar::Var(v) => {
                if self.is_var {
                    match v.resolve(py, context) {
                        Ok(val) => val,
                        Err(e) => {
                            let is_vdne =
                                e.is_instance_of::<crate::errors::VariableDoesNotExist>(py);
                            if is_vdne {
                                if ignore_failures {
                                    py.None().into_bound(py)
                                } else {
                                    let string_if_invalid = get_string_if_invalid(py, context)?;
                                    let sii_str: String = string_if_invalid.extract()?;
                                    if !sii_str.is_empty() {
                                        if sii_str.contains("%s") {
                                            let formatted = sii_str.replace("%s", &v.var);
                                            return Ok(formatted.into_pyobject(py)?.into_any());
                                        } else {
                                            return Ok(string_if_invalid);
                                        }
                                    } else {
                                        string_if_invalid
                                    }
                                }
                            } else {
                                return Err(e);
                            }
                        }
                    }
                } else {
                    // Var that's actually a literal.
                    v.resolve(py, context)?
                }
            }
            FilterExpressionVar::Constant(opt) => match opt {
                Some(s) => mark_safe_fn.call1((s.as_str(),))?,
                None => py.None().into_bound(py),
            },
        };

        for (i, parsed_filter) in self.filters.iter().enumerate() {
            let func = &filter_funcs[i];

            let mut arg_vals: Vec<Bound<'py, PyAny>> = Vec::with_capacity(parsed_filter.args.len());
            for arg in &parsed_filter.args {
                if !arg.is_lookup {
                    let val = match &arg.constant {
                        Some(s) => mark_safe_fn.call1((s.as_str(),))?,
                        None => py.None().into_bound(py),
                    };
                    arg_vals.push(val);
                } else {
                    let var = arg.variable.as_ref().expect("lookup arg without variable");
                    arg_vals.push(var.resolve(py, context)?);
                }
            }

            // Check expects_localtime.
            let expects_localtime = func
                .getattr("expects_localtime")
                .ok()
                .and_then(|v| v.is_truthy().ok())
                .unwrap_or(false);
            if expects_localtime {
                let template_localtime = py
                    .import("django.utils.timezone")?
                    .getattr("template_localtime")?;
                let use_tz = context.getattr("use_tz")?;
                obj = template_localtime.call1((&obj, use_tz))?;
            }

            // Check needs_autoescape.
            let needs_autoescape = func
                .getattr("needs_autoescape")
                .ok()
                .and_then(|v| v.is_truthy().ok())
                .unwrap_or(false);

            let is_safe_before = obj.is_instance(safe_data_cls)?;

            let new_obj = if needs_autoescape {
                let autoescape = context.getattr("autoescape")?;
                // Build args: (obj, *arg_vals, autoescape=context.autoescape)
                let mut positional: Vec<Bound<'py, PyAny>> = Vec::with_capacity(1 + arg_vals.len());
                positional.push(obj.clone());
                positional.extend(arg_vals.iter().cloned());
                let args_tuple = PyTuple::new(py, &positional)?;
                let kwargs = PyDict::new(py);
                kwargs.set_item("autoescape", autoescape)?;
                func.call(&args_tuple, Some(&kwargs))?
            } else {
                let mut positional: Vec<Bound<'py, PyAny>> = Vec::with_capacity(1 + arg_vals.len());
                positional.push(obj.clone());
                positional.extend(arg_vals.iter().cloned());
                let args_tuple = PyTuple::new(py, &positional)?;
                func.call(&args_tuple, None)?
            };

            // Check is_safe flag.
            let filter_is_safe = func
                .getattr("is_safe")
                .ok()
                .and_then(|v| v.is_truthy().ok())
                .unwrap_or(false);

            if filter_is_safe && is_safe_before {
                obj = mark_safe_fn.call1((&new_obj,))?;
            } else {
                obj = new_obj;
            }
        }

        Ok(obj)
    }
}

impl std::fmt::Display for FilterExpression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.token)
    }
}

// Tests

#[cfg(test)]
#[allow(clippy::approx_constant)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_integer() {
        let v = Variable::new("42").unwrap();
        assert!(matches!(v.value, VariableValue::Int(42)));
        assert!(!v.translate);
    }

    #[test]
    fn test_parse_negative_integer() {
        let v = Variable::new("-1").unwrap();
        assert!(matches!(v.value, VariableValue::Int(-1)));
    }

    #[test]
    fn test_parse_zero() {
        let v = Variable::new("0").unwrap();
        assert!(matches!(v.value, VariableValue::Int(0)));
    }

    #[test]
    fn test_parse_float() {
        let v = Variable::new("3.14").unwrap();
        match &v.value {
            VariableValue::Float(f) => assert!((*f - 3.14).abs() < f64::EPSILON),
            other => panic!("expected Float, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_scientific_notation() {
        let v = Variable::new("1e10").unwrap();
        match &v.value {
            VariableValue::Float(f) => assert!((*f - 1e10).abs() < 1.0),
            other => panic!("expected Float, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_scientific_notation_uppercase() {
        let v = Variable::new("2.5E3").unwrap();
        match &v.value {
            VariableValue::Float(f) => assert!((*f - 2500.0).abs() < f64::EPSILON),
            other => panic!("expected Float, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_trailing_dot_invalid() {
        // "2." fails number parse, then falls through to lookup split.
        let v = Variable::new("2.").unwrap();
        match &v.value {
            VariableValue::Lookups(parts) => {
                assert_eq!(parts, &["2", ""]);
            }
            other => panic!("expected Lookups for '2.', got {:?}", other),
        }
    }

    #[test]
    fn test_parse_negative_float() {
        let v = Variable::new("-1.5").unwrap();
        match &v.value {
            VariableValue::Float(f) => assert!((*f - (-1.5)).abs() < f64::EPSILON),
            other => panic!("expected Float, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_double_quoted_string() {
        let v = Variable::new(r#""hello world""#).unwrap();
        match &v.value {
            VariableValue::StringLiteral(s) => assert_eq!(s, "hello world"),
            other => panic!("expected StringLiteral, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_single_quoted_string() {
        let v = Variable::new("'hello world'").unwrap();
        match &v.value {
            VariableValue::StringLiteral(s) => assert_eq!(s, "hello world"),
            other => panic!("expected StringLiteral, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_escaped_double_quote() {
        let v = Variable::new(r#""a \"bc\"""#).unwrap();
        match &v.value {
            VariableValue::StringLiteral(s) => assert_eq!(s, r#"a "bc""#),
            other => panic!("expected StringLiteral, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_escaped_backslash() {
        let v = Variable::new(r#""path\\to""#).unwrap();
        match &v.value {
            VariableValue::StringLiteral(s) => assert_eq!(s, r"path\to"),
            other => panic!("expected StringLiteral, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_empty_string() {
        let v = Variable::new(r#""""#).unwrap();
        match &v.value {
            VariableValue::StringLiteral(s) => assert_eq!(s, ""),
            other => panic!("expected StringLiteral, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_simple_variable() {
        let v = Variable::new("article").unwrap();
        match &v.value {
            VariableValue::Lookups(parts) => {
                assert_eq!(parts, &["article"]);
            }
            other => panic!("expected Lookups, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_dotted_variable() {
        let v = Variable::new("article.section.title").unwrap();
        match &v.value {
            VariableValue::Lookups(parts) => {
                assert_eq!(parts, &["article", "section", "title"]);
            }
            other => panic!("expected Lookups, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_variable_with_index() {
        let v = Variable::new("items.0.name").unwrap();
        match &v.value {
            VariableValue::Lookups(parts) => {
                assert_eq!(parts, &["items", "0", "name"]);
            }
            other => panic!("expected Lookups, got {:?}", other),
        }
    }

    #[test]
    fn test_translate_double_quoted() {
        let v = Variable::new(r#"_("hello")"#).unwrap();
        assert!(v.translate);
        match &v.value {
            VariableValue::StringLiteral(s) => assert_eq!(s, "hello"),
            other => panic!("expected StringLiteral, got {:?}", other),
        }
    }

    #[test]
    fn test_translate_single_quoted() {
        let v = Variable::new("_('hello')").unwrap();
        assert!(v.translate);
        match &v.value {
            VariableValue::StringLiteral(s) => assert_eq!(s, "hello"),
            other => panic!("expected StringLiteral, got {:?}", other),
        }
    }

    #[test]
    fn test_translate_variable_lookup() {
        // `_(var.path)` becomes a translate=true lookup.
        let v = Variable::new("_(myvar)").unwrap();
        assert!(v.translate);
        match &v.value {
            VariableValue::Lookups(parts) => assert_eq!(parts, &["myvar"]),
            other => panic!("expected Lookups, got {:?}", other),
        }
    }

    #[test]
    fn test_reject_leading_underscore() {
        let err = Variable::new("_private").unwrap_err();
        match err {
            TemplateError::TemplateSyntaxError(msg) => {
                assert!(msg.contains("underscores"), "unexpected message: {}", msg);
            }
            other => panic!("expected TemplateSyntaxError, got {:?}", other),
        }
    }

    #[test]
    fn test_reject_underscore_after_dot() {
        let err = Variable::new("obj._private").unwrap_err();
        match err {
            TemplateError::TemplateSyntaxError(msg) => {
                assert!(msg.contains("underscores"), "unexpected message: {}", msg);
            }
            other => panic!("expected TemplateSyntaxError, got {:?}", other),
        }
    }

    #[test]
    fn test_reject_plus_in_variable() {
        let err = Variable::new("a+b").unwrap_err();
        match err {
            TemplateError::TemplateSyntaxError(msg) => {
                assert!(
                    msg.contains("Invalid character"),
                    "unexpected message: {}",
                    msg
                );
                assert!(msg.contains('+'), "unexpected message: {}", msg);
            }
            other => panic!("expected TemplateSyntaxError, got {:?}", other),
        }
    }

    #[test]
    fn test_reject_minus_in_variable() {
        let err = Variable::new("a-b").unwrap_err();
        match err {
            TemplateError::TemplateSyntaxError(msg) => {
                assert!(
                    msg.contains("Invalid character"),
                    "unexpected message: {}",
                    msg
                );
                assert!(msg.contains('-'), "unexpected message: {}", msg);
            }
            other => panic!("expected TemplateSyntaxError, got {:?}", other),
        }
    }

    #[test]
    fn test_unescape_not_quoted() {
        assert_eq!(unescape_string_literal("hello"), None);
    }

    #[test]
    fn test_unescape_mismatched_quotes() {
        assert_eq!(unescape_string_literal("\"hello'"), None);
    }

    #[test]
    fn test_unescape_simple() {
        assert_eq!(unescape_string_literal("\"abc\""), Some("abc".to_owned()));
        assert_eq!(unescape_string_literal("'abc'"), Some("abc".to_owned()));
    }

    #[test]
    fn test_unescape_escaped_quotes() {
        assert_eq!(
            unescape_string_literal(r#""a \"b\"""#),
            Some(r#"a "b""#.to_owned())
        );
    }

    #[test]
    fn test_unescape_escaped_backslash() {
        assert_eq!(
            unescape_string_literal(r#""a\\b""#),
            Some(r"a\b".to_owned())
        );
    }

    #[test]
    fn test_filter_regex_compiles() {
        // Ensure the lazy regex initializes without panic.
        let _ = FILTER_RE.is_match("test");
    }

    #[test]
    fn test_filter_regex_matches_simple_var() {
        let caps = FILTER_RE.captures("variable").unwrap();
        assert_eq!(caps.name("var").unwrap().as_str(), "variable");
    }

    #[test]
    fn test_filter_regex_matches_quoted_constant() {
        let caps = FILTER_RE.captures(r#""hello""#).unwrap();
        assert_eq!(caps.name("constant").unwrap().as_str(), r#""hello""#);
    }

    #[test]
    fn test_parse_filter_expression_simple_var() {
        let fe = FilterExpression::parse("variable", |_name| unreachable!("no filters expected"))
            .unwrap();
        assert!(fe.is_var);
        assert!(fe.filters.is_empty());
        assert_eq!(fe.token, "variable");
    }

    #[test]
    fn test_parse_filter_expression_with_filter() {
        let fe = FilterExpression::parse("variable|upper", |name| {
            assert_eq!(name, "upper");
            Ok(ParsedFilter {
                name: name.to_owned(),
                args: vec![],
            })
        })
        .unwrap();
        assert!(fe.is_var);
        assert_eq!(fe.filters.len(), 1);
        assert_eq!(fe.filters[0].name, "upper");
        assert!(fe.filters[0].args.is_empty());
    }

    #[test]
    fn test_parse_filter_expression_with_arg() {
        let fe = FilterExpression::parse(r#"variable|default:"fallback""#, |name| {
            assert_eq!(name, "default");
            Ok(ParsedFilter {
                name: name.to_owned(),
                args: vec![],
            })
        })
        .unwrap();
        assert_eq!(fe.filters.len(), 1);
        assert_eq!(fe.filters[0].name, "default");
        assert_eq!(fe.filters[0].args.len(), 1);
        assert!(!fe.filters[0].args[0].is_lookup);
        assert_eq!(fe.filters[0].args[0].constant.as_deref(), Some("fallback"));
    }

    #[test]
    fn test_parse_filter_expression_chained() {
        let mut call_count = 0;
        let fe = FilterExpression::parse("var|filter1:arg1|filter2", |name| {
            call_count += 1;
            Ok(ParsedFilter {
                name: name.to_owned(),
                args: vec![],
            })
        })
        .unwrap();
        assert_eq!(fe.filters.len(), 2);
        assert_eq!(fe.filters[0].name, "filter1");
        assert_eq!(fe.filters[1].name, "filter2");
        // filter1 has a var_arg "arg1"
        assert_eq!(fe.filters[0].args.len(), 1);
        assert!(fe.filters[0].args[0].is_lookup);
    }

    #[test]
    fn test_parse_filter_expression_constant_head() {
        let fe = FilterExpression::parse(r#""hello"|upper"#, |name| {
            Ok(ParsedFilter {
                name: name.to_owned(),
                args: vec![],
            })
        })
        .unwrap();
        assert!(!fe.is_var);
        match &fe.var {
            FilterExpressionVar::Constant(Some(s)) => assert_eq!(s, "hello"),
            other => panic!("expected Constant(Some), got {:?}", other),
        }
    }

    #[test]
    fn test_parse_filter_expression_numeric_head() {
        let fe = FilterExpression::parse("42|add:1", |name| {
            Ok(ParsedFilter {
                name: name.to_owned(),
                args: vec![],
            })
        })
        .unwrap();
        // In Django, Variable("42") produces a Variable with literal=42.
        // Django's `is_var = isinstance(var_obj, Variable)` is True, but
        // semantically the value doesn't need context resolution -- it's a
        // pre-resolved literal. Our Rust implementation tracks whether context
        // resolution is actually needed (i.e., has lookups), so is_var is false
        // for numeric literals. This is a semantic refinement, not a bug.
        assert!(!fe.is_var);
    }

    #[test]
    fn test_parse_filter_expression_remainder_error() {
        let err = FilterExpression::parse("var|", |_| {
            Ok(ParsedFilter {
                name: String::new(),
                args: vec![],
            })
        })
        .unwrap_err();
        match err {
            TemplateError::TemplateSyntaxError(msg) => {
                assert!(
                    msg.contains("Could not parse the remainder"),
                    "unexpected: {}",
                    msg
                );
            }
            other => panic!("expected TemplateSyntaxError, got {:?}", other),
        }
    }

    #[test]
    fn test_variable_display() {
        let v = Variable::new("article.section").unwrap();
        assert_eq!(format!("{}", v), "article.section");
    }

    #[test]
    fn test_filter_expression_display() {
        let fe = FilterExpression::parse("var|upper", |name| {
            Ok(ParsedFilter {
                name: name.to_owned(),
                args: vec![],
            })
        })
        .unwrap();
        assert_eq!(format!("{}", fe), "var|upper");
    }
}
