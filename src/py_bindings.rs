//! PyO3 bindings: `Template`, `Context`, `Engine` API-compatible with
//! Django's `django.template`.

use std::collections::HashMap;

use pyo3::exceptions::{PyKeyError, PyRuntimeError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTuple};

use crate::context::{self as ctx, Value};
use crate::filters::get_default_filters;
use crate::template;

fn pydict_to_context_dict(dict: Option<&Bound<'_, PyAny>>) -> PyResult<Option<ctx::ContextDict>> {
    match dict {
        None => Ok(None),
        Some(obj) => {
            if obj.is_none() {
                return Ok(None);
            }
            let pydict: &Bound<'_, PyDict> = obj.cast().map_err(|_| {
                PyRuntimeError::new_err("Context argument must be a dict or None")
            })?;
            let mut map = HashMap::new();
            for (k, v) in pydict.iter() {
                let key: String = k.extract()?;
                let value = Value::from(&v);
                map.insert(key, value);
            }
            Ok(Some(map))
        }
    }
}

fn context_dict_to_pydict<'py>(
    py: Python<'py>,
    dict: &HashMap<String, Value>,
) -> PyResult<Bound<'py, PyDict>> {
    let pydict = PyDict::new(py);
    for (k, v) in dict {
        pydict.set_item(k, v.to_pyobject(py))?;
    }
    Ok(pydict)
}

/// Clone-based PyContext bridge. Mutations don't propagate back; used
/// only on the rare Python custom-tag render path.
pub fn context_to_py_context(
    py: Python<'_>,
    context: &ctx::Context,
) -> PyResult<Py<PyContext>> {
    let cloned = context.clone();
    Py::new(py, PyContext { inner: cloned })
}

/// Drop-in `django.template.context.Context`.
#[pyclass(name = "Context", module = "django_template_oxide._rust")]
pub struct PyContext {
    pub inner: ctx::Context,
}

#[pymethods]
impl PyContext {
    /// `Context(dict_=None, autoescape=True, use_l10n=None, use_tz=None, string_if_invalid=None)`.
    #[new]
    #[pyo3(signature = (dict_=None, /, autoescape=true, use_l10n=None, use_tz=None, string_if_invalid=None))]
    fn new(
        dict_: Option<&Bound<'_, PyAny>>,
        autoescape: bool,
        use_l10n: Option<bool>,
        use_tz: Option<bool>,
        string_if_invalid: Option<String>,
    ) -> PyResult<Self> {
        let values = pydict_to_context_dict(dict_)?;
        let mut inner = ctx::Context::new(values);
        inner.autoescape = autoescape;
        inner.use_l10n = use_l10n;
        inner.use_tz = use_tz;
        inner.string_if_invalid = string_if_invalid.unwrap_or_default();
        Ok(PyContext { inner })
    }

    fn __getitem__(&self, py: Python<'_>, key: &str) -> PyResult<Py<PyAny>> {
        match self.inner.get(key) {
            Some(v) => Ok(v.to_pyobject(py)),
            None => Err(PyKeyError::new_err(key.to_owned())),
        }
    }

    /// Expose dict keys as attributes (mirrors RequestContext.request).
    /// `__getattr__` only fires on missed lookups so it can't shadow
    /// pyclass methods.
    fn __getattr__(&self, py: Python<'_>, name: &str) -> PyResult<Py<PyAny>> {
        match self.inner.get(name) {
            Some(v) => Ok(v.to_pyobject(py)),
            None => Err(pyo3::exceptions::PyAttributeError::new_err(name.to_owned())),
        }
    }

    fn __setitem__(&mut self, key: String, value: &Bound<'_, PyAny>) {
        self.inner.set(key, Value::from(value));
    }

    fn __contains__(&self, key: &str) -> bool {
        self.inner.contains(key)
    }

    /// Delete from the highest scope containing `key`.
    fn __delitem__(&mut self, key: &str) -> PyResult<()> {
        for d in self.inner.base.dicts.iter_mut().rev() {
            if d.contains_key(key) {
                d.remove(key);
                return Ok(());
            }
        }
        Err(PyKeyError::new_err(key.to_owned()))
    }

    /// `BaseContext.push` + `ContextDict` (context.py:14-26, 53-60).
    /// Returns a dict-subclass-context-manager that pops on exit.
    #[pyo3(signature = (*args, **kwargs))]
    fn push(
        slf: Bound<'_, Self>,
        args: &Bound<'_, PyTuple>,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Py<PyContextDict>> {
        let py = slf.py();
        let mut values: ctx::ContextDict = HashMap::new();

        if !args.is_empty() {
            let first = args.get_item(0)?;
            if let Ok(d) = first.cast::<PyDict>() {
                for (k, v) in d.iter() {
                    let key: String = k.extract()?;
                    values.insert(key, Value::from(&v));
                }
            }
        }

        if let Some(kw) = kwargs {
            for (k, v) in kw.iter() {
                let key: String = k.extract()?;
                values.insert(key, Value::from(&v));
            }
        }

        {
            let mut borrowed = slf.borrow_mut();
            if values.is_empty() {
                borrowed.inner.push();
            } else {
                borrowed.inner.push_with(values);
            }
        }

        Py::new(
            py,
            PyContextDict {
                context: slf.unbind(),
            },
        )
    }

    /// Backwards-compatibility shim for direct callers that still
    /// expect the old `push()` return type. Kept around but undocumented.
    #[allow(dead_code)]
    fn _push_no_cm(
        &mut self,
        args: &Bound<'_, PyTuple>,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<()> {
        let mut values: ctx::ContextDict = HashMap::new();

        if !args.is_empty() {
            let first = args.get_item(0)?;
            if let Ok(d) = first.cast::<PyDict>() {
                for (k, v) in d.iter() {
                    let key: String = k.extract()?;
                    values.insert(key, Value::from(&v));
                }
            }
        }

        if let Some(kw) = kwargs {
            for (k, v) in kw.iter() {
                let key: String = k.extract()?;
                values.insert(key, Value::from(&v));
            }
        }

        if values.is_empty() {
            self.inner.push();
        } else {
            self.inner.push_with(values);
        }
        Ok(())
    }

    fn pop(&mut self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        if self.inner.base.dicts.len() <= 1 {
            return Err(PyRuntimeError::new_err(
                "pop() called on Context with only the builtins layer remaining",
            ));
        }
        let popped = self.inner.pop();
        let pydict = context_dict_to_pydict(py, &popped)?;
        Ok(pydict.into_any().unbind())
    }

    #[pyo3(signature = (key, default=None))]
    fn get(&self, py: Python<'_>, key: &str, default: Option<&Bound<'_, PyAny>>) -> Py<PyAny> {
        match self.inner.get(key) {
            Some(v) => v.to_pyobject(py),
            None => match default {
                Some(d) => d.clone().unbind(),
                None => py.None(),
            },
        }
    }

    /// Pushes a new scope and returns `self` (context manager).
    fn update(slf: Py<Self>, py: Python<'_>, other_dict: &Bound<'_, PyDict>) -> PyResult<Py<Self>> {
        let mut this = slf.borrow_mut(py);
        let mut values: ctx::ContextDict = HashMap::new();
        for (k, v) in other_dict.iter() {
            let key: String = k.extract()?;
            values.insert(key, Value::from(&v));
        }
        this.inner.push_with(values);
        drop(this);
        Ok(slf)
    }

    fn flatten(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let flat = self.inner.flatten();
        let pydict = context_dict_to_pydict(py, &flat)?;
        Ok(pydict.into_any().unbind())
    }

    #[pyo3(signature = (values=None))]
    fn new_child(&self, values: Option<&Bound<'_, PyDict>>) -> PyResult<PyContext> {
        let dict = match values {
            Some(d) => {
                let mut map = HashMap::new();
                for (k, v) in d.iter() {
                    let key: String = k.extract()?;
                    map.insert(key, Value::from(&v));
                }
                Some(map)
            }
            None => None,
        };
        Ok(PyContext {
            inner: self.inner.new_child(dict),
        })
    }

    /// Mirrors `Context.new`. Exposed as Python `.new` (since `#[new]`
    /// reserves the Rust method name).
    #[pyo3(name = "new", signature = (values=None))]
    fn py_new(&self, values: Option<&Bound<'_, PyDict>>) -> PyResult<PyContext> {
        self.new_child(values)
    }

    fn __enter__(slf: Py<Self>, py: Python<'_>) -> Py<Self> {
        slf.borrow_mut(py).inner.push();
        slf
    }

    fn __exit__(
        &mut self,
        _exc_type: &Bound<'_, PyAny>,
        _exc_val: &Bound<'_, PyAny>,
        _exc_tb: &Bound<'_, PyAny>,
    ) -> bool {
        if self.inner.base.dicts.len() > 1 {
            self.inner.pop();
        }
        false
    }

    #[getter]
    fn autoescape(&self) -> bool {
        self.inner.autoescape
    }

    #[setter]
    fn set_autoescape(&mut self, value: bool) {
        self.inner.autoescape = value;
    }

    #[getter]
    fn use_l10n(&self) -> Option<bool> {
        self.inner.use_l10n
    }

    #[setter]
    fn set_use_l10n(&mut self, value: Option<bool>) {
        self.inner.use_l10n = value;
    }

    #[getter]
    fn use_tz(&self) -> Option<bool> {
        self.inner.use_tz
    }

    #[setter]
    fn set_use_tz(&mut self, value: Option<bool>) {
        self.inner.use_tz = value;
    }

    /// Mirrors `Context.template` (context.py:152).
    #[getter]
    fn template(&self, py: Python<'_>) -> Py<PyAny> {
        match &self.inner.template {
            Some(t) => t.obj.clone_ref(py),
            None => py.None(),
        }
    }

    /// Mirrors `Context.render_context`. The returned `PyRenderContext`
    /// mutates the underlying Rust `RenderContext` in place.
    #[getter]
    fn render_context(slf: Bound<'_, Self>) -> PyResult<Py<PyRenderContext>> {
        let py = slf.py();
        Py::new(
            py,
            PyRenderContext {
                context: slf.unbind(),
            },
        )
    }

    /// Set during Template.render (`context.template_name = self.name`).
    #[getter]
    fn template_name(&self, py: Python<'_>) -> Py<PyAny> {
        match &self.inner.template_name {
            Some(name) => pyo3::types::PyString::new(py, name).into_any().unbind(),
            None => py.None(),
        }
    }

    #[setter]
    fn set_template_name(&mut self, value: Option<String>) {
        self.inner.template_name = value;
    }

    /// Snapshot of `BaseContext.dicts`. Mutations on the returned list
    /// do NOT propagate back.
    #[getter]
    fn dicts(&self, py: Python<'_>) -> PyResult<Py<pyo3::types::PyList>> {
        let list = pyo3::types::PyList::empty(py);
        for d in self.inner.base.dicts.iter() {
            let py_dict = PyDict::new(py);
            for (k, v) in d.iter() {
                py_dict.set_item(k, v.to_pyobject(py))?;
            }
            list.append(py_dict)?;
        }
        Ok(list.unbind())
    }

    /// Context manager mirroring `Context.bind_template` (context.py:155-163).
    fn bind_template(
        slf: Bound<'_, Self>,
        template: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyContextBoundTemplate>> {
        let py = slf.py();
        {
            let mut borrowed = slf.borrow_mut();
            if borrowed.inner.template.is_some() {
                return Err(pyo3::exceptions::PyRuntimeError::new_err(
                    "Context is already bound to a template",
                ));
            }
            // Capture template.name as Django's Template.render does.
            let name: Option<String> = template
                .getattr(pyo3::intern!(py, "name"))
                .ok()
                .and_then(|v| v.extract::<Option<String>>().ok().flatten());
            borrowed.inner.template = Some(crate::context::TemplateRef {
                name: name.unwrap_or_default(),
                obj: template.clone().unbind(),
            });
        }
        Py::new(
            py,
            PyContextBoundTemplate {
                context: slf.unbind(),
            },
        )
    }

    fn __repr__(&self) -> String {
        let flat = self.inner.flatten();
        let keys: Vec<&str> = flat.keys().map(|k| k.as_str()).collect();
        format!("<Context keys={:?}>", keys)
    }

    fn __len__(&self) -> usize {
        self.inner.base.keys().len()
    }
}

/// Context-manager from `bind_template`; `__exit__` clears
/// `context.template`. Mirrors `Context.bind_template` (context.py:155-163).
#[pyclass(
    name = "ContextBoundTemplate",
    module = "django_template_oxide._rust"
)]
pub struct PyContextBoundTemplate {
    context: Py<PyContext>,
}

#[pymethods]
impl PyContextBoundTemplate {
    fn __enter__<'py>(slf: Bound<'py, Self>) -> Bound<'py, Self> {
        slf
    }

    fn __exit__(
        &self,
        py: Python<'_>,
        _exc_type: &Bound<'_, PyAny>,
        _exc_value: &Bound<'_, PyAny>,
        _traceback: &Bound<'_, PyAny>,
    ) -> PyResult<bool> {
        let mut ctx = self.context.bind(py).borrow_mut();
        ctx.inner.template = None;
        Ok(false)
    }
}

/// Pops the parent scope on `__exit__`. Mirrors
/// `django.template.context.ContextDict` minus the `dict` subclass.
#[pyclass(name = "ContextDict", module = "django_template_oxide._rust")]
pub struct PyContextDict {
    context: Py<PyContext>,
}

#[pymethods]
impl PyContextDict {
    fn __enter__<'py>(slf: Bound<'py, Self>) -> Bound<'py, Self> {
        slf
    }

    fn __exit__(
        &self,
        py: Python<'_>,
        _exc_type: &Bound<'_, PyAny>,
        _exc_value: &Bound<'_, PyAny>,
        _traceback: &Bound<'_, PyAny>,
    ) -> PyResult<bool> {
        let mut ctx = self.context.bind(py).borrow_mut();
        if ctx.inner.base.dicts.len() > 1 {
            ctx.inner.pop();
        }
        Ok(false)
    }
}

/// Borrow-through view of a `Context`'s `RenderContext`. Mirrors
/// Django's top-only `__getitem__` semantics (context.py).
#[pyclass(
    name = "RenderContext",
    module = "django_template_oxide._rust"
)]
pub struct PyRenderContext {
    context: Py<PyContext>,
}

#[pymethods]
impl PyRenderContext {
    /// Currently bound template (mirrors `RenderContext.template`).
    #[getter]
    fn template(&self, py: Python<'_>) -> Py<PyAny> {
        let ctx = self.context.bind(py).borrow();
        match &ctx.inner.render_context.template {
            Some(t) => t.obj.clone_ref(py),
            None => py.None(),
        }
    }

    /// Accepts a template (reads `.name`) or `None` to clear. Direct
    /// assignment used by Cotton's `InlineTemplate.render`.
    #[setter]
    fn set_template(&mut self, value: &Bound<'_, PyAny>) -> PyResult<()> {
        let py = value.py();
        let mut ctx = self.context.bind(py).borrow_mut();
        if value.is_none() {
            ctx.inner.render_context.template = None;
        } else {
            let name: String = value
                .getattr(pyo3::intern!(py, "name"))
                .ok()
                .and_then(|v| v.extract::<Option<String>>().ok().flatten())
                .unwrap_or_default();
            ctx.inner.render_context.template = Some(crate::context::TemplateRef {
                name,
                obj: value.clone().unbind(),
            });
        }
        Ok(())
    }

    /// Top-of-stack lookup. Accepts arbitrary Python keys (mirrors
    /// `render_context[self] = ...` patterns).
    fn __getitem__(&self, key: &Bound<'_, PyAny>, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let rk = crate::context::RenderKey::from_py(key)?;
        let ctx = self.context.bind(py).borrow();
        match ctx.inner.render_context.get_key(&rk) {
            Some(v) => Ok(v.to_pyobject(py)),
            None => Err(pyo3::exceptions::PyKeyError::new_err(
                key.clone().unbind(),
            )),
        }
    }

    fn __setitem__(
        &mut self,
        key: &Bound<'_, PyAny>,
        value: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let rk = crate::context::RenderKey::from_py(key)?;
        let mut ctx = self.context.bind(value.py()).borrow_mut();
        ctx.inner
            .render_context
            .set_key(rk, crate::context::Value::from(value));
        Ok(())
    }

    fn __contains__(&self, key: &Bound<'_, PyAny>, py: Python<'_>) -> PyResult<bool> {
        let rk = crate::context::RenderKey::from_py(key)?;
        let ctx = self.context.bind(py).borrow();
        Ok(ctx.inner.render_context.contains_key(&rk))
    }

    #[pyo3(signature = (key, default=None))]
    fn get(
        &self,
        key: &Bound<'_, PyAny>,
        default: Option<Py<PyAny>>,
        py: Python<'_>,
    ) -> PyResult<Py<PyAny>> {
        let rk = crate::context::RenderKey::from_py(key)?;
        let ctx = self.context.bind(py).borrow();
        Ok(match ctx.inner.render_context.get_key(&rk) {
            Some(v) => v.to_pyobject(py),
            None => default.unwrap_or_else(|| py.None()),
        })
    }

    /// Mirrors `RenderContext.push_state`.
    #[pyo3(signature = (template, isolated_context=true))]
    fn push_state(
        slf: Bound<'_, Self>,
        template: &Bound<'_, PyAny>,
        isolated_context: bool,
    ) -> PyResult<Py<PyRenderContextState>> {
        let py = slf.py();

        let old_template = {
            let parent_ref = slf.borrow().context.clone_ref(py);
            let mut parent = parent_ref.bind(py).borrow_mut();
            let old = parent.inner.render_context.template.take();
            let name: Option<String> = template
                .getattr(pyo3::intern!(py, "name"))
                .ok()
                .and_then(|v| v.extract::<Option<String>>().ok().flatten());
            parent.inner.render_context.template =
                Some(crate::context::TemplateRef {
                    name: name.unwrap_or_default(),
                    obj: template.clone().unbind(),
                });
            if isolated_context {
                parent.inner.render_context.push();
            }
            old
        };

        Py::new(
            py,
            PyRenderContextState {
                context: slf.borrow().context.clone_ref(py),
                old_template: std::sync::Mutex::new(old_template),
                isolated_context,
            },
        )
    }

    fn __repr__(&self, py: Python<'_>) -> String {
        let ctx = self.context.bind(py).borrow();
        let keys = ctx.inner.render_context.keys();
        format!("<RenderContext keys={:?}>", keys)
    }
}

/// `__exit__` restores the saved template ref and pops the dict layer
/// if `isolated_context` was true. Mirrors the `try/finally` of
/// `RenderContext.push_state`.
#[pyclass(
    name = "RenderContextState",
    module = "django_template_oxide._rust"
)]
pub struct PyRenderContextState {
    context: Py<PyContext>,
    /// `Mutex` so we can move out on `__exit__(&self)` while keeping
    /// the pyclass `Sync` (GC may drop from any thread). Zero contention.
    old_template: std::sync::Mutex<Option<crate::context::TemplateRef>>,
    isolated_context: bool,
}

#[pymethods]
impl PyRenderContextState {
    fn __enter__<'py>(slf: Bound<'py, Self>) -> Bound<'py, Self> {
        slf
    }

    fn __exit__(
        &self,
        py: Python<'_>,
        _exc_type: &Bound<'_, PyAny>,
        _exc_value: &Bound<'_, PyAny>,
        _traceback: &Bound<'_, PyAny>,
    ) -> PyResult<bool> {
        let mut ctx = self.context.bind(py).borrow_mut();
        // Restore template before pop (matches Django's finally order).
        ctx.inner.render_context.template = self
            .old_template
            .lock()
            .expect("PyRenderContextState mutex poisoned")
            .take();
        if self.isolated_context && ctx.inner.render_context.dicts.len() > 1 {
            ctx.inner.render_context.pop();
        }
        Ok(false)
    }
}

/// Drop-in `django.template.base.Template`. `unsendable` (NodeList
/// contains non-Send `dyn Node`).
#[pyclass(name = "Template", module = "django_template_oxide._rust")]
pub struct PyTemplate {
    inner: template::Template,
    origin_value: Option<Py<PyAny>>,
    engine_value: Option<Py<PyAny>>,
}

#[pymethods]
impl PyTemplate {
    /// `Template(template_string, engine=None, origin=None, name=None)`.
    #[new]
    #[pyo3(signature = (template_string, engine=None, origin=None, name=None))]
    fn new(
        py: Python<'_>,
        template_string: &str,
        engine: Option<Py<PyAny>>,
        origin: Option<Py<PyAny>>,
        name: Option<String>,
    ) -> PyResult<Self> {
        // Match Django: fall back to `Engine.get_default()`. Without
        // this, non-native filters like `|slugify` would fail because
        // `parser.filters` would stay empty.
        let engine = match engine {
            Some(e) => Some(e),
            None => py
                .import("django.template.engine")
                .and_then(|m| m.getattr("Engine"))
                .and_then(|cls| cls.call_method0("get_default"))
                .map(|e| e.unbind())
                .ok(),
        };

        let (debug, string_if_invalid) = match &engine {
            Some(eng) => {
                let eng_bound = eng.bind(py);
                let debug = eng_bound
                    .getattr("debug")
                    .and_then(|v: Bound<'_, PyAny>| v.extract::<bool>())
                    .unwrap_or(false);
                let sii = eng_bound
                    .getattr("string_if_invalid")
                    .and_then(|v: Bound<'_, PyAny>| v.extract::<String>())
                    .ok()
                    .filter(|s: &String| !s.is_empty());
                (debug, sii)
            }
            None => (false, None),
        };

        // Engine -> compile_nodelist so its template_builtins
        // (cotton, debug_toolbar, OPTIONS["builtins"]) register
        // on the parser before parsing.
        let engine_bound = engine.as_ref().map(|e| e.bind(py));
        let inner = template::Template::new_with_engine(
            template_string,
            name,
            debug,
            string_if_invalid,
            engine_bound.as_ref().map(|b| b as &pyo3::Bound<'_, pyo3::PyAny>),
        )
        .map_err(|e| -> PyErr { e.into() })?;

        Ok(PyTemplate {
            inner,
            origin_value: origin,
            engine_value: engine,
        })
    }

    #[pyo3(signature = (context=None))]
    fn render(
        slf: Bound<'_, Self>,
        context: Option<&Bound<'_, PyAny>>,
        py: Python<'_>,
    ) -> PyResult<String> {
        let _g_entry = crate::prof::Guard::new("PyTemplate::render:entry");
        let this = slf.borrow();
        let mut rust_context = match context {
            Some(obj) => {
                if obj.is_none() {
                    ctx::Context::new(None)
                } else if let Ok(pyctx) = obj.cast::<PyContext>() {
                    let _g = crate::prof::Guard::new("PyTemplate::render:from_PyContext");
                    // Clone so rendering doesn't mutate the caller's.
                    pyctx.borrow().inner.clone()
                } else if let Ok(d) = obj.cast::<PyDict>() {
                    let _g = crate::prof::Guard::new("PyTemplate::render:from_dict");
                    let mut map = HashMap::new();
                    for (k, v) in d.iter() {
                        let key: String = k.extract()?;
                        map.insert(key, Value::from(&v));
                    }
                    ctx::Context::new(Some(map))
                } else if obj
                    .hasattr(pyo3::intern!(py, "flatten"))
                    .unwrap_or(false)
                    && obj
                        .hasattr(pyo3::intern!(py, "autoescape"))
                        .unwrap_or(false)
                {
                    // Duck-type as django.template.context.Context;
                    // flatten + copy engine settings into our Context.
                    let flat = obj.call_method0(pyo3::intern!(py, "flatten"))?;
                    let flat_dict = flat.cast::<PyDict>().map_err(|_| {
                        PyRuntimeError::new_err(
                            "Context.flatten() did not return a dict",
                        )
                    })?;
                    let mut map = HashMap::new();
                    for (k, v) in flat_dict.iter() {
                        let key: String = k.extract()?;
                        map.insert(key, Value::from(&v));
                    }
                    let mut ctx = ctx::Context::new(Some(map));
                    if let Ok(ae) =
                        obj.getattr(pyo3::intern!(py, "autoescape"))
                    {
                        ctx.autoescape = ae.extract::<bool>().unwrap_or(true);
                    }
                    if let Ok(use_l10n) =
                        obj.getattr(pyo3::intern!(py, "use_l10n"))
                    {
                        ctx.use_l10n = use_l10n
                            .extract::<Option<bool>>()
                            .ok()
                            .flatten();
                    }
                    if let Ok(use_tz) =
                        obj.getattr(pyo3::intern!(py, "use_tz"))
                    {
                        ctx.use_tz = use_tz
                            .extract::<Option<bool>>()
                            .ok()
                            .flatten();
                    }
                    ctx
                } else {
                    return Err(PyRuntimeError::new_err(
                        "render() argument must be a Context, dict, or None",
                    ));
                }
            }
            None => ctx::Context::new(None),
        };

        // Propagate string_if_invalid from the template to the context
        // if the context doesn't already have one set.
        if rust_context.string_if_invalid.is_empty() {
            rust_context.string_if_invalid = this.inner.string_if_invalid.clone();
        }

        // Bind `self` to `context.template` for the duration of the
        // render - mirrors Django's `Template.render` which wraps the
        // nodelist call in `with context.bind_template(self):`. Many
        // Python tag implementations (InclusionNode, debug tooling,
        // the partial template resolver, several
        // `django.contrib.*` load tags) reach for
        // `context.template.engine` mid-render, so leaving it `None`
        // produces `'NoneType' object has no attribute 'engine'` at
        // the worst possible moment. Skip the bind if a parent
        // already bound a template (matches Django's idempotent
        // wrap).
        let already_bound = rust_context.template.is_some();
        if !already_bound {
            let name = this.inner.name.clone().unwrap_or_default();
            rust_context.template = Some(crate::context::TemplateRef {
                name,
                obj: slf.clone().into_any().unbind(),
            });
            if rust_context.template_name.is_none() {
                rust_context.template_name = this.inner.name.clone();
            }
        }

        let result: Result<String, crate::errors::TemplateError> =
            this.inner.render(py, &mut rust_context);

        if !already_bound {
            rust_context.template = None;
            rust_context.template_name = None;
        }

        result.map_err(|e| -> PyErr { e.into() })
    }

    #[getter]
    fn source(&self) -> &str {
        &self.inner.source
    }

    /// Exposes only Python-backed nodes (PyOpaqueNode). Native Rust
    /// nodes have no Python representation `isinstance` could match.
    /// django-cotton's `_extract_vars_from_template` relies on this.
    #[getter]
    fn nodelist(&self, py: Python<'_>) -> PyResult<Py<pyo3::types::PyList>> {
        let list = pyo3::types::PyList::empty(py);
        for entry in self.inner.nodelist.iter_entries() {
            if let crate::nodes::NodeEntry::Boxed(boxed) = entry {
                if let Some(py_node) = boxed.as_py_node() {
                    list.append(py_node.clone_ref(py))?;
                }
            }
        }
        Ok(list.unbind())
    }

    #[getter]
    fn name(&self) -> Option<&str> {
        self.inner.name.as_deref()
    }

    #[getter]
    fn origin(&self, py: Python<'_>) -> Py<PyAny> {
        match &self.origin_value {
            Some(o) => o.clone_ref(py),
            None => py.None(),
        }
    }

    #[getter]
    fn engine(&self, py: Python<'_>) -> Py<PyAny> {
        match &self.engine_value {
            Some(e) => e.clone_ref(py),
            None => py.None(),
        }
    }

    fn __repr__(&self) -> String {
        match &self.inner.name {
            Some(n) => format!("<Template name=\"{}\">", n),
            None => "<Template>".to_owned(),
        }
    }
}

/// Drop-in `django.template.engine.Engine`. `unsendable` because
/// `from_string` returns `PyTemplate`.
#[pyclass(name = "Engine", module = "django_template_oxide._rust")]
pub struct PyEngine {
    dirs: Vec<String>,
    #[allow(dead_code)]
    app_dirs: bool,
    debug: bool,
    string_if_invalid: String,
    autoescape: bool,
    /// Forwarded to Django's loader infrastructure.
    #[allow(dead_code)]
    libraries: Option<Py<PyAny>>,
    #[allow(dead_code)]
    builtins: Option<Py<PyAny>>,
}

#[pymethods]
impl PyEngine {
    #[new]
    #[pyo3(signature = (
        dirs=None,
        app_dirs=false,
        debug=false,
        string_if_invalid=None,
        libraries=None,
        builtins=None,
        autoescape=true,
    ))]
    fn new(
        dirs: Option<Vec<String>>,
        app_dirs: bool,
        debug: bool,
        string_if_invalid: Option<String>,
        libraries: Option<Py<PyAny>>,
        builtins: Option<Py<PyAny>>,
        autoescape: bool,
    ) -> Self {
        PyEngine {
            dirs: dirs.unwrap_or_default(),
            app_dirs,
            debug,
            string_if_invalid: string_if_invalid.unwrap_or_default(),
            autoescape,
            libraries,
            builtins,
        }
    }

    fn from_string(&self, template_code: &str, py: Python<'_>) -> PyResult<PyTemplate> {
        let sii = if self.string_if_invalid.is_empty() {
            None
        } else {
            Some(self.string_if_invalid.clone())
        };

        let inner = template::Template::new(template_code, None, self.debug, sii)
            .map_err(|e| -> PyErr { e.into() })?;

        // Engine settings as a dict so `template.engine.string_if_invalid` works.
        let engine_dict = PyDict::new(py);
        engine_dict.set_item("debug", self.debug)?;
        engine_dict.set_item("string_if_invalid", &self.string_if_invalid)?;
        engine_dict.set_item("autoescape", self.autoescape)?;

        Ok(PyTemplate {
            inner,
            origin_value: None,
            engine_value: Some(engine_dict.into_any().unbind()),
        })
    }

    /// Delegates to Django's `Engine.get_template`.
    fn get_template(&self, template_name: &str, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let django_engine = py.import("django.template")?.getattr("engines")?;
        let engine = django_engine.get_item("django")?;
        let tmpl = engine.call_method1("get_template", (template_name,))?;
        Ok(tmpl.unbind())
    }

    #[getter]
    fn dirs(&self) -> Vec<String> {
        self.dirs.clone()
    }

    #[getter]
    fn debug(&self) -> bool {
        self.debug
    }

    #[getter]
    fn string_if_invalid(&self) -> &str {
        &self.string_if_invalid
    }

    #[getter]
    fn autoescape(&self) -> bool {
        self.autoescape
    }

    #[classmethod]
    fn get_default(_cls: &Bound<'_, pyo3::types::PyType>, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let engine_mod = py.import("django.template")?;
        let engine_cls = engine_mod.getattr("Engine")?;
        let default = engine_cls.call_method0("get_default")?;
        Ok(default.unbind())
    }

    fn __repr__(&self) -> String {
        format!(
            "<Engine dirs={:?} debug={} autoescape={}>",
            self.dirs, self.debug, self.autoescape
        )
    }
}

/// Convenience render-from-string.
#[pyfunction]
#[pyo3(signature = (template_name_or_string, context=None, request=None))]
fn render_to_string(
    py: Python<'_>,
    template_name_or_string: &str,
    context: Option<&Bound<'_, PyAny>>,
    request: Option<&Bound<'_, PyAny>>,
) -> PyResult<String> {
    let _ = request; // RequestContext support TBD.

    let ctx_dict = match context {
        Some(obj) => {
            if obj.is_none() {
                None
            } else if let Ok(d) = obj.cast::<PyDict>() {
                let mut map = HashMap::new();
                for (k, v) in d.iter() {
                    let key: String = k.extract()?;
                    map.insert(key, Value::from(&v));
                }
                Some(map)
            } else {
                return Err(PyRuntimeError::new_err(
                    "context must be a dict or None",
                ));
            }
        }
        None => None,
    };

    let inner = template::Template::new(template_name_or_string, None, false, None)
        .map_err(|e| -> PyErr { e.into() })?;
    let mut rust_context = ctx::Context::new(ctx_dict);
    inner
        .render(py, &mut rust_context)
        .map_err(|e| -> PyErr { e.into() })
}

/// Python callable wrapping a native Rust filter, so the parser /
/// `FilterExpression` can call native and user filters uniformly.
#[pyclass(name = "NativeFilterWrapper", module = "django_template_oxide._rust")]
pub struct NativeFilterWrapper {
    name: String,
    func: fn(&Value, &[Value], bool) -> Value,
    #[pyo3(get)]
    is_safe: bool,
    #[pyo3(get)]
    needs_autoescape: bool,
    #[pyo3(get)]
    expects_localtime: bool,
}

#[pymethods]
impl NativeFilterWrapper {
    #[pyo3(signature = (value, *args, autoescape=None))]
    fn __call__(
        &self,
        value: &Bound<'_, PyAny>,
        args: &Bound<'_, PyTuple>,
        autoescape: Option<bool>,
    ) -> PyResult<Py<PyAny>> {
        let py = value.py();
        let rust_value = py_to_value(py, value);
        let mut rust_args: Vec<Value> = Vec::with_capacity(args.len());
        for i in 0..args.len() {
            let arg = args.get_item(i)?;
            rust_args.push(py_to_value(py, &arg));
        }
        let ae = autoescape.unwrap_or(false);
        let result = (self.func)(&rust_value, &rust_args, ae);
        Ok(value_to_py(py, &result))
    }

    fn __repr__(&self) -> String {
        format!("<NativeFilterWrapper '{}'>", self.name)
    }
}

/// Python -> `Value`, preserving SafeData.
fn py_to_value(py: Python<'_>, obj: &Bound<'_, PyAny>) -> Value {
    let is_safe = py
        .import("django.utils.safestring")
        .and_then(|m| m.getattr("SafeData"))
        .and_then(|cls| obj.is_instance(&cls))
        .unwrap_or(false);

    let mut val = Value::from(obj);

    if is_safe {
        if let Value::String(s) = val {
            val = Value::SafeString(s.into());
        }
    }

    val
}

/// Convert a Rust `Value` to a Python object, preserving safe-string status.
fn value_to_py(py: Python<'_>, val: &Value) -> Py<PyAny> {
    match val {
        Value::SafeString(s) => {
            // Wrap in mark_safe to preserve safety through the filter chain.
            if let Ok(mark_safe) = py
                .import("django.utils.safestring")
                .and_then(|m| m.getattr("mark_safe"))
            {
                let s_ref: &str = &s;
                if let Ok(result) = mark_safe.call1((s_ref,)) {
                    return result.unbind();
                }
            }
            // Fallback: return as a plain string.
            let s_ref: &str = &s;
            s_ref.into_pyobject(py).unwrap().into_any().unbind()
        }
        _ => val.to_pyobject(py),
    }
}

/// Filters delegated to Django's Python implementations.
const PYTHON_DELEGATED_FILTERS: &[&str] = &["date", "time", "timesince", "timeuntil"];

/// Register native filters as `NativeFilterWrapper`s on the parser.
/// Date/time filters route to `django.template.defaultfilters` instead.
pub fn register_default_filters(py: Python<'_>, parser: &mut crate::parser::Parser) {
    let native_filters = get_default_filters();

    let django_filters = py
        .import("django.template.defaultfilters")
        .ok();

    for (name, native_filter) in native_filters {
        if PYTHON_DELEGATED_FILTERS.contains(&name.as_str()) {
            if let Some(ref df_module) = django_filters {
                if let Ok(py_filter) = df_module.getattr(name.as_str()) {
                    parser.filters.insert(name.clone(), py_filter.unbind());
                    continue;
                }
            }
            // Django not available: fall through to the Rust stub.
        }

        let wrapper = NativeFilterWrapper {
            name: name.clone(),
            func: native_filter.func,
            is_safe: native_filter.is_safe,
            needs_autoescape: native_filter.needs_autoescape,
            expects_localtime: native_filter.expects_localtime,
        };

        let py_wrapper = Py::new(py, wrapper)
            .expect("Failed to create NativeFilterWrapper");

        parser.filters.insert(name.clone(), py_wrapper.into_any());
    }
}

/// Register Python bindings on the module.
pub fn register(m: &Bound<'_, pyo3::types::PyModule>) -> PyResult<()> {
    m.add_class::<PyContext>()?;
    m.add_class::<PyContextDict>()?;
    m.add_class::<PyContextBoundTemplate>()?;
    m.add_class::<PyRenderContext>()?;
    m.add_class::<PyRenderContextState>()?;
    m.add_class::<PyTemplate>()?;
    m.add_class::<PyEngine>()?;
    m.add_class::<NativeFilterWrapper>()?;
    m.add_function(wrap_pyfunction!(render_to_string, m)?)?;
    Ok(())
}
