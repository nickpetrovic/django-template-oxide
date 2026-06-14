//! Django drop-in compatibility: Python proxies so third-party
//! `@register.tag`/`@register.filter` code and custom `Node` subclasses
//! interact with our engine as if it were Django's own. Proxies are
//! zero-cost when unused.

use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::PyString;
use std::sync::OnceLock;

use crate::context::Context;
use crate::errors::TemplateError;
use crate::lexer::{Token, TokenType};
use crate::nodes::{Node, NodeList, Origin};

/// Python proxy for `django.template.base.Token` (base.py:358-403).
/// Carries a copy of the Rust `Token` fields so the Rust side may
/// advance the stream without invalidating a `PyToken` handed to Python.
#[pyclass(
    name = "Token",
    module = "django_template_oxide._rust",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub struct PyToken {
    token_type_obj: Py<PyAny>,
    contents_obj: Py<PyAny>,
    lineno: usize,
    /// Django uses `(start, end)` when DebugLexer is on. We track only
    /// start; `(start, start)` is exposed when present, else `None`.
    position: Option<(usize, usize)>,
    kind: TokenType,
    contents_rust: String,
}

#[pymethods]
impl PyToken {
    /// Django `TokenType` enum member (identity-equal to the one
    /// imported from `django.template.base`).
    #[getter]
    fn token_type(&self, py: Python<'_>) -> Py<PyAny> {
        self.token_type_obj.clone_ref(py)
    }

    #[getter]
    fn contents(&self, py: Python<'_>) -> Py<PyAny> {
        self.contents_obj.clone_ref(py)
    }

    #[getter]
    fn lineno(&self) -> usize {
        self.lineno
    }

    /// `(start, end)`. Falls back to `(0, len(contents))` when our
    /// non-debug Lexer didn't record a position, so Django's
    /// `start, end = token.position` unpack still succeeds.
    #[getter]
    fn position(&self) -> (usize, usize) {
        match self.position {
            Some(pos) => pos,
            None => (0, self.contents_rust.len()),
        }
    }

    /// Mirrors `base.py:390-403`: preserves `_("...")` translation markers.
    fn split_contents(&self) -> Vec<String> {
        Token::new(self.kind, self.contents_rust.clone(), None, self.lineno).split_contents()
    }

    fn __repr__(&self, py: Python<'_>) -> String {
        let token_name = match self.kind {
            TokenType::Text => "Text",
            TokenType::Var => "Var",
            TokenType::Block => "Block",
            TokenType::Comment => "Comment",
        };
        let truncated: String = self
            .contents_rust
            .chars()
            .take(20)
            .collect::<String>()
            .replace('\n', "");
        let _ = py;
        format!("<{} token: \"{}...\">", token_name, truncated)
    }
}

impl PyToken {
    pub fn from_rust_token(py: Python<'_>, token: &Token) -> PyResult<Self> {
        let token_type_obj = cached_django_token_type(py, token.token_type)?.clone_ref(py);
        let contents_obj = pyo3::types::PyString::new(py, &token.contents)
            .into_any()
            .unbind();
        let position = token.position.map(|p| (p, p + token.source_len));
        Ok(Self {
            token_type_obj,
            contents_obj,
            lineno: token.lineno,
            position,
            kind: token.token_type,
            contents_rust: token.contents.clone(),
        })
    }
}

/// Map Rust `TokenType` to Django's `TokenType` enum member. Identity
/// equality with Python imports is required for drop-in compatibility.
fn cached_django_token_type<'py>(py: Python<'py>, kind: TokenType) -> PyResult<&'py Py<PyAny>> {
    /// Interned slot per variant; lives for the process.
    struct Cached {
        text: Py<PyAny>,
        var: Py<PyAny>,
        block: Py<PyAny>,
        comment: Py<PyAny>,
    }

    static CACHE: OnceLock<Cached> = OnceLock::new();

    let cached = match CACHE.get() {
        Some(c) => c,
        None => {
            let module = py.import("django.template.base")?;
            let cls = module.getattr("TokenType")?;
            let cached = Cached {
                text: cls.getattr("TEXT")?.unbind(),
                var: cls.getattr("VAR")?.unbind(),
                block: cls.getattr("BLOCK")?.unbind(),
                comment: cls.getattr("COMMENT")?.unbind(),
            };
            // First writer wins; race losers reference the same objects.
            let _ = CACHE.set(cached);
            CACHE.get().expect("CACHE was just set or had a value")
        }
    };

    Ok(match kind {
        TokenType::Text => &cached.text,
        TokenType::Var => &cached.var,
        TokenType::Block => &cached.block,
        TokenType::Comment => &cached.comment,
    })
}

/// Hand a `&mut Context` to a Python callback. Mutations propagate
/// back via `mem::swap`. If the callback stashes the PyContext, post-
/// return access sees an empty placeholder (safe, but best-effort).
pub fn render_with_borrowed_context<F, R>(
    py: Python<'_>,
    context: &mut Context,
    f: F,
) -> PyResult<R>
where
    F: FnOnce(Bound<'_, crate::py_bindings::PyContext>) -> PyResult<R>,
{
    let mut placeholder = Context::new(None);
    std::mem::swap(context, &mut placeholder);
    let py_ctx = Py::new(py, crate::py_bindings::PyContext { inner: placeholder })?;

    let result = f(py_ctx.bind(py).clone());

    // Swap mutated Context back; PyContext is left with the placeholder.
    {
        let bound = py_ctx.bind(py);
        let mut borrowed = bound.borrow_mut();
        std::mem::swap(context, &mut borrowed.inner);
    }

    result
}

/// A `Node` that delegates rendering to a Python instance returned by
/// a third-party tag compile function (`Library.tag()`).
#[derive(Debug)]
pub struct PyOpaqueNode {
    /// Must support `.render_annotated(context)` (Django default) or
    /// `.render(context)` fallback.
    pub py_node: Py<PyAny>,
    pub token_field: Option<Token>,
    pub origin_field: Option<Origin>,
}

impl PyOpaqueNode {
    pub fn new(py_node: Py<PyAny>) -> Self {
        Self {
            py_node,
            token_field: None,
            origin_field: None,
        }
    }
}

impl Clone for PyOpaqueNode {
    fn clone(&self) -> Self {
        Python::attach(|py| Self {
            py_node: self.py_node.clone_ref(py),
            token_field: self.token_field.clone(),
            origin_field: self.origin_field.clone(),
        })
    }
}

impl Node for PyOpaqueNode {
    crate::impl_node_metadata!();

    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        // Swap the Rust Context into a PyContext for the call so the
        // Python node's `context['x'] = 1` mutations remain visible.
        render_with_borrowed_context(py, context, |py_ctx_bound| {
            // Prefer `render_annotated`; fall back to `render`.
            let method_name = intern!(py, "render_annotated");
            let result = match self
                .py_node
                .bind(py)
                .call_method1(method_name, (&py_ctx_bound,))
            {
                Ok(v) => v,
                Err(e) => {
                    if e.is_instance_of::<pyo3::exceptions::PyAttributeError>(py) {
                        self.py_node
                            .bind(py)
                            .call_method1(intern!(py, "render"), (&py_ctx_bound,))?
                    } else {
                        return Err(e);
                    }
                }
            };
            let result_pystr = result.cast::<PyString>().map_err(|_| {
                pyo3::exceptions::PyTypeError::new_err(
                    "Python Node.render_annotated() returned a non-string value",
                )
            })?;
            Ok(result_pystr.to_str()?.to_owned())
        })
        .map_err(TemplateError::from)
    }

    fn child_nodelists(&self) -> &[&str] {
        // Python nodes own their own NodeLists; we can't introspect.
        &[]
    }

    #[inline]
    fn as_py_node(&self) -> Option<&Py<pyo3::PyAny>> {
        Some(&self.py_node)
    }
}

/// Python proxy for `django.template.base.NodeList`. Built by
/// PyParser's `parse()` and rendered later via `.render(context)`.
#[pyclass(name = "NodeList", module = "django_template_oxide._rust")]
pub struct PyNodeList {
    pub inner: NodeList,
}

#[pymethods]
impl PyNodeList {
    /// Render all child nodes and return a `SafeString`. Accepts our
    /// `PyContext` (zero-copy) or a Django `Context` (flattened to a
    /// fresh Rust Context inheriting `autoescape`/`use_l10n`/`use_tz`;
    /// mutations don't propagate back).
    fn render(&self, py: Python<'_>, context: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let rendered = if let Ok(py_ctx) = context.cast::<crate::py_bindings::PyContext>() {
            let mut borrowed = py_ctx.borrow_mut();
            self.inner
                .render(py, &mut borrowed.inner)
                .map_err(<PyErr as From<TemplateError>>::from)?
        } else {
            let flat = context.call_method0("flatten")?;
            let pydict = flat.cast::<pyo3::types::PyDict>().map_err(|_| {
                pyo3::exceptions::PyTypeError::new_err(
                    "PyNodeList.render: context.flatten() did not return a dict",
                )
            })?;
            let mut values = std::collections::HashMap::new();
            for (k, v) in pydict.iter() {
                values.insert(k.extract::<String>()?, crate::context::Value::from(&v));
            }
            let mut rust_ctx = Context::new(Some(values));
            if let Ok(ae) = context.getattr(intern!(py, "autoescape")) {
                rust_ctx.autoescape = ae.extract::<bool>().unwrap_or(true);
            }
            if let Ok(l10n) = context.getattr(intern!(py, "use_l10n")) {
                rust_ctx.use_l10n = l10n.extract::<Option<bool>>().ok().flatten();
            }
            if let Ok(tz) = context.getattr(intern!(py, "use_tz")) {
                rust_ctx.use_tz = tz.extract::<Option<bool>>().ok().flatten();
            }
            self.inner
                .render(py, &mut rust_ctx)
                .map_err(<PyErr as From<TemplateError>>::from)?
        };

        // mark_safe on a Python str to match Django's contract.
        let s = rendered.as_str();
        let mark_safe = py
            .import("django.utils.safestring")?
            .getattr(intern!(py, "mark_safe"))?;
        let py_str = pyo3::types::PyString::new(py, s);
        Ok(mark_safe.call1((py_str,))?.unbind())
    }

    fn __len__(&self) -> usize {
        self.inner.len()
    }

    fn __bool__(&self) -> bool {
        !self.inner.is_empty()
    }

    /// `nodelist[i]` -> Python object. Native nodes are wrapped in a
    /// `django.template.base.TextNode`/`Node` lookalike; `PyOpaqueNode`
    /// returns the underlying Python instance.
    fn __getitem__(&self, py: Python<'_>, index: isize) -> PyResult<Py<PyAny>> {
        let len = self.inner.len() as isize;
        let normalised = if index < 0 { len + index } else { index };
        if normalised < 0 || normalised >= len {
            return Err(pyo3::exceptions::PyIndexError::new_err(
                "NodeList index out of range",
            ));
        }
        self.node_at(py, normalised as usize)
    }

    fn __iter__(&self, py: Python<'_>) -> PyResult<Py<pyo3::types::PyIterator>> {
        let list = pyo3::types::PyList::empty(py);
        for i in 0..self.inner.len() {
            list.append(self.node_at(py, i)?)?;
        }
        Ok(list.try_iter()?.unbind())
    }

    /// Recursive `isinstance(node, nodetype)` filter. Mirrors
    /// `NodeList.get_nodes_by_type` (base.py:1093-1098).
    fn get_nodes_by_type(
        &self,
        py: Python<'_>,
        nodetype: &Bound<'_, PyAny>,
    ) -> PyResult<Py<pyo3::types::PyList>> {
        let result = pyo3::types::PyList::empty(py);
        for i in 0..self.inner.len() {
            let node_obj = self.node_at(py, i)?;
            if node_obj.bind(py).is_instance(nodetype)? {
                result.append(&node_obj)?;
            }
            // Recurse via the Python node's own get_nodes_by_type if exposed.
            if let Ok(get_nodes) = node_obj.bind(py).getattr(intern!(py, "get_nodes_by_type")) {
                if let Ok(children) = get_nodes.call1((nodetype,)) {
                    if let Ok(children_list) = children.cast::<pyo3::types::PyList>() {
                        for child in children_list.iter() {
                            if !child.is(&node_obj) {
                                result.append(child)?;
                            }
                        }
                    }
                }
            }
        }
        Ok(result.unbind())
    }

    /// Matches `base.py:611-612, 1088`.
    #[getter]
    fn contains_nontext(&self) -> bool {
        self.inner.contains_nontext
    }

    fn __repr__(&self) -> String {
        format!("<NodeList len={}>", self.inner.len())
    }
}

impl PyNodeList {
    /// Build a Python view of the i-th node:
    /// - `Text` -> `TextNode(arc)`
    /// - `Variable` -> `VariableNode(filter_expression)`
    /// - `Boxed(node)`: `node.as_py_node()` or a generic `Node()`
    fn node_at(&self, py: Python<'_>, i: usize) -> PyResult<Py<PyAny>> {
        let entry = self.inner.nodes.get(i).ok_or_else(|| {
            pyo3::exceptions::PyIndexError::new_err("NodeList index out of range")
        })?;
        let base_mod = py.import("django.template.base")?;
        match entry {
            crate::nodes::NodeEntry::Text(arc) => {
                let text_node_cls = base_mod.getattr(intern!(py, "TextNode"))?;
                Ok(text_node_cls.call1((arc.as_ref(),))?.unbind())
            }
            crate::nodes::NodeEntry::Variable(var_node) => {
                // Build a real `VariableNode` so isinstance checks
                // succeed. Pass a duck-typed `PyFilterExpression`
                // matching what PyParser already hands user code.
                let variable_node_cls = base_mod.getattr(intern!(py, "VariableNode"))?;
                let fe_wrapper = PyFilterExpression {
                    inner: var_node.filter_expression.clone(),
                };
                let fe_py = Py::new(py, fe_wrapper)?;
                Ok(variable_node_cls.call1((fe_py,))?.unbind())
            }
            crate::nodes::NodeEntry::Boxed(node) => {
                if let Some(py_node) = node.as_py_node() {
                    return Ok(py_node.clone_ref(py));
                }
                let node_cls = base_mod.getattr(intern!(py, "Node"))?;
                Ok(node_cls.call0()?.unbind())
            }
        }
    }
}

/// Python proxy for `django.template.base.Parser`. Holds a raw
/// pointer because the Rust Parser is stack-allocated in the outer
/// `parse()` loop.
///
/// SAFETY: constructed before the Python compile-function call,
/// dropped after. Stashing the PyParser across the call boundary is UB.
#[pyclass(name = "Parser", module = "django_template_oxide._rust", unsendable)]
pub struct PyParser {
    parser_ptr: *mut crate::parser::Parser,
}

#[pymethods]
impl PyParser {
    /// Parse until a `parse_until` block command. Terminator stays on
    /// the stream (matches Django's `Parser.parse`).
    #[pyo3(signature = (parse_until=None))]
    fn parse(
        &self,
        py: Python<'_>,
        parse_until: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<PyNodeList> {
        let until_strings: Vec<String> = match parse_until {
            None => Vec::new(),
            Some(obj) => obj.extract::<Vec<String>>().map_err(|_| {
                pyo3::exceptions::PyTypeError::new_err(
                    "parser.parse: parse_until must be a list/tuple of strings or None",
                )
            })?,
        };
        let until_refs: Vec<&str> = until_strings.iter().map(|s| s.as_str()).collect();

        let parser = self.parser();
        let nodelist = parser
            .parse(&until_refs)
            .map_err(<PyErr as From<TemplateError>>::from)?;
        let _ = py;
        Ok(PyNodeList { inner: nodelist })
    }

    fn next_token(&self, py: Python<'_>) -> PyResult<PyToken> {
        let parser = self.parser();
        if !parser.has_tokens() {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "parser.next_token: no more tokens in the stream",
            ));
        }
        let token = parser.next_token();
        PyToken::from_rust_token(py, &token)
    }

    fn prepend_token(&self, token: &PyToken) {
        let parser = self.parser();
        let pos = token.position.map(|(start, _end)| start);
        let source_len = token
            .position
            .map(|(start, end)| end - start)
            .unwrap_or(token.contents_rust.len());
        let rust_token = Token::new(token.kind, token.contents_rust.clone(), pos, token.lineno)
            .with_source_len(source_len);
        parser.prepend_token(rust_token);
    }

    fn delete_first_token(&self) {
        let parser = self.parser();
        parser.delete_first_token();
    }

    fn skip_past(&self, endtag: &str) -> PyResult<()> {
        let parser = self.parser();
        parser
            .skip_past(endtag)
            .map_err(<PyErr as From<TemplateError>>::from)
    }

    /// Returns a `PyFilterExpression` (`.resolve(context)`-able).
    fn compile_filter(&self, py: Python<'_>, token_string: &str) -> PyResult<Py<PyAny>> {
        let parser = self.parser();
        let fe = parser
            .compile_filter(token_string)
            .map_err(<PyErr as From<TemplateError>>::from)?;
        let wrapper = PyFilterExpression { inner: fe };
        Ok(Py::new(py, wrapper)?.into_any())
    }

    /// Mirrors `base.py:619-630`. Returns the exception (callers do
    /// `raise parser.error(...)`). We don't round-trip Tokens onto
    /// `.token` since third-party code rarely reads it.
    fn error(&self, py: Python<'_>, _token: &PyToken, e: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        if e.is_instance_of::<pyo3::exceptions::PyBaseException>() {
            return Ok(e.clone().unbind());
        }
        let exc_mod = py.import("django.template.exceptions")?;
        let exc_cls = exc_mod.getattr(intern!(py, "TemplateSyntaxError"))?;
        Ok(exc_cls.call1((e,))?.unbind())
    }

    /// Read-only snapshot of registered tags. Returns the merge of the
    /// Python shadow registry (every Python compile fn ever seen) plus
    /// live Python overrides in `parser.tags`. django-cotton's
    /// `snapshot_parser_library` needs the merged view so a captured
    /// library still understands `{% if %}` / `{% for %}` etc.
    #[getter]
    fn tags(&self, py: Python<'_>) -> PyResult<Py<pyo3::types::PyDict>> {
        let parser = self.parser();
        let dict = pyo3::types::PyDict::new(py);

        for (name, py_fn) in parser.python_tag_shadow.iter() {
            dict.set_item(name, py_fn.clone_ref(py))?;
        }

        // Live overrides win (e.g. {% load %} after the shadow's last write).
        for (name, compile_fn) in parser.tags.iter() {
            if let crate::parser::TagCompileFunc::Python(py_fn) = compile_fn {
                dict.set_item(name, py_fn.clone_ref(py))?;
            }
        }
        Ok(dict.unbind())
    }

    /// All filters as Python callables (built-ins are wrapped in
    /// `NativeFilterWrapper`). Mirrors `Parser.filters`.
    #[getter]
    fn filters(&self, py: Python<'_>) -> PyResult<Py<pyo3::types::PyDict>> {
        let parser = self.parser();
        let dict = pyo3::types::PyDict::new(py);
        for (name, py_fn) in parser.filters.iter() {
            dict.set_item(name, py_fn.clone_ref(py))?;
        }
        Ok(dict.unbind())
    }

    /// Django 5.2+ scratchpad dict (base.py:510). Persistent across
    /// compile-fn invocations within a parse pass.
    #[getter]
    fn extra_data(&self, py: Python<'_>) -> PyResult<Py<pyo3::types::PyDict>> {
        let parser = self.parser();
        if parser.python_extra_data.is_none() {
            parser.python_extra_data = Some(pyo3::types::PyDict::new(py).unbind());
        }
        Ok(parser.python_extra_data.as_ref().unwrap().clone_ref(py))
    }

    /// Mirrors `Parser.find_filter` (base.py:678).
    fn find_filter(&self, py: Python<'_>, filter_name: &str) -> PyResult<Py<PyAny>> {
        let parser = self.parser();
        match parser.filters.get(filter_name) {
            Some(f) => Ok(f.clone_ref(py)),
            None => {
                let exc_mod = py.import("django.template.exceptions")?;
                let exc_cls = exc_mod.getattr(intern!(py, "TemplateSyntaxError"))?;
                Err(pyo3::PyErr::from_value(
                    exc_cls.call1((format!("Invalid filter: '{}'", filter_name),))?,
                ))
            }
        }
    }

    /// `Parser.origin`; may be `None` (e.g. `Template(source)`).
    #[getter]
    fn origin(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let parser = self.parser();
        match &parser.origin {
            None => Ok(py.None()),
            Some(o) => {
                // name + template_name; loader is None.
                let mod_obj = py.import("django.template.base")?;
                let cls = mod_obj.getattr(intern!(py, "Origin"))?;
                let kwargs = pyo3::types::PyDict::new(py);
                kwargs.set_item(intern!(py, "name"), &o.name)?;
                if let Some(ref tn) = o.template_name {
                    kwargs.set_item(intern!(py, "template_name"), tn)?;
                }
                Ok(cls.call((), Some(&kwargs))?.unbind())
            }
        }
    }

    fn __repr__(&self) -> String {
        let parser = self.parser();
        format!(
            "<Parser tags={} filters={} tokens_remaining={}>",
            parser.tags.len(),
            parser.filters.len(),
            parser.tokens_remaining(),
        )
    }
}

impl PyParser {
    /// The bridge's single raw deref. Sound while the lent `&mut Parser`
    /// is live: the PyParser is dropped before the compile call returns,
    /// and the GIL serializes the (stack-nested) re-entrant access.
    #[allow(clippy::mut_from_ref)]
    fn parser(&self) -> &mut crate::parser::Parser {
        unsafe { &mut *self.parser_ptr }
    }

    /// # Safety
    ///
    /// `parser_ptr` must point to a live, exclusively-borrowed Parser.
    /// PyParser must be dropped before the Parser is moved or dropped.
    pub unsafe fn from_raw(parser_ptr: *mut crate::parser::Parser) -> Self {
        Self { parser_ptr }
    }
}

#[pyclass(name = "FilterExpression", module = "django_template_oxide._rust")]
pub struct PyFilterExpression {
    pub inner: crate::variable::FilterExpression,
}

#[pymethods]
impl PyFilterExpression {
    /// Mirrors `FilterExpression.resolve` (base.py:765).
    #[pyo3(signature = (context, ignore_failures=false))]
    fn resolve(
        &self,
        py: Python<'_>,
        context: &Bound<'_, PyAny>,
        ignore_failures: bool,
    ) -> PyResult<Py<PyAny>> {
        let py_ctx = context
            .cast::<crate::py_bindings::PyContext>()
            .map_err(|_| {
                pyo3::exceptions::PyTypeError::new_err(
                    "FilterExpression.resolve: context must be a django_template_oxide.Context",
                )
            })?;
        let borrowed = py_ctx.borrow();
        let value = if ignore_failures {
            crate::nodes::resolve_expression_ignore_failures(py, &self.inner, &borrowed.inner)
        } else {
            crate::nodes::resolve_expression_rust(py, &self.inner, &borrowed.inner)
        }
        .map_err(<PyErr as From<TemplateError>>::from)?;
        Ok(value.to_pyobject(py))
    }

    /// Parsed base. Django stores a `Variable` or resolved literal
    /// (base.py:782, 737); third-party introspection (django-cotton,
    /// debug-toolbar) reads this directly.
    #[getter]
    fn var(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        use crate::variable::FilterExpressionVar;
        match &self.inner.var {
            FilterExpressionVar::Var(variable) => {
                // Real Variable so `isinstance(fe.var, Variable)` works.
                let base_mod = py.import("django.template.base")?;
                let variable_cls = base_mod.getattr(intern!(py, "Variable"))?;
                let var_obj = variable_cls.call1((&variable.var,))?;
                Ok(var_obj.unbind())
            }
            FilterExpressionVar::Constant(Some(s)) => {
                let safestring = py.import("django.utils.safestring")?;
                let mark_safe = safestring.getattr(intern!(py, "mark_safe"))?;
                Ok(mark_safe.call1((s.as_str(),))?.unbind())
            }
            FilterExpressionVar::Constant(None) => Ok(py.None()),
        }
    }

    /// True iff the base is a Variable (base.py:783).
    #[getter]
    fn is_var(&self) -> bool {
        matches!(self.inner.var, crate::variable::FilterExpressionVar::Var(_))
    }

    /// Returns `[(name, [args])]` tuples so `len(fe.filters)` works.
    #[getter]
    fn filters(&self, py: Python<'_>) -> PyResult<Py<pyo3::types::PyList>> {
        let list = pyo3::types::PyList::empty(py);
        for parsed in &self.inner.filters {
            let args = pyo3::types::PyList::empty(py);
            for arg in &parsed.args {
                args.append(format!("{:?}", arg))?;
            }
            let tup = pyo3::types::PyTuple::new(
                py,
                [
                    parsed.name.clone().into_pyobject(py)?.into_any().unbind(),
                    args.unbind().into_any(),
                ],
            )?;
            list.append(tup)?;
        }
        Ok(list.unbind())
    }

    /// Verbatim expression token (base.py:843).
    fn __str__(&self) -> String {
        self.inner.token.clone()
    }

    fn __repr__(&self) -> String {
        format!("<FilterExpression {:?}>", self.inner.token)
    }
}

/// Register drop-in classes on the Rust module.
pub fn register(m: &Bound<'_, pyo3::types::PyModule>) -> PyResult<()> {
    m.add_class::<PyToken>()?;
    m.add_class::<PyNodeList>()?;
    m.add_class::<PyParser>()?;
    m.add_class::<PyFilterExpression>()?;
    Ok(())
}
