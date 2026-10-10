//! `{% cache timeout fragment_name vary_on... %}`. Port of
//! `defaulttags.do_cache`. Renders via `django.core.cache`.

use pyo3::prelude::*;

use crate::context::Context;
use crate::errors::TemplateError;
use crate::impl_node_metadata;
use crate::lexer::Token;
use crate::nodes::{Node, NodeList, Origin};
use crate::parser::Parser;
use crate::variable::FilterExpression;

use super::{is_variable_does_not_exist, resolve_value};

#[derive(Debug)]
pub struct CacheNode {
    expire_time_expr: FilterExpression,
    /// Raw fragment name string (not a filter expression).
    fragment_name_str: String,
    vary_on: Vec<FilterExpression>,
    /// From `using="alias"`.
    cache_alias: Option<FilterExpression>,
    nodelist: NodeList,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for CacheNode {
    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        let unknown_variable = |fe: &FilterExpression| -> TemplateError {
            let described = match &fe.var {
                crate::variable::FilterExpressionVar::Var(variable) => {
                    match pyo3::types::PyString::new(py, &variable.var).repr() {
                        Ok(name) => format!("<Variable: {name}>"),
                        Err(error) => return error.into(),
                    }
                }
                crate::variable::FilterExpressionVar::Constant(Some(constant)) => {
                    match pyo3::types::PyString::new(py, constant).repr() {
                        Ok(text) => text.to_string(),
                        Err(error) => return error.into(),
                    }
                }
                crate::variable::FilterExpressionVar::Constant(None) => "None".to_owned(),
            };
            TemplateError::TemplateSyntaxError(format!(
                "\"cache\" tag got an unknown variable: {described}"
            ))
        };
        let builtins = py.import("builtins")?;
        let expire_time = match resolve_value(py, &self.expire_time_expr, context) {
            Ok(value) => value.to_pyobject(py).into_bound(py),
            Err(error) if is_variable_does_not_exist(py, &error) => {
                return Err(unknown_variable(&self.expire_time_expr));
            }
            Err(error) => return Err(error),
        };
        let expire_time = if expire_time.is_none() {
            expire_time
        } else {
            match builtins.getattr("int")?.call1((&expire_time,)) {
                Ok(seconds) => seconds,
                Err(error)
                    if error.is_instance_of::<pyo3::exceptions::PyValueError>(py)
                        || error.is_instance_of::<pyo3::exceptions::PyTypeError>(py) =>
                {
                    return Err(TemplateError::TemplateSyntaxError(format!(
                        "\"cache\" tag got a non-integer timeout value: {}",
                        expire_time.repr()?
                    )));
                }
                Err(error) => return Err(error.into()),
            }
        };

        let cache_module = py.import("django.core.cache")?;
        let caches = cache_module.getattr("caches")?;
        let invalid_backend = cache_module.getattr("InvalidCacheBackendError")?;
        let fragment_cache = match &self.cache_alias {
            Some(alias_expr) => {
                let alias = match resolve_value(py, alias_expr, context) {
                    Ok(alias) => alias.to_pyobject(py).into_bound(py),
                    Err(error) if is_variable_does_not_exist(py, &error) => {
                        return Err(unknown_variable(alias_expr));
                    }
                    Err(error) => return Err(error),
                };
                match caches.get_item(&alias) {
                    Ok(cache) => cache,
                    Err(error) if error.is_instance(py, &invalid_backend) => {
                        return Err(TemplateError::TemplateSyntaxError(format!(
                            "Invalid cache name specified for cache tag: {}",
                            alias.repr()?
                        )));
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            None => match caches.get_item("template_fragments") {
                Ok(cache) => cache,
                Err(error) if error.is_instance(py, &invalid_backend) => {
                    caches.get_item("default")?
                }
                Err(error) => return Err(error.into()),
            },
        };

        let mut vary_on = Vec::with_capacity(self.vary_on.len());
        for fe in &self.vary_on {
            vary_on.push(resolve_value(py, fe, context)?.to_pyobject(py));
        }
        let cache_key = py
            .import("django.core.cache.utils")?
            .getattr("make_template_fragment_key")?
            .call1((self.fragment_name_str.as_str(), vary_on))?;
        let cached = fragment_cache.call_method1("get", (&cache_key,))?;
        if !cached.is_none() {
            return Ok(cached.str()?.to_string());
        }
        let rendered = self.nodelist.render(py, context)?;
        let rendered = rendered.as_str().to_owned();
        let dj = crate::python_cache::django(py)?;
        let safe_value = dj.mark_safe.bind(py).call1((rendered.as_str(),))?;
        fragment_cache.call_method1("set", (&cache_key, safe_value, expire_time))?;
        Ok(rendered)
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &["nodelist"]
    }

    fn walk_children(&self, visit: &mut dyn FnMut(&NodeList)) {
        visit(&self.nodelist);
    }
}

/// {% cache timeout fragment_name [vary_on ...] [using="alias"] %}
pub fn compile_cache(parser: &mut Parser, token: &Token) -> Result<Box<dyn Node>, TemplateError> {
    let nodelist = parser.parse(&["endcache"])?;
    parser.delete_first_token();

    let bits = token.split_contents();
    if bits.len() < 3 {
        let tag_repr = crate::python_cache::attach(|py| -> PyResult<String> {
            Ok(pyo3::types::PyString::new(py, &bits[0]).repr()?.to_string())
        })?;
        return Err(TemplateError::TemplateSyntaxError(format!(
            "'{tag_repr}' tag requires at least 2 arguments."
        )));
    }

    let mut cache_alias = None;
    let mut end = bits.len();
    if bits.len() > 3 {
        let last = &bits[bits.len() - 1];
        if let Some(alias_str) = last.strip_prefix("using=") {
            cache_alias = Some(parser.compile_filter(alias_str)?);
            end = bits.len() - 1;
        }
    }

    let expire_time_expr = parser.compile_filter(&bits[1])?;
    let fragment_name_str = bits[2].clone();

    let mut vary_on = Vec::new();
    for bit in &bits[3..end] {
        vary_on.push(parser.compile_filter(bit)?);
    }

    Ok(Box::new(CacheNode {
        expire_time_expr,
        fragment_name_str,
        vary_on,
        cache_alias,
        nodelist,
        token_field: None,
        origin_field: None,
    }))
}
