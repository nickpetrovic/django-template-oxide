//! Cached Django modules/attributes. First call imports; subsequent
//! calls are a single pointer load + Arc bump. Centralised so every
//! Django dependency is grep-able from one file.

use std::sync::OnceLock;

use pyo3::prelude::*;
use pyo3::types::PyAnyMethods;

pub struct DjangoModules {
    pub template_base: Py<PyAny>,
    pub variable_cls: Py<PyAny>,
    pub variable_node_cls: Py<PyAny>,
    pub text_node_cls: Py<PyAny>,
    pub node_cls: Py<PyAny>,
    pub origin_cls: Py<PyAny>,
    pub render_value_in_context: Py<PyAny>,

    pub context_cls: Py<PyAny>,

    pub template_syntax_error_cls: Py<PyAny>,

    pub safestring: Py<PyAny>,
    pub mark_safe: Py<PyAny>,
    pub safe_data_cls: Py<PyAny>,

    pub translation: Py<PyAny>,
    pub gettext_lazy: Py<PyAny>,
    pub pgettext_lazy: Py<PyAny>,
}

impl DjangoModules {
    fn init(py: Python<'_>) -> PyResult<Self> {
        let base = py.import("django.template.base")?;
        let ctx = py.import("django.template.context")?;
        let exc = py.import("django.template.exceptions")?;
        let ss = py.import("django.utils.safestring")?;
        let tr = py.import("django.utils.translation")?;

        Ok(Self {
            variable_cls: base.getattr("Variable")?.unbind(),
            variable_node_cls: base.getattr("VariableNode")?.unbind(),
            text_node_cls: base.getattr("TextNode")?.unbind(),
            node_cls: base.getattr("Node")?.unbind(),
            origin_cls: base.getattr("Origin")?.unbind(),
            render_value_in_context: base.getattr("render_value_in_context")?.unbind(),
            template_base: base.into_any().unbind(),

            context_cls: ctx.getattr("Context")?.unbind(),

            template_syntax_error_cls: exc.getattr("TemplateSyntaxError")?.unbind(),

            mark_safe: ss.getattr("mark_safe")?.unbind(),
            safe_data_cls: ss.getattr("SafeData")?.unbind(),
            safestring: ss.into_any().unbind(),

            gettext_lazy: tr.getattr("gettext_lazy")?.unbind(),
            pgettext_lazy: tr.getattr("pgettext_lazy")?.unbind(),
            translation: tr.into_any().unbind(),
        })
    }
}

static DJANGO: OnceLock<DjangoModules> = OnceLock::new();

/// Errors if Django imports fail (broken install). First writer wins
/// the race; losers reference the same Python objects.
pub fn django(py: Python<'_>) -> PyResult<&'static DjangoModules> {
    if let Some(m) = DJANGO.get() {
        return Ok(m);
    }
    let modules = DjangoModules::init(py)?;
    let _ = DJANGO.set(modules);
    Ok(DJANGO.get().expect("DJANGO was just set or had a value"))
}
