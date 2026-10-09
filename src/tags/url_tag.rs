//! `{% url %}`. Port of `defaulttags.url`.

use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;

use crate::context::{Context, Value};
use crate::errors::TemplateError;
use crate::impl_node_metadata;
use crate::lexer::Token;
use crate::nodes::{Node, Origin};
use crate::parser::Parser;
use crate::variable::FilterExpression;

use super::resolve_value;

struct UrlFunctions {
    reverse: Py<PyAny>,
    no_reverse_match: Py<PyAny>,
}

static URL_FUNCTIONS: PyOnceLock<UrlFunctions> = PyOnceLock::new();

fn url_functions(py: Python<'_>) -> PyResult<&'static UrlFunctions> {
    URL_FUNCTIONS.get_or_try_init(py, || {
        let urls = py.import("django.urls")?;
        Ok(UrlFunctions {
            reverse: urls.getattr("reverse")?.unbind(),
            no_reverse_match: urls.getattr("NoReverseMatch")?.unbind(),
        })
    })
}

fn current_app<'py>(
    py: Python<'py>,
    request: &Bound<'py, PyAny>,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    let attribute_error =
        |error: &PyErr| error.is_instance_of::<pyo3::exceptions::PyAttributeError>(py);
    match request.getattr(pyo3::intern!(py, "current_app")) {
        Ok(app) => return Ok(Some(app)),
        Err(error) if attribute_error(&error) => {}
        Err(error) => return Err(error),
    }
    match request
        .getattr(pyo3::intern!(py, "resolver_match"))
        .and_then(|resolver| resolver.getattr(pyo3::intern!(py, "namespace")))
    {
        Ok(namespace) => Ok(Some(namespace)),
        Err(error) if attribute_error(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

#[derive(Debug)]
pub struct UrlNode {
    view_name: FilterExpression,
    args: Vec<FilterExpression>,
    kwargs: Vec<(String, FilterExpression)>,
    asvar: Option<String>,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for UrlNode {
    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        let call_kwargs = pyo3::types::PyDict::new(py);
        if !self.args.is_empty() {
            let mut args = Vec::with_capacity(self.args.len());
            for fe in &self.args {
                args.push(resolve_value(py, fe, context)?.to_pyobject(py));
            }
            call_kwargs.set_item(
                pyo3::intern!(py, "args"),
                pyo3::types::PyList::new(py, args)?,
            )?;
        }
        if !self.kwargs.is_empty() {
            let kwargs = pyo3::types::PyDict::new(py);
            for (key, fe) in &self.kwargs {
                kwargs.set_item(key, resolve_value(py, fe, context)?.to_pyobject(py))?;
            }
            call_kwargs.set_item(pyo3::intern!(py, "kwargs"), kwargs)?;
        }
        let view_name = resolve_value(py, &self.view_name, context)?;

        let functions = url_functions(py)?;

        if let Some(request) = &context.request
            && let Some(current_app) = current_app(py, request.bind(py))?
        {
            call_kwargs.set_item(pyo3::intern!(py, "current_app"), current_app)?;
        }
        let url = match functions
            .reverse
            .bind(py)
            .call((view_name.to_pyobject(py),), Some(&call_kwargs))
        {
            Ok(url) => Value::from(&url),
            Err(error)
                if self.asvar.is_some()
                    && error.is_instance(py, functions.no_reverse_match.bind(py)) =>
            {
                Value::String(String::new())
            }
            Err(error) => return Err(error.into()),
        };

        if let Some(ref asvar) = self.asvar {
            context.set(asvar.clone(), url);
            return Ok(String::new());
        }
        crate::nodes::render_value_in_context(&url, context)
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &[]
    }
}

pub fn compile_url(parser: &mut Parser, token: &Token) -> Result<Box<dyn Node>, TemplateError> {
    static KWARG_RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"^(?:(\w+)=)?(.+)").expect("valid regex"));

    let bits = token.split_contents();
    if bits.len() < 2 {
        return Err(TemplateError::TemplateSyntaxError(format!(
            "'{}' takes at least one argument, a URL pattern name.",
            bits[0]
        )));
    }

    let view_name = parser.compile_filter(&bits[1])?;

    let mut args = Vec::new();
    let mut kwargs = Vec::new();
    let mut asvar = None;

    let mut rest = &bits[2..];
    if rest.len() >= 2 && rest[rest.len() - 2] == "as" {
        asvar = Some(rest[rest.len() - 1].clone());
        rest = &rest[..rest.len() - 2];
    }
    for bit in rest {
        let captures = KWARG_RE.captures(bit).ok_or_else(|| {
            TemplateError::TemplateSyntaxError("Malformed arguments to url tag".into())
        })?;
        let fe = parser.compile_filter(&captures[2])?;
        match captures.get(1) {
            Some(name) => kwargs.push((name.as_str().to_owned(), fe)),
            None => args.push(fe),
        }
    }

    Ok(Box::new(UrlNode {
        view_name,
        args,
        kwargs,
        asvar,
        token_field: None,
        origin_field: None,
    }))
}
