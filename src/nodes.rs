//! Node rendering: port of Django's `django.template.base.Node`,
//! `NodeList`, `TextNode`, `VariableNode`, and `render_value_in_context`.

use std::sync::Arc;

use pyo3::prelude::*;

use crate::context::{Context, Value};
use crate::errors::TemplateError;
use crate::lexer::Token;
use crate::utils::SafeString;
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
    /// for direct dispatch; other nodes stay boxed. Use `push_variable`
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

    /// In-place equivalent of `render`. Container nodes stream child
    /// output into the surrounding buffer without intermediate allocations.
    #[inline]
    pub fn render_into(
        &self,
        py: Python<'_>,
        context: &mut Context,
        out: &mut String,
    ) -> Result<(), TemplateError> {
        let debug = context.debug;
        for (index, entry) in self.nodes.iter().enumerate() {
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
                        let e = e.at_node_index(index);
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

#[derive(Debug, Clone)]
pub struct VariableNode {
    pub filter_expression: FilterExpression,
    pub token_field: Option<Token>,
    pub origin_field: Option<Origin>,
}

impl VariableNode {
    pub fn new(filter_expression: FilterExpression) -> Self {
        Self {
            filter_expression,
            token_field: None,
            origin_field: None,
        }
    }
}

impl Node for VariableNode {
    impl_node_metadata!();

    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        let mut out = String::new();
        self.render_annotated_into(py, context, &mut out)?;
        Ok(out)
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
            resolve_with_filters_inner(py, &self.filter_expression, context, false)?
        };
        render_value_in_context_into(&output, context, out)
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

pub fn resolve_expression_ignore_failures(
    py: Python<'_>,
    fe: &FilterExpression,
    context: &Context,
) -> Result<Value, TemplateError> {
    if fe.filters.is_empty() {
        let Some(mut val) = resolve_base_variable_opt(py, fe, context)? else {
            return Ok(Value::None);
        };
        if let crate::variable::FilterExpressionVar::Var(variable) = &fe.var
            && variable.translate
        {
            val = apply_translation_rust(py, &val, variable.message_context.as_deref())?;
        }
        Ok(val)
    } else {
        resolve_with_filters_ignore_failures(py, fe, context)
    }
}

/// `ignore_failures=True` variant of `resolve_with_filters_rust`.
#[inline]
fn resolve_with_filters_ignore_failures(
    py: Python<'_>,
    fe: &FilterExpression,
    context: &Context,
) -> Result<Value, TemplateError> {
    resolve_with_filters_inner(py, fe, context, true)
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
    let dj = crate::python_cache::django(py)?;

    // Replace % with %% to avoid accidental formatting (matches
    // Variable._resolve_lookup).
    let value_str = value.to_string().replace('%', "%%");

    let msgid = if matches!(value, Value::SafeString(_)) {
        dj.mark_safe.bind(py).call1((value_str.as_str(),))?
    } else {
        pyo3::types::PyString::new(py, &value_str).into_any()
    };

    let result = match message_context {
        Some(msg_ctx) => dj.pgettext_lazy.bind(py).call1((msg_ctx, &msgid))?,
        None => dj.gettext_lazy.bind(py).call1((&msgid,))?,
    };

    Ok(Value::from(&result))
}

#[inline]
fn resolve_base_variable(
    py: Python<'_>,
    fe: &FilterExpression,
    context: &Context,
) -> Result<Value, TemplateError> {
    match resolve_base_variable_opt(py, fe, context)? {
        Some(value) => Ok(value),
        None => Ok(Value::String(missing_variable_message(fe, context))),
    }
}

enum Lookup {
    Found(Value),
    Missing {
        index: usize,
        container: Option<Py<PyAny>>,
    },
}

impl Lookup {
    #[inline]
    fn found(self) -> Option<Value> {
        match self {
            Lookup::Found(value) => Some(value),
            Lookup::Missing { .. } => None,
        }
    }

    #[inline]
    fn offset(self, by: usize) -> Self {
        match self {
            Lookup::Missing { index, container } => Lookup::Missing {
                index: index + by,
                container,
            },
            found => found,
        }
    }
}

#[inline]
fn resolve_variable_opt(
    py: Python<'_>,
    variable: &crate::variable::Variable,
    context: &Context,
) -> Result<Option<Value>, TemplateError> {
    resolve_variable_lookup(py, variable, context).map(Lookup::found)
}

fn resolve_variable_lookup(
    py: Python<'_>,
    variable: &crate::variable::Variable,
    context: &Context,
) -> Result<Lookup, TemplateError> {
    if !variable.is_lookup() {
        if let Some(s) = variable.as_string_literal() {
            return Ok(Lookup::Found(Value::SafeString(s.to_owned().into())));
        }
        if let Some(n) = variable.as_int_literal() {
            return Ok(Lookup::Found(Value::Int(n)));
        }
        if let Some(f) = variable.as_float_literal() {
            return Ok(Lookup::Found(Value::Float(f)));
        }
        if let Some(text) = variable.as_big_int_literal() {
            let number = py.get_type::<pyo3::types::PyInt>().call1((text,))?;
            return Ok(Lookup::Found(Value::PyObject(number.unbind())));
        }
    }

    let parts: &[String] = variable
        .lookups()
        .expect("non-lookup variable handled by literal arms above");

    if let Some(slot) = variable.batch_slot()
        && let Some(cache) = context.loop_batch_cache.as_ref()
        && cache.loopvar == parts[0]
    {
        let tuple = cache.current_tuple.bind(py);
        if let Ok(val) = tuple.get_item(slot as usize) {
            return Ok(Lookup::Found(value_from_pyany_fast(&val)));
        }
    }

    let Some(head) = context.get(&parts[0]) else {
        return Ok(Lookup::Missing {
            index: 0,
            container: None,
        });
    };

    if let Value::PyObject(obj) = head {
        if parts.len() > 1 {
            return resolve_pyobject_lookups_opt(
                py,
                obj.bind(py),
                &parts[1..],
                &context.string_if_invalid,
            )
            .map(|lookup| lookup.offset(1));
        }
        return resolve_pyobject_callable(py, head, &context.string_if_invalid).map(Lookup::Found);
    }

    let mut cur: &Value = head;
    for (i, part) in parts[1..].iter().enumerate() {
        let native_step = match cur {
            Value::Dict(map) => map.get(part.as_str()),
            Value::List(items) => part.parse::<usize>().ok().and_then(|idx| items.get(idx)),
            _ => None,
        };
        match native_step {
            Some(v) => cur = v,
            None => {
                let obj = cur.to_pyobject(py);
                return resolve_pyobject_lookups_opt(
                    py,
                    obj.bind(py),
                    &parts[1 + i..],
                    &context.string_if_invalid,
                )
                .map(|lookup| lookup.offset(1 + i));
            }
        }
    }
    if matches!(cur, Value::PyObject(_)) {
        return resolve_pyobject_callable(py, cur, &context.string_if_invalid).map(Lookup::Found);
    }
    Ok(Lookup::Found(cur.clone()))
}

fn missing_variable_message(fe: &FilterExpression, context: &Context) -> String {
    match &fe.var {
        crate::variable::FilterExpressionVar::Var(variable) => {
            format_invalid_message(&context.string_if_invalid, &variable.var)
        }
        crate::variable::FilterExpressionVar::Constant(_) => context.string_if_invalid.clone(),
    }
}

#[inline]
fn resolve_base_variable_opt(
    py: Python<'_>,
    fe: &FilterExpression,
    context: &Context,
) -> Result<Option<Value>, TemplateError> {
    let _g = crate::prof::Guard::new("resolve_base_variable");
    use crate::variable::FilterExpressionVar;

    match &fe.var {
        FilterExpressionVar::Var(variable) => resolve_variable_opt(py, variable, context),
        FilterExpressionVar::Constant(opt) => match opt {
            Some(s) => Ok(Some(Value::SafeString(s.clone().into()))),
            None => Ok(Some(Value::None)),
        },
    }
}

#[inline]
fn resolve_pyobject_lookups_opt(
    py: Python<'_>,
    start: &Bound<'_, pyo3::PyAny>,
    parts: &[String],
    string_if_invalid: &str,
) -> Result<Lookup, TemplateError> {
    let resolved = resolve_python_lookup_chain(py, start, parts, string_if_invalid);
    crate::filters::invalidate_locale_snapshot();
    resolved
}

fn resolve_python_lookup_chain<'py>(
    py: Python<'py>,
    start: &Bound<'py, pyo3::PyAny>,
    parts: &[String],
    string_if_invalid: &str,
) -> Result<Lookup, TemplateError> {
    let _g = crate::prof::Guard::new("resolve_pyobject_lookups");
    use pyo3::types::{PyDict, PyList, PyTuple};

    let mut current = start.clone();
    let mut missing_at = 0;

    let call_if_callable = |current: &mut Bound<'py, pyo3::PyAny>| -> Result<(), Option<PyErr>> {
        if is_primitive_or_collection(current) || !type_is_callable(current) {
            return Ok(());
        }
        maybe_call_template_callable(py, current, string_if_invalid)
            .map_err(|error| Some(PyErr::from(error)))
    };

    let result: Result<Bound<'_, pyo3::PyAny>, Option<pyo3::PyErr>> = (|| {
        call_if_callable(&mut current)?;
        for (index, bit) in parts.iter().enumerate() {
            missing_at = index;
            if current.is_exact_instance_of::<PyDict>() {
                let d = current.cast::<PyDict>().expect("checked above");
                if let Some(val) = d.get_item(bit).map_err(Some)? {
                    current = val;
                    call_if_callable(&mut current)?;
                    continue;
                }
            } else if let Ok(idx) = bit.parse::<isize>() {
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
                            let lookup_error = attr_err
                                .is_instance_of::<pyo3::exceptions::PyAttributeError>(py)
                                || attr_err.is_instance_of::<pyo3::exceptions::PyTypeError>(py);
                            if !lookup_error {
                                return Err(Some(attr_err));
                            }
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
                                return Err(Some(attr_err));
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

            call_if_callable(&mut current)?;
        }

        Ok(current.clone())
    })();

    match result {
        Ok(val) => Ok(Lookup::Found(value_from_pyany_fast(&val))),
        Err(None) => Ok(Lookup::Missing {
            index: missing_at,
            container: Some(current.unbind()),
        }),
        Err(Some(e)) => {
            let silent = e
                .value(py)
                .getattr("silent_variable_failure")
                .ok()
                .and_then(|v| v.is_truthy().ok())
                .unwrap_or(false);

            if silent {
                Ok(Lookup::Found(Value::String(string_if_invalid.to_owned())))
            } else {
                Err(TemplateError::PythonError(e))
            }
        }
    }
}

fn resolve_pyobject_callable(
    py: Python<'_>,
    value: &Value,
    string_if_invalid: &str,
) -> Result<Value, TemplateError> {
    if let Value::PyObject(obj) = value {
        let mut current = obj.bind(py).clone();
        if current.is_callable() {
            let called = maybe_call_template_callable(py, &mut current, string_if_invalid);
            crate::filters::invalidate_locale_snapshot();
            called?;
            return Ok(Value::from(&current));
        }
    }
    Ok(value.clone())
}

/// Auto-call callable lookup result. Shared between the dict fast path
/// and the slow path of `resolve_pyobject_lookups`.
pub(crate) fn maybe_call_template_callable<'py>(
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
            if call_err.is_instance_of::<pyo3::exceptions::PyTypeError>(py) {
                // Per Django: missing required args -> string_if_invalid; a
                // TypeError raised inside a zero-arg call -> re-raise. `bind()`
                // succeeding means it took no args, so the error came from within.
                let required_args = py
                    .import("inspect")
                    .and_then(|inspect| inspect.getattr("signature"))
                    .and_then(|sig_fn| sig_fn.call1((&*current,)))
                    .and_then(|sig| sig.call_method0("bind"))
                    .is_err();
                if required_args {
                    *current = string_if_invalid.into_pyobject(py).unwrap().into_any();
                    Ok(())
                } else {
                    Err(TemplateError::from(call_err))
                }
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

/// Cheap pre-filter for an auto-callable leaf: callable and not an exact
/// primitive/collection. `do_not_call_in_templates`/`alters_data` are checked
/// later by [`maybe_call_template_callable`].
#[inline]
pub(crate) fn is_template_leaf_callable(obj: &Bound<'_, pyo3::PyAny>) -> bool {
    !is_primitive_or_collection(obj) && type_is_callable(obj)
}

type FastHashMap<K, V> = std::collections::HashMap<K, V, foldhash::fast::FixedState>;

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
}

/// Get or compute `obj`'s type behaviour. First sighting pays two FFI
/// attribute checks; later sightings are a hashmap probe.
#[inline]
fn type_behavior(obj: &Bound<'_, pyo3::PyAny>) -> TypeBehavior {
    let type_ptr = obj.get_type_ptr() as usize;

    TYPE_BEHAVIOR_CACHE.with(|cache| {
        if let Some((_, cached)) = cache.borrow().get(&type_ptr) {
            return *cached;
        }
        let py_type = obj.get_type();
        let behavior = TypeBehavior {
            supports_getitem: py_type.hasattr("__getitem__").unwrap_or(false),
            is_callable: obj.is_callable(),
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
        if let Ok(f) = val.extract::<f64>()
            && !f.is_nan()
        {
            return Value::Float(f);
        }
    } else if val.is_none() {
        return Value::None;
    }
    Value::from(val)
}

fn resolve_with_filters_rust(
    py: Python<'_>,
    fe: &FilterExpression,
    context: &Context,
) -> Result<Value, TemplateError> {
    resolve_with_filters_inner(py, fe, context, false)
}

#[derive(Clone, Copy)]
struct PythonFilterFlags {
    expects_localtime: bool,
    needs_autoescape: bool,
    is_safe: bool,
}

fn python_filter_flags(py: Python<'_>, func: &Bound<'_, pyo3::PyAny>) -> PythonFilterFlags {
    type FlagCache =
        std::sync::Mutex<std::collections::HashMap<usize, (Py<pyo3::PyAny>, PythonFilterFlags)>>;
    static CACHE: std::sync::OnceLock<FlagCache> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let key = func.as_ptr() as usize;
    if let Some(flags) = cache
        .lock()
        .ok()
        .and_then(|guard| guard.get(&key).map(|(_, flags)| *flags))
    {
        return flags;
    }
    let flag = |name: &Bound<'_, pyo3::types::PyString>| -> bool {
        func.getattr(name)
            .ok()
            .and_then(|v| v.is_truthy().ok())
            .unwrap_or(false)
    };
    let flags = PythonFilterFlags {
        expects_localtime: flag(pyo3::intern!(py, "expects_localtime")),
        needs_autoescape: flag(pyo3::intern!(py, "needs_autoescape")),
        is_safe: flag(pyo3::intern!(py, "is_safe")),
    };
    if let Ok(mut guard) = cache.lock() {
        guard.insert(key, (func.clone().unbind(), flags));
    }
    flags
}

pub(crate) fn call_python_filter(
    py: Python<'_>,
    py_func: &Py<pyo3::PyAny>,
    obj: &Value,
    args: &[Value],
    context: &Context,
) -> Result<Value, TemplateError> {
    let func = py_func.bind(py);
    let flags = python_filter_flags(py, func);
    let is_datetime = match obj {
        Value::PyObject(value) => value.bind(py).is_instance_of::<pyo3::types::PyDateTime>(),
        _ => false,
    };
    let input = if flags.expects_localtime && is_datetime {
        crate::filters::template_localtime(py, obj, context.use_tz)?
    } else {
        obj.clone()
    };
    let mut call_args: Vec<Py<pyo3::PyAny>> = Vec::with_capacity(1 + args.len());
    call_args.push(input.to_pyobject(py));
    call_args.extend(args.iter().map(|v| v.to_pyobject(py)));
    let call_args = pyo3::types::PyTuple::new(py, call_args)?;
    let result = if flags.needs_autoescape {
        let kwargs = pyo3::types::PyDict::new(py);
        kwargs.set_item(pyo3::intern!(py, "autoescape"), context.autoescape)?;
        func.call(call_args, Some(&kwargs))
    } else {
        func.call1(call_args)
    };
    crate::filters::invalidate_locale_snapshot();
    let value = Value::from(&result?);
    if flags.is_safe && crate::filters::is_safe_data(py, &input) {
        crate::filters::mark_safe_value(py, value)
    } else {
        Ok(value)
    }
}

/// Resolve a single filter argument (var lookup, translatable literal,
/// or plain constant) to a `Value`.
#[inline]
fn context_repr(py: Python<'_>, context: &Context) -> PyResult<String> {
    let layers = pyo3::types::PyList::empty(py);
    for layer in &context.base.dicts {
        let dict = pyo3::types::PyDict::new(py);
        for (key, value) in layer {
            dict.set_item(key, value.to_pyobject(py))?;
        }
        layers.append(dict)?;
    }
    Ok(layers.repr()?.to_string())
}

fn resolve_filter_arg(
    py: Python<'_>,
    arg: &crate::variable::FilterArg,
    context: &Context,
) -> Result<Value, TemplateError> {
    if arg.is_lookup {
        let var = arg.variable.as_ref().expect("lookup arg without variable");
        let mut value = match resolve_variable_lookup(py, var, context)? {
            Lookup::Found(value) => value,
            Lookup::Missing { index, container } => {
                let bit = var
                    .lookups()
                    .and_then(|parts| parts.get(index))
                    .cloned()
                    .unwrap_or_else(|| var.var.clone());
                let described = match container {
                    Some(container) => container.bind(py).repr()?.to_string(),
                    None => context_repr(py, context)?,
                };
                return Err(TemplateError::VariableDoesNotExist {
                    msg: "Failed lookup for key [%s] in %s".into(),
                    params: vec![bit, described],
                });
            }
        };
        if var.translate {
            value = apply_translation_rust(py, &value, var.message_context.as_deref())?;
        }
        Ok(value)
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

#[inline]
fn resolve_with_filters_inner(
    py: Python<'_>,
    fe: &FilterExpression,
    context: &Context,
    ignore_failures: bool,
) -> Result<Value, TemplateError> {
    let _g = crate::prof::Guard::new("resolve_with_filters_cached");

    let resolved = {
        let _g2 = crate::prof::Guard::new("filters: base_resolve");
        resolve_base_variable_opt(py, fe, context)?
    };

    let mut obj = match resolved {
        Some(mut value) => {
            if let crate::variable::FilterExpressionVar::Var(variable) = &fe.var
                && variable.translate
            {
                value = apply_translation_rust(py, &value, variable.message_context.as_deref())?;
            }
            value
        }
        None if ignore_failures => Value::None,
        None if !context.string_if_invalid.is_empty() => {
            return Ok(Value::String(missing_variable_message(fe, context)));
        }
        None => Value::String(String::new()),
    };

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

        let result = if let Some(native) = fe.natives.get(idx).copied().flatten() {
            let _g3 = crate::prof::Guard::new("filters: native_dispatch");
            native.call(py, &obj, arg_vals, context.autoescape, context.use_tz)?
        } else {
            let py_func = fe.filter_funcs.get(idx).ok_or_else(|| {
                TemplateError::TemplateSyntaxError(format!(
                    "Invalid filter: '{}'",
                    parsed_filter.name,
                ))
            })?;
            call_python_filter(py, py_func, &obj, arg_vals, context)?
        };

        obj = result;
    }

    Ok(obj)
}

pub fn render_value_in_context(value: &Value, context: &Context) -> Result<String, TemplateError> {
    let mut out = String::new();
    render_value_in_context_into(value, context, &mut out)?;
    Ok(out)
}

pub fn render_value_in_context_into(
    value: &Value,
    context: &Context,
    out: &mut String,
) -> Result<(), TemplateError> {
    use std::fmt::Write;

    match value {
        Value::String(s) => {
            if context.autoescape {
                crate::utils::html_escape_into(s, out);
            } else {
                out.push_str(s);
            }
        }
        Value::SafeString(s) => out.push_str(s),
        Value::Bool(true) => out.push_str("True"),
        Value::Bool(false) => out.push_str("False"),
        Value::None => out.push_str("None"),
        Value::Int(n) if context.renders_integers_natively() => {
            let _ = write!(out, "{n}");
        }
        Value::Float(f) if context.renders_integers_natively() => {
            crate::python_cache::attach(|py| render_float_into(py, *f, context, out))?;
        }
        Value::PyObject(obj) => {
            crate::python_cache::attach(|py| render_python_value_into(obj.bind(py), context, out))?;
        }
        other => {
            crate::python_cache::attach(|py| {
                let obj = other.to_pyobject(py);
                render_python_value_into(obj.bind(py), context, out)
            })?;
        }
    }
    Ok(())
}

fn render_float_into(
    py: Python<'_>,
    value: f64,
    context: &Context,
    out: &mut String,
) -> Result<(), TemplateError> {
    let repr = crate::context::python_float_repr(value);
    let text = if context.use_l10n == Some(false) {
        repr
    } else {
        match crate::filters::decimal_separator(py)? {
            Some(separator) if !repr.contains('e') => {
                if &*separator == "." {
                    repr
                } else {
                    repr.replacen('.', &separator, 1)
                }
            }
            _ => {
                let obj = Value::Float(value).to_pyobject(py);
                return render_localized_into(obj.bind(py), context, out);
            }
        }
    };
    if context.autoescape {
        crate::utils::html_escape_into(&text, out);
    } else {
        out.push_str(&text);
    }
    Ok(())
}

fn render_python_value_into(
    value: &Bound<'_, pyo3::PyAny>,
    context: &Context,
    out: &mut String,
) -> Result<(), TemplateError> {
    use pyo3::types::{PyBool, PyInt, PyString};
    use std::fmt::Write;

    let py = value.py();
    if value.is_exact_instance_of::<PyString>() {
        let s = value.cast::<PyString>().expect("exact str").to_cow()?;
        if context.autoescape {
            crate::utils::html_escape_into(&s, out);
        } else {
            out.push_str(&s);
        }
        return Ok(());
    }
    if value.is_exact_instance_of::<PyBool>() {
        out.push_str(if value.is_truthy()? { "True" } else { "False" });
        return Ok(());
    }
    if value.is_none() {
        out.push_str("None");
        return Ok(());
    }
    if value.is_exact_instance_of::<PyInt>()
        && context.renders_integers_natively()
        && let Ok(n) = value.extract::<i64>()
    {
        let _ = write!(out, "{n}");
        return Ok(());
    }
    if value.is_exact_instance_of::<pyo3::types::PyFloat>() && context.renders_integers_natively() {
        return render_float_into(py, value.extract::<f64>()?, context, out);
    }
    let rendered = render_localized_into(value, context, out);
    crate::filters::invalidate_locale_snapshot();
    rendered
}

fn render_localized_into(
    value: &Bound<'_, pyo3::PyAny>,
    context: &Context,
    out: &mut String,
) -> Result<(), TemplateError> {
    use pyo3::types::{PyDate, PyDateTime, PyFloat, PyInt, PyString, PyTime};

    let py = value.py();
    let dj = crate::python_cache::django(py)?;
    let localized_time = if value.is_instance_of::<PyDateTime>() {
        dj.template_localtime
            .bind(py)
            .call1((value, optional_bool(py, context.use_tz)))?
    } else {
        value.clone()
    };
    let localizable = localized_time.is_instance_of::<PyInt>()
        || localized_time.is_instance_of::<PyFloat>()
        || localized_time.is_instance_of::<PyDate>()
        || localized_time.is_instance_of::<PyTime>()
        || localized_time.is_instance(dj.decimal_cls.bind(py))?;
    let localized = if localizable && !localized_time.is_instance_of::<PyString>() {
        dj.localize
            .bind(py)
            .call1((localized_time, optional_bool(py, context.use_l10n)))?
    } else {
        localized_time
    };
    let text = if localized.is_instance_of::<PyString>() {
        localized
    } else {
        localized.str()?.into_any()
    };
    if !context.autoescape {
        out.push_str(&text.str()?.to_cow()?);
    } else if text.hasattr(pyo3::intern!(py, "__html__"))? {
        let html = text.call_method0(pyo3::intern!(py, "__html__"))?;
        out.push_str(&html.str()?.to_cow()?);
    } else {
        crate::utils::html_escape_into(&text.str()?.to_cow()?, out);
    }
    Ok(())
}

pub(crate) fn optional_bool(py: Python<'_>, value: Option<bool>) -> Bound<'_, pyo3::PyAny> {
    match value {
        Some(flag) => pyo3::types::PyBool::new(py, flag).to_owned().into_any(),
        None => py.None().into_bound(py),
    }
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
        crate::python_cache::attach(|py| {
            let node = TextNode::new("Hello, world!");
            let mut ctx = Context::new(None);
            assert_eq!(node.render(py, &mut ctx).unwrap(), "Hello, world!");
        });
    }

    #[test]
    fn test_text_node_render_annotated_skips_errors() {
        crate::python_cache::attach(|py| {
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
        crate::python_cache::attach(|py| {
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
        crate::python_cache::attach(|py| {
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
        let ctx = Context::new(None);
        assert_eq!(
            render_value_in_context(&Value::String("<b>bold</b>".into()), &ctx).unwrap(),
            "&lt;b&gt;bold&lt;/b&gt;"
        );
    }

    #[test]
    fn test_render_value_safe_string_not_escaped() {
        let ctx = Context::new(None);
        assert_eq!(
            render_value_in_context(&Value::SafeString("<b>bold</b>".into()), &ctx).unwrap(),
            "<b>bold</b>"
        );
    }

    #[test]
    fn test_render_value_autoescape_off() {
        let mut ctx = Context::new(None);
        ctx.autoescape = false;
        assert_eq!(
            render_value_in_context(&Value::String("<b>bold</b>".into()), &ctx).unwrap(),
            "<b>bold</b>"
        );
    }

    #[test]
    fn test_render_value_int() {
        let ctx = Context::new(None);
        assert_eq!(
            render_value_in_context(&Value::Int(42), &ctx).unwrap(),
            "42"
        );
    }

    #[test]
    fn test_render_value_none() {
        let ctx = Context::new(None);
        assert_eq!(render_value_in_context(&Value::None, &ctx).unwrap(), "None");
    }

    #[test]
    fn test_render_value_bool() {
        let ctx = Context::new(None);
        assert_eq!(
            render_value_in_context(&Value::Bool(true), &ctx).unwrap(),
            "True"
        );
    }

    #[test]
    fn test_python_float_repr() {
        let cases = [
            (3.0, "3.0"),
            (1234.5678, "1234.5678"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (1.5e16, "1.5e+16"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1e-7, "1e-07"),
            (-0.5, "-0.5"),
            (-0.0, "-0.0"),
            (0.1 + 0.2, "0.30000000000000004"),
            (f64::NAN, "nan"),
            (f64::NEG_INFINITY, "-inf"),
        ];
        for (value, expected) in cases {
            assert_eq!(crate::context::python_float_repr(value), expected);
        }
    }

    #[test]
    fn test_variable_node_render_simple() {
        crate::python_cache::attach(|py| {
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
        crate::python_cache::attach(|py| {
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
        crate::python_cache::attach(|py| {
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
