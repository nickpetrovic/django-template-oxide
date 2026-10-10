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

    pub template_syntax_error_cls: Py<PyAny>,
    pub template_does_not_exist_cls: Py<PyAny>,
    pub variable_does_not_exist_cls: Py<PyAny>,

    pub safestring: Py<PyAny>,
    pub mark_safe: Py<PyAny>,
    pub safe_data_cls: Py<PyAny>,

    pub translation: Py<PyAny>,
    pub gettext_lazy: Py<PyAny>,
    pub pgettext_lazy: Py<PyAny>,

    pub template_localtime: Py<PyAny>,

    pub safe_string_cls: Py<PyAny>,
    pub localize: Py<PyAny>,
    pub settings: Py<PyAny>,
    pub builtin_filters: Py<PyAny>,
    pub get_language: Py<PyAny>,
    pub gettext: Py<PyAny>,
    pub dates: Py<PyAny>,
    pub decimal_cls: Py<PyAny>,
    pub get_format: Py<PyAny>,
    pub trans_real: Py<PyAny>,
}

impl DjangoModules {
    fn init(py: Python<'_>) -> PyResult<Self> {
        let base = py.import("django.template.base")?;
        let exc = py.import("django.template.exceptions")?;
        let ss = py.import("django.utils.safestring")?;
        let tr = py.import("django.utils.translation")?;
        let tz = py.import("django.utils.timezone")?;
        let formats = py.import("django.utils.formats")?;
        let conf = py.import("django.conf")?;
        let defaultfilters = py.import("django.template.defaultfilters")?;

        let variable_does_not_exist_cls = base.getattr("VariableDoesNotExist")?.unbind();

        Ok(Self {
            variable_cls: base.getattr("Variable")?.unbind(),
            variable_node_cls: base.getattr("VariableNode")?.unbind(),
            text_node_cls: base.getattr("TextNode")?.unbind(),
            node_cls: base.getattr("Node")?.unbind(),
            origin_cls: base.getattr("Origin")?.unbind(),
            template_base: base.into_any().unbind(),

            template_syntax_error_cls: exc.getattr("TemplateSyntaxError")?.unbind(),
            template_does_not_exist_cls: exc.getattr("TemplateDoesNotExist")?.unbind(),
            variable_does_not_exist_cls,

            mark_safe: ss.getattr("mark_safe")?.unbind(),
            safe_data_cls: ss.getattr("SafeData")?.unbind(),
            safe_string_cls: ss.getattr("SafeString")?.unbind(),
            safestring: ss.into_any().unbind(),

            gettext_lazy: tr.getattr("gettext_lazy")?.unbind(),
            pgettext_lazy: tr.getattr("pgettext_lazy")?.unbind(),
            get_language: tr.getattr("get_language")?.unbind(),
            gettext: tr.getattr("gettext")?.unbind(),
            translation: tr.into_any().unbind(),

            template_localtime: tz.getattr("template_localtime")?.unbind(),
            localize: formats.getattr("localize")?.unbind(),
            settings: conf.getattr("settings")?.unbind(),
            builtin_filters: defaultfilters
                .getattr("register")?
                .getattr("filters")?
                .unbind(),
            dates: py.import("django.utils.dates")?.into_any().unbind(),
            decimal_cls: py.import("decimal")?.getattr("Decimal")?.unbind(),
            get_format: formats.getattr("get_format")?.unbind(),
            trans_real: py
                .import("django.utils.translation.trans_real")?
                .into_any()
                .unbind(),
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

#[inline]
pub fn attach<F, R>(f: F) -> R
where
    F: for<'py> FnOnce(Python<'py>) -> R,
{
    if unsafe { pyo3::ffi::PyThreadState_GetUnchecked() }.is_null() {
        Python::attach(f)
    } else {
        f(unsafe { Python::assume_attached() })
    }
}
