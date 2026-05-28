//! `{% cache timeout fragment_name vary_on... %}`. Port of
//! `defaulttags.do_cache`. Renders via `django.core.cache`.

use std::collections::HashMap;

use pyo3::prelude::*;

use crate::context::{Context, Value};
use crate::errors::TemplateError;
use crate::impl_node_metadata;
use crate::lexer::Token;
use crate::nodes::{Node, NodeList, Origin};
use crate::parser::Parser;
use crate::variable::FilterExpression;

use super::resolve_if_value;

#[derive(Debug)]
pub struct CacheNode {
    expire_time_expr: FilterExpression,
    fragment_name: FilterExpression,
    vary_on: Vec<FilterExpression>,
    /// From `using "alias"`.
    cache_alias: Option<FilterExpression>,
    nodelist: NodeList,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for CacheNode {
    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        let timeout_val = resolve_if_value(py, &self.expire_time_expr, context);
        let timeout: Option<i64> = match &timeout_val {
            Value::Int(n) => Some(*n),
            Value::Float(f) => Some(*f as i64),
            Value::None => None,
            Value::String(s) => s.parse::<i64>().ok(),
            Value::SafeString(s) => s.parse::<i64>().ok(),
            _ => Some(0),
        };

        let frag_val = resolve_if_value(py, &self.fragment_name, context);
        let fragment_name = frag_val.to_string();

        let vary_on: Vec<String> = self
            .vary_on
            .iter()
            .map(|fe| {
                let val = resolve_if_value(py, fe, context);
                val.to_string()
            })
            .collect();

        let cache_alias = match &self.cache_alias {
            Some(fe) => {
                let val = resolve_if_value(py, fe, context);
                val.to_string()
            }
            None => "default".to_owned(),
        };

        let result: Result<String, TemplateError> = (|| {
            let cache_utils = py.import("django.templatetags.cache").map_err(|e| {
                TemplateError::Internal(format!("Cannot import django.templatetags.cache: {e}"))
            })?;
            let make_key = cache_utils.getattr("make_template_fragment_key").map_err(|e| {
                TemplateError::Internal(format!("Cannot get make_template_fragment_key: {e}"))
            })?;
            let py_vary: Vec<&str> = vary_on.iter().map(|s| s.as_str()).collect();
            let cache_key = make_key
                .call1((fragment_name.as_str(), py_vary))
                .map_err(|e| TemplateError::Internal(format!("make_template_fragment_key failed: {e}")))?
                .extract::<String>()
                .map_err(|e| TemplateError::Internal(format!("cache key not a string: {e}")))?;

            let caches = py
                .import("django.core.cache")
                .map_err(|e| TemplateError::Internal(format!("Cannot import django.core.cache: {e}")))?
                .getattr("caches")
                .map_err(|e| TemplateError::Internal(format!("Cannot get caches: {e}")))?;
            let cache = caches
                .get_item(&cache_alias)
                .map_err(|e| TemplateError::Internal(format!("Cannot get cache '{}': {e}", cache_alias)))?;

            let cached = cache
                .call_method1("get", (cache_key.as_str(),))
                .map_err(|e| TemplateError::Internal(format!("cache.get() failed: {e}")))?;

            if !cached.is_none() {
                return cached
                    .extract::<String>()
                    .map_err(|e| TemplateError::Internal(format!("cached value not a string: {e}")));
            }

            // Miss: render and cache.
            let safe = self.nodelist.render(py, context)?;
            let rendered = safe.as_str().to_owned();

            let set_kwargs = pyo3::types::PyDict::new(py);
            if let Some(t) = timeout {
                cache
                    .call_method("set", (cache_key.as_str(), rendered.as_str(), t), None)
                    .map_err(|e| TemplateError::Internal(format!("cache.set() failed: {e}")))?;
            } else {
                cache
                    .call_method1("set", (cache_key.as_str(), rendered.as_str()))
                    .map_err(|e| TemplateError::Internal(format!("cache.set() failed: {e}")))?;
            }
            let _ = set_kwargs;

            Ok(rendered)
        })();

        result
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &["nodelist"]
    }

    fn walk_children(&self, visit: &mut dyn FnMut(&NodeList)) {
        visit(&self.nodelist);
    }
}

/// {% cache timeout fragment_name [vary_on ...] [using "alias"] %}
pub fn compile_cache(parser: &mut Parser, token: &Token) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();
    if bits.len() < 3 {
        return Err(TemplateError::TemplateSyntaxError(
            "'cache' tag requires at least 2 arguments: timeout and fragment name.".into(),
        ));
    }

    let expire_time_expr = parser.compile_filter(&bits[1])?;
    let fragment_name = parser.compile_filter(&bits[2])?;

    let mut vary_on = Vec::new();
    let mut cache_alias = None;
    let mut i = 3;
    while i < bits.len() {
        if bits[i] == "using" {
            if i + 1 < bits.len() {
                cache_alias = Some(parser.compile_filter(&bits[i + 1])?);
                i += 2;
            } else {
                return Err(TemplateError::TemplateSyntaxError(
                    "'cache' tag expected a cache alias after 'using'.".into(),
                ));
            }
        } else {
            vary_on.push(parser.compile_filter(&bits[i])?);
            i += 1;
        }
    }

    let nodelist = parser.parse(&["endcache"])?;
    parser.delete_first_token();

    Ok(Box::new(CacheNode {
        expire_time_expr,
        fragment_name,
        vary_on,
        cache_alias,
        nodelist,
        token_field: None,
        origin_field: None,
    }))
}
