//! `{% url %}`. Port of `defaulttags.url`.

use once_cell::sync::OnceCell;
use pyo3::prelude::*;

use crate::context::{Context, Value};
use crate::errors::TemplateError;
use crate::impl_node_metadata;
use crate::lexer::Token;
use crate::nodes::{Node, Origin};
use crate::parser::Parser;
use crate::variable::FilterExpression;

use super::resolve_if_value;

static REVERSE_FN: OnceCell<Py<PyAny>> = OnceCell::new();

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
        let view_val = resolve_if_value(py, &self.view_name, context);
        let view_name_str = view_val.to_string();

        let args: Vec<Value> = self
            .args
            .iter()
            .map(|fe| resolve_if_value(py, fe, context))
            .collect();

        let kwargs: Vec<(String, Value)> = self
            .kwargs
            .iter()
            .map(|(k, fe)| (k.clone(), resolve_if_value(py, fe, context)))
            .collect();

        let url_result: Result<String, TemplateError> = (|| {
            let reverse = REVERSE_FN
                .get_or_try_init(|| {
                    py.import("django.urls")
                        .and_then(|m| m.getattr("reverse"))
                        .map(Bound::unbind)
                })
                .map_err(|e: PyErr| {
                    TemplateError::Internal(format!("Failed to get django.urls.reverse: {e}"))
                })?
                .bind(py);

            let py_args = if args.is_empty() {
                None
            } else {
                let list: Vec<_> = args.iter().map(|v| v.to_pyobject(py)).collect();
                Some(pyo3::types::PyList::new(py, list).map_err(|e| {
                    TemplateError::Internal(format!("Failed to create args list: {e}"))
                })?)
            };

            let py_kwargs = if kwargs.is_empty() {
                None
            } else {
                let dict = pyo3::types::PyDict::new(py);
                for (k, v) in &kwargs {
                    dict.set_item(k, v.to_pyobject(py)).map_err(|e| {
                        TemplateError::Internal(format!("Failed to set kwarg: {e}"))
                    })?;
                }
                Some(dict)
            };

            let call_kwargs = pyo3::types::PyDict::new(py);
            call_kwargs
                .set_item("viewname", view_name_str.as_str())
                .map_err(|e| TemplateError::Internal(format!("{e}")))?;
            if let Some(a) = py_args {
                call_kwargs
                    .set_item("args", a)
                    .map_err(|e| TemplateError::Internal(format!("{e}")))?;
            }
            if let Some(kw) = py_kwargs {
                call_kwargs
                    .set_item("kwargs", kw)
                    .map_err(|e| TemplateError::Internal(format!("{e}")))?;
            }

            // current_app for namespace resolution. Matches
            // URLNode.render: request.current_app -> resolver_match.namespace.
            if let Some(Value::PyObject(request_obj)) = context.get("request") {
                let bound = request_obj.bind(py);
                let current_app = match bound.getattr("current_app") {
                    Ok(val) => Some(val),
                    Err(_) => match bound.getattr("resolver_match") {
                        Ok(rm) if !rm.is_none() => rm.getattr("namespace").ok(),
                        _ => None,
                    },
                };
                if let Some(ref app) = current_app {
                    let _ = call_kwargs.set_item("current_app", app);
                }
            }

            let result = reverse.call((), Some(&call_kwargs)).map_err(|e| {
                // Check if this is a NoReverseMatch exception and
                // propagate it directly so callers can catch it.
                let is_no_reverse = py
                    .import("django.urls")
                    .ok()
                    .and_then(|m| m.getattr("NoReverseMatch").ok())
                    .map(|cls| e.is_instance(py, &cls))
                    .unwrap_or(false);
                if is_no_reverse {
                    TemplateError::PythonError(e)
                } else {
                    TemplateError::Internal(format!("reverse() failed: {e}"))
                }
            })?;
            result
                .extract::<String>()
                .map_err(|e| TemplateError::Internal(format!("reverse() result not a string: {e}")))
        })();

        match url_result {
            Ok(url) => {
                if let Some(ref asvar) = self.asvar {
                    context.set(asvar.clone(), Value::String(url));
                    Ok(String::new())
                } else {
                    // Django conditional_escapes URL output.
                    Ok(crate::nodes::render_value_in_context(
                        &Value::String(url),
                        context,
                    ))
                }
            }
            Err(e) => {
                if let Some(ref asvar) = self.asvar {
                    context.set(asvar.clone(), Value::String(String::new()));
                    Ok(String::new())
                } else {
                    Err(e)
                }
            }
        }
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &[]
    }
}

pub fn compile_url(parser: &mut Parser, token: &Token) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();
    if bits.len() < 2 {
        return Err(TemplateError::TemplateSyntaxError(
            "'url' tag requires at least one argument (the URL name).".into(),
        ));
    }

    let view_name = parser.compile_filter(&bits[1])?;

    let mut args = Vec::new();
    let mut kwargs = Vec::new();
    let mut asvar = None;

    let mut i = 2;
    while i < bits.len() {
        if bits[i] == "as" && i + 1 < bits.len() {
            asvar = Some(bits[i + 1].clone());
            break;
        }

        if let Some((key, val)) = bits[i].split_once('=') {
            let fe = parser.compile_filter(val)?;
            kwargs.push((key.to_owned(), fe));
        } else {
            let fe = parser.compile_filter(&bits[i])?;
            args.push(fe);
        }
        i += 1;
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
