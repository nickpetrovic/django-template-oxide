//! Node rendering: port of Django's `django.template.base.Node`,
//! `NodeList`, `TextNode`, `VariableNode`, and `render_value_in_context`.

use std::sync::Arc;

use pyo3::prelude::*;

use crate::context::{Context, Value};
use crate::errors::TemplateError;
use crate::filters::get_default_filters;
use crate::lexer::Token;
use crate::utils::{SafeString, html_escape};
use crate::variable::FilterExpression;

/// Origin name for templates from non-loader sources. Matches Django's
/// `django.template.base.UNKNOWN_SOURCE`.
pub const UNKNOWN_SOURCE: &str = "<unknown source>";

/// Attach `template_debug` to a `TemplateError` when debug mode is on.
/// Mirrors Django's `Node.render_annotated` (base.py:1044-1068) which
/// calls `context.render_context.template.get_exception_info(e, token)`.
fn attach_template_debug(
    py: Python<'_>,
    err: TemplateError,
    token: Option<&Token>,
    _origin: Option<&Origin>,
    context: &Context,
) -> TemplateError {
    let token = match token {
        Some(t) => t,
        None => return err,
    };

    // Convert the Rust error to a Python exception first so we can
    // set attributes on it.
    let py_err: pyo3::PyErr = err.into();
    let exc_obj = py_err.value(py);

    // Don't overwrite if already set (matches Django's `not hasattr(e, "template_debug")`).
    if exc_obj.hasattr("template_debug").unwrap_or(false) {
        return TemplateError::PythonError(py_err);
    }

    // Get the template from render_context (set by conftest / Django's push_state).
    let template_obj = context
        .render_context
        .template
        .as_ref()
        .map(|t| t.obj.bind(py));
    let template_obj = match template_obj {
        Some(t) => t,
        None => {
            // Fall back to context.template
            match context.template.as_ref() {
                Some(t) => t.obj.bind(py),
                None => return TemplateError::PythonError(py_err),
            }
        }
    };

    // Build a Python Token for get_exception_info
    if let Ok(py_token) = crate::django_drop_in::PyToken::from_rust_token(py, token) {
        let py_token_obj = match pyo3::Py::new(py, py_token) {
            Ok(t) => t,
            Err(_) => return TemplateError::PythonError(py_err),
        };

        if let Ok(get_exc_info) = template_obj.getattr("get_exception_info")
            && let Ok(debug_info) = get_exc_info.call1((&py_err, py_token_obj))
        {
            let _ = exc_obj.setattr("template_debug", debug_info);
        }
    }

    TemplateError::PythonError(py_err)
}

/// Where a template was loaded from. Mirrors `django.template.base.Origin`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub name: String,
    pub template_name: Option<String>,
    /// Loader name (string label only; no Python interop yet).
    pub loader: Option<String>,
}

impl Origin {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            template_name: None,
            loader: None,
        }
    }

    pub fn with_template_name(mut self, template_name: impl Into<String>) -> Self {
        self.template_name = Some(template_name.into());
        self
    }

    pub fn with_loader(mut self, loader: impl Into<String>) -> Self {
        self.loader = Some(loader.into());
        self
    }
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

/// Core rendering abstraction. Mirrors `django.template.base.Node`.
pub trait Node: std::fmt::Debug + Send + Sync {
    /// Downcast view. Used by the BodyProgram compiler to specialise
    /// known node kinds (`IfNode`, `ForNode`).
    fn as_any(&self) -> &dyn std::any::Any;

    /// `py` is required because rendering may cross into Python.
    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError>;

    /// On error, attaches the culprit token for Django's debug page.
    /// `TextNode` overrides this to skip exception handling.
    fn render_annotated(
        &self,
        py: Python<'_>,
        context: &mut Context,
    ) -> Result<String, TemplateError> {
        self.render(py, context)
    }

    /// Append rendered output directly into `out`. Fast path used by
    /// `NodeList::render` to avoid a per-node intermediate `String`
    /// allocation. Leaf nodes override with zero/single-allocation impls.
    #[inline]
    fn render_annotated_into(
        &self,
        py: Python<'_>,
        context: &mut Context,
        out: &mut String,
    ) -> Result<(), TemplateError> {
        let fragment = self.render_annotated(py, context)?;
        out.push_str(&fragment);
        Ok(())
    }

    fn token(&self) -> Option<&Token>;
    fn origin(&self) -> Option<&Origin>;
    fn set_token(&mut self, token: Token);
    fn set_origin(&mut self, origin: Origin);

    /// True if this node must appear first (e.g. `{% extends %}`).
    fn must_be_first(&self) -> bool {
        false
    }

    /// Names of child `NodeList` fields. Default matches Django's
    /// `child_nodelists = ("nodelist",)`.
    fn child_nodelists(&self) -> &[&str] {
        &["nodelist"]
    }

    fn get_child_nodelist(&self, _name: &str) -> Option<&NodeList> {
        None
    }

    /// Visit each owned `NodeList`. Default is no-op (leaf node).
    /// Container tags (if/for/with) override to expose their children.
    fn walk_children(&self, _visit: &mut dyn FnMut(&NodeList)) {}

    /// `BlockNode` returns `Some((name, Arc<NodeList>))`; default `None`.
    fn as_block_node_ref(&self) -> Option<(String, std::sync::Arc<NodeList>)> {
        None
    }

    /// `TextNode` returns its text; default `None`.
    fn as_text_bytes(&self) -> Option<&str> {
        None
    }

    /// `TextNode` returns its `Arc<str>` (refcount bump); default `None`.
    fn as_text_arc(&self) -> Option<std::sync::Arc<str>> {
        None
    }

    /// `VariableNode` returns `Some(self)`; default `None`. Used by
    /// tree walkers (e.g. blocktranslate builds its message by
    /// inspecting `{{ var }}` nodes in its body).
    fn as_variable_node(&self) -> Option<&VariableNode> {
        None
    }

    /// For nodes constructed from a foreign Python `Node`, expose the
    /// underlying `Py<PyAny>` so callers can `isinstance` on it
    /// (django-cotton's `_extract_vars_from_template` relies on this).
    fn as_py_node(&self) -> Option<&Py<pyo3::PyAny>> {
        None
    }
}

/// Expands the five identical `Node` trait methods: `as_any`, `token`,
/// `origin`, `set_token`, `set_origin`. Implementing structs must have
/// `token_field: Option<Token>` and `origin_field: Option<Origin>`.
#[macro_export]
macro_rules! impl_node_metadata {
    () => {
        #[inline]
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        #[inline]
        fn token(&self) -> Option<&$crate::lexer::Token> {
            self.token_field.as_ref()
        }

        #[inline]
        fn origin(&self) -> Option<&$crate::nodes::Origin> {
            self.origin_field.as_ref()
        }

        #[inline]
        fn set_token(&mut self, token: $crate::lexer::Token) {
            self.token_field = Some(token);
        }

        #[inline]
        fn set_origin(&mut self, origin: $crate::nodes::Origin) {
            self.origin_field = Some(origin);
        }
    };
}

/// Flat-enum entry. TextNode and VariableNode get direct dispatch (no
/// vtable); all other nodes are boxed in `Boxed`.
#[derive(Debug)]
pub enum NodeEntry {
    /// Literal text; `push_str` is a memcpy with no vtable call.
    Text(std::sync::Arc<str>),
    /// Stored inline (no Box) so LLVM can inline through the resolve chain.
    Variable(Box<VariableNode>),
    Boxed(Box<dyn Node>),
}

/// Ordered list of nodes. Mirrors `django.template.base.NodeList`.
#[derive(Debug)]
pub struct NodeList {
    pub nodes: Vec<NodeEntry>,
    pub contains_nontext: bool,
    /// Sum of TextNode byte lengths; lower-bound for buffer pre-allocation.
    text_bytes: usize,
}

impl NodeList {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            contains_nontext: false,
            text_bytes: 0,
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            nodes: Vec::with_capacity(capacity),
            contains_nontext: false,
            text_bytes: 0,
        }
    }

    /// Append a node. TextNodes get unwrapped into `NodeEntry::Text`
    /// for direct dispatch; other nodes stay boxed. Use [`push_variable`]
    /// directly to avoid trait-object indirection for VariableNodes.
    pub fn push(&mut self, node: Box<dyn Node>) {
        if let Some(text_arc) = node.as_text_arc() {
            self.text_bytes = self.text_bytes.saturating_add(text_arc.len());
            self.nodes.push(NodeEntry::Text(text_arc));
        } else {
            self.contains_nontext = true;
            self.nodes.push(NodeEntry::Boxed(node));
        }
    }

    /// Direct-dispatch path for VariableNodes. The render path calls
    /// `VariableNode::render_annotated_into` monomorphically.
    pub fn push_variable(&mut self, var_node: Box<VariableNode>) {
        self.contains_nontext = true;
        self.nodes.push(NodeEntry::Variable(var_node));
    }

    /// Render to a single shared buffer. Mirrors `NodeList.render` but
    /// uses `render_annotated_into` to avoid per-child allocations.
    pub fn render(
        &self,
        py: Python<'_>,
        context: &mut Context,
    ) -> Result<SafeString, TemplateError> {
        let _g = crate::prof::Guard::new("NodeList::render");

        // Pre-size: known text bytes + 16 per non-text node.
        let estimated = self
            .text_bytes
            .saturating_add(self.nodes.len().saturating_mul(16));
        let mut parts = String::with_capacity(estimated);

        self.render_into(py, context, &mut parts)?;
        Ok(SafeString::new(parts))
    }

    /// In-place equivalent of [`render`]. Container nodes stream child
    /// output into the surrounding buffer without intermediate allocations.
    #[inline]
    pub fn render_into(
        &self,
        py: Python<'_>,
        context: &mut Context,
        out: &mut String,
    ) -> Result<(), TemplateError> {
        let debug = context.debug;
        for entry in &self.nodes {
            match entry {
                NodeEntry::Text(s) => out.push_str(s),
                NodeEntry::Variable(var_node) => {
                    if let Err(e) = var_node.render_annotated_into(py, context, out) {
                        if debug {
                            return Err(attach_template_debug(
                                py,
                                e,
                                var_node.token(),
                                var_node.origin(),
                                context,
                            ));
                        }
                        return Err(e);
                    }
                }
                NodeEntry::Boxed(node) => {
                    if let Err(e) = node.render_annotated_into(py, context, out) {
                        if debug {
                            return Err(attach_template_debug(
                                py,
                                e,
                                node.token(),
                                node.origin(),
                                context,
                            ));
                        }
                        return Err(e);
                    }
                }
            }
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn iter_entries(&self) -> std::slice::Iter<'_, NodeEntry> {
        self.nodes.iter()
    }

    /// Iterate non-text entries as `&dyn Node`. Text entries are skipped
    /// (no Token, no children, no behaviour); VariableNode is upcast.
    pub fn iter(&self) -> impl Iterator<Item = &dyn Node> {
        self.nodes.iter().filter_map(|entry| match entry {
            NodeEntry::Boxed(n) => Some(n.as_ref()),
            NodeEntry::Variable(v) => Some(v.as_ref() as &dyn Node),
            NodeEntry::Text(_) => None,
        })
    }
}

impl Default for NodeList {
    fn default() -> Self {
        Self::new()
    }
}

/// Literal text node. Mirrors `django.template.base.TextNode`. Text is
/// `Arc<str>` so cloning is a refcount bump; the hot render path
/// (`render_annotated_into`) pushes raw bytes directly to the output.
#[derive(Debug, Clone)]
pub struct TextNode {
    pub s: Arc<str>,
    pub token_field: Option<Token>,
    pub origin_field: Option<Origin>,
}

impl TextNode {
    pub fn new(s: impl Into<Arc<str>>) -> Self {
        Self {
            s: s.into(),
            token_field: None,
            origin_field: None,
        }
    }
}

impl Node for TextNode {
    impl_node_metadata!();

    fn render(&self, _py: Python<'_>, _context: &mut Context) -> Result<String, TemplateError> {
        Ok((*self.s).to_owned())
    }

    /// Skip exception handling: TextNode cannot fail. Matches Django's
    /// `TextNode.render_annotated`.
    fn render_annotated(
        &self,
        _py: Python<'_>,
        _context: &mut Context,
    ) -> Result<String, TemplateError> {
        Ok((*self.s).to_owned())
    }

    /// Zero-alloc fast path: push literal bytes into the shared buffer.
    #[inline]
    fn render_annotated_into(
        &self,
        _py: Python<'_>,
        _context: &mut Context,
        out: &mut String,
    ) -> Result<(), TemplateError> {
        out.push_str(&self.s);
        Ok(())
    }

    fn child_nodelists(&self) -> &[&str] {
        &[]
    }

    #[inline]
    fn as_text_bytes(&self) -> Option<&str> {
        Some(&self.s)
    }

    #[inline]
    fn as_text_arc(&self) -> Option<std::sync::Arc<str>> {
        Some(std::sync::Arc::clone(&self.s))
    }
}

/// Resolves a `FilterExpression` and renders the result. Mirrors
/// `django.template.base.VariableNode`.
pub struct VariableNode {
    pub filter_expression: FilterExpression,
    /// Python filter callables, indexed parallel to `filter_expression.filters`.
    pub filter_funcs: Vec<Py<PyAny>>,
    /// Pre-resolved native filter pointers, set at construction to
    /// avoid a HashMap probe per render. `None` entries fall back to
    /// the registry. Saves several hundred ns per filter site in loops.
    pub native_filter_cache: Vec<Option<&'static crate::filters::NativeFilter>>,
    /// Pre-resolved `FilterId` per slot. Match-dispatch lets LLVM inline
    /// known filter bodies (`default`, `safe`, `upper`).
    pub native_filter_ids: Vec<crate::filters::FilterId>,
    pub token_field: Option<Token>,
    pub origin_field: Option<Origin>,
}

impl std::fmt::Debug for VariableNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `NativeFilter` deliberately omits Debug (function pointers).
        f.debug_struct("VariableNode")
            .field("filter_expression", &self.filter_expression)
            .field("filter_funcs", &self.filter_funcs)
            .field("native_filter_ids", &self.native_filter_ids)
            .field(
                "native_filter_cache_resolved",
                &self
                    .native_filter_cache
                    .iter()
                    .filter(|n| n.is_some())
                    .count(),
            )
            .field("token_field", &self.token_field)
            .field("origin_field", &self.origin_field)
            .finish()
    }
}

impl Clone for VariableNode {
    fn clone(&self) -> Self {
        Python::attach(|py| Self {
            filter_expression: self.filter_expression.clone(),
            filter_funcs: self.filter_funcs.iter().map(|f| f.clone_ref(py)).collect(),
            native_filter_cache: self.native_filter_cache.clone(),
            native_filter_ids: self.native_filter_ids.clone(),
            token_field: self.token_field.clone(),
            origin_field: self.origin_field.clone(),
        })
    }
}

impl VariableNode {
    pub fn new(filter_expression: FilterExpression) -> Self {
        let native_filter_cache = build_native_filter_cache(&filter_expression);
        let native_filter_ids = filter_expression
            .filters
            .iter()
            .map(|pf| crate::filters::FilterId::from_name(&pf.name))
            .collect();
        Self {
            filter_expression,
            filter_funcs: Vec::new(),
            native_filter_cache,
            native_filter_ids,
            token_field: None,
            origin_field: None,
        }
    }

    pub fn with_filters(filter_expression: FilterExpression, filter_funcs: Vec<Py<PyAny>>) -> Self {
        let native_filter_cache = build_native_filter_cache(&filter_expression);
        let native_filter_ids = filter_expression
            .filters
            .iter()
            .map(|pf| crate::filters::FilterId::from_name(&pf.name))
            .collect();
        Self {
            filter_expression,
            filter_funcs,
            native_filter_cache,
            native_filter_ids,
            token_field: None,
            origin_field: None,
        }
    }
}

/// Resolve `fe.filters` against the static native registry once.
/// `None` entries are filters not in the registry.
fn build_native_filter_cache(
    fe: &FilterExpression,
) -> Vec<Option<&'static crate::filters::NativeFilter>> {
    let registry = get_default_filters();
    fe.filters.iter().map(|pf| registry.get(&pf.name)).collect()
}

impl Node for VariableNode {
    impl_node_metadata!();

    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        if self.filter_expression.filters.is_empty() {
            let output = resolve_variable_rust(py, &self.filter_expression, context)?;
            render_value_in_context_checked(&output, context).map_err(TemplateError::PythonError)
        } else {
            let output = resolve_with_filters_rust_cached(
                py,
                &self.filter_expression,
                context,
                Some(&self.native_filter_cache),
                Some(&self.native_filter_ids),
                Some(&self.filter_funcs),
            )?;
            render_value_in_context_checked(&output, context).map_err(TemplateError::PythonError)
        }
    }

    /// Push the rendered variable directly into the output buffer,
    /// avoiding the intermediate `String` of the default path.
    #[inline]
    fn render_annotated_into(
        &self,
        py: Python<'_>,
        context: &mut Context,
        out: &mut String,
    ) -> Result<(), TemplateError> {
        let output = if self.filter_expression.filters.is_empty() {
            resolve_variable_rust(py, &self.filter_expression, context)?
        } else {
            resolve_with_filters_rust_cached(
                py,
                &self.filter_expression,
                context,
                Some(&self.native_filter_cache),
                Some(&self.native_filter_ids),
                Some(&self.filter_funcs),
            )?
        };
        if let Value::PyObject(_) = &output {
            let rendered = render_value_in_context_checked(&output, context)
                .map_err(TemplateError::PythonError)?;
            out.push_str(&rendered);
        } else {
            render_value_in_context_into(&output, context, out);
        }
        Ok(())
    }

    fn child_nodelists(&self) -> &[&str] {
        &[]
    }

    #[inline]
    fn as_variable_node(&self) -> Option<&VariableNode> {
        Some(self)
    }
}

/// Resolve a `FilterExpression` from tag code (`{% if %}`, `{% for %}`,
/// etc.). Handles filters, PyObject lookups, and translation.
pub fn resolve_expression_rust(
    py: Python<'_>,
    fe: &FilterExpression,
    context: &Context,
) -> Result<Value, TemplateError> {
    if fe.filters.is_empty() {
        resolve_variable_rust(py, fe, context)
    } else {
        resolve_with_filters_rust(py, fe, context)
    }
}

/// Render `string_if_invalid` for a missing variable. Mirrors Django's
/// `FilterExpression.resolve`: single `%s` substitution with the original
/// var expression; otherwise return unchanged.
fn format_invalid_message(string_if_invalid: &str, var_expr: &str) -> String {
    if string_if_invalid.contains("%s") {
        string_if_invalid.replacen("%s", var_expr, 1)
    } else {
        string_if_invalid.to_owned()
    }
}

/// Index a string by char position (`{{ s.0 }}`), preserving safety.
/// Returns `None` on non-integer or out-of-range.
fn string_index_lookup(s: &str, part: &str, was_safe: bool) -> Option<Value> {
    let idx = part.parse::<usize>().ok()?;
    let ch = s.chars().nth(idx)?;
    let ch_str = ch.to_string();
    Some(if was_safe {
        Value::SafeString(ch_str.into())
    } else {
        Value::String(ch_str)
    })
}

/// Like `resolve_expression_rust` but missing variables resolve to
/// `Value::None`. Matches Django's `resolve(context, ignore_failures=True)`.
///
/// Django's `FilterExpression.resolve(context, ignore_failures=True)`
/// returns `None` when the variable doesn't exist, regardless of the
/// engine's `string_if_invalid` setting.  We detect a missing variable
/// by temporarily blanking `string_if_invalid` so the base resolver
/// produces an empty string on miss, then convert that empty string to
/// `Value::None`.
pub fn resolve_expression_ignore_failures(
    py: Python<'_>,
    fe: &FilterExpression,
    context: &Context,
) -> Result<Value, TemplateError> {
    if fe.filters.is_empty() {
        let mut val = resolve_base_variable(py, fe, context)?;
        if let crate::variable::FilterExpressionVar::Var(variable) = &fe.var
            && variable.translate
        {
            val = apply_translation_rust(py, &val, variable.message_context.as_deref())?;
        }
        if fe.is_var {
            match &val {
                Value::String(s) if s.is_empty() => {
                    return Ok(Value::None);
                }
                Value::String(s) if !context.string_if_invalid.is_empty() => {
                    // If the resolved value equals what string_if_invalid
                    // would produce, the variable was missing.
                    if let crate::variable::FilterExpressionVar::Var(variable) = &fe.var {
                        let expected =
                            format_invalid_message(&context.string_if_invalid, &variable.var);
                        if s == &expected {
                            return Ok(Value::None);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(val)
    } else {
        resolve_with_filters_ignore_failures(py, fe, context)
    }
}

/// Resolve a filter-argument variable lookup (`{{ x|filter:foo.bar }}`),
/// walking dot-segments. Dict bases stay Rust-side; PyObject bases go
/// through [`resolve_pyobject_lookups`]; other bases resolve to empty
/// string per Django's semantics. First-segment miss falls back to
/// int/float parsing of the full var_name.
#[inline]
fn resolve_lookup_arg_native(py: Python<'_>, context: &Context, var_name: &str) -> Value {
    let mut parts = var_name.split('.');
    let first = parts.next().expect("split yields at least one item");
    let head = match context.get(first) {
        Some(v) => v,
        None => {
            return if let Ok(n) = var_name.parse::<i64>() {
                Value::Int(n)
            } else if let Ok(f) = var_name.parse::<f64>() {
                Value::Float(f)
            } else {
                Value::String(String::new())
            };
        }
    };

    let mut current = head.clone();
    loop {
        let part = match parts.next() {
            Some(p) => p,
            None => return current,
        };
        match current {
            Value::Dict(map) => {
                current = map
                    .get(part)
                    .cloned()
                    .unwrap_or(Value::String(String::new()));
            }
            Value::PyObject(obj) => {
                // Delegate the whole tail to Python in one FFI hop.
                let mut tail: Vec<String> = Vec::with_capacity(1 + parts.size_hint().0);
                tail.push(part.to_owned());
                tail.extend(parts.map(String::from));
                return resolve_pyobject_lookups(py, obj.bind(py), &tail, "")
                    .unwrap_or(Value::String(String::new()));
            }
            _ => return Value::String(String::new()),
        }
    }
}

/// `ignore_failures=True` variant of `resolve_with_filters_rust`.
#[inline]
fn resolve_with_filters_ignore_failures(
    py: Python<'_>,
    fe: &FilterExpression,
    context: &Context,
) -> Result<Value, TemplateError> {
    resolve_with_filters_inner(py, fe, context, None, None, None, true)
}

/// Resolve a `FilterExpression`, falling back to Python for PyObject lookups.
fn resolve_variable_rust(
    py: Python<'_>,
    fe: &FilterExpression,
    context: &Context,
) -> Result<Value, TemplateError> {
    let mut value = resolve_base_variable(py, fe, context)?;

    if let crate::variable::FilterExpressionVar::Var(variable) = &fe.var
        && variable.translate
    {
        value = apply_translation_rust(py, &value, variable.message_context.as_deref())?;
    }

    Ok(value)
}

/// Apply Django's translation via `gettext_lazy` or `pgettext_lazy`.
fn apply_translation_rust(
    py: Python<'_>,
    value: &Value,
    message_context: Option<&str>,
) -> Result<Value, TemplateError> {
    let dj = crate::python_cache::django(py)
        .map_err(|e| TemplateError::Internal(format!("Cannot import Django modules: {e}")))?;

    // Replace % with %% to avoid accidental formatting (matches
    // Variable._resolve_lookup).
    let value_str = value.to_string().replace('%', "%%");

    let is_safe = matches!(value, Value::SafeString(_));

    let msgid = if is_safe {
        dj.mark_safe
            .bind(py)
            .call1((value_str.as_str(),))
            .map_err(|e| TemplateError::Internal(format!("{e}")))?
    } else {
        value_str
            .into_pyobject(py)
            .map_err(|e| TemplateError::Internal(format!("{e}")))?
            .into_any()
    };

    let result = if let Some(msg_ctx) = message_context {
        dj.pgettext_lazy
            .bind(py)
            .call1((msg_ctx, &msgid))
            .map_err(|e| TemplateError::Internal(format!("{e}")))?
    } else {
        dj.gettext_lazy
            .bind(py)
            .call1((&msgid,))
            .map_err(|e| TemplateError::Internal(format!("{e}")))?
    };

    Ok(Value::from(&result))
}

/// Resolve just the base variable part of a `FilterExpression`. Native
/// Rust types stay Rust-side; PyObject values go through Python's
/// attribute/item lookup, matching `Variable._resolve_lookup`.
// Deliberately not `inline(always)`: large body, no measurable benefit.
#[inline]
fn resolve_base_variable(
    py: Python<'_>,
    fe: &FilterExpression,
    context: &Context,
) -> Result<Value, TemplateError> {
    let _g = crate::prof::Guard::new("resolve_base_variable");
    use crate::variable::FilterExpressionVar;

    match &fe.var {
        FilterExpressionVar::Var(variable) => {
            if !variable.is_lookup() {
                // String literals are marked safe (Django mark_safe).
                if let Some(s) = variable.as_string_literal() {
                    return Ok(Value::SafeString(s.to_owned().into()));
                }
                if let Some(n) = variable.as_int_literal() {
                    return Ok(Value::Int(n));
                }
                if let Some(f) = variable.as_float_literal() {
                    return Ok(Value::Float(f));
                }
            }

            // Pre-parsed lookup path from `variable.value`; re-splitting
            // `variable.var` here would burn ~2500 Vec allocations per
            // render on heavy templates.
            let parts: &[String] = variable
                .lookups()
                .expect("non-lookup variable handled by literal arms above");

            // Batched-loop fast path: if the for-loop's ForBatchPlan
            // pre-stamped a slot on this variable, read the resolved
            // value from the current tuple (single FFI call, no
            // getattr chain). Outside batched loops the block is a
            // single null-check.
            if let Some(slot) = variable.batch_slot()
                && let Some(cache) = context.loop_batch_cache.as_ref()
            {
                let tuple = cache.current_tuple.bind(py);
                if let Ok(val) = tuple.get_item(slot as usize) {
                    return Ok(value_from_pyany_fast(&val));
                }
            }

            // Borrow head; we only clone when walking Rust Dict/List.
            let head = match context.get(&parts[0]) {
                Some(v) => v,
                None => {
                    return Ok(Value::String(format_invalid_message(
                        &context.string_if_invalid,
                        &variable.var,
                    )));
                }
            };

            // PyObject head: delegate the whole chain to Python.
            if let Value::PyObject(obj) = head {
                if parts.len() > 1 {
                    return resolve_pyobject_lookups(
                        py,
                        obj.bind(py),
                        &parts[1..],
                        &context.string_if_invalid,
                    );
                }
                return resolve_pyobject_callable(py, head, &context.string_if_invalid);
            }

            // Walk Rust Dict/List by borrowed reference; only the leaf
            // is cloned. Cloning per step previously cost ~22us per
            // render for `forloop.counter`-heavy templates.
            let mut cur: &Value = head;
            for (i, part) in parts[1..].iter().enumerate() {
                match cur {
                    Value::Dict(map) => match map.get(part.as_str()) {
                        Some(v) => {
                            cur = v;
                        }
                        None => {
                            return Ok(Value::String(format_invalid_message(
                                &context.string_if_invalid,
                                &variable.var,
                            )));
                        }
                    },
                    Value::List(items) => {
                        match part.parse::<usize>().ok().and_then(|idx| items.get(idx)) {
                            Some(v) => {
                                cur = v;
                            }
                            None => {
                                return Ok(Value::String(format_invalid_message(
                                    &context.string_if_invalid,
                                    &variable.var,
                                )));
                            }
                        }
                    }
                    // String/SafeString/PyObject are terminal: return
                    // directly so the loop body keeps walking &Value.
                    Value::String(s) => {
                        return Ok(string_index_lookup(s.as_str(), part, false).unwrap_or_else(
                            || {
                                Value::String(format_invalid_message(
                                    &context.string_if_invalid,
                                    &variable.var,
                                ))
                            },
                        ));
                    }
                    Value::SafeString(s) => {
                        return Ok(string_index_lookup(s.as_ref(), part, true).unwrap_or_else(
                            || {
                                Value::String(format_invalid_message(
                                    &context.string_if_invalid,
                                    &variable.var,
                                ))
                            },
                        ));
                    }
                    Value::PyObject(obj) => {
                        return resolve_pyobject_lookups(
                            py,
                            obj.bind(py),
                            &parts[1 + i..],
                            &context.string_if_invalid,
                        );
                    }
                    _ => {
                        return Ok(Value::String(format_invalid_message(
                            &context.string_if_invalid,
                            &variable.var,
                        )));
                    }
                }
            }
            // End of chain via Dict/List borrows; single leaf clone.
            if matches!(cur, Value::PyObject(_)) {
                return resolve_pyobject_callable(py, cur, &context.string_if_invalid);
            }
            Ok(cur.clone())
        }
        FilterExpressionVar::Constant(opt) => match opt {
            Some(s) => Ok(Value::SafeString(s.clone().into())),
            None => Ok(Value::None),
        },
    }
}

/// Lookup chain against a Python object, matching `Variable._resolve_lookup`.
/// Per step: try `obj[key]`, then `getattr`, then int-index. Auto-calls
/// callables unless `do_not_call_in_templates` or `alters_data` is set.
/// `silent_variable_failure` exceptions are silenced; others propagate.
#[inline]
fn resolve_pyobject_lookups(
    py: Python<'_>,
    start: &Bound<'_, pyo3::PyAny>,
    parts: &[String],
    string_if_invalid: &str,
) -> Result<Value, TemplateError> {
    let _g = crate::prof::Guard::new("resolve_pyobject_lookups");
    use pyo3::types::{PyDict, PyList, PyTuple};

    let mut current = start.clone();

    // Primitives/collections are never callable in template terms.
    if !is_primitive_or_collection(&current) && type_is_callable(&current) {
        let do_not_call = current
            .getattr("do_not_call_in_templates")
            .ok()
            .and_then(|v| v.is_truthy().ok())
            .unwrap_or(false);
        if !do_not_call {
            let alters_data = current
                .getattr("alters_data")
                .ok()
                .and_then(|v| v.is_truthy().ok())
                .unwrap_or(false);
            if alters_data {
                return Ok(Value::String(string_if_invalid.to_owned()));
            }
            if let Ok(val) = current.call0() {
                current = val;
            }
        }
    }

    // Err(None) = soft failure (caller substitutes string_if_invalid);
    // Err(Some(e)) = real PyErr to consider propagating. Using
    // Option<PyErr> avoids allocating a PyKeyError just to discard it.
    let result: Result<Bound<'_, pyo3::PyAny>, Option<pyo3::PyErr>> = (|| {
        for bit in parts {
            // Dict fast path. IMPORTANT: `is_exact_instance_of`, NOT
            // `cast`: subclasses must use the slower path so Python-
            // level `__getitem__` overrides are honoured (base.py:963).
            if current.is_exact_instance_of::<PyDict>() {
                let d = current.cast::<PyDict>().expect("checked above");
                if let Some(val) = d.get_item(bit).map_err(Some)? {
                    current = val;
                    // Django auto-calls callables from dict lookups
                    // (test_basic_syntax38).
                    if !is_primitive_or_collection(&current) && current.is_callable() {
                        maybe_call_template_callable(py, &mut current, string_if_invalid).map_err(
                            |e| match e {
                                TemplateError::PythonError(pe) => Some(pe),
                                _ => None,
                            },
                        )?;
                    }
                    continue;
                }
                // Key missing: fall through to attribute lookup.
            }
            // List/tuple integer index.
            else if let Ok(idx) = bit.parse::<isize>() {
                if let Ok(list) = current.cast::<PyList>() {
                    if let Ok(val) = list.get_item(idx as usize) {
                        current = val;
                        continue;
                    }
                } else if let Ok(tup) = current.cast::<PyTuple>()
                    && let Ok(val) = tup.get_item(idx as usize)
                {
                    current = val;
                    continue;
                }
            }

            // General lookup. Skip `get_item` when the type doesn't
            // support it (per-type cache by ptr) to avoid a TypeError
            // FFI round-trip per step for plain class instances.
            let bit_py_owned = interned_pystring(py, bit);
            let bit_py = bit_py_owned.bind(py);
            let dict_result = if type_supports_getitem(py, &current) {
                current.get_item(bit_py).map_err(|e| {
                    if e.is_instance_of::<pyo3::exceptions::PyTypeError>(py)
                        || e.is_instance_of::<pyo3::exceptions::PyKeyError>(py)
                        || e.is_instance_of::<pyo3::exceptions::PyValueError>(py)
                        || e.is_instance_of::<pyo3::exceptions::PyIndexError>(py)
                        || e.is_instance_of::<pyo3::exceptions::PyAttributeError>(py)
                    {
                        None
                    } else {
                        let silent = e
                            .value(py)
                            .getattr("silent_variable_failure")
                            .ok()
                            .and_then(|v| v.is_truthy().ok())
                            .unwrap_or(false);
                        if silent { None } else { Some(e) }
                    }
                })
            } else {
                Err(None)
            };

            current = match dict_result {
                Ok(val) => val,
                Err(Some(propagate_err)) => return Err(Some(propagate_err)),
                Err(None) => {
                    match current.getattr(bit_py) {
                        Ok(val) => val,
                        Err(attr_err) => {
                            // If the attribute is in dir() but raised,
                            // it's a @property that errored. Check
                            // silent_variable_failure; if not set,
                            // propagate per Django's _resolve_lookup.
                            let in_dir = current
                                .dir()
                                .ok()
                                .map(|dir_list| {
                                    dir_list.iter().any(|item| {
                                        item.extract::<String>().map(|s| s == *bit).unwrap_or(false)
                                    })
                                })
                                .unwrap_or(false);
                            if in_dir {
                                let silent = attr_err
                                    .value(py)
                                    .getattr("silent_variable_failure")
                                    .ok()
                                    .and_then(|v| v.is_truthy().ok())
                                    .unwrap_or(false);
                                if !silent {
                                    return Err(Some(attr_err));
                                }
                                return Err(None);
                            }

                            // Missing attribute: try int index per
                            // `Variable._resolve_lookup`, else soft fail.
                            match bit.parse::<i64>() {
                                Ok(idx) => match current.get_item(idx) {
                                    Ok(val) => val,
                                    Err(_) => return Err(None),
                                },
                                Err(_) => return Err(None),
                            }
                        }
                    }
                }
            };

            // Callable check after each step (cached per-type to avoid
            // repeating the FFI for the same Python type).
            if !is_primitive_or_collection(&current) && type_is_callable(&current) {
                let do_not_call = current
                    .getattr("do_not_call_in_templates")
                    .ok()
                    .and_then(|v| v.is_truthy().ok())
                    .unwrap_or(false);

                if !do_not_call {
                    let alters_data = current
                        .getattr("alters_data")
                        .ok()
                        .and_then(|v| v.is_truthy().ok())
                        .unwrap_or(false);

                    if alters_data {
                        let sii = string_if_invalid.into_pyobject(py).unwrap().into_any();
                        return Ok(sii);
                    }

                    match current.call0() {
                        Ok(val) => current = val,
                        Err(call_err) => {
                            if call_err.is_instance_of::<pyo3::exceptions::PyTypeError>(py) {
                                let inspect = py.import("inspect").map_err(Some)?;
                                let sig_fn = inspect.getattr("signature").map_err(Some)?;
                                match sig_fn.call1((&current,)) {
                                    Ok(sig) => match sig.call_method0("bind") {
                                        Err(_) => {
                                            let sii = string_if_invalid
                                                .into_pyobject(py)
                                                .unwrap()
                                                .into_any();
                                            return Ok(sii);
                                        }
                                        Ok(_) => return Err(Some(call_err)),
                                    },
                                    Err(_) => {
                                        let sii =
                                            string_if_invalid.into_pyobject(py).unwrap().into_any();
                                        return Ok(sii);
                                    }
                                }
                            } else {
                                return Err(Some(call_err));
                            }
                        }
                    }
                }
            }
        }

        Ok(current)
    })();

    match result {
        Ok(val) => {
            // Skip `Value::from`'s `getattr("__html__")` FFI for plain
            // str/int/bool/None and bare dict/list. SafeString/datetime/
            // Promise/etc. fall through to `Value::from`.
            use pyo3::types::{PyBool, PyFloat, PyInt, PyString};
            if val.is_exact_instance_of::<PyString>() {
                if let Ok(s) = val.extract::<String>() {
                    return Ok(Value::String(s));
                }
            } else if val.is_exact_instance_of::<PyBool>() {
                return Ok(Value::Bool(val.extract::<bool>().unwrap_or(false)));
            } else if val.is_exact_instance_of::<PyInt>() {
                if let Ok(n) = val.extract::<i64>() {
                    return Ok(Value::Int(n));
                }
            } else if val.is_exact_instance_of::<PyFloat>() {
                if let Ok(f) = val.extract::<f64>() {
                    return Ok(Value::Float(f));
                }
            } else if val.is_none() {
                return Ok(Value::None);
            }
            Ok(Value::from(&val))
        }
        Err(None) => Ok(Value::String(string_if_invalid.to_owned())),
        // Respect silent_variable_failure; treat KeyError as soft
        // failure (Django historical); otherwise propagate.
        Err(Some(e)) => {
            let silent = e
                .value(py)
                .getattr("silent_variable_failure")
                .ok()
                .and_then(|v| v.is_truthy().ok())
                .unwrap_or(false);

            if silent || e.is_instance_of::<pyo3::exceptions::PyKeyError>(py) {
                Ok(Value::String(string_if_invalid.to_owned()))
            } else {
                Err(TemplateError::PythonError(e))
            }
        }
    }
}

/// Auto-call top-level callable PyObjects per Django semantics.
fn resolve_pyobject_callable(
    py: Python<'_>,
    value: &Value,
    string_if_invalid: &str,
) -> Result<Value, TemplateError> {
    if let Value::PyObject(obj) = value {
        let bound = obj.bind(py);
        if bound.is_callable() {
            let do_not_call = bound
                .getattr("do_not_call_in_templates")
                .ok()
                .and_then(|v| v.is_truthy().ok())
                .unwrap_or(false);

            if do_not_call {
                return Ok(value.clone());
            }

            let alters_data = bound
                .getattr("alters_data")
                .ok()
                .and_then(|v| v.is_truthy().ok())
                .unwrap_or(false);

            if alters_data {
                return Ok(Value::String(string_if_invalid.to_owned()));
            }

            match bound.call0() {
                Ok(val) => return Ok(Value::from(&val)),
                Err(call_err) => {
                    if call_err.is_instance_of::<pyo3::exceptions::PyTypeError>(py) {
                        return Ok(Value::String(String::new()));
                    }
                    return Err(TemplateError::from(call_err));
                }
            }
        }
    }
    Ok(value.clone())
}

/// Auto-call callable lookup result. Shared between the dict fast path
/// and the slow path of `resolve_pyobject_lookups`.
fn maybe_call_template_callable<'py>(
    py: Python<'py>,
    current: &mut Bound<'py, pyo3::PyAny>,
    string_if_invalid: &str,
) -> Result<(), TemplateError> {
    let do_not_call = current
        .getattr("do_not_call_in_templates")
        .ok()
        .and_then(|v| v.is_truthy().ok())
        .unwrap_or(false);
    if do_not_call {
        return Ok(());
    }

    let alters_data = current
        .getattr("alters_data")
        .ok()
        .and_then(|v| v.is_truthy().ok())
        .unwrap_or(false);
    if alters_data {
        *current = string_if_invalid.into_pyobject(py).unwrap().into_any();
        return Ok(());
    }

    match current.call0() {
        Ok(val) => {
            *current = val;
            Ok(())
        }
        Err(call_err) => {
            // TypeError (required args) becomes string_if_invalid.
            if call_err.is_instance_of::<pyo3::exceptions::PyTypeError>(py) {
                *current = string_if_invalid.into_pyobject(py).unwrap().into_any();
                Ok(())
            } else {
                Err(TemplateError::from(call_err))
            }
        }
    }
}

/// True for exact str/int/float/bool/dict/list/tuple. These are never
/// template-callable. Subclasses fall through to the slow callable
/// check, which is correct.
#[inline]
fn is_primitive_or_collection(obj: &Bound<'_, pyo3::PyAny>) -> bool {
    use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple};

    obj.is_none()
        || obj.is_exact_instance_of::<PyString>()
        || obj.is_exact_instance_of::<PyDict>()
        || obj.is_exact_instance_of::<PyList>()
        || obj.is_exact_instance_of::<PyTuple>()
        || obj.is_exact_instance_of::<PyInt>()
        || obj.is_exact_instance_of::<PyFloat>()
        || obj.is_exact_instance_of::<PyBool>()
}

type FastHashMap<K, V> =
    std::collections::HashMap<K, V, std::hash::BuildHasherDefault<crate::context::FastHasher>>;

thread_local! {
    /// Per-type cache of `__getitem__` and callable bits. Holds a
    /// strong `Py<PyType>` so addresses can't be reused for different
    /// types after GC (the cache is keyed by type-pointer address).
    static TYPE_BEHAVIOR_CACHE: std::cell::RefCell<
        FastHashMap<usize, (Py<pyo3::types::PyType>, TypeBehavior)>
    > = std::cell::RefCell::new(FastHashMap::default());

    /// Interned `Py<PyString>` for attribute names. Without this each
    /// `getattr(name)` reallocates a PyString despite name being static.
    static PYSTRING_INTERN_CACHE: std::cell::RefCell<
        FastHashMap<String, Py<pyo3::types::PyString>>
    > = std::cell::RefCell::new(FastHashMap::default());
}

/// Interned `Py<PyString>` for `name`. First sighting allocates and
/// interns; subsequent are an Arc bump.
#[inline]
fn interned_pystring(py: Python<'_>, name: &str) -> Py<pyo3::types::PyString> {
    PYSTRING_INTERN_CACHE.with(|cache| {
        if let Some(p) = cache.borrow().get(name) {
            return p.clone_ref(py);
        }
        let py_str = pyo3::types::PyString::intern(py, name).unbind();
        cache
            .borrow_mut()
            .insert(name.to_owned(), py_str.clone_ref(py));
        py_str
    })
}

/// Cached per-type lookup-protocol bits.
#[derive(Copy, Clone)]
struct TypeBehavior {
    supports_getitem: bool,
    is_callable: bool,
    /// SafeData status. `None` means not queried; populated lazily by
    /// `is_value_safe`.
    is_safe_data: Option<bool>,
}

/// Get or compute `obj`'s type behaviour. First sighting pays two FFI
/// attribute checks; later sightings are a hashmap probe.
#[inline]
fn type_behavior(obj: &Bound<'_, pyo3::PyAny>) -> TypeBehavior {
    let py_type = obj.get_type();
    let type_ptr = py_type.as_ptr() as usize;

    TYPE_BEHAVIOR_CACHE.with(|cache| {
        if let Some((_, cached)) = cache.borrow().get(&type_ptr) {
            return *cached;
        }
        let behavior = TypeBehavior {
            supports_getitem: py_type.hasattr("__getitem__").unwrap_or(false),
            is_callable: obj.is_callable(),
            is_safe_data: None,
        };
        cache
            .borrow_mut()
            .insert(type_ptr, (py_type.clone().unbind(), behavior));
        behavior
    })
}

#[inline]
fn type_supports_getitem(_py: Python<'_>, obj: &Bound<'_, pyo3::PyAny>) -> bool {
    type_behavior(obj).supports_getitem
}

#[inline]
fn type_is_callable(obj: &Bound<'_, pyo3::PyAny>) -> bool {
    type_behavior(obj).is_callable
}

/// Fast `Value::from` skipping the `__html__` FFI for plain
/// str/int/bool/float/None. Non-primitives fall back to `Value::from`.
#[inline]
pub fn value_from_pyany_fast(val: &Bound<'_, pyo3::PyAny>) -> Value {
    use pyo3::types::{PyBool, PyFloat, PyInt, PyString};
    if val.is_exact_instance_of::<PyString>() {
        if let Ok(s) = val.extract::<String>() {
            return Value::String(s);
        }
    } else if val.is_exact_instance_of::<PyBool>() {
        return Value::Bool(val.extract::<bool>().unwrap_or(false));
    } else if val.is_exact_instance_of::<PyInt>() {
        if let Ok(n) = val.extract::<i64>() {
            return Value::Int(n);
        }
    } else if val.is_exact_instance_of::<PyFloat>() {
        if let Ok(f) = val.extract::<f64>() {
            return Value::Float(f);
        }
    } else if val.is_none() {
        return Value::None;
    }
    Value::from(val)
}

/// Resolve a `FilterExpression` applying filters natively in Rust.
fn resolve_with_filters_rust(
    py: Python<'_>,
    fe: &FilterExpression,
    context: &Context,
) -> Result<Value, TemplateError> {
    resolve_with_filters_rust_cached(py, fe, context, None, None, None)
}

/// Call a Python filter callable. Honours `needs_autoescape` (passes
/// `autoescape=` kwarg) and `is_safe` (wraps safe input in SafeString).
fn call_python_filter(
    py: Python<'_>,
    py_func: &Py<pyo3::PyAny>,
    obj: &Value,
    args: &[Value],
    context: &Context,
) -> Result<Value, TemplateError> {
    let func = py_func.bind(py);
    let py_obj = obj.to_pyobject(py);
    let py_args: Vec<Py<pyo3::PyAny>> = args.iter().map(|v| v.to_pyobject(py)).collect();

    let is_safe = func
        .getattr(pyo3::intern!(py, "is_safe"))
        .ok()
        .and_then(|v| v.extract::<bool>().ok())
        .unwrap_or(false);
    let needs_autoescape = func
        .getattr(pyo3::intern!(py, "needs_autoescape"))
        .ok()
        .and_then(|v| v.extract::<bool>().ok())
        .unwrap_or(false);

    let was_safe = matches!(obj, Value::SafeString(_));

    let result = if needs_autoescape {
        let mut all_args: Vec<Bound<'_, pyo3::PyAny>> = Vec::with_capacity(1 + py_args.len());
        all_args.push(py_obj.into_bound(py));
        for a in py_args {
            all_args.push(a.into_bound(py));
        }
        let args_tuple = pyo3::types::PyTuple::new(py, &all_args)?;
        let kwargs = pyo3::types::PyDict::new(py);
        kwargs.set_item(pyo3::intern!(py, "autoescape"), context.autoescape)?;
        func.call(args_tuple, Some(&kwargs))?
    } else {
        let mut all_args: Vec<Bound<'_, pyo3::PyAny>> = Vec::with_capacity(1 + py_args.len());
        all_args.push(py_obj.into_bound(py));
        for a in py_args {
            all_args.push(a.into_bound(py));
        }
        let args_tuple = pyo3::types::PyTuple::new(py, &all_args)?;
        func.call1(args_tuple)?
    };

    // Preserve safety when both input was safe and filter is_safe.
    let value = Value::from(&result);
    if is_safe
        && was_safe
        && let Value::String(s) = value
    {
        return Ok(Value::SafeString(s.into()));
    }
    Ok(value)
}

/// Django's `SafeData` check, cached per type in `TYPE_BEHAVIOR_CACHE`.
/// First sighting pays the FFI; subsequent calls are a hashmap probe.
fn is_value_safe(value: &Value) -> bool {
    match value {
        Value::SafeString(_) => true,
        Value::PyObject(obj) => Python::attach(|py| {
            let bound = obj.bind(py);
            let py_type = bound.get_type();
            let type_ptr = py_type.as_ptr() as usize;
            TYPE_BEHAVIOR_CACHE.with(|cache| {
                if let Some((_, b)) = cache.borrow().get(&type_ptr)
                    && let Some(is_safe) = b.is_safe_data
                {
                    return is_safe;
                }
                let cls = match safedata_class(py) {
                    Some(c) => c,
                    None => return false,
                };
                let is_safe = bound
                    .str()
                    .and_then(|s| s.is_instance(cls.bind(py)))
                    .unwrap_or(false);
                let mut guard = cache.borrow_mut();
                let entry = guard.entry(type_ptr).or_insert_with(|| {
                    (
                        py_type.clone().unbind(),
                        TypeBehavior {
                            supports_getitem: false,
                            is_callable: false,
                            is_safe_data: None,
                        },
                    )
                });
                entry.1.is_safe_data = Some(is_safe);
                is_safe
            })
        }),
        _ => false,
    }
}

/// Cached `SafeData` class (via `python_cache::django`). `None` only
/// if Django itself fails to import.
fn safedata_class(py: Python<'_>) -> Option<&'static Py<pyo3::PyAny>> {
    crate::python_cache::django(py)
        .ok()
        .map(|dj| &dj.safe_data_cls)
}

/// Resolve a single filter argument (var lookup, translatable literal,
/// or plain constant) to a `Value`.
#[inline]
fn resolve_filter_arg(
    py: Python<'_>,
    arg: &crate::variable::FilterArg,
    context: &Context,
) -> Result<Value, TemplateError> {
    if arg.is_lookup {
        let var = arg.variable.as_ref().expect("lookup arg without variable");
        // Django's `FilterExpression.resolve` (base.py:803-809) calls
        // `arg.resolve(context)` which calls `Variable.resolve()`.
        // Variable._resolve_lookup raises VariableDoesNotExist when
        // the variable is missing. This propagates uncaught through
        // FilterExpression.resolve. Oxide must match this behaviour.
        //
        // Skip the check for numeric literals and string literals
        // (they don't need context lookup) and for the variable part
        // of dotted paths (resolve_lookup_arg_native handles those).
        if !var.is_literal() {
            let first_part = var.var.split('.').next().unwrap_or("");
            if context.get(first_part).is_none() {
                return Err(TemplateError::VariableDoesNotExist {
                    msg: "Failed lookup for key [%s]".into(),
                    params: vec![first_part.to_owned()],
                });
            }
        }
        Ok(resolve_lookup_arg_native(py, context, &var.var))
    } else if let Some(var) = &arg.variable {
        // Translatable constant: _("...").
        let mut val = match var.as_string_literal() {
            Some(s) => Value::SafeString(s.to_owned().into()),
            None => Value::String(var.var.clone()),
        };
        if var.translate {
            val = apply_translation_rust(py, &val, var.message_context.as_deref())?;
        }
        Ok(val)
    } else {
        // Constant: cached at parse time; clone is an Arc bump.
        Ok(arg.cached_constant().cloned().unwrap_or(Value::None))
    }
}

// Same rationale as `resolve_base_variable` re: not `inline(always)`.
#[inline]
fn resolve_with_filters_rust_cached(
    py: Python<'_>,
    fe: &FilterExpression,
    context: &Context,
    cached: Option<&[Option<&'static crate::filters::NativeFilter>]>,
    filter_ids: Option<&[crate::filters::FilterId]>,
    filter_funcs: Option<&[Py<PyAny>]>,
) -> Result<Value, TemplateError> {
    resolve_with_filters_inner(py, fe, context, cached, filter_ids, filter_funcs, false)
}

/// Canonical filter-resolution implementation.
///
/// `cached`: pre-resolved native filter pointers (skip HashMap probe).
/// `filter_funcs`: Python callables for non-native filters loaded via
/// `{% load %}`.
/// `ignore_failures`: missing base var becomes `Value::None` before
/// filters run (Django's `resolve(ignore_failures=True)`).
#[inline]
fn resolve_with_filters_inner(
    py: Python<'_>,
    fe: &FilterExpression,
    context: &Context,
    cached: Option<&[Option<&'static crate::filters::NativeFilter>]>,
    filter_ids: Option<&[crate::filters::FilterId]>,
    filter_funcs: Option<&[Py<PyAny>]>,
    ignore_failures: bool,
) -> Result<Value, TemplateError> {
    let _g = crate::prof::Guard::new("resolve_with_filters_cached");
    let registry = if cached.is_none() {
        Some(get_default_filters())
    } else {
        None
    };

    let mut obj = {
        let _g2 = crate::prof::Guard::new("filters: base_resolve");
        resolve_base_variable(py, fe, context)?
    };

    if let crate::variable::FilterExpressionVar::Var(variable) = &fe.var
        && variable.translate
    {
        obj = apply_translation_rust(py, &obj, variable.message_context.as_deref())?;
    }

    // Django's FilterExpression.resolve (base.py:792-798): when the
    // base variable is missing and string_if_invalid is non-empty,
    // return string_if_invalid immediately WITHOUT running filters.
    if !ignore_failures
        && fe.is_var
        && !context.string_if_invalid.is_empty()
        && let crate::variable::FilterExpressionVar::Var(variable) = &fe.var
    {
        // Detect a variable miss: the resolved value equals what
        // format_invalid_message would produce.
        let expected = format_invalid_message(&context.string_if_invalid, &variable.var);
        if let Value::String(ref s) = obj
            && *s == expected
        {
            return Ok(obj);
        }
    }

    // Convert empty-string miss to None so `default` / `default_if_none`
    // see the right input.
    if ignore_failures
        && fe.is_var
        && let Value::String(ref s) = obj
        && s.is_empty()
        && context.string_if_invalid.is_empty()
    {
        obj = Value::None;
    }

    for (idx, parsed_filter) in fe.filters.iter().enumerate() {
        let n_args = parsed_filter.args.len();
        let mut stack_args: [Value; 4] = [Value::None, Value::None, Value::None, Value::None];
        let mut heap_args: Vec<Value> = Vec::new();
        let arg_vals: &[Value] = if n_args <= 4 {
            for (i, arg) in parsed_filter.args.iter().enumerate() {
                stack_args[i] = resolve_filter_arg(py, arg, context)?;
            }
            &stack_args[..n_args]
        } else {
            heap_args.reserve_exact(n_args);
            for arg in &parsed_filter.args {
                heap_args.push(resolve_filter_arg(py, arg, context)?);
            }
            &heap_args[..]
        };

        // Native lookup; missing entries fall through to filter_funcs.
        let native: Option<&'static crate::filters::NativeFilter> = cached
            .and_then(|c| c.get(idx).copied().flatten())
            .or_else(|| registry.and_then(|r| r.get(&parsed_filter.name)));

        let result = if let Some(native) = native {
            let _g3 = crate::prof::Guard::new("filters: native_dispatch");
            let autoescape = if native.needs_autoescape {
                context.autoescape
            } else {
                false
            };
            let was_safe = if native.is_safe {
                is_value_safe(&obj)
            } else {
                false
            };
            // Per-filter timing label for prof::Guard.
            let filter_name_static: &'static str = match parsed_filter.name.as_str() {
                "default" => "filter:default",
                "date" => "filter:date",
                "title" => "filter:title",
                "upper" => "filter:upper",
                "lower" => "filter:lower",
                "length" => "filter:length",
                "default_if_none" => "filter:default_if_none",
                "safe" => "filter:safe",
                "escape" => "filter:escape",
                _ => "filter:other",
            };
            let _g4 = crate::prof::Guard::new(filter_name_static);
            let result = match filter_ids.and_then(|ids| ids.get(idx).copied()) {
                Some(crate::filters::FilterId::External) | None => {
                    (native.func)(&obj, arg_vals, autoescape)
                }
                Some(id) => id.dispatch(&obj, arg_vals, autoescape, Some(native.func)),
            };
            if native.is_safe && was_safe {
                match result {
                    Value::String(s) => Value::SafeString(s.into()),
                    other => other,
                }
            } else {
                result
            }
        } else {
            // Python filter (custom user filter from {% load %}).
            // Source priority: explicit `filter_funcs` (VariableNode
            // hot path) then `fe.filter_funcs` (tag-arg fallback for
            // {% with %}, {% if %} conditions, {% for ... in %}, etc.).
            let py_func = filter_funcs
                .and_then(|ffs| ffs.get(idx))
                .or_else(|| fe.filter_funcs.get(idx))
                .ok_or_else(|| {
                    TemplateError::TemplateSyntaxError(format!(
                        "Invalid filter: '{}'",
                        parsed_filter.name,
                    ))
                })?;
            if py_func.bind(py).is_none() {
                return Err(TemplateError::TemplateSyntaxError(format!(
                    "Invalid filter: '{}'",
                    parsed_filter.name,
                )));
            }
            call_python_filter(py, py_func, &obj, arg_vals, context)?
        };

        obj = result;
    }

    Ok(obj)
}

/// `Value` to template-output string, honouring autoescape. Mirrors
/// `django.template.base.render_value_in_context`. PyObject values are
/// routed through Django for `localize` / `template_localtime` /
/// `conditional_escape` correctness.
pub fn render_value_in_context(value: &Value, context: &Context) -> String {
    render_value_in_context_checked(value, context).unwrap_or_default()
}

pub fn render_value_in_context_checked(
    value: &Value,
    context: &Context,
) -> Result<String, pyo3::PyErr> {
    let _g = crate::prof::Guard::new("render_value_in_context");
    if let Value::PyObject(obj) = value {
        return Python::attach(|py| {
            let py_obj = obj.bind(py);
            let dj = match crate::python_cache::django(py) {
                Ok(d) => d,
                Err(_) => return fallback_render_pyobject_result(py_obj, context.autoescape),
            };
            let py_ctx = match dj.context_cls.bind(py).call0() {
                Ok(c) => c,
                Err(_) => return fallback_render_pyobject_result(py_obj, context.autoescape),
            };
            let _ = py_ctx.setattr("autoescape", context.autoescape);
            let _ = py_ctx.setattr("use_l10n", context.use_l10n.unwrap_or(false));
            let _ = py_ctx.setattr("use_tz", context.use_tz.unwrap_or(false));
            dj.render_value_in_context
                .bind(py)
                .call1((py_obj, &py_ctx))?
                .str()
                .map(|s| s.to_string_lossy().into_owned())
        });
    }

    Ok(if context.autoescape {
        match value {
            Value::SafeString(s) => s.to_string(),
            Value::PyObject(_) => unreachable!(),
            other => html_escape(&other.to_string()),
        }
    } else {
        value.to_string()
    })
}

/// Fallback when Django isn't importable: `str()` + optional escape.
/// Propagates `__str__` exceptions instead of swallowing them.
fn fallback_render_pyobject_result(
    py_obj: &pyo3::Bound<'_, pyo3::PyAny>,
    autoescape: bool,
) -> Result<String, pyo3::PyErr> {
    let s = py_obj.str()?.to_string_lossy().into_owned();
    let is_safe = py_obj.getattr("__html__").is_ok();
    if autoescape && !is_safe {
        Ok(html_escape(&s))
    } else {
        Ok(s)
    }
}

/// Append-to-buffer variant of [`render_value_in_context`]. Hot path:
/// String/SafeString/Int/Bool/None go directly to `out`. PyObject and
/// Float/List/Dict fall back to the alloc path.
pub fn render_value_in_context_into(value: &Value, context: &Context, out: &mut String) {
    use std::fmt::Write;

    match value {
        Value::String(s) => {
            if context.autoescape {
                crate::utils::html_escape_into(s, out);
            } else {
                out.push_str(s);
            }
        }
        // SafeString opts out of escaping. Matches conditional_escape.
        Value::SafeString(s) => {
            out.push_str(s);
        }
        Value::Int(n) => {
            let _ = write!(out, "{n}");
        }
        Value::Bool(true) => out.push_str("True"),
        Value::Bool(false) => out.push_str("False"),
        Value::None => out.push_str("None"),
        Value::PyObject(obj) => {
            render_pyobject_into(obj, context, out);
        }
        other => {
            let rendered = render_value_in_context(other, context);
            out.push_str(&rendered);
        }
    }
}

/// Stringify and escape a PyObject into `out`. Fast path: exact
/// str/int/bool/None. Slow path: Django's `render_value_in_context`
/// for datetimes/decimals/SafeString subclasses/promises/etc.
fn render_pyobject_into(obj: &Py<pyo3::PyAny>, context: &Context, out: &mut String) {
    use pyo3::types::{PyBool, PyInt, PyString};
    use std::fmt::Write;

    Python::attach(|py| {
        let bound = obj.bind(py);

        // Exact PyString only: SafeString (a str subclass) must NOT be
        // html-escaped and falls through to the `__html__` slow path.
        if bound.is_exact_instance_of::<PyString>()
            && let Ok(s) = bound.extract::<String>()
        {
            if context.autoescape {
                crate::utils::html_escape_into(&s, out);
            } else {
                out.push_str(&s);
            }
            return;
        }

        // Bool first: PyBool is a PyInt subclass.
        if bound.is_exact_instance_of::<PyBool>() {
            out.push_str(if bound.extract::<bool>().unwrap_or(false) {
                "True"
            } else {
                "False"
            });
            return;
        }

        // BigInts fall through to the slow path.
        if bound.is_exact_instance_of::<PyInt>()
            && let Ok(n) = bound.extract::<i64>()
        {
            let _ = write!(out, "{n}");
            return;
        }

        if bound.is_none() {
            out.push_str("None");
            return;
        }

        render_pyobject_via_django(bound, context, out);
    });
}

fn render_pyobject_via_django(
    bound: &pyo3::Bound<'_, pyo3::PyAny>,
    context: &Context,
    out: &mut String,
) {
    let py = bound.py();
    let rendered = render_value_in_context(&Value::PyObject(bound.clone().unbind()), context);
    let _ = py;
    out.push_str(&rendered);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{Context, ContextDict, Value};
    use crate::variable::{FilterExpression, ParsedFilter};

    fn dict_from(pairs: &[(&str, Value)]) -> ContextDict {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn test_origin_display() {
        let o = Origin::new("templates/base.html");
        assert_eq!(o.to_string(), "templates/base.html");
    }

    #[test]
    fn test_origin_builder() {
        let o = Origin::new("base.html")
            .with_template_name("base.html")
            .with_loader("filesystem");
        assert_eq!(o.template_name.as_deref(), Some("base.html"));
        assert_eq!(o.loader.as_deref(), Some("filesystem"));
    }

    #[test]
    fn test_text_node_render() {
        Python::attach(|py| {
            let node = TextNode::new("Hello, world!");
            let mut ctx = Context::new(None);
            assert_eq!(node.render(py, &mut ctx).unwrap(), "Hello, world!");
        });
    }

    #[test]
    fn test_text_node_render_annotated_skips_errors() {
        Python::attach(|py| {
            let node = TextNode::new("<script>alert('xss')</script>");
            let mut ctx = Context::new(None);
            let result = node.render_annotated(py, &mut ctx).unwrap();
            assert_eq!(result, "<script>alert('xss')</script>");
        });
    }

    #[test]
    fn test_text_node_child_nodelists_empty() {
        let node = TextNode::new("text");
        assert!(node.child_nodelists().is_empty());
    }

    #[test]
    fn test_text_node_set_token_and_origin() {
        let mut node = TextNode::new("text");
        assert!(node.token().is_none());
        assert!(node.origin().is_none());

        node.set_token(Token::new(crate::lexer::TokenType::Text, "text", None, 1));
        node.set_origin(Origin::new("test.html"));

        assert!(node.token().is_some());
        assert!(node.origin().is_some());
    }

    #[test]
    fn test_nodelist_render_joins() {
        Python::attach(|py| {
            let mut nl = NodeList::new();
            nl.push(Box::new(TextNode::new("Hello, ")));
            nl.push(Box::new(TextNode::new("world!")));

            let mut ctx = Context::new(None);
            let result = nl.render(py, &mut ctx).unwrap();
            assert_eq!(result.as_str(), "Hello, world!");
        });
    }

    #[test]
    fn test_nodelist_empty_render() {
        Python::attach(|py| {
            let nl = NodeList::new();
            let mut ctx = Context::new(None);
            let result = nl.render(py, &mut ctx).unwrap();
            assert_eq!(result.as_str(), "");
        });
    }

    #[test]
    fn test_nodelist_contains_nontext() {
        let mut nl = NodeList::new();
        assert!(!nl.contains_nontext);

        nl.push(Box::new(TextNode::new("text")));
        assert!(!nl.contains_nontext);

        // VariableNode is not a TextNode, so contains_nontext should flip.
        let fe = FilterExpression::parse("var", |_| {
            Ok(ParsedFilter {
                name: String::new(),
                args: vec![],
            })
        })
        .unwrap();
        nl.push(Box::new(VariableNode::new(fe)));
        assert!(nl.contains_nontext);
    }

    #[test]
    fn test_nodelist_len_and_is_empty() {
        let mut nl = NodeList::new();
        assert!(nl.is_empty());
        assert_eq!(nl.len(), 0);

        nl.push(Box::new(TextNode::new("a")));
        assert!(!nl.is_empty());
        assert_eq!(nl.len(), 1);
    }

    #[test]
    fn test_render_value_autoescape_on() {
        let ctx = Context::new(None); // autoescape=true by default
        assert_eq!(
            render_value_in_context(&Value::String("<b>bold</b>".into()), &ctx),
            "&lt;b&gt;bold&lt;/b&gt;"
        );
    }

    #[test]
    fn test_render_value_safe_string_not_escaped() {
        let ctx = Context::new(None);
        assert_eq!(
            render_value_in_context(&Value::SafeString("<b>bold</b>".into()), &ctx),
            "<b>bold</b>"
        );
    }

    #[test]
    fn test_render_value_autoescape_off() {
        let mut ctx = Context::new(None);
        ctx.autoescape = false;
        assert_eq!(
            render_value_in_context(&Value::String("<b>bold</b>".into()), &ctx),
            "<b>bold</b>"
        );
    }

    #[test]
    fn test_render_value_int() {
        let ctx = Context::new(None);
        assert_eq!(render_value_in_context(&Value::Int(42), &ctx), "42");
    }

    #[test]
    fn test_render_value_none() {
        let ctx = Context::new(None);
        assert_eq!(render_value_in_context(&Value::None, &ctx), "None");
    }

    #[test]
    fn test_render_value_bool() {
        let ctx = Context::new(None);
        assert_eq!(render_value_in_context(&Value::Bool(true), &ctx), "True");
    }

    #[test]
    fn test_variable_node_render_simple() {
        Python::attach(|py| {
            let fe = FilterExpression::parse("name", |_| {
                Ok(ParsedFilter {
                    name: String::new(),
                    args: vec![],
                })
            })
            .unwrap();
            let node = VariableNode::new(fe);
            let mut ctx = Context::new(Some(dict_from(&[("name", Value::String("Alice".into()))])));
            let result = node.render(py, &mut ctx).unwrap();
            assert_eq!(result, "Alice");
        });
    }

    #[test]
    fn test_variable_node_render_escapes_html() {
        Python::attach(|py| {
            let fe = FilterExpression::parse("content", |_| {
                Ok(ParsedFilter {
                    name: String::new(),
                    args: vec![],
                })
            })
            .unwrap();
            let node = VariableNode::new(fe);
            let mut ctx = Context::new(Some(dict_from(&[(
                "content",
                Value::String("<script>xss</script>".into()),
            )])));
            let result = node.render(py, &mut ctx).unwrap();
            assert_eq!(result, "&lt;script&gt;xss&lt;/script&gt;");
        });
    }

    #[test]
    fn test_variable_node_render_missing_variable() {
        Python::attach(|py| {
            let fe = FilterExpression::parse("missing", |_| {
                Ok(ParsedFilter {
                    name: String::new(),
                    args: vec![],
                })
            })
            .unwrap();
            let node = VariableNode::new(fe);
            let mut ctx = Context::new(None);
            let result = node.render(py, &mut ctx).unwrap();
            assert_eq!(result, "");
        });
    }

    #[test]
    fn test_variable_node_child_nodelists_empty() {
        let fe = FilterExpression::parse("x", |_| {
            Ok(ParsedFilter {
                name: String::new(),
                args: vec![],
            })
        })
        .unwrap();
        let node = VariableNode::new(fe);
        assert!(node.child_nodelists().is_empty());
    }
}
